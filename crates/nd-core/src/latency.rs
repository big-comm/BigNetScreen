//! Choosing between a quick response and smooth playback.
//!
//! These two cannot both be had, and it is worth being precise about why: the
//! pointer travels inside the same frames as the video. Buffering delays
//! everything by the same amount, so any promise of "smooth film *and* an
//! instant pointer" in one picture is false.
//!
//! What buffering does and does not fix matters just as much:
//!
//! - it absorbs variance in arrival (frames landing 10 ms apart and then
//!   80 ms apart), and that is what makes playback look smooth;
//! - it does nothing for lack of capacity. When the encoder or the network
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
//! latency, held by the RTP sink, on Miracast; the negotiated `targetDelay` on
//! Cast mirroring; and leaving the receiver's own buffer alone on the Cast HTTP
//! path.
//!
//! The knobs are not equally strong. On Miracast the buffering happens here
//! and the delay is ours to set. On Cast the packets leave as soon as they are
//! encoded, so the only lever is `targetDelay`, a request the receiver may
//! clamp or ignore: film mode against a Chromecast can change nothing. The
//! negotiation logs the delay it asked for.

use std::sync::atomic::{AtomicU8, Ordering};

/// What to favour when delay and smoothness pull in opposite directions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Profile {
    /// As little delay as the receiver will agree to hold.
    ///
    /// Only the Cast paths can go below [`Profile::Responsive`]: on Miracast
    /// the buffering is ours and responsive already asks for the pipeline's own
    /// minimum, so there is nothing left to give up. A busy network or an
    /// impatient receiver stutters, which is why it is offered rather than
    /// assumed.
    Low,
    /// The lowest delay the pipeline can sustain.
    #[default]
    Responsive,
    /// Buffered playback: the picture rides out network variance, and
    /// everything, the pointer included, arrives later.
    Film,
}

/// Extra buffering asked of the pipeline in [`Profile::Film`].
///
/// Enough to ride out ordinary Wi-Fi variance; short enough that dragging a
/// window onto the extra screen still feels connected to the hand doing it.
pub const FILM_PIPELINE_LATENCY_MS: u64 = 400;

/// The playout delay asked of a Cast receiver in [`Profile::Film`].
pub const FILM_PLAYOUT_DELAY_MS: u32 = 400;

/// The playout delay asked of a Cast receiver in [`Profile::Low`].
///
/// Zero: ask the receiver to show each frame as it arrives and hold nothing.
/// Every millisecond here is added to the delay between the screen and the
/// television, and this profile is the one a person picks to *use* the computer
/// on that television rather than watch it.
///
/// The trade: with no buffer a late packet is too late, and on a congested
/// network that is a visible stutter. Zero has not been verified against a
/// receiver; 50 ms has.
pub const LOW_PLAYOUT_DELAY_MS: u32 = 0;

static PROFILE: AtomicU8 = AtomicU8::new(0);

/// Sets the profile for the whole process. The interface calls this.
pub fn set(profile: Profile) {
    PROFILE.store(
        match profile {
            Profile::Responsive => 0,
            Profile::Film => 1,
            Profile::Low => 2,
        },
        Ordering::Relaxed,
    );
    tracing::info!(?profile, "latency profile");
}

/// The profile in force.
pub fn current() -> Profile {
    match PROFILE.load(Ordering::Relaxed) {
        1 => Profile::Film,
        2 => Profile::Low,
        _ => Profile::Responsive,
    }
}

/// Is buffered playback in force?
pub fn is_film() -> bool {
    current() == Profile::Film
}

/// The playout delay to ask a Cast receiver for, given the profile.
///
/// The only knob where [`Profile::Low`] differs from [`Profile::Responsive`]:
/// everywhere else responsive already asks for the minimum, and there is
/// nothing below a minimum to offer.
pub fn cast_playout_delay_ms(responsive: u32) -> u32 {
    match current() {
        Profile::Low => LOW_PLAYOUT_DELAY_MS,
        Profile::Responsive => responsive,
        Profile::Film => FILM_PLAYOUT_DELAY_MS,
    }
}

/// Serializes the tests that change the profile or the settings that set it.
///
/// The profile is process-wide (it describes what the person is doing, not a
/// property of one stream), and loading the settings file overwrites it with
/// whatever the machine running the tests has configured. The settings are
/// loaded before the lock is handed out, so a test that reads them without the
/// lock finds them cached and never loads the file in the middle of another.
#[cfg(test)]
pub(crate) fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::settings::current();
    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_responsive() {
        assert_eq!(Profile::default(), Profile::Responsive);
    }

    #[test]
    fn film_switch_toggles() {
        let _lock = exclusive();
        set(Profile::Film);
        assert!(is_film());
        set(Profile::Responsive);
        assert!(!is_film());
    }

    #[test]
    fn film_latency_stays_in_range() {
        // Below 150 ms there is not enough slack to absorb Wi-Fi variance; above
        // 1 s dragging a window onto the screen stops feeling connected.
        const _: () = assert!(FILM_PIPELINE_LATENCY_MS >= 150);
        const _: () = assert!(FILM_PIPELINE_LATENCY_MS <= 1000);
    }
}
