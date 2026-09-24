//! A virtual sound card, so a transmission can carry *chosen* audio.
//!
//! Capturing the default output's monitor sends whatever the computer is
//! playing — a notification, a private call, the video the person is watching
//! on the other screen. This module adds a second, fake output device. Anything
//! routed to it in the system's sound settings is transmitted; everything else
//! stays on the computer.
//!
//! The card is a PipeWire/PulseAudio `module-null-sink`: a sink that discards
//! what it receives and exposes a `.monitor` source carrying the same audio.
//! We load it and read its monitor.
//!
//! ## It lives as long as the service
//!
//! The module belongs to the audio server, not to this process, so nothing
//! takes it away but us: the service calls [`remove`] on its way out, and
//! [`ensure`] when it starts with the option on or a cast needs it. It used to
//! outlive the service on purpose, so that routing survived; it does anyway,
//! because the session manager remembers a stream's target by the sink's name.
//! What outliving it cost was a sink that discards everything sitting on the
//! output list of a machine that was no longer transmitting — and a person who
//! had chosen it as their output, to transmit, left with no sound at all.
//!
//! Nothing here changes a default device. Adding an output is not choosing one.

use std::time::Duration;

use crate::{NdError, Result};

/// The sink's name in the audio server. Also the prefix of its monitor.
pub const SINK_NAME: &str = "bignetscreen";

/// The monitor source to capture from: what was routed to the card.
pub const MONITOR: &str = "bignetscreen.monitor";

/// What the person sees in their sound settings.
const DESCRIPTION: &str = "BigNetScreen";

/// `pactl` answers in milliseconds; this only bounds a server that is wedged.
const PACTL_TIMEOUT: Duration = Duration::from_secs(3);
static CARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn pactl(args: &[&str]) -> Result<String> {
    let output = tokio::time::timeout(
        PACTL_TIMEOUT,
        tokio::process::Command::new("pactl")
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| NdError::Audio("the sound server did not answer `pactl`".into()))?
    .map_err(|e| NdError::Audio(format!("could not run `pactl`: {e}")))?;

    if !output.status.success() {
        return Err(NdError::Audio(format!(
            "`pactl {}` failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The index of the module backing our card, if it is loaded.
///
/// Matched on the arguments rather than on the module name: `module-null-sink`
/// is what a dozen other programs use for their own cards, and unloading by
/// name would take theirs down with ours.
async fn module_index() -> Result<Option<u32>> {
    let listing = pactl(&["list", "modules", "short"]).await?;
    Ok(listing.lines().find_map(|line| {
        let mut fields = line.split('\t');
        let index = fields.next()?;
        if fields.next()? != "module-null-sink" {
            return None;
        }
        fields
            .next()?
            .split_whitespace()
            .any(|argument| argument == format!("sink_name={SINK_NAME}"))
            .then(|| index.parse().ok())
            .flatten()
    }))
}

/// Whether the card is there right now.
pub async fn exists() -> Result<bool> {
    Ok(module_index().await?.is_some())
}

/// Makes sure the card exists. Doing it twice is not an error.
pub async fn ensure() -> Result<()> {
    let _card = CARD.lock().await;
    if exists().await? {
        return Ok(());
    }
    // The inner quotes are part of the value `pactl` parses, not shell syntax:
    // this is passed as one argv element, so the shell never sees it.
    pactl(&[
        "load-module",
        "module-null-sink",
        &format!("sink_name={SINK_NAME}"),
        &format!("sink_properties=device.description=\"{DESCRIPTION}\""),
    ])
    .await?;
    tracing::info!(sink = SINK_NAME, "virtual sound card created");
    Ok(())
}

/// Removes the card. Removing one that is not there is not an error.
pub async fn remove() -> Result<()> {
    let _card = CARD.lock().await;
    let Some(index) = module_index().await? else {
        return Ok(());
    };
    pactl(&["unload-module", &index.to_string()]).await?;
    tracing::info!(sink = SINK_NAME, "virtual sound card removed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_monitor_is_the_sink_plus_the_suffix_the_server_appends() {
        // Two constants that must agree, and nothing at runtime would notice
        // if they drifted — the capture would simply record silence from a
        // device that does not exist.
        assert_eq!(MONITOR, format!("{SINK_NAME}.monitor"));
    }

    /// Only meaningful against a running sound server, so it is not part of the
    /// ordinary gate. `cargo test -p nd-core -- --ignored virtual_sound_card`
    #[tokio::test]
    #[ignore = "needs a running PipeWire or PulseAudio"]
    async fn a_virtual_sound_card_can_be_created_found_and_removed() {
        let was_there = exists().await.unwrap();
        ensure().await.expect("create");
        assert!(exists().await.unwrap(), "created but not found");
        // Idempotent: a second call must not load a second module.
        ensure().await.expect("create again");
        let listing = pactl(&["list", "modules", "short"]).await.unwrap();
        let ours = listing
            .lines()
            .filter(|l| l.contains(&format!("sink_name={SINK_NAME}")))
            .count();
        assert_eq!(ours, 1, "a second call loaded a second card");

        remove().await.expect("remove");
        assert!(!exists().await.unwrap(), "removed but still found");
        remove().await.expect("removing twice is not an error");

        if was_there {
            ensure()
                .await
                .expect("put back what the machine already had");
        }
    }
}
