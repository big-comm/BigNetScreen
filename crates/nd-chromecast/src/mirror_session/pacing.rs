//! Pacing the mirroring sockets: packets leave in bursts bounded per
//! [`BURST_INTERVAL`], as Open Screen's sender paces them.
//!
//! Every send in [`super::sender`] goes through a [`BurstPacer`]; video and
//! its repairs share one, audio has its own (see [`super::stream`]).

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The rate a burst of packets may reach, in bits per second.
///
/// Open Screen's `kDefaultMaxBurstBitrate`, and its header gives the point of
/// pacing: sending in measured bursts lets the operating system "collect many
/// small packets into a short-term buffer", which "can be critical for good
/// performance over shared-medium networks (such as 802.11 WiFi)". It is a
/// ceiling on how fast, never on how much.
///
/// Handing a whole frame to the socket at once overruns the queue the access
/// point keeps for the receiver, and the loss comes back in clusters, in step
/// with the key frames, the largest bursts.
const MAX_BURST_BITRATE: usize = 24 * 1024 * 1024;
/// The window the burst ceiling is measured over (Open Screen's
/// `kDefaultBurstInterval`).
pub(super) const BURST_INTERVAL: Duration = Duration::from_millis(10);
/// What one burst window may carry, in bytes.
pub(super) const BURST_BUDGET_BYTES: usize =
    MAX_BURST_BITRATE / 8 * BURST_INTERVAL.as_millis() as usize / 1000;

/// The most video the sender can actually push, in kbit/s.
///
/// The burst budget resets at each window and unspent room does not carry, so
/// [`MAX_BURST_BITRATE`] caps the average rate too. Asking the encoder for
/// more than the pacer drains buys no detail: the surplus piles up in front of
/// the socket and the picture falls further behind for as long as the session
/// runs.
///
/// An eighth is left free: a stream that fills the pacer exactly has no room
/// to resend a lost packet without adding backlog it can never drain, and the
/// audio track and the RTP and Cast headers come out of the same budget. Not
/// more than an eighth, so the cap still clears 1440p60's table bitrate and
/// holds only 2160p60.
pub(crate) const MAX_VIDEO_BITRATE_KBPS: u32 = MAX_BURST_BITRATE as u32 / 1000 / 8 * 7;

/// One bounded burst budget shared by audio, video, reports and repairs.
/// The mutex protects only accounting; neither sleep nor send holds it.
#[derive(Clone)]
pub(super) struct BurstPacer {
    budget: Arc<Mutex<BurstBudget>>,
    pub(super) stop: Arc<AtomicBool>,
}

struct BurstBudget {
    started: Instant,
    bytes: usize,
}

impl BurstBudget {
    fn reserve(&mut self, now: Instant, bytes: usize) -> Duration {
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= BURST_INTERVAL {
            self.started = now;
            self.bytes = 0;
        }
        if self.bytes + bytes <= BURST_BUDGET_BYTES {
            self.bytes += bytes;
            Duration::ZERO
        } else {
            BURST_INTERVAL.saturating_sub(now.saturating_duration_since(self.started))
        }
    }
}

impl BurstPacer {
    pub(super) fn new(stop: Arc<AtomicBool>) -> Self {
        Self {
            budget: Arc::new(Mutex::new(BurstBudget {
                started: Instant::now(),
                bytes: 0,
            })),
            stop,
        }
    }

    pub(super) fn send(&self, socket: &UdpSocket, packet: &[u8]) -> std::io::Result<usize> {
        if packet.len() > BURST_BUDGET_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "packet exceeds pacing budget",
            ));
        }
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "Cast stopped",
                ));
            }
            let wait = self
                .budget
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .reserve(Instant::now(), packet.len());
            if wait.is_zero() {
                return socket.send(packet);
            }
            // Small slices make a multi-window keyframe cancellable too.
            std::thread::sleep(wait.min(Duration::from_millis(2)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_mode_asks_the_encoder_for_more_video_than_the_pacer_can_drain() {
        // The table asks 30 Mbit for 2160p60 against a pacer that drains 24:
        // the surplus would queue in front of the socket for the whole session.
        let pacer_kbps =
            (BURST_BUDGET_BYTES * 8) as u32 * (1000 / BURST_INTERVAL.as_millis() as u32) / 1000;
        for (width, height) in [
            (640, 480),
            (1280, 720),
            (1920, 1080),
            (2560, 1440),
            (3840, 2160),
        ] {
            for fps in [30, 60] {
                let cfg = nd_core::pipeline::StreamConfig {
                    width,
                    height,
                    fps,
                    bitrate_kbps: 0,
                    ..Default::default()
                };
                let asked = crate::mirror::cast_bitrate_kbps(&cfg);
                assert!(
                    asked < pacer_kbps,
                    "{width}x{height}@{fps} asks {asked} kbps against a {pacer_kbps} kbps pacer"
                );
            }
        }
        // And the cap has to be what is doing the holding, not a table that
        // happens to stay small.
        let uncapped = nd_core::pipeline::StreamConfig {
            width: 3840,
            height: 2160,
            fps: 60,
            bitrate_kbps: 0,
            ..Default::default()
        };
        assert!(uncapped.scaled_bitrate_kbps() > MAX_VIDEO_BITRATE_KBPS);
        assert_eq!(
            crate::mirror::cast_bitrate_kbps(&uncapped),
            MAX_VIDEO_BITRATE_KBPS
        );
        // 1440p60 asks 21 Mbit and works; the cap must not start holding it.
        let works = nd_core::pipeline::StreamConfig {
            width: 2560,
            height: 1440,
            fps: 60,
            bitrate_kbps: 0,
            ..Default::default()
        };
        assert_eq!(
            crate::mirror::cast_bitrate_kbps(&works),
            works.scaled_bitrate_kbps()
        );
    }

    #[test]
    fn a_burst_window_carries_about_one_key_frame_worth() {
        // 24 Mbit/s over 10 ms. Getting the unit conversion wrong here is
        // silent: too large and pacing never engages, too small and every
        // frame is spread over a sleep it did not need.
        // Open Screen writes the ceiling as `24 << 20`, so the megabit here is
        // 1048576 bits and not 1000000 — 3_145_728 bytes a second.
        assert_eq!(BURST_BUDGET_BYTES, 31_457);
        // Which is 26 packets of the size this sender emits: a whole ordinary
        // frame at 1080p30/10 Mbit fits in roughly one window, and only a key
        // frame is spread across several.
        assert_eq!(BURST_BUDGET_BYTES / crate::rtp::MAX_PACKET_SIZE, 26);
    }

    #[test]
    fn burst_budget_resets_by_time_and_never_holds_a_stale_partial_window() {
        let now = Instant::now();
        let mut budget = BurstBudget {
            started: now,
            bytes: 0,
        };
        assert_eq!(budget.reserve(now, BURST_BUDGET_BYTES), Duration::ZERO);
        assert_eq!(budget.reserve(now, 1), BURST_INTERVAL);
        assert_eq!(budget.reserve(now + BURST_INTERVAL, 1), Duration::ZERO);
        assert_eq!(budget.bytes, 1);
    }

    #[test]
    fn stopped_pacer_never_sends_another_packet() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let stop = Arc::new(AtomicBool::new(true));
        let pacer = BurstPacer::new(stop);
        assert_eq!(
            pacer.send(&socket, &[1]).unwrap_err().kind(),
            std::io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn audio_never_waits_for_a_window_video_has_emptied() {
        // A clone shares the budget, which is what video's own senders want
        // and what audio must not have.
        let video = BurstPacer::new(Default::default());
        assert!(Arc::ptr_eq(&video.clone().budget, &video.budget));
        let audio = BurstPacer::new(Default::default());
        assert!(!Arc::ptr_eq(&audio.budget, &video.budget));

        // A window holds about one average video frame — 31457 bytes against 31250 at
        // 1080p60 and 15 Mbit — so an average frame very nearly empties it and
        // anything above average takes more than one window. Whatever shares
        // the budget waits for a window video has left something in.
        let window_bytes = BURST_BUDGET_BYTES;
        let average_video_frame = 15_000_000 / 8 / 60;
        assert!(
            average_video_frame * 10 > window_bytes * 9,
            "a video frame no longer fills a burst window: \
             {average_video_frame} against {window_bytes}"
        );
        // An Opus packet is three orders of magnitude smaller and arrives once
        // per window. It cannot congest a 24 Mbit/s ceiling.
        let opus_packet = 128_000 / 8 / 100;
        assert!(opus_packet * 50 < window_bytes, "{opus_packet}");
    }
}
