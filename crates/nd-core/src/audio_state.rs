//! Leaving the computer's sound the way it was found.
//!
//! Sharing a screen captures the default output's monitor. That capture is an
//! ordinary recording — it changes no setting — but its *arrival and departure*
//! shake the audio graph, and on a desktop with an effects chain in the middle
//! (JamesDSP, an echo canceller, a filter-chain node) the shaking has a cost:
//!
//! 1. the session ends and the capture disappears;
//! 2. the effects sink goes idle, and its owner disconnects and rebuilds its
//!    filter — for a moment that sink **does not exist** in the graph;
//! 3. WirePlumber, finding the configured default gone, falls back to the raw
//!    hardware output **and writes that down** as the configured default;
//! 4. the effects sink comes back, and nothing moves the default onto it again.
//!
//! The result is what a person actually experiences: after sharing, the sound
//! comes out of the wrong device, plugging headphones in does nothing, and the
//! desktop's own sound settings appear not to work — because the preference
//! they are showing was rewritten underneath them.
//!
//! None of that is a setting this application changed, which is precisely why
//! it has to be this application that puts it back. So: while a session runs,
//! the current defaults are noted; when it ends, anything that moves them in
//! the next few seconds is undone.
//!
//! ## What it deliberately does not do
//!
//! - **It never overrides a choice the person made.** The value restored is the
//!   last one seen *while the session was healthy*, so moving the sound to
//!   headphones halfway through a cast is kept, not reverted.
//! - **It never resurrects a device that is gone.** If the output was unplugged
//!   during the session, restoring it would point the desktop at nothing.
//! - **It gives up quietly.** Without `pactl` there is no restoring to be done,
//!   and that is logged once rather than treated as a failure to cast.

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often the defaults are re-read while a session runs.
///
/// Frequent enough to notice the person switching output mid-cast, rare enough
/// that the cost — one short-lived process — is irrelevant next to encoding
/// video.
const WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// How long after a session ends the defaults are still guarded.
///
/// Measured against the behaviour being corrected: the effects daemon takes
/// about a second to rebuild its filter, and WirePlumber rewrites the default
/// within that window. Five seconds covers it with room to spare.
const SETTLE_WINDOW: Duration = Duration::from_secs(5);

/// How often to check during that window.
const SETTLE_INTERVAL: Duration = Duration::from_millis(400);

/// Which default is being talked about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Sink,
    Source,
}

impl Kind {
    fn get_command(self) -> &'static str {
        match self {
            Kind::Sink => "get-default-sink",
            Kind::Source => "get-default-source",
        }
    }

    fn set_command(self) -> &'static str {
        match self {
            Kind::Sink => "set-default-sink",
            Kind::Source => "set-default-source",
        }
    }

    fn list_argument(self) -> &'static str {
        match self {
            Kind::Sink => "sinks",
            Kind::Source => "sources",
        }
    }
}

/// The default output and input, as they were.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Defaults {
    pub sink: Option<String>,
    pub source: Option<String>,
}

impl Defaults {
    /// Reads what the sound server currently considers default.
    pub fn now() -> Self {
        Self {
            sink: read_default(Kind::Sink),
            source: read_default(Kind::Source),
        }
    }

    /// Is there anything here to restore?
    pub fn is_empty(&self) -> bool {
        self.sink.is_none() && self.source.is_none()
    }
}

/// Watches the defaults for as long as it is alive, and puts them back if the
/// end of the session disturbs them.
///
/// Created before the pipeline is built and dropped after it is torn down.
/// Dropping it is what starts the guarding: the value it defends is the last
/// one it saw while the session was still running.
pub struct AudioGuard {
    /// The last defaults seen while the session was healthy.
    last: Arc<Mutex<Defaults>>,
    /// Tells the watcher to stop.
    running: Arc<std::sync::atomic::AtomicBool>,
}

impl AudioGuard {
    /// Starts noting the defaults.
    ///
    /// Returns a guard even when there is no `pactl`: the session must not
    /// depend on this, and a guard that has nothing to restore simply does
    /// nothing.
    pub fn start() -> Self {
        let last = Arc::new(Mutex::new(Defaults::now()));
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));

        {
            let last = last.clone();
            let running = running.clone();
            tokio::spawn(async move {
                while running.load(std::sync::atomic::Ordering::SeqCst) {
                    tokio::time::sleep(WATCH_INTERVAL).await;
                    if !running.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    // Whatever is default *now*, while the session is healthy,
                    // is the person's current intention — including a switch
                    // they made halfway through the cast.
                    let current = Defaults::now();
                    if !current.is_empty() {
                        if let Ok(mut guard) = last.lock() {
                            *guard = current;
                        }
                    }
                }
            });
        }

        Self { last, running }
    }
}

impl Drop for AudioGuard {
    fn drop(&mut self) {
        self.running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let expected = match self.last.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        if expected.is_empty() {
            return;
        }
        tokio::spawn(async move { guard_defaults(expected).await });
    }
}

/// Keeps the defaults on `expected` for a few seconds after a session ends.
async fn guard_defaults(expected: Defaults) {
    let deadline = tokio::time::Instant::now() + SETTLE_WINDOW;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(SETTLE_INTERVAL).await;
        restore(Kind::Sink, expected.sink.as_deref());
        restore(Kind::Source, expected.source.as_deref());
    }
}

/// Puts one default back, if something moved it and the device is still there.
fn restore(kind: Kind, expected: Option<&str>) {
    let Some(expected) = expected else {
        return;
    };
    let Some(current) = read_default(kind) else {
        return;
    };
    if current == expected {
        return;
    }
    // Never point the desktop at a device that is gone: an output unplugged
    // during the session is not something to restore, it is something that
    // genuinely left.
    if !device_exists(kind, expected) {
        tracing::info!(
            ?kind,
            %expected,
            "the previous default is no longer present; leaving the new one alone"
        );
        return;
    }
    tracing::info!(
        ?kind,
        %current,
        %expected,
        "the default moved when the session ended; putting it back"
    );
    pactl(&[kind.set_command(), expected]);
}

/// Reads a default device's name.
fn read_default(kind: Kind) -> Option<String> {
    let output = pactl(&[kind.get_command()])?;
    let name = output.trim().to_string();
    // `pactl` answers "@DEFAULT_SINK@" when it has nothing better to say, which
    // is not a device name and must not be handed back to `set-default-sink`.
    if name.is_empty() || name.starts_with('@') {
        return None;
    }
    Some(name)
}

/// Is this device still in the graph?
fn device_exists(kind: Kind, name: &str) -> bool {
    let Some(output) = pactl(&["list", "short", kind.list_argument()]) else {
        return false;
    };
    output
        .lines()
        .filter_map(|line| line.split('\t').nth(1))
        .any(|listed| listed == name)
}

/// Runs `pactl`, or gives up.
///
/// Everything this module does is a courtesy; nothing here may turn into a
/// reason a cast fails. A missing `pactl`, a sound server that is not running,
/// a non-zero exit — all of them mean the same thing: there is nothing to
/// restore.
fn pactl(args: &[&str]) -> Option<String> {
    let output = Command::new("pactl").args(args).output().ok()?;
    if !output.status.success() {
        tracing::debug!(?args, status = ?output.status, "pactl refused");
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_to_restore_is_recognised_as_such() {
        assert!(Defaults::default().is_empty());
        assert!(!Defaults {
            sink: Some("speakers".into()),
            source: None,
        }
        .is_empty());
    }

    #[test]
    fn the_two_kinds_never_share_a_command() {
        // Sink and source are set by different commands; swapping them would
        // point the microphone at the speakers, and both calls would "succeed".
        assert_ne!(Kind::Sink.get_command(), Kind::Source.get_command());
        assert_ne!(Kind::Sink.set_command(), Kind::Source.set_command());
        assert_ne!(Kind::Sink.list_argument(), Kind::Source.list_argument());
    }

    #[test]
    fn the_guarding_window_outlasts_the_disturbance() {
        // The effects daemon rebuilds its filter about a second after the
        // capture ends, and the default is rewritten in that window. Checking
        // only once, immediately, would restore the value before the thing that
        // overwrites it has even happened.
        assert!(SETTLE_WINDOW >= Duration::from_secs(3));
        assert!(SETTLE_INTERVAL < SETTLE_WINDOW);
        assert!(
            SETTLE_WINDOW.as_millis() / SETTLE_INTERVAL.as_millis() >= 5,
            "too few checks to catch a late change"
        );
    }
}
