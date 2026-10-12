//! What the window says, in the person's language.
//!
//! The service may be started by D-Bus activation with a bare environment, and
//! its other clients may run under another locale, so nothing translated
//! crosses the bus: the service publishes kinds and tags, and these functions
//! turn them into sentences.

use nd_core::settings::Settings;
use nd_service::wire::{self, StatusKind, refusal};

use crate::{tr, tr_n};

/// The status line under the window title. A kind this build has no sentence
/// for is shown as its tag rather than as an empty line.
pub(super) fn describe_status(status: &wire::Status, session: &wire::Session) -> String {
    match status.kind() {
        Some(StatusKind::Searching) => tr!("Searching…"),
        Some(StatusKind::Found) => tr_n!(
            "{} receiver found",
            "{} receivers found",
            status.count as usize
        )
        .replace("{}", &status.count.to_string()),
        Some(StatusKind::Empty) => tr!("No receivers found"),
        Some(StatusKind::DiscoveryOff) => tr!("Automatic discovery is off"),
        Some(StatusKind::Connecting) => format!("{} · {}", status.display_name, tr!("Connecting…")),
        Some(StatusKind::Stopping) => tr!("Stopping…"),
        Some(StatusKind::Sending) => tr!("Sending files…"),
        Some(StatusKind::NdiRuntimeMissing) => tr!("NDI runtime required"),
        Some(StatusKind::Error) => tr!("Sharing failed"),
        Some(StatusKind::Streaming) => {
            if session.width > 0 {
                format!(
                    "{} · {} × {} · {} Hz",
                    status.display_name, session.width, session.height, session.fps
                )
            } else {
                format!("{} · {}", status.display_name, tr!("Connected"))
            }
        }
        Some(StatusKind::Idle) | None => status.kind.clone(),
    }
}

/// How a failure to save the preferences begins.
pub(super) const SAVE_FAILED: &str = "could not save settings";

/// A refusal from the service, in the person's language.
///
/// The service answers with a tag for the cases it knows and the underlying
/// message for the rest, so the common refusals read as sentences and nothing
/// is swallowed.
pub(super) fn describe_refusal(reason: &str) -> String {
    if reason.is_empty() {
        return String::new();
    }
    known_refusal(reason)
        .unwrap_or_else(|| tr!("Sharing could not be completed. Open Details to see why."))
}

/// The sentence for a refusal this build recognises.
pub(super) fn known_refusal(reason: &str) -> Option<String> {
    if reason.contains(refusal::SOUND_CARD) {
        return Some(tr!(
            "Could not prepare the BigNetScreen sound output. Check the audio options in Settings and try again."
        ));
    }
    if reason.starts_with(SAVE_FAILED) {
        return Some(tr!("Could not save settings"));
    }
    let tail = reason.rsplit(": ").next().unwrap_or(reason);
    let sentence = match tail {
        refusal::UNKNOWN_RECEIVER => {
            tr!("That device is no longer available. Search for devices and pick it again.")
        }
        refusal::NOT_CASTABLE => {
            tr!("This device can be found on the network, but BigNetScreen cannot share to it.")
        }
        refusal::NDI_UNAVAILABLE => {
            tr!("NDI support could not be loaded. Reinstall BigNetScreen to restore it.")
        }
        refusal::NDI_CPU_UNSUPPORTED => tr!(
            "The NDI runtime does not support this processor, so installing it again will not help."
        ),
        refusal::WEBRTC_UNAVAILABLE => tr!(
            "Sharing with a web browser needs GStreamer's WebRTC plugins, which this system \
             does not have."
        ),
        refusal::STREAM_RUNNING => tr!("A stream is already running"),
        nd_chromecast::identity::IDENTITY_CHANGED => tr!(
            "This receiver did not prove it is the same device used before, so the screen was \
             not sent. Someone on the network may be impersonating it. If you replaced or reset \
             the device, open it in Devices and choose Trust again."
        ),
        _ => return None,
    };
    Some(sentence)
}

pub(super) fn audio_summary(settings: &Settings) -> String {
    let sound = if !settings.system_audio {
        tr!("System audio off")
    } else if settings.virtual_audio {
        tr!("Only applications routed to BigNetScreen")
    } else {
        tr!("All system audio")
    };
    let microphone = if settings.microphone {
        tr!("Microphone on")
    } else {
        tr!("Microphone off")
    };
    format!("{sound} · {microphone}")
}

/// Turns a provider's untranslated error into something a person can act on.
pub(super) fn friendly_reason(provider: &str, reason: &str) -> String {
    if provider == "wfd-p2p" {
        if nd_capture::is_sandboxed() {
            return tr!(
                "Miracast does not work in the Flatpak build (it needs the system NetworkManager)"
            );
        }
        // Checked before the Wi-Fi Direct case: a computer with no Wi-Fi
        // adapter has no card to blame.
        if reason.contains(nd_net::p2p::NO_WIFI_ADAPTER) {
            return tr!(
                "Miracast needs a Wi-Fi adapter, and this computer has none. Casting over \
                 the network still works."
            );
        }
        if reason.contains(nd_net::p2p::NO_P2P_DEVICE) {
            return tr!("Miracast unavailable: this Wi-Fi card does not support Wi-Fi Direct");
        }
        return tr!(
            "Miracast is unavailable. Check that Wi-Fi is enabled, or share with a web browser."
        );
    }
    if matches!(provider, "mdns" | "dlna") {
        return tr!("Local network discovery is unavailable. Check your network connection.");
    }
    tr!("Device discovery is unavailable. Open Details to see why.")
}

/// What the web page says, in the application's language.
pub(super) fn web_page_text() -> Vec<(String, String)> {
    [
        (
            "prompt",
            tr!("Enter the PIN shown on the computer that is sharing."),
        ),
        ("join", tr!("Watch")),
        (
            "wrong_pin",
            tr!("That PIN is not right. Check the computer's screen."),
        ),
        (
            "locked",
            tr!("Too many attempts. Wait half a minute and try again."),
        ),
        ("connecting", tr!("Connecting…")),
        (
            "failed",
            tr!("Could not connect. Make sure both devices are on the same network."),
        ),
        ("ended", tr!("The sharing has ended.")),
        (
            "fullscreen_hint",
            tr!("Tap or click the picture for full screen."),
        ),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(kind: &str) -> wire::Status {
        wire::Status {
            kind: kind.into(),
            ..wire::Status::of(StatusKind::Idle)
        }
    }

    #[test]
    fn unknown_shape_is_shown() {
        // A newer service could publish a kind this build has no sentence for.
        // Printing the tag beats an empty subtitle that says nothing is wrong.
        let unknown = describe_status(&status("something-new"), &wire::Session::idle());
        assert_eq!(unknown, "something-new");
    }

    #[test]
    fn negotiated_mode_shown_when_known() {
        let mut session = wire::Session::idle();
        session.id = "x".into();
        let mut streaming = status("streaming");
        streaming.display_name = "TV".into();
        assert!(!describe_status(&streaming, &session).contains('×'));

        session.width = 1280;
        session.height = 720;
        session.fps = 60;
        let described = describe_status(&streaming, &session);
        assert!(described.contains("1280 × 720"), "{described}");
    }

    #[test]
    fn wrapped_refusal_reads_as_sentence() {
        // zbus prefixes the reason with the error name; the tag is at the end.
        let wrapped = "org.freedesktop.DBus.Error.Failed: unknown-receiver";
        assert_eq!(
            describe_refusal(wrapped),
            describe_refusal("unknown-receiver")
        );
        assert!(!describe_refusal(wrapped).is_empty());
        assert_eq!(
            describe_refusal("something odd"),
            tr!("Sharing could not be completed. Open Details to see why.")
        );
        assert!(describe_refusal("").is_empty());
    }
}
