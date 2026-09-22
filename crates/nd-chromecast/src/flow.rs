//! Cast's wire frame id is only eight bits. Keep the expanded, monotonic ACK
//! locally, and never allow an unacknowledged window to reach half that range.
//! This is a protocol safety limit, NOT an adaptive congestion controller.

use std::time::Instant;

pub(crate) const MAX_UNACKED_FRAMES: u32 = 120;

#[derive(Debug)]
pub(crate) struct AckWindow {
    last_sent: i64,
    checkpoint: i64,
    newest_reference_time: Option<u64>,
    pub(crate) last_progress: Instant,
}

impl Default for AckWindow {
    fn default() -> Self {
        Self {
            last_sent: -1,
            checkpoint: -1,
            newest_reference_time: None,
            last_progress: Instant::now(),
        }
    }
}

impl AckWindow {
    pub(crate) fn pending(&self) -> u32 {
        (self.last_sent - self.checkpoint) as u32
    }

    pub(crate) fn can_send(&self) -> bool {
        self.pending() < MAX_UNACKED_FRAMES
    }

    pub(crate) fn checkpoint(&self) -> i64 {
        self.checkpoint
    }

    /// Publish before the first packet can reach the receiver, so a fast ACK
    /// does not race with recording its own frame. Errors then end the session.
    pub(crate) fn sent(&mut self, frame_id: u32) {
        debug_assert!(self.can_send());
        debug_assert_eq!(i64::from(frame_id), self.last_sent + 1);
        // An idle track has no ACK obligation. Start a fresh deadline when
        // the first frame after idle becomes outstanding, not eight seconds
        // after a checkpoint that may belong to an earlier active period.
        if self.pending() == 0 {
            self.last_progress = Instant::now();
        }
        self.last_sent = i64::from(frame_id);
    }

    /// Expand to the most recent SENT id matching the wire byte. Never accept
    /// a future id or move a checkpoint backwards on reordered UDP feedback.
    /// XR reference time additionally distinguishes older datagrams across
    /// whole 256-frame cycles when the receiver supplies it.
    pub(crate) fn acknowledge(&mut self, wire: u8, reference_time: Option<u64>) -> bool {
        if let (Some(new), Some(old)) = (reference_time, self.newest_reference_time) {
            if new < old {
                return false;
            }
        }
        let mut expanded = (self.last_sent & !0xff) | i64::from(wire);
        if expanded > self.last_sent {
            expanded -= 256;
        }
        if expanded < self.checkpoint {
            return false;
        }
        if expanded > self.checkpoint {
            self.last_progress = Instant::now();
            self.checkpoint = expanded;
        }
        if reference_time.is_some() {
            self.newest_reference_time = reference_time;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn startup_is_bounded_even_without_one_feedback_packet() {
        let mut window = AckWindow::default();
        for id in 0..MAX_UNACKED_FRAMES {
            assert!(window.can_send());
            window.sent(id);
        }
        assert_eq!(window.pending(), 120);
        assert!(!window.can_send());
        assert!(window.acknowledge(0, None));
        assert_eq!(window.pending(), 119);
        assert!(window.can_send());
    }

    #[test]
    fn new_outstanding_frame_restarts_timeout_after_an_idle_track() {
        let mut window = AckWindow::default();
        let before = Instant::now();
        window.last_progress = before - Duration::from_secs(60);
        window.sent(0);
        assert!(window.last_progress >= before);
        // Adding more outstanding frames must NOT keep renewing the timeout.
        let original = window.last_progress;
        window.sent(1);
        assert_eq!(window.last_progress, original);
    }

    #[test]
    fn sentinel_future_and_reordered_ids_do_not_invent_progress() {
        let mut window = AckWindow::default();
        assert!(window.acknowledge(255, None)); // before frame zero
        assert!(!window.acknowledge(0, None));
        for id in 0..100 {
            window.sent(id);
        }
        assert!(!window.acknowledge(100, None)); // not sent yet
        assert!(window.acknowledge(90, None));
        assert_eq!(window.pending(), 9);
        assert!(!window.acknowledge(80, None));
        assert_eq!(window.pending(), 9);
    }

    #[test]
    fn every_wire_wrap_keeps_full_ids_and_all_window_distances() {
        for last in 120..4096i64 {
            for count in 1..=120i64 {
                let mut window = AckWindow {
                    last_sent: last,
                    checkpoint: last - count,
                    ..Default::default()
                };
                assert!(window.acknowledge((last - 1) as u8, None));
                assert_eq!(window.checkpoint(), last - 1);
                assert_eq!(window.pending(), 1);
                assert!(!window.acknowledge((last - 2) as u8, None));
            }
        }
    }

    #[test]
    fn xr_rejects_an_old_packet_even_after_an_entire_wire_cycle() {
        let mut window = AckWindow {
            last_sent: 1000,
            checkpoint: 990,
            newest_reference_time: Some(500),
            ..Default::default()
        };
        assert!(!window.acknowledge(999u32 as u8, Some(499)));
        assert_eq!(window.pending(), 10);
        assert!(window.acknowledge(999u32 as u8, Some(501)));
        assert_eq!(window.pending(), 1);
    }
}
