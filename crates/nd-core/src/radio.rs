//! Radio silence while a stream is running.
//!
//! Looking for receivers and streaming to one of them compete for the **same
//! antenna**. A Wi-Fi Direct scan is not a discreet query: it makes the radio
//! hop across the social channels, and on every hop the link to the access
//! point goes silent for tens of milliseconds.
//!
//! This went unnoticed for a long time because protocol work gets tested with
//! example programs, and those look for nothing while they stream. Through the
//! GUI — which keeps scanning so that a device switched on later still shows up
//! in the list — the very same code produced choppy audio and extra delay:
//! video recovers by asking for retransmission, audio has a deadline to be
//! played and whatever misses it becomes a hole in the sound.
//!
//! So while a stream is running, scanning stops. mDNS discovery keeps going: a
//! handful of multicast packets on the channel we are already on, no hopping.

use std::sync::OnceLock;

use tokio::sync::watch;

/// How many streams are currently running.
fn sessions() -> &'static (watch::Sender<usize>, watch::Receiver<usize>) {
    static SESSIONS: OnceLock<(watch::Sender<usize>, watch::Receiver<usize>)> = OnceLock::new();
    SESSIONS.get_or_init(|| watch::channel(0))
}

/// Marks a running stream; releases the radio when dropped.
///
/// A guard rather than an on/off pair, on purpose: a session can end through an
/// error, a cancellation or end of stream, and forgetting the "off" on any one
/// of those paths would leave discovery disabled until the app is closed.
#[derive(Debug)]
pub struct QuietGuard {
    _private: (),
}

impl Drop for QuietGuard {
    fn drop(&mut self) {
        sessions().0.send_modify(|n| *n = n.saturating_sub(1));
    }
}

/// Asks for radio silence for as long as the returned value is alive.
pub fn quiet() -> QuietGuard {
    sessions().0.send_modify(|n| *n += 1);
    QuietGuard { _private: () }
}

/// Is a stream currently running?
pub fn is_quiet() -> bool {
    *sessions().1.borrow() > 0
}

/// Waits until a stream starts.
pub async fn until_quiet() {
    let mut rx = sessions().1.clone();
    while *rx.borrow_and_update() == 0 {
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// Waits until the last stream ends.
pub async fn until_free() {
    let mut rx = sessions().1.clone();
    while *rx.borrow_and_update() > 0 {
        if rx.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The state is global (it is the machine's radio, after all), so these
    /// tests cannot run at the same time: one would see the silence requested
    /// by another.
    async fn exclusive() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        LOCK.lock().await
    }

    #[tokio::test]
    async fn the_radio_is_free_until_someone_asks_for_silence() {
        let _lock = exclusive().await;
        assert!(!is_quiet());
        let guard = quiet();
        assert!(is_quiet());
        drop(guard);
        assert!(!is_quiet());
    }

    #[tokio::test]
    async fn silence_lasts_while_any_session_is_alive() {
        let _lock = exclusive().await;
        // Two streams at once: the first one to end must not turn scanning back
        // on and ruin the other.
        let a = quiet();
        let b = quiet();
        drop(a);
        assert!(is_quiet(), "a stream is still running");
        drop(b);
        assert!(!is_quiet());
    }

    #[tokio::test]
    async fn waiting_for_the_radio_wakes_up_when_it_frees() {
        let _lock = exclusive().await;
        let guard = quiet();
        let waiter = tokio::spawn(until_free());
        // Still busy: the wait must not be over.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());

        drop(guard);
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("the wait must wake up when the radio frees up")
            .unwrap();
    }
}
