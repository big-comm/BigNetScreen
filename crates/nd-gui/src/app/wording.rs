//! What the window says, in the person's language.
//!
//! The service may be started by D-Bus activation with a bare environment, and
//! its other clients may run under another locale, so nothing translated
//! crosses the bus: the service publishes kinds and tags, and these functions
//! turn them into sentences.

use nd_core::settings::Settings;
use nd_service::wire;

use crate::{tr, tr_n};

/// The status line under the window title. A kind this build has no sentence
/// for is shown as its tag rather than as an empty line.
pub(super) fn describe_status(status: &wire::Status, session: &wire::Session) -> String {
    match status.kind.as_str() {
        "searching" => tr!("Searching…"),
        "found" => tr_n!(
            "{} receiver found",
            "{} receivers found",
            status.count as usize
        )
        .replace("{}", &status.count.to_string()),
        "empty" => tr!("No receivers found"),
        "discovery-off" => tr!("Automatic discovery is off"),
        "connecting" => format!("{} · {}", status.display_name, tr!("Connecting…")),
        "stopping" => tr!("Stopping…"),
        "sending" => tr!("Sending files…"),
        "ndi-runtime-missing" => tr!("NDI runtime required"),
        "error" => describe_refusal(&status.detail),
        "streaming" => {
            if session.width > 0 {
                format!(
                    "{} · {} × {} · {} Hz",
                    status.display_name, session.width, session.height, session.fps
                )
            } else {
                format!("{} · {}", status.display_name, tr!("Connected"))
            }
        }
        _ => status.kind.clone(),
    }
}

/// A refusal from the service, in the person's language.
///
/// The service answers with a tag for the cases it knows and the underlying
/// message for the rest, so the common refusals read as sentences and nothing
/// is swallowed.
pub(super) fn describe_refusal(reason: &str) -> String {
    if reason.is_empty() {
        return String::new();
    }
    if reason.contains("virtual sound card") {
        return tr!(
            "Could not prepare the BigNetScreen sound output. Check the audio options in Settings and try again."
        );
    }
    if reason.contains("could not save settings") {
        return tr!("Could not save settings");
    }
    let tail = reason.rsplit(": ").next().unwrap_or(reason);
    match tail {
        "unknown-receiver" => tr!("That device is no longer available"),
        "not-castable" => tr!("shows up in discovery only"),
        "ndi-unavailable" => tr!("NDI unavailable. See the NDI setup guide."),
        "webrtc-unavailable" => tr!(
            "Sharing with a web browser needs GStreamer's WebRTC plugins, which this system \
             does not have."
        ),
        "a stream is already running" => tr!("A stream is already running"),
        nd_chromecast::identity::IDENTITY_CHANGED => tr!(
            "This receiver did not prove it is the same device used before, so the screen was \
             not sent. Someone on the network may be impersonating it."
        ),
        _ => tr!("Sharing could not be completed. Open Details to see the reason."),
    }
}

pub(super) fn audio_summary(settings: &Settings) -> String {
    let sound = if !settings.system_audio {
        tr!("Application sound off")
    } else if settings.virtual_audio {
        tr!("Sound routed to BigNetScreen")
    } else {
        tr!("All computer sound")
    };
    let microphone = if settings.microphone {
        tr!("Microphone on")
    } else {
        tr!("Microphone off")
    };
    format!("{sound} · {microphone}")
}

/// Turns a provider's untranslated error into something a person can act on.
/// The Miracast cases match substrings of the errors in `nd_net::p2p`.
pub(super) fn friendly_reason(provider: &str, reason: &str) -> String {
    if provider == "wfd-p2p" {
        if nd_capture::is_sandboxed() {
            return tr!(
                "Miracast does not work in the Flatpak build (it needs the system NetworkManager)"
            );
        }
        // Checked before the Wi-Fi Direct case: a computer with no Wi-Fi
        // adapter has no card to blame.
        if reason.contains("no Wi-Fi adapter") {
            return tr!(
                "Miracast needs a Wi-Fi adapter, and this computer has none. Casting over \
                 the network still works."
            );
        }
        if reason.contains("Wi-Fi P2P") || reason.contains("Wi-Fi Direct") {
            return tr!("Miracast unavailable: this Wi-Fi card does not support Wi-Fi Direct");
        }
        return tr!(
            "Miracast is unavailable. Check that Wi-Fi is enabled, or share with a web browser."
        );
    }
    if matches!(provider, "mdns" | "dlna") {
        return tr!("Local network discovery is unavailable. Check your network connection.");
    }
    tr!("Device discovery is unavailable. Open Details to see the reason.")
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
        wire::Status::of(kind)
    }

    #[test]
    fn a_shape_the_client_does_not_know_is_shown_rather_than_swallowed() {
        // A newer service could publish a kind this build has no sentence for.
        // Printing the tag beats an empty subtitle that says nothing is wrong.
        let unknown = describe_status(&status("something-new"), &wire::Session::idle());
        assert_eq!(unknown, "something-new");
    }

    #[test]
    fn the_negotiated_mode_is_shown_only_once_there_is_one() {
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
    fn a_refusal_reads_as_a_sentence_even_wrapped_in_a_bus_error() {
        // zbus prefixes the reason with the error name; the tag is at the end.
        let wrapped = "org.freedesktop.DBus.Error.Failed: unknown-receiver";
        assert_eq!(
            describe_refusal(wrapped),
            describe_refusal("unknown-receiver")
        );
        assert!(!describe_refusal(wrapped).is_empty());
        assert_eq!(
            describe_refusal("something odd"),
            tr!("Sharing could not be completed. Open Details to see the reason.")
        );
        assert!(describe_refusal("").is_empty());
    }
}
