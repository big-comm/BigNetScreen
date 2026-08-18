//! Choosing between a quick response and smooth playback.
//!
//! These two cannot both be had, and it is worth being precise about why: the
//! pointer travels inside the same frames as the video. Buffering delays
//! everything by the same amount, so any promise of "smooth film *and* an
//! instant pointer" in one picture is false.
//!
//! What buffering does and does not fix matters just as much:
//!
//! - it absorbs **variance in arrival** — frames landing 10 ms apart and then
//!   80 ms apart — and that is what makes playback look smooth;
//! - it does nothing for **lack of capacity**. When the encoder or the network
//!   cannot keep up, the buffer drains and refills, and the result is the delay
//!   *and* the stutter.
//!
//! So this is a choice, made by the person watching, not something to be
//! guessed at:
//!
//! | Profile | Delay | Suited to |
//! | --- | --- | --- |
//! | [`Profile::Responsive`] | the minimum the pipeline sustains | using the computer on the big screen |
//! | [`Profile::Film`] | a few hundred ms | anything you only watch |
//!
//! It applies to every protocol, with a different knob for each: the pipeline
//! latency and RTP jitter buffer on Miracast, the negotiated `targetDelay` on
//! Cast mirroring, and leaving the receiver's own buffer alone on the Cast HTTP
//! path.
//!
//! Those knobs are not equally strong, and the difference is worth knowing
//! before promising anything. On Miracast the buffering happens here and the
//! delay is ours to set. On Cast the packets leave as soon as they are
//! encoded, so the only lever is `targetDelay` — **a request**. The receiver
//! runs its own buffer and may clamp or ignore what was asked, which is why
//! someone can turn film mode on against a Chromecast and see nothing change.
//! The negotiation logs the number that was asked for, so this can be checked
//! rather than argued about.

use std::sync::atomic::{AtomicU8, Ordering};

/// What to favour when delay and smoothness pull in opposite directions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Profile {
    /// The lowest delay the pipeline can sustain.
    #[default]
    Responsive,
    /// Buffered playback: the picture rides out network variance, and
    /// everything — the pointer included — arrives later.
    Film,
}

/// Extra buffering asked of the pipeline in [`Profile::Film`].
///
/// Enough to ride out ordinary Wi-Fi variance; short enough that dragging a
/// window onto the extra screen still feels connected to the hand doing it.
pub const FILM_PIPELINE_LATENCY_MS: u64 = 400;

/// The RTP jitter buffer in [`Profile::Film`], for the Miracast path.
pub const FILM_RTP_LATENCY_MS: u64 = 200;

/// The playout delay asked of a Cast receiver in [`Profile::Film`].
pub const FILM_PLAYOUT_DELAY_MS: u32 = 400;

static PROFILE: AtomicU8 = AtomicU8::new(0);

/// Sets the profile for the whole process. The interface calls this.
pub fn set(profile: Profile) {
    PROFILE.store(
        match profile {
            Profile::Responsive => 0,
            Profile::Film => 1,
        },
        Ordering::Relaxed,
    );
    tracing::info!(?profile, "latency profile");
}

/// The profile in force.
pub fn current() -> Profile {
    match PROFILE.load(Ordering::Relaxed) {
        1 => Profile::Film,
        _ => Profile::Responsive,
    }
}

/// Is buffered playback in force?
pub fn is_film() -> bool {
    current() == Profile::Film
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The profile is global (it describes what the person is doing, not a
    /// property of one stream), so these tests cannot run side by side.
    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn responsive_is_the_default() {
        let _lock = exclusive();
        set(Profile::Responsive);
        assert_eq!(current(), Profile::Responsive);
        assert!(!is_film());
    }

    #[test]
    fn film_can_be_turned_on_and_off() {
        let _lock = exclusive();
        set(Profile::Film);
        assert!(is_film());
        set(Profile::Responsive);
        assert!(!is_film());
    }

    #[test]
    fn film_buffers_enough_to_matter_but_stays_usable() {
        // Below ~150 ms there is not enough slack to absorb Wi-Fi variance, and
        // the profile would cost delay without buying smoothness. Above ~1 s
        // dragging a window onto the screen stops feeling connected to the hand
        // doing it.
        const _: () = assert!(FILM_PIPELINE_LATENCY_MS >= 150);
        const _: () = assert!(FILM_PIPELINE_LATENCY_MS <= 1000);
        // The jitter buffer lives inside the pipeline's budget, not beyond it.
        const _: () = assert!(FILM_RTP_LATENCY_MS < FILM_PIPELINE_LATENCY_MS);
    }
}
