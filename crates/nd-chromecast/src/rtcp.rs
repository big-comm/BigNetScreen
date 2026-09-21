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
    // Length in 32-bit words, minus one (RFC 3550 rule).
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
        // Length in 32-bit words minus one: 28/4 - 1 = 6.
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
    validated_blocks(packet)
        .unwrap_or_default()
        .into_iter()
        .map(classify_block)
        .collect()
}

/// Validate the WHOLE datagram before publishing any state. A valid prefix
/// followed by a truncated block must not acknowledge frames or request IDRs.
/// Returned slices exclude RTCP padding (only legal on the last block).
fn validated_blocks(packet: &[u8]) -> Option<Vec<&[u8]>> {
    if packet.is_empty() {
        return None;
    }
    let mut blocks = Vec::new();
    let mut offset = 0;
    while offset < packet.len() {
        let header = packet.get(offset..offset + 4)?;
        if header[0] >> 6 != 2 || !(200..=207).contains(&header[1]) {
            return None;
        }
        let words = usize::from(u16::from_be_bytes([header[2], header[3]]));
        let end = offset.checked_add((words + 1) * 4)?;
        let mut block = packet.get(offset..end)?;
        if header[0] & 0x20 != 0 {
            if end != packet.len() {
                return None;
            }
            let padding = usize::from(*block.last()?);
            if padding == 0 || padding > block.len().saturating_sub(4) {
                return None;
            }
            block = &block[..block.len() - padding];
        }
        let count = usize::from(header[0] & 0x1f);
        let minimum = match header[1] {
            PT_SENDER_REPORT => 28 + 24 * count,
            PT_RECEIVER_REPORT => 8 + 24 * count,
            PT_APPLICATION_DEFINED => 12,
            PT_PAYLOAD_SPECIFIC | 205 => 12,
            PT_EXTENDED_REPORTS => 8,
            _ => 4,
        };
        if block.len() < minimum {
            return None;
        }
        if header[1] == PT_PAYLOAD_SPECIFIC && count == 1 && block.len() != 12 {
            return None;
        }
        if header[1] == PT_PAYLOAD_SPECIFIC
            && count == 15
            && block.get(12..16) == Some(CAST_IDENTIFIER.as_slice())
            && (block.len() < 20 || block.len() < 20 + usize::from(block[17]) * 4)
        {
            return None;
        }
        if header[1] == PT_EXTENDED_REPORTS {
            let mut pos = 8;
            while pos < block.len() {
                let xr = block.get(pos..pos + 4)?;
                let size = (usize::from(u16::from_be_bytes([xr[2], xr[3]])) + 1) * 4;
                block.get(pos..pos + size)?;
                if xr[0] == 4 && size != 12 {
                    // Receiver Reference Time Report
                    return None;
                }
                pos += size;
            }
        }
        blocks.push(block);
        offset = end;
    }
    Some(blocks)
}

fn classify_block(block: &[u8]) -> ReceiverPacket {
    match (block[1], block[0] & 0x1f) {
        (PT_RECEIVER_REPORT, _) => ReceiverPacket::ReceiverReport,
        (PT_EXTENDED_REPORTS, _) => ReceiverPacket::ExtendedReport,
        (PT_APPLICATION_DEFINED, _) => ReceiverPacket::ReceiverLog,
        (PT_PAYLOAD_SPECIFIC, 1) => ReceiverPacket::PictureLossIndication,
        (PT_PAYLOAD_SPECIFIC, 15) if block.get(12..16) == Some(CAST_IDENTIFIER.as_slice()) => {
            ReceiverPacket::CastFeedback
        }
        (other, _) => ReceiverPacket::Other(other),
    }
}

/// Classifies a fully validated RTCP datagram; RTP and malformed tails fail.
pub fn classify_receiver_packet(packet: &[u8]) -> Option<ReceiverPacket> {
    validated_blocks(packet)?
        .first()
        .map(|block| classify_block(block))
}

/// PLI must address a negotiated pair, not merely arrive on the right socket.
pub fn picture_loss_for(packet: &[u8], receiver_ssrc: u32, sender_ssrc: u32) -> bool {
    validated_blocks(packet).is_some_and(|blocks| {
        blocks.into_iter().any(|block| {
            classify_block(block) == ReceiverPacket::PictureLossIndication
                && block[4..8] == receiver_ssrc.to_be_bytes()
                && block[8..12] == sender_ssrc.to_be_bytes()
        })
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
        // Subtype 15 = Cast feedback (NACKs, checkpoint).
        let mut p = packet(0x8F, PT_PAYLOAD_SPECIFIC, 4, 16);
        p[12..16].copy_from_slice(CAST_IDENTIFIER);
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
        assert_eq!(classify_receiver_packet(&[0x81, 201]), None, "too short");
        // Declares 100 words but is only 8 bytes long.
        let p = packet(0x81, PT_RECEIVER_REPORT, 100, 4);
        assert_eq!(classify_receiver_packet(&p), None, "inconsistent length");
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
        let mut feedback = block(0x8F, PT_PAYLOAD_SPECIFIC, 5);
        feedback[12..16].copy_from_slice(CAST_IDENTIFIER);
        packet.extend(feedback);
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
    fn rejects_a_valid_prefix_followed_by_a_truncated_block() {
        let mut packet = block(0x80, PT_RECEIVER_REPORT, 1);
        // A block that declares more than exists.
        packet.extend_from_slice(&[0x80, PT_PAYLOAD_SPECIFIC, 0xFF, 0xFF]);
        let blocks = parse_compound(&packet);
        assert!(blocks.is_empty());
    }

    #[test]
    fn returns_nothing_for_rtp() {
        let mut rtp = vec![0x80, 101];
        rtp.resize(60, 0);
        assert!(classify_receiver_packet(&rtp).is_none());
    }
}

// ---------------------------------------------------------------------------
// Cast feedback: retransmission requests
// ---------------------------------------------------------------------------

/// Identifier word of the Cast feedback block.
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
                if let Some(id) = self
                    .packet_id
                    .checked_add(bit + 1)
                    .filter(|id| *id != 0xFFFF)
                {
                    ids.push(id);
                }
            }
        }
        Some(ids)
    }
}

/// The contents of a Cast feedback block.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CastFeedback {
    /// The receiver that sent this report (must match ANSWER.ssrcs).
    pub receiver_ssrc: u32,
    /// Optional XR reference timestamp used to discard reordered feedback.
    pub reference_time: Option<u64>,
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
    parse_cast_feedbacks(packet).into_iter().next()
}

/// A compound packet can contain feedback for more than one negotiated track.
pub fn parse_cast_feedbacks(packet: &[u8]) -> Vec<CastFeedback> {
    let Some(blocks) = validated_blocks(packet) else {
        return Vec::new();
    };
    let mut feedbacks = Vec::new();
    for block in &blocks {
        if classify_block(block) != ReceiverPacket::CastFeedback || block.len() < 20 {
            continue;
        }
        let receiver_ssrc = u32::from_be_bytes(block[4..8].try_into().unwrap());
        let sender_ssrc = u32::from_be_bytes(block[8..12].try_into().unwrap());
        let mut reference_time = None;
        for xr in &blocks {
            if xr[1] != PT_EXTENDED_REPORTS || xr[4..8] != receiver_ssrc.to_be_bytes() {
                continue;
            }
            let mut pos = 8;
            while pos < xr.len() {
                let size = (usize::from(u16::from_be_bytes([xr[pos + 2], xr[pos + 3]])) + 1) * 4;
                if xr[pos] == 4 {
                    reference_time = Some(u64::from_be_bytes(
                        xr[pos + 4..pos + 12].try_into().unwrap(),
                    ));
                }
                pos += size;
            }
        }
        let mut nacks = Vec::with_capacity(usize::from(block[17]));
        for loss in block[20..20 + usize::from(block[17]) * 4]
            .as_chunks::<4>()
            .0
        {
            nacks.push(Nack {
                frame_id: loss[0],
                packet_id: u16::from_be_bytes([loss[1], loss[2]]),
                bitmask: loss[3],
            });
        }
        feedbacks.push(CastFeedback {
            receiver_ssrc,
            reference_time,
            sender_ssrc,
            checkpoint_frame_id: block[16],
            nacks,
        });
    }
    feedbacks
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
        b.extend_from_slice(&100_002u32.to_be_bytes()); // receiver ssrc
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
        let feedback = parse_cast_feedback(&block).expect("feedback block");
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
        let feedback = parse_cast_feedback(&packet).expect("feedback in the compound packet");
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
        // No partial ACK/NACK state may escape a malformed packet.
        assert!(parse_cast_feedback(&block).is_none());
    }
    #[test]
    fn every_truncation_and_malformed_tail_is_rejected_atomically() {
        let valid = feedback_block(42, &[(43, 1, 3)]);
        for end in 0..valid.len() {
            assert!(parse_cast_feedback(&valid[..end]).is_none(), "prefix {end}");
        }
        let mut packet = valid.clone();
        packet.push(0);
        assert!(parse_cast_feedback(&packet).is_none());
        let mut packet = valid;
        packet[17] = 255;
        assert!(parse_cast_feedback(&packet).is_none());
    }

    #[test]
    fn only_fmt_fifteen_is_cast_feedback() {
        let mut packet = feedback_block(42, &[]);
        packet[0] = 0x82;
        assert!(parse_cast_feedback(&packet).is_none());
    }

    #[test]
    fn padding_is_removed_and_must_be_final_and_nonzero() {
        let mut packet = feedback_block(42, &[]);
        packet[0] |= 0x20;
        packet[3] += 1;
        packet.extend_from_slice(&[0, 0, 0, 4]);
        assert_eq!(
            parse_cast_feedback(&packet).unwrap().checkpoint_frame_id,
            42
        );
        let mut bad = packet.clone();
        *bad.last_mut().unwrap() = 0;
        assert!(parse_cast_feedback(&bad).is_none());
        packet.extend(feedback_block(43, &[]));
        assert!(parse_cast_feedback(&packet).is_none());
    }

    #[test]
    fn feedback_keeps_receiver_identity_and_multiple_tracks() {
        let mut packet = feedback_block(42, &[]);
        packet.extend(feedback_block(43, &[]));
        let reports = parse_cast_feedbacks(&packet);
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].receiver_ssrc, 100_002);
    }

    #[test]
    fn nack_bitmap_never_wraps_packet_numbers_or_requests_the_sentinel() {
        let nack = Nack {
            frame_id: 0,
            packet_id: 0xfffe,
            bitmask: 0xff,
        };
        assert_eq!(nack.packet_ids(), Some(vec![0xfffe]));
    }

    #[test]
    fn pli_is_scoped_to_both_ssrcs() {
        let mut packet = vec![0x81, PT_PAYLOAD_SPECIFIC, 0, 2];
        packet.extend_from_slice(&100_002u32.to_be_bytes());
        packet.extend_from_slice(&100_001u32.to_be_bytes());
        assert!(picture_loss_for(&packet, 100_002, 100_001));
        assert!(!picture_loss_for(&packet, 100_004, 100_001));
        assert!(!picture_loss_for(&packet, 100_002, 100_003));
    }
}
