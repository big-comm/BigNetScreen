//! Moving the mirroring encoder's bitrate with what the link carries.
//!
//! The mode's bitrate comes from a table of resolution and frame rate; a
//! Wi-Fi link is not constant, so the sender thread in
//! [`crate::mirror_session`] feeds each window of sending to [`RateControl`].
//!
//! Shaped after Chrome's mirroring sender, which updates every 500 ms, keeps a
//! floor of 300 kbit, raises by a tenth at a time, and lowers the rate only
//! when the sender is actually dropping frames, not on any dip in an estimate.
//!
//! Two differences:
//!
//! - It starts at the mode's own bitrate, not Chrome's conservative 5 Mbit.
//!   Starting low costs seconds of visibly worse picture at the top of every
//!   session on a link that could carry the mode. Optimistic start, quick
//!   retreat, slow return.
//! - The congestion signal is loss and our own dropped frames. There is no
//!   bandwidth estimate here (Chrome's comes from its streaming library), and
//!   none is invented: retransmissions and skipped frames are measured.

use std::time::Duration;

/// How often the rate is reconsidered. Chrome's interval.
pub(crate) const UPDATE_INTERVAL: Duration = Duration::from_millis(500);

/// Never below this, whatever the link does. Chrome's `kMinVideoBitrate`.
const FLOOR_KBPS: u32 = 300;

/// Retransmissions past this share of what was sent count as congestion.
///
/// Not zero: a Wi-Fi link loses the odd packet with no trouble at all, and
/// treating one lost packet in a thousand as congestion would ratchet the
/// picture down for nothing.
const LOSS_LIMIT: f64 = 0.02;

/// Windows of health before the rate is allowed back up, so one quiet moment
/// in a bad patch does not undo the retreat.
const HEALTHY_WINDOWS_BEFORE_RAISING: u32 = 4;

/// What one window of sending looked like.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Window {
    pub packets_sent: u64,
    pub packets_resent: u64,
    /// Frames the flow control skipped because the receiver was behind. This
    /// is the receiver saying it cannot keep up, which no loss figure shows.
    pub frames_dropped: u64,
}

impl Window {
    fn congested(&self) -> bool {
        if self.frames_dropped > 0 {
            return true;
        }
        self.packets_sent > 0
            && (self.packets_resent as f64 / self.packets_sent as f64) > LOSS_LIMIT
    }
}

/// The running totals at the start of the current window.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Baseline {
    packets: u32,
    resent: u64,
    dropped: u64,
}

impl Baseline {
    /// The window since the previous call. `packets` is the sender report's
    /// 32-bit counter, which wraps in a long session.
    pub(crate) fn advance(&mut self, packets: u32, resent: u64, dropped: u64) -> Window {
        let window = Window {
            packets_sent: u64::from(packets.wrapping_sub(self.packets)),
            packets_resent: resent - self.resent,
            frames_dropped: dropped - self.dropped,
        };
        *self = Self {
            packets,
            resent,
            dropped,
        };
        window
    }
}

#[derive(Debug)]
pub(crate) struct RateControl {
    ceiling_kbps: u32,
    current_kbps: u32,
    healthy_windows: u32,
}

impl RateControl {
    /// `ceiling` is the mode's bitrate, already held to what the receiver
    /// accepts and what the pacer can drain. The rate never goes above it:
    /// this decides when to send *less*, never when to exceed what was agreed.
    pub(crate) fn new(ceiling_kbps: u32) -> Self {
        Self {
            ceiling_kbps: ceiling_kbps.max(FLOOR_KBPS),
            current_kbps: ceiling_kbps.max(FLOOR_KBPS),
            healthy_windows: 0,
        }
    }

    /// Feeds one window in. Returns the new rate only when it changed, so a
    /// caller can leave the encoder alone the rest of the time.
    pub(crate) fn observe(&mut self, window: Window) -> Option<u32> {
        let previous = self.current_kbps;
        if window.congested() {
            self.healthy_windows = 0;
            // A fifth off, at once. A link that is losing packets is already
            // hurting the picture, and creeping down keeps it hurting.
            self.current_kbps = (self.current_kbps * 4 / 5).max(FLOOR_KBPS);
        } else {
            self.healthy_windows += 1;
            if self.healthy_windows >= HEALTHY_WINDOWS_BEFORE_RAISING
                && self.current_kbps < self.ceiling_kbps
            {
                self.healthy_windows = 0;
                self.current_kbps = (self.current_kbps * 11 / 10).min(self.ceiling_kbps);
            }
        }
        (self.current_kbps != previous).then_some(self.current_kbps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> Window {
        Window {
            packets_sent: 1000,
            packets_resent: 0,
            frames_dropped: 0,
        }
    }

    #[test]
    fn counter_wrap_counts_sent_packets() {
        let mut baseline = Baseline::default();
        baseline.advance(u32::MAX - 5, 0, 0);
        let window = baseline.advance(10, 2, 1);
        assert_eq!(
            window,
            Window {
                packets_sent: 16,
                packets_resent: 2,
                frames_dropped: 1,
            }
        );
    }

    #[test]
    fn healthy_session_keeps_its_rate() {
        let mut rate = RateControl::new(20_000);
        for _ in 0..50 {
            assert_eq!(rate.observe(healthy()), None);
        }
        assert_eq!(rate.current_kbps, 20_000);
    }

    #[test]
    fn receiver_lag_lowers_rate_at_once() {
        let mut rate = RateControl::new(20_000);
        let behind = Window {
            frames_dropped: 1,
            ..healthy()
        };
        assert_eq!(rate.observe(behind), Some(16_000));
        assert_eq!(rate.observe(behind), Some(12_800));
    }

    #[test]
    fn sporadic_loss_keeps_the_rate() {
        // One in a thousand. A link that does this is a link that works.
        let mut rate = RateControl::new(20_000);
        let scratchy = Window {
            packets_resent: 1,
            ..healthy()
        };
        for _ in 0..10 {
            assert_eq!(rate.observe(scratchy), None);
        }
        assert_eq!(rate.current_kbps, 20_000);
    }

    #[test]
    fn sustained_loss_lowers_the_rate() {
        let mut rate = RateControl::new(20_000);
        let lossy = Window {
            packets_resent: 30,
            ..healthy()
        };
        assert_eq!(rate.observe(lossy), Some(16_000));
    }

    #[test]
    fn rate_recovers_slowly_to_ceiling() {
        let mut rate = RateControl::new(20_000);
        rate.observe(Window {
            frames_dropped: 1,
            ..healthy()
        });
        assert_eq!(rate.current_kbps, 16_000);

        // Health alone is not enough; it has to last.
        for _ in 0..HEALTHY_WINDOWS_BEFORE_RAISING - 1 {
            assert_eq!(rate.observe(healthy()), None);
        }
        assert_eq!(rate.observe(healthy()), Some(17_600));

        for _ in 0..500 {
            rate.observe(healthy());
        }
        assert_eq!(
            rate.current_kbps, 20_000,
            "never above what the receiver agreed to"
        );
    }

    #[test]
    fn rate_stays_above_the_floor() {
        let mut rate = RateControl::new(20_000);
        let awful = Window {
            frames_dropped: 10,
            ..healthy()
        };
        for _ in 0..200 {
            rate.observe(awful);
        }
        assert_eq!(rate.current_kbps, FLOOR_KBPS);
    }

    #[test]
    fn mode_below_floor_is_raised() {
        // Nothing useful is sent at 100 kbit, and the arithmetic below would
        // have a ceiling under its own floor.
        let rate = RateControl::new(100);
        assert_eq!(rate.current_kbps, FLOOR_KBPS);
    }

    #[test]
    fn idle_window_counts_as_healthy() {
        // Nothing sent, nothing lost: no evidence either way, and the ratio
        // would divide by zero.
        let mut rate = RateControl::new(20_000);
        rate.observe(Window {
            frames_dropped: 1,
            ..healthy()
        });
        let idle = Window::default();
        assert!(!idle.congested());
        for _ in 0..HEALTHY_WINDOWS_BEFORE_RAISING {
            rate.observe(idle);
        }
        // Silence counts as health: a session with nothing to send has nothing
        // to be congested about, and holding the rate down after a quiet patch
        // would punish a still screen.
        assert_eq!(rate.current_kbps, 17_600);
    }
}
