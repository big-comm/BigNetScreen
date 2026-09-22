//! Moving the encoder's bitrate with what the link is actually carrying.
//!
//! Until now the bitrate was chosen once, from a table of resolution and frame
//! rate, and held for the whole session. A Wi-Fi link is not a constant, and
//! the picture degrading while nothing adjusted is what every report of a bad
//! session this week had in common.
//!
//! Shaped after Chrome's mirroring sender, which updates every 500 ms, keeps a
//! floor of 300 kbit, raises by a tenth at a time, and — the part worth
//! copying most — only lowers the rate when the sender is *actually* dropping
//! frames, rather than on any dip in an estimate.
//!
//! Two deliberate differences:
//!
//! - **It starts at the mode's own bitrate, not at Chrome's conservative
//!   5 Mbit.** Chrome starts low because it knows nothing about the link.
//!   Neither do we, but starting low costs about seven seconds of visibly
//!   worse picture at the top of every session on a link that was going to
//!   carry it, and every link reported here has. Optimistic start, quick
//!   retreat, slow return.
//! - **The congestion signal is loss and our own dropped frames, not a
//!   bandwidth estimate.** We have no estimate: Chrome's comes from its
//!   streaming library. Calling what we have an estimate would be inventing a
//!   number. Retransmissions and frames the flow control skipped are measured.

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
    fn a_session_that_goes_well_is_never_touched() {
        let mut rate = RateControl::new(20_000);
        for _ in 0..50 {
            assert_eq!(rate.observe(healthy()), None);
        }
        assert_eq!(rate.current_kbps, 20_000);
    }

    #[test]
    fn the_receiver_falling_behind_lowers_the_rate_at_once() {
        let mut rate = RateControl::new(20_000);
        let behind = Window {
            frames_dropped: 1,
            ..healthy()
        };
        assert_eq!(rate.observe(behind), Some(16_000));
        assert_eq!(rate.observe(behind), Some(12_800));
    }

    #[test]
    fn the_odd_lost_packet_is_not_congestion() {
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
    fn sustained_loss_is_congestion_and_the_rate_retreats() {
        let mut rate = RateControl::new(20_000);
        let lossy = Window {
            packets_resent: 30,
            ..healthy()
        };
        assert_eq!(rate.observe(lossy), Some(16_000));
    }

    #[test]
    fn the_rate_comes_back_slowly_and_stops_where_it_was_allowed_to() {
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
    fn it_never_goes_below_the_floor_however_bad_the_link_gets() {
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
    fn a_mode_below_the_floor_is_raised_to_it_rather_than_honoured() {
        // Nothing useful is sent at 100 kbit, and the arithmetic below would
        // have a ceiling under its own floor.
        let rate = RateControl::new(100);
        assert_eq!(rate.current_kbps, FLOOR_KBPS);
    }

    #[test]
    fn an_idle_window_is_not_read_as_a_healthy_one_by_accident() {
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
        // Silence counts as health here, deliberately: a session with nothing
        // to send has nothing to be congested about, and holding the rate down
        // after a quiet patch would punish a still screen.
        assert_eq!(rate.current_kbps, 17_600);
    }
}
