//! Proves that the audio guard puts the default output back.
//!
//! ```sh
//! cargo run -p nd-core --example audio_restore
//! ```
//!
//! The failure this guards against cannot be reproduced without a receiver: it
//! needs a real session to end, an effects chain to rebuild itself and
//! WirePlumber to rewrite the default. What *can* be reproduced is the part
//! that matters — **something moved the default and the guard moved it back** —
//! by moving it deliberately and watching.
//!
//! Two things had to be learnt the hard way here, and both are now built in:
//!
//! - an HDMI output with nothing plugged into it accepts `set-default-sink`
//!   with a successful exit code and **stays unselected**, so a run built on
//!   that candidate moves nothing and "passes" having tested nothing;
//! - the guard is quick. Checking whether the move landed *after* the guard is
//!   armed races with it, and losing that race reads as "the move failed".
//!
//! So a usable candidate is found first, with the guard out of the way, and
//! only then is the guard armed and the move repeated.

use std::process::Command;
use std::time::Duration;

use nd_core::audio_state::{AudioGuard, Defaults};

fn pactl(args: &[&str]) -> Option<String> {
    let output = Command::new("pactl").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).to_string())
}

fn set_default(sink: &str) {
    pactl(&["set-default-sink", sink]);
}

fn current_default() -> String {
    Defaults::now().sink.unwrap_or_default()
}

/// Finds a sink this machine will actually accept as the default.
///
/// Leaves the default back on `original` before returning, whatever happens.
fn usable_candidate(original: &str) -> Option<String> {
    let listed = pactl(&["list", "short", "sinks"])?;
    let candidates: Vec<String> = listed
        .lines()
        .filter_map(|line| line.split('\t').nth(1))
        .filter(|name| *name != original)
        .map(|name| name.to_string())
        .collect();

    for candidate in candidates {
        set_default(&candidate);
        std::thread::sleep(Duration::from_millis(300));
        let took = current_default() == candidate;
        set_default(original);
        std::thread::sleep(Duration::from_millis(200));
        if took {
            return Some(candidate);
        }
        println!("  {candidate} will not take the default; trying another");
    }
    None
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let Some(original) = Defaults::now().sink else {
        eprintln!("no default sink to test with (is a sound server running?)");
        return;
    };
    println!("default output now: {original}");

    println!("looking for a sink this machine will accept as default…");
    let Some(candidate) = usable_candidate(&original) else {
        println!("\nno other sink here will take the default; nothing can be proven");
        set_default(&original);
        return;
    };
    println!("  {candidate} works as a stand-in for the disturbance");

    // From here on it is the shape of a real session: the guard notes the
    // defaults while it lives, and defends them once it is dropped.
    let guard = AudioGuard::start();
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(guard);

    println!("moving the default to {candidate}, as the end of a session does");
    set_default(&candidate);

    tokio::time::sleep(Duration::from_millis(1500)).await;
    let after = current_default();
    println!("  a moment later: {after}");

    if after == original {
        println!("\n=> the guard put the default output back");
    } else {
        println!("\n=> the guard did NOT restore it; putting it back by hand");
        set_default(&original);
    }
}
