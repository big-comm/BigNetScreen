//! Cast Streaming RTP packetisation.
//!
//! Cast does not use plain RTP: after the 12-byte RTP header comes a header of
//! its own carrying frame and packet identification, so the receiver can
//! reassemble the frame and ask for whatever is missing to be resent.
//!
//! ```text
//!  0                   1                   2                   3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |V=2|P|X| CC=0  |M|     PT      |      sequence number          |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                        RTP timestamp                          |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |           synchronization source (SSRC) identifier            |
//! +=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+
//! |K|R| EXT count |     FID       |             PID               |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |           Max PID             |  RFID (only when R=1)         |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```
//!
//! Reference: Open Screen's [`rtp_defines.h`] and [`rtp_packetizer.cc`].
//!
//! [`rtp_defines.h`]: https://chromium.googlesource.com/openscreen/+/HEAD/cast/streaming/impl/rtp_defines.h
//! [`rtp_packetizer.cc`]: https://chromium.googlesource.com/openscreen/+/HEAD/cast/streaming/impl/rtp_packetizer.cc

use aes::cipher::{KeyIvInit, StreamCipher};

/// Key-frame bit in the Cast header.
const KEY_FRAME_BIT: u8 = 0b1000_0000;
/// Bit signalling the presence of the reference frame id.
const REFERENCE_ID_BIT: u8 = 0b0100_0000;
/// RTP marker bit (the frame's last packet).
const MARKER_BIT: u8 = 0b1000_0000;
/// The RTP first byte: version 2, no padding, no extension, no CSRC.
const RTP_FIRST_BYTE: u8 = 0b1000_0000;

/// The fixed RTP header.
const RTP_HEADER_SIZE: usize = 12;
/// The Cast header without optional fields.
const CAST_HEADER_SIZE: usize = 6;

/// Maximum size of an RTP packet over IPv4/UDP on Ethernet.
///
/// 1500 (MTU) − 20 (IPv4) − 8 (UDP). Going past it fragments at the IP layer,
/// and one lost fragment takes the whole packet down.
pub const MAX_PACKET_SIZE: usize = 1500 - 20 - 8;

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

/// Encrypts a whole frame with AES-CTR-128.
///
/// The IV is derived from the frame id: its low 32 bits written big-endian at
/// offset 8 of a zeroed block, XORed with the mask negotiated in the OFFER.
/// This is Open Screen's construction (`frame_crypto.cc`); any divergence
/// produces unreadable video **with no error at all**.
pub fn encrypt_frame(key: &[u8; 16], iv_mask: &[u8; 16], frame_id: u32, data: &[u8]) -> Vec<u8> {
    let mut nonce = [0u8; 16];
    nonce[8..12].copy_from_slice(&frame_id.to_be_bytes());
    for (byte, mask) in nonce.iter_mut().zip(iv_mask.iter()) {
        *byte ^= mask;
    }

    let mut out = data.to_vec();
    let mut cipher = Aes128Ctr::new(key.into(), &nonce.into());
    cipher.apply_keystream(&mut out);
    out
}

/// An encoded frame ready to go out to the receiver.
#[derive(Clone, Debug)]
pub struct Frame<'a> {
    /// The frame's sequential identifier (starts at 0 and grows).
    pub frame_id: u32,
    /// The frame this one depends on. `None` on a key frame.
    pub reference_frame_id: Option<u32>,
    /// Timestamp in the stream's time base (90 kHz for video).
    pub rtp_timestamp: u32,
    /// The **already encrypted** data.
    pub payload: &'a [u8],
}

/// Packetisation state for one stream (video or audio).
#[derive(Debug)]
pub struct Packetizer {
    ssrc: u32,
    payload_type: u8,
    sequence: u16,
    max_packet_size: usize,
}

impl Packetizer {
    pub fn new(ssrc: u32, payload_type: u8) -> Self {
        Self {
            ssrc,
            payload_type,
            sequence: 0,
            max_packet_size: MAX_PACKET_SIZE,
        }
    }

    /// This stream's SSRC (used by RTCP as well).
    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// How many payload bytes fit in a packet.
    pub fn max_payload_size(&self) -> usize {
        // The worst case includes the reference frame id.
        self.max_packet_size - RTP_HEADER_SIZE - CAST_HEADER_SIZE - 1
    }

    /// Splits a frame into packets ready for the socket.
    pub fn packetize(&mut self, frame: &Frame<'_>) -> Vec<Vec<u8>> {
        let payload_limit = self.max_payload_size();
        // "At least one packet, even with no payload bytes" — an empty frame
        // still has to exist in the numbering, or the receiver stalls waiting
        // for it.
        let total = frame.payload.len().div_ceil(payload_limit).max(1);
        let max_packet_id = (total - 1) as u16;

        (0..total)
            .map(|index| {
                let start = index * payload_limit;
                let end = ((index + 1) * payload_limit).min(frame.payload.len());
                let chunk = &frame.payload[start..end];
                let is_last = index == total - 1;
                self.build_packet(frame, index as u16, max_packet_id, chunk, is_last)
            })
            .collect()
    }

    fn build_packet(
        &mut self,
        frame: &Frame<'_>,
        packet_id: u16,
        max_packet_id: u16,
        chunk: &[u8],
        is_last: bool,
    ) -> Vec<u8> {
        let is_key = frame.reference_frame_id.is_none();
        let mut packet = Vec::with_capacity(
            RTP_HEADER_SIZE + CAST_HEADER_SIZE + usize::from(!is_key) + chunk.len(),
        );

        // --- RTP ---
        packet.push(RTP_FIRST_BYTE);
        // The marker flags the frame's last packet.
        packet.push(if is_last { MARKER_BIT } else { 0 } | (self.payload_type & 0x7F));
        packet.extend_from_slice(&self.sequence.to_be_bytes());
        packet.extend_from_slice(&frame.rtp_timestamp.to_be_bytes());
        packet.extend_from_slice(&self.ssrc.to_be_bytes());
        self.sequence = self.sequence.wrapping_add(1);

        // --- Cast ---
        let mut flags = 0u8;
        if is_key {
            flags |= KEY_FRAME_BIT;
        }
        if frame.reference_frame_id.is_some() {
            flags |= REFERENCE_ID_BIT;
        }
        // No extensions: the count stays zero in the low 6 bits.
        packet.push(flags);
        // The ids travel truncated to 8 bits; the receiver reconstructs the
        // full value from what it has already seen.
        packet.push((frame.frame_id & 0xFF) as u8);
        packet.extend_from_slice(&packet_id.to_be_bytes());
        packet.extend_from_slice(&max_packet_id.to_be_bytes());
        if let Some(reference) = frame.reference_frame_id {
            packet.push((reference & 0xFF) as u8);
        }

        packet.extend_from_slice(chunk);
        packet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(packet: &[u8]) -> (u8, u8, u16, u32, u32, u8, u8, u16, u16) {
        (
            packet[0],
            packet[1],
            u16::from_be_bytes([packet[2], packet[3]]),
            u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
            u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]),
            packet[12],
            packet[13],
            u16::from_be_bytes([packet[14], packet[15]]),
            u16::from_be_bytes([packet[16], packet[17]]),
        )
    }

    #[test]
    fn key_frame_packet_has_the_right_header() {
        let mut p = Packetizer::new(100_001, 101);
        let payload = vec![0xAB; 100];
        let packets = p.packetize(&Frame {
            frame_id: 0,
            reference_frame_id: None,
            rtp_timestamp: 90_000,
            payload: &payload,
        });
        assert_eq!(packets.len(), 1);

        let (b0, b1, seq, ts, ssrc, flags, fid, pid, max_pid) = parse(&packets[0]);
        assert_eq!(b0, 0b1000_0000, "version 2, no padding/extension/CSRC");
        assert_eq!(b1 & 0x7F, 101, "payload type");
        assert_eq!(
            b1 & MARKER_BIT,
            MARKER_BIT,
            "a single packet is the last one"
        );
        assert_eq!(seq, 0);
        assert_eq!(ts, 90_000);
        assert_eq!(ssrc, 100_001);
        assert_eq!(flags & KEY_FRAME_BIT, KEY_FRAME_BIT, "it is a key frame");
        assert_eq!(
            flags & REFERENCE_ID_BIT,
            0,
            "a key frame references nothing"
        );
        assert_eq!(flags & 0b0011_1111, 0, "no extensions");
        assert_eq!(fid, 0);
        assert_eq!(pid, 0);
        assert_eq!(max_pid, 0);
        // No RFID: the payload starts right after the 18 header bytes.
        assert_eq!(&packets[0][18..], &payload[..]);
    }

    #[test]
    fn dependent_frame_carries_the_reference_id() {
        let mut p = Packetizer::new(7, 101);
        let payload = vec![1, 2, 3];
        let packets = p.packetize(&Frame {
            frame_id: 5,
            reference_frame_id: Some(4),
            rtp_timestamp: 0,
            payload: &payload,
        });
        let flags = packets[0][12];
        assert_eq!(flags & KEY_FRAME_BIT, 0);
        assert_eq!(flags & REFERENCE_ID_BIT, REFERENCE_ID_BIT);
        assert_eq!(packets[0][13], 5, "frame id");
        assert_eq!(packets[0][18], 4, "reference frame id");
        assert_eq!(&packets[0][19..], &payload[..]);
    }

    #[test]
    fn frame_ids_are_truncated_to_eight_bits() {
        // Ids go past 255 in a long session; the receiver reconstructs the
        // full value, but the field on the wire is 8 bits wide.
        let mut p = Packetizer::new(1, 101);
        let packets = p.packetize(&Frame {
            frame_id: 300,
            reference_frame_id: Some(299),
            rtp_timestamp: 0,
            payload: &[0u8; 4],
        });
        assert_eq!(packets[0][13], (300 & 0xFF) as u8);
        assert_eq!(packets[0][18], (299 & 0xFF) as u8);
    }

    #[test]
    fn a_large_frame_is_split_and_only_the_last_is_marked() {
        let mut p = Packetizer::new(1, 101);
        let size = p.max_payload_size() * 3 + 17;
        let payload = vec![0x5A; size];
        let packets = p.packetize(&Frame {
            frame_id: 1,
            reference_frame_id: Some(0),
            rtp_timestamp: 0,
            payload: &payload,
        });
        assert_eq!(packets.len(), 4);

        for (index, packet) in packets.iter().enumerate() {
            let (_, b1, seq, _, _, _, _, pid, max_pid) = parse(packet);
            assert_eq!(seq as usize, index, "a continuous sequence");
            assert_eq!(pid as usize, index);
            assert_eq!(max_pid, 3, "they all announce the same total");
            let is_last = index == 3;
            assert_eq!(
                b1 & MARKER_BIT != 0,
                is_last,
                "only the last packet carries the marker"
            );
            assert!(packet.len() <= MAX_PACKET_SIZE, "packet too large");
        }

        // No byte is lost or duplicated by the split.
        let rebuilt: Vec<u8> = packets.iter().flat_map(|p| p[19..].to_vec()).collect();
        assert_eq!(rebuilt, payload);
    }

    #[test]
    fn an_empty_frame_still_produces_one_packet() {
        let mut p = Packetizer::new(1, 101);
        let packets = p.packetize(&Frame {
            frame_id: 0,
            reference_frame_id: None,
            rtp_timestamp: 0,
            payload: &[],
        });
        assert_eq!(packets.len(), 1, "the receiver expects the frame to exist");
        assert_eq!(packets[0].len(), 18);
    }

    #[test]
    fn sequence_numbers_are_continuous_across_frames() {
        let mut p = Packetizer::new(1, 101);
        let mut expected = 0u16;
        for frame_id in 0..3 {
            let packets = p.packetize(&Frame {
                frame_id,
                reference_frame_id: (frame_id > 0).then(|| frame_id - 1),
                rtp_timestamp: frame_id * 3000,
                payload: &[0u8; 10],
            });
            for packet in packets {
                assert_eq!(u16::from_be_bytes([packet[2], packet[3]]), expected);
                expected = expected.wrapping_add(1);
            }
        }
    }

    #[test]
    fn packets_never_exceed_the_ethernet_mtu() {
        let mut p = Packetizer::new(1, 101);
        let payload = vec![0u8; 100_000];
        for packet in p.packetize(&Frame {
            frame_id: 2,
            reference_frame_id: Some(1),
            rtp_timestamp: 0,
            payload: &payload,
        }) {
            assert!(
                packet.len() <= MAX_PACKET_SIZE,
                "fragmentaria em IP: {} bytes",
                packet.len()
            );
        }
    }

    // --- criptografia ---

    #[test]
    fn encryption_round_trips() {
        // AES-CTR is symmetric: encrypting twice returns the original.
        let key = [0x11u8; 16];
        let mask = [0x22u8; 16];
        let plain = b"an encoded video frame".to_vec();
        let cipher = encrypt_frame(&key, &mask, 42, &plain);
        assert_ne!(cipher, plain);
        assert_eq!(encrypt_frame(&key, &mask, 42, &cipher), plain);
    }

    #[test]
    fn each_frame_gets_a_different_keystream() {
        // Reusing the keystream across frames would leak the content by XOR.
        let key = [0u8; 16];
        let mask = [0u8; 16];
        let plain = vec![0u8; 32];
        let a = encrypt_frame(&key, &mask, 1, &plain);
        let b = encrypt_frame(&key, &mask, 2, &plain);
        assert_ne!(a, b);
    }

    #[test]
    fn iv_derivation_matches_open_screen() {
        // The IV is the frame id big-endian at offset 8, XORed with the mask.
        // Verified by computing the keystream of a zeroed block with the
        // expected IV and comparing it against the function's output.
        use aes::cipher::{KeyIvInit, StreamCipher};

        let key = [0xAAu8; 16];
        let mask = [0x0Fu8; 16];
        let frame_id = 0x0102_0304u32;

        let mut expected_iv = [0u8; 16];
        expected_iv[8..12].copy_from_slice(&frame_id.to_be_bytes());
        for (byte, m) in expected_iv.iter_mut().zip(mask.iter()) {
            *byte ^= m;
        }

        let mut reference = vec![0u8; 48];
        let mut cipher = Aes128Ctr::new(&key.into(), &expected_iv.into());
        cipher.apply_keystream(&mut reference);

        let produced = encrypt_frame(&key, &mask, frame_id, &[0u8; 48]);
        assert_eq!(produced, reference, "the IV derivation diverged");
    }

    #[test]
    fn encryption_preserves_length() {
        // CTR is a stream cipher: the length does not change, and the split
        // into packets depends on that.
        for size in [0, 1, 15, 16, 17, 5000] {
            let out = encrypt_frame(&[3u8; 16], &[4u8; 16], 9, &vec![0u8; size]);
            assert_eq!(out.len(), size);
        }
    }
}
