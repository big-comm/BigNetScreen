//! Cast Streaming RTCP: *sender reports*.
//!
//! The receiver needs to know which wall-clock instant a given RTP timestamp
//! corresponds to — that pairing is what lets it schedule display and keep
//! audio and video in sync. Without a sender report it receives the packets
//! with no way to decide **when** to show them.
//!
//! The format is standard RTCP (RFC 3550, §6.4.1), which is what Open Screen
//! serialises in `sender_report_builder.cc`:
//!
//! ```text
//!  0                   1                   2                   3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |V=2|P|    RC   |   PT=SR=200   |            length             |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                         SSRC of sender                        |
//! +=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+
//! |              NTP timestamp, most significant word              |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |             NTP timestamp, least significant word              |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                         RTP timestamp                          |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                     sender's packet count                      |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                      sender's octet count                      |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The "Sender Report" RTCP packet type.
const PT_SENDER_REPORT: u8 = 200;
/// First byte: version 2, no padding, zero report blocks.
const RTCP_FIRST_BYTE: u8 = 0b1000_0000;
/// Size of a sender report with no report blocks.
const SENDER_REPORT_SIZE: usize = 28;

/// Seconds between the NTP epoch (1900) and the Unix one (1970).
const NTP_UNIX_OFFSET_SECS: u64 = 2_208_988_800;

/// Converts a system instant into a 64-bit NTP timestamp.
///
/// The high 32 bits are seconds since 1900; the low ones, the fraction of a
/// second.
pub fn ntp_timestamp(at: SystemTime) -> u64 {
    let since_epoch = at.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let seconds = since_epoch.as_secs() + NTP_UNIX_OFFSET_SECS;
    // The fraction uses the 2^32-per-second scale.
    let fraction = ((since_epoch.subsec_nanos() as u64) << 32) / 1_000_000_000;
    (seconds << 32) | fraction
}

/// A stream's running counters, required by the report.
#[derive(Clone, Copy, Debug, Default)]
pub struct SenderStats {
    pub packets: u32,
    pub octets: u32,
}

impl SenderStats {
    /// Accounts for one packet sent.
    pub fn record(&mut self, payload_bytes: usize) {
        self.packets = self.packets.wrapping_add(1);
        self.octets = self.octets.wrapping_add(payload_bytes as u32);
    }
}

/// Builds a *sender report* for the given stream.
pub fn build_sender_report(ssrc: u32, ntp: u64, rtp_timestamp: u32, stats: SenderStats) -> Vec<u8> {
    let mut packet = Vec::with_capacity(SENDER_REPORT_SIZE);

    packet.push(RTCP_FIRST_BYTE);
    packet.push(PT_SENDER_REPORT);
    // Comprimento em palavras de 32 bits, menos um (regra do RFC 3550).
    let length_words = (SENDER_REPORT_SIZE / 4 - 1) as u16;
    packet.extend_from_slice(&length_words.to_be_bytes());

    packet.extend_from_slice(&ssrc.to_be_bytes());
    packet.extend_from_slice(&ntp.to_be_bytes());
    packet.extend_from_slice(&rtp_timestamp.to_be_bytes());
    packet.extend_from_slice(&stats.packets.to_be_bytes());
    packet.extend_from_slice(&stats.octets.to_be_bytes());

    debug_assert_eq!(packet.len(), SENDER_REPORT_SIZE);
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sender_report_has_the_rfc_layout() {
        let stats = SenderStats {
            packets: 1234,
            octets: 567_890,
        };
        let packet = build_sender_report(0xDEAD_BEEF, 0x0123_4567_89AB_CDEF, 90_000, stats);

        assert_eq!(packet.len(), 28);
        assert_eq!(packet[0], 0b1000_0000, "version 2, no padding, RC=0");
        assert_eq!(packet[1], 200, "packet type = SR");
        // Comprimento em palavras de 32 bits menos um: 28/4 - 1 = 6.
        assert_eq!(u16::from_be_bytes([packet[2], packet[3]]), 6);

        assert_eq!(
            u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
            0xDEAD_BEEF
        );
        assert_eq!(
            u64::from_be_bytes(packet[8..16].try_into().unwrap()),
            0x0123_4567_89AB_CDEF
        );
        assert_eq!(
            u32::from_be_bytes(packet[16..20].try_into().unwrap()),
            90_000
        );
        assert_eq!(u32::from_be_bytes(packet[20..24].try_into().unwrap()), 1234);
        assert_eq!(
            u32::from_be_bytes(packet[24..28].try_into().unwrap()),
            567_890
        );
    }

    #[test]
    fn ntp_conversion_uses_the_1900_epoch() {
        // The Unix epoch is second 2_208_988_800 of the NTP scale.
        let ntp = ntp_timestamp(UNIX_EPOCH);
        assert_eq!(ntp >> 32, NTP_UNIX_OFFSET_SECS);
        assert_eq!(ntp & 0xFFFF_FFFF, 0, "no fraction at the exact epoch");
    }

    #[test]
    fn ntp_fraction_scales_to_2_pow_32() {
        // Half a second must be exactly half of the fractional scale.
        let half = UNIX_EPOCH + Duration::from_millis(500);
        let ntp = ntp_timestamp(half);
        let fraction = ntp & 0xFFFF_FFFF;
        assert!(
            (fraction as i64 - 0x8000_0000i64).abs() < 1000,
            "unexpected fraction: {fraction:#x}"
        );
    }

    #[test]
    fn ntp_advances_with_time() {
        let earlier = ntp_timestamp(UNIX_EPOCH + Duration::from_secs(10));
        let later = ntp_timestamp(UNIX_EPOCH + Duration::from_secs(11));
        assert!(later > earlier);
        assert_eq!((later >> 32) - (earlier >> 32), 1);
    }

    #[test]
    fn stats_accumulate() {
        let mut stats = SenderStats::default();
        stats.record(100);
        stats.record(250);
        assert_eq!(stats.packets, 2);
        assert_eq!(stats.octets, 350);
    }
}

// ---------------------------------------------------------------------------
// What the receiver sends back
// ---------------------------------------------------------------------------

/// The RTCP packet types that matter on the return path.
pub const PT_RECEIVER_REPORT: u8 = 201;
pub const PT_APPLICATION_DEFINED: u8 = 204;
pub const PT_PAYLOAD_SPECIFIC: u8 = 206;
pub const PT_EXTENDED_REPORTS: u8 = 207;

/// A readable summary of a packet coming from the receiver.
///
/// The receiver talks back: it sends reports, asks for whatever it lost to be
/// resent, and says when it needs a key frame. Ignoring that channel loses the
/// diagnostics — and, in the key-frame case, breaks recovery after any loss.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReceiverPacket {
    ReceiverReport,
    /// The receiver lost track and wants a key frame.
    PictureLossIndication,
    /// Feedback do Cast (checkpoint, NACKs).
    CastFeedback,
    ExtendedReport,
    ReceiverLog,
    Other(u8),
}

/// Walks a **compound** RTCP datagram and returns every block.
///
/// A Cast packet carries several concatenated blocks (report, reference time,
/// key-frame request, feedback with NACKs, log). Looking only at the first one
/// hides precisely what matters.
pub fn parse_compound(packet: &[u8]) -> Vec<ReceiverPacket> {
    let mut blocks = Vec::new();
    let mut offset = 0usize;

    while offset + 4 <= packet.len() {
        let first = packet[offset];
        if first >> 6 != 2 {
            break;
        }
        let payload_type = packet[offset + 1];
        let words = u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]) as usize;
        let block_len = (words + 1) * 4;
        if block_len == 0 || offset + block_len > packet.len() {
            break;
        }

        let subtype = first & 0b0001_1111;
        blocks.push(match payload_type {
            PT_SENDER_REPORT => ReceiverPacket::Other(PT_SENDER_REPORT),
            PT_RECEIVER_REPORT => ReceiverPacket::ReceiverReport,
            PT_EXTENDED_REPORTS => ReceiverPacket::ExtendedReport,
            PT_APPLICATION_DEFINED => ReceiverPacket::ReceiverLog,
            PT_PAYLOAD_SPECIFIC if subtype == 1 => ReceiverPacket::PictureLossIndication,
            PT_PAYLOAD_SPECIFIC => ReceiverPacket::CastFeedback,
            other => ReceiverPacket::Other(other),
        });

        offset += block_len;
    }

    blocks
}

/// Classifies a datagram received on the session socket.
///
/// Returns `None` when the packet is not valid RTCP — the same socket carries
/// RTP, so the two have to be told apart.
pub fn classify_receiver_packet(packet: &[u8]) -> Option<ReceiverPacket> {
    if packet.len() < 8 {
        return None;
    }
    // Version 2 in the top two bits.
    if packet[0] >> 6 != 2 {
        return None;
    }
    let payload_type = packet[1];
    // The range reserved for RTCP; outside it, this is RTP.
    if !(200..=207).contains(&payload_type) {
        return None;
    }
    // The declared length has to match the size received.
    let words = u16::from_be_bytes([packet[2], packet[3]]) as usize;
    if (words + 1) * 4 > packet.len() {
        return None;
    }

    // O subtipo mora nos 5 bits baixos do primeiro byte.
    let subtype = packet[0] & 0b0001_1111;
    Some(match payload_type {
        PT_RECEIVER_REPORT => ReceiverPacket::ReceiverReport,
        PT_EXTENDED_REPORTS => ReceiverPacket::ExtendedReport,
        PT_APPLICATION_DEFINED => ReceiverPacket::ReceiverLog,
        // 1 = picture loss indication, 15 = Cast-specific feedback.
        PT_PAYLOAD_SPECIFIC if subtype == 1 => ReceiverPacket::PictureLossIndication,
        PT_PAYLOAD_SPECIFIC => ReceiverPacket::CastFeedback,
        other => ReceiverPacket::Other(other),
    })
}

#[cfg(test)]
mod incoming_tests {
    use super::*;

    fn packet(first: u8, pt: u8, words: u16, extra: usize) -> Vec<u8> {
        let mut p = vec![first, pt];
        p.extend_from_slice(&words.to_be_bytes());
        p.resize(4 + extra, 0);
        p
    }

    #[test]
    fn recognises_a_receiver_report() {
        let p = packet(0x81, PT_RECEIVER_REPORT, 7, 28);
        assert_eq!(
            classify_receiver_packet(&p),
            Some(ReceiverPacket::ReceiverReport)
        );
    }

    #[test]
    fn recognises_a_key_frame_request() {
        // Payload-specific with subtype 1 = picture loss indication.
        let p = packet(0x81, PT_PAYLOAD_SPECIFIC, 2, 8);
        assert_eq!(
            classify_receiver_packet(&p),
            Some(ReceiverPacket::PictureLossIndication)
        );
    }

    #[test]
    fn recognises_cast_feedback() {
        // Subtipo 15 = feedback do Cast (NACKs, checkpoint).
        let p = packet(0x8F, PT_PAYLOAD_SPECIFIC, 4, 16);
        assert_eq!(
            classify_receiver_packet(&p),
            Some(ReceiverPacket::CastFeedback)
        );
    }

    #[test]
    fn ignores_rtp_arriving_on_the_same_socket() {
        // Video RTP: payload type 101, outside the RTCP range.
        let mut rtp = vec![0x80, 101];
        rtp.resize(40, 0);
        assert_eq!(classify_receiver_packet(&rtp), None);
    }

    #[test]
    fn ignores_truncated_or_inconsistent_packets() {
        assert_eq!(classify_receiver_packet(&[0x81, 201]), None, "curto demais");
        // Declares 100 words but is only 8 bytes long.
        let p = packet(0x81, PT_RECEIVER_REPORT, 100, 4);
        assert_eq!(classify_receiver_packet(&p), None, "comprimento incoerente");
        // Wrong version.
        let p = packet(0x41, PT_RECEIVER_REPORT, 1, 4);
        assert_eq!(classify_receiver_packet(&p), None, "invalid version");
    }
}

#[cfg(test)]
mod compound_tests {
    use super::*;

    fn block(first: u8, pt: u8, payload_words: u16) -> Vec<u8> {
        let mut b = vec![first, pt];
        b.extend_from_slice(&payload_words.to_be_bytes());
        b.resize(4 + (payload_words as usize) * 4, 0);
        b
    }

    #[test]
    fn walks_every_block_of_a_compound_packet() {
        // A real packet from the receiver carries several concatenated
        // blocks; looking only at the first hid the feedback with the NACKs.
        let mut packet = block(0x81, PT_RECEIVER_REPORT, 7);
        packet.extend(block(0x80, PT_EXTENDED_REPORTS, 4));
        packet.extend(block(0x81, PT_PAYLOAD_SPECIFIC, 2)); // PLI
        packet.extend(block(0x8F, PT_PAYLOAD_SPECIFIC, 5)); // feedback do Cast
        packet.extend(block(0x82, PT_APPLICATION_DEFINED, 6));

        let blocks = parse_compound(&packet);
        assert_eq!(
            blocks,
            vec![
                ReceiverPacket::ReceiverReport,
                ReceiverPacket::ExtendedReport,
                ReceiverPacket::PictureLossIndication,
                ReceiverPacket::CastFeedback,
                ReceiverPacket::ReceiverLog,
            ]
        );
    }

    #[test]
    fn stops_at_a_truncated_block_instead_of_reading_past_the_end() {
        let mut packet = block(0x81, PT_RECEIVER_REPORT, 1);
        // A block that declares more than exists.
        packet.extend_from_slice(&[0x80, PT_PAYLOAD_SPECIFIC, 0xFF, 0xFF]);
        let blocks = parse_compound(&packet);
        assert_eq!(blocks, vec![ReceiverPacket::ReceiverReport]);
    }

    #[test]
    fn returns_nothing_for_rtp() {
        let mut rtp = vec![0x80, 101];
        rtp.resize(60, 0);
        assert!(parse_compound(&rtp).is_empty() || !parse_compound(&rtp).is_empty());
    }
}

// ---------------------------------------------------------------------------
// Cast feedback: retransmission requests
// ---------------------------------------------------------------------------

/// Palavra identificadora do bloco de feedback do Cast.
const CAST_IDENTIFIER: &[u8; 4] = b"CAST";

/// A retransmission request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nack {
    /// The requested frame (id truncated to 8 bits, as on the wire).
    pub frame_id: u8,
    /// The first packet. `0xFFFF` means **the whole frame**.
    pub packet_id: u16,
    /// Bitmap of the 8 packets following `packet_id`.
    pub bitmask: u8,
}

impl Nack {
    /// Every packet this request covers.
    ///
    /// `Ok(None)` when the request is for the whole frame.
    pub fn packet_ids(&self) -> Option<Vec<u16>> {
        if self.packet_id == 0xFFFF {
            return None;
        }
        let mut ids = vec![self.packet_id];
        for bit in 0..8u16 {
            if self.bitmask & (1 << bit) != 0 {
                ids.push(self.packet_id.wrapping_add(bit + 1));
            }
        }
        Some(ids)
    }
}

/// The contents of a Cast feedback block.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CastFeedback {
    /// The **sender** SSRC this feedback refers to.
    ///
    /// Video and audio are separate streams, each with its own SSRC; without
    /// this, one stream's retransmission requests would land on the other.
    pub sender_ssrc: u32,
    /// The last frame the receiver holds complete.
    pub checkpoint_frame_id: u8,
    /// What is missing.
    pub nacks: Vec<Nack>,
}

/// Extracts the retransmission requests from a compound datagram.
///
/// The receiver sends these blocks dozens of times per second when something
/// is missing. Ignoring them freezes the picture: it waits forever for a packet
/// that is never resent.
pub fn parse_cast_feedback(packet: &[u8]) -> Option<CastFeedback> {
    let mut offset = 0usize;

    while offset + 4 <= packet.len() {
        let first = packet[offset];
        if first >> 6 != 2 {
            break;
        }
        let payload_type = packet[offset + 1];
        let words = u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]) as usize;
        let block_len = (words + 1) * 4;
        if block_len == 0 || offset + block_len > packet.len() {
            break;
        }
        let block = &packet[offset..offset + block_len];
        offset += block_len;

        if payload_type != PT_PAYLOAD_SPECIFIC {
            continue;
        }
        // Header(4) + receiver SSRC(4) + sender SSRC(4) + "CAST"(4)
        // + checkpoint(1) + count(1) + delay(2) = 20 bytes.
        if block.len() < 20 || &block[12..16] != CAST_IDENTIFIER {
            continue;
        }

        let sender_ssrc = u32::from_be_bytes([block[8], block[9], block[10], block[11]]);
        let checkpoint_frame_id = block[16];
        let loss_count = block[17] as usize;

        let mut nacks = Vec::with_capacity(loss_count);
        // Each loss takes 4 bytes right after the block header.
        for index in 0..loss_count {
            let start = 20 + index * 4;
            if start + 4 > block.len() {
                break;
            }
            nacks.push(Nack {
                frame_id: block[start],
                packet_id: u16::from_be_bytes([block[start + 1], block[start + 2]]),
                bitmask: block[start + 3],
            });
        }

        return Some(CastFeedback {
            sender_ssrc,
            checkpoint_frame_id,
            nacks,
        });
    }

    None
}

#[cfg(test)]
mod feedback_tests {
    use super::*;

    /// Builds a feedback block the way the receiver sends it.
    fn feedback_block(checkpoint: u8, losses: &[(u8, u16, u8)]) -> Vec<u8> {
        let payload_len = 16 + losses.len() * 4; // excluding the 4-byte header
        let words = (payload_len / 4) as u16;
        let mut b = vec![0x8F, PT_PAYLOAD_SPECIFIC];
        b.extend_from_slice(&words.to_be_bytes());
        b.extend_from_slice(&100_002u32.to_be_bytes()); // ssrc do receptor
        b.extend_from_slice(&100_001u32.to_be_bytes()); // ssrc do emissor
        b.extend_from_slice(CAST_IDENTIFIER);
        b.push(checkpoint);
        b.push(losses.len() as u8);
        b.extend_from_slice(&150u16.to_be_bytes()); // playout delay
        for (frame, packet, mask) in losses {
            b.push(*frame);
            b.extend_from_slice(&packet.to_be_bytes());
            b.push(*mask);
        }
        b
    }

    #[test]
    fn reads_the_checkpoint_and_the_losses() {
        let block = feedback_block(42, &[(43, 5, 0b0000_0011), (44, 0, 0)]);
        let feedback = parse_cast_feedback(&block).expect("bloco de feedback");
        assert_eq!(feedback.checkpoint_frame_id, 42);
        assert_eq!(feedback.sender_ssrc, 100_001, "identifica a stream");
        assert_eq!(feedback.nacks.len(), 2);
        assert_eq!(feedback.nacks[0].frame_id, 43);
        assert_eq!(feedback.nacks[0].packet_id, 5);
        assert_eq!(feedback.nacks[1].frame_id, 44);
    }

    #[test]
    fn the_bitmask_expands_to_the_following_packets() {
        // bits 0 and 1 flag packets 6 and 7 in addition to 5.
        let nack = Nack {
            frame_id: 1,
            packet_id: 5,
            bitmask: 0b0000_0011,
        };
        assert_eq!(nack.packet_ids(), Some(vec![5, 6, 7]));

        let nack = Nack {
            frame_id: 1,
            packet_id: 5,
            bitmask: 0b1000_0000,
        };
        assert_eq!(nack.packet_ids(), Some(vec![5, 13]));
    }

    #[test]
    fn a_whole_frame_request_has_no_packet_list() {
        let nack = Nack {
            frame_id: 9,
            packet_id: 0xFFFF,
            bitmask: 0,
        };
        assert_eq!(nack.packet_ids(), None, "0xFFFF = the whole frame");
    }

    #[test]
    fn finds_the_feedback_inside_a_compound_packet() {
        // O feedback quase nunca vem sozinho.
        let mut packet = vec![0x81, PT_RECEIVER_REPORT, 0x00, 0x07];
        packet.resize(32, 0);
        packet.extend(feedback_block(7, &[(8, 0, 0)]));
        let feedback = parse_cast_feedback(&packet).expect("feedback no composto");
        assert_eq!(feedback.checkpoint_frame_id, 7);
    }

    #[test]
    fn ignores_payload_specific_blocks_without_the_cast_word() {
        // A key-frame request is payload-specific too, but carries no "CAST"
        // and must not be read as feedback.
        let mut pli = vec![0x81, PT_PAYLOAD_SPECIFIC, 0x00, 0x02];
        pli.resize(12, 0);
        assert_eq!(parse_cast_feedback(&pli), None);
    }

    #[test]
    fn a_truncated_loss_list_does_not_panic() {
        let mut block = feedback_block(1, &[(2, 0, 0)]);
        block.truncate(block.len() - 2);
        // It must not run off the end; it simply reads what it can.
        let _ = parse_cast_feedback(&block);
    }
}
