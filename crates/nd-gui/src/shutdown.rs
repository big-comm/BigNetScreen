//! UI shutdown policy, independent of GTK and the network.
//! Keep the window/runtime alive until asynchronous session cleanup finishes.
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(30);
#[derive(Default)]
pub(crate) struct Shutdown {
    requested_at: Option<Instant>,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Wait,
    Close,
    TimedOut,
}
impl Shutdown {
    pub fn request(&mut self, now: Instant) {
        self.requested_at.get_or_insert(now);
    }
    pub fn cancel(&mut self) {
        self.requested_at = None;
    }
    pub fn is_pending(&self) -> bool {
        self.requested_at.is_some()
    }
    pub fn poll(&mut self, now: Instant, busy: bool) -> Action {
        let Some(started) = self.requested_at else {
            return Action::Wait;
        };
        if !busy {
            self.cancel();
            return Action::Close;
        }
        if now.saturating_duration_since(started) >= DEADLINE {
            self.cancel();
            return Action::TimedOut;
        }
        Action::Wait
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn closing_waits_for_session_completion() {
        let mut s = Shutdown::default();
        let now = Instant::now();
        s.request(now);
        assert_eq!(s.poll(now, true), Action::Wait);
        assert_eq!(s.poll(now + Duration::from_secs(1), false), Action::Close);
        assert!(!s.is_pending());
    }
    #[test]
    fn repeated_close_does_not_extend_deadline_or_force_teardown() {
        let mut s = Shutdown::default();
        let now = Instant::now();
        s.request(now);
        s.request(now + Duration::from_secs(20));
        assert_eq!(s.poll(now + DEADLINE, true), Action::TimedOut);
        assert_eq!(s.poll(now + DEADLINE, false), Action::Wait);
    }
    #[test]
    fn cleanup_failure_keeps_window_open_for_the_error() {
        let mut s = Shutdown::default();
        let now = Instant::now();
        s.request(now);
        s.cancel();
        assert_eq!(s.poll(now, false), Action::Wait);
    }
}
