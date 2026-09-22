//! The session service: everything BigNetScreen does that is not drawing.
//!
//! A window is one way to ask for a stream, not the thing that holds it. This
//! crate holds it, so closing the window stops drawing and nothing else — and
//! so a panel applet, a script or a keyboard shortcut can ask for the same
//! things the window asks for, through the same door.
//!
//! - [`engine`] is the state and the work: discovery, the running cast, the
//!   files being sent.
//! - [`wire`] is what crosses D-Bus.
//! - [`dbus`] is the interface and its client proxy.
//!
//! ## The service does not speak the user's language
//!
//! It may be started by D-Bus activation with a bare environment, and its
//! clients can each be under a different locale, so it reports state and never
//! prose. The one exception is the page a **web browser** is shown when someone
//! shares to one: that text is not for a client, it is for a stranger on the
//! network, and the client that started the session passes it in already
//! translated. Without it the page stays in the source language — see
//! [`web_page_text`].

pub mod cast;
pub mod dbus;
pub mod engine;
pub mod wire;

/// The bus name, object path and interface the service answers on.
///
/// `.Service` on the end, and not the bare application id: GTK claims its
/// application id on the session bus for every `GApplication`, so the window
/// already owns `br.com.biglinux.BigNetScreen`. Sharing it meant the service
/// could never take the name while a window was open, and a client calling it
/// reached the window — which serves no interface of ours. Found by running
/// the two together; neither one alone shows it.
pub const BUS_NAME: &str = "br.com.biglinux.BigNetScreen.Service";
pub const OBJECT_PATH: &str = "/br/com/biglinux/BigNetScreen";

/// What the web page says, in whatever language the caller supplied.
///
/// The keys match the field names. Anything missing keeps the source-language
/// wording, which is the honest fallback: the service has no catalogue and no
/// locale worth trusting, and a client that cares passes its own strings.
pub fn web_page_text(title: String, overrides: &[(String, String)]) -> nd_webrtc::PageText {
    let pick = |key: &str, fallback: &str| -> String {
        overrides
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| fallback.to_string())
    };
    nd_webrtc::PageText {
        title,
        prompt: pick(
            "prompt",
            "Enter the PIN shown on the computer that is sharing.",
        ),
        join: pick("join", "Watch"),
        wrong_pin: pick(
            "wrong_pin",
            "That PIN is not right. Check the computer's screen.",
        ),
        locked: pick(
            "locked",
            "Too many attempts. Wait half a minute and try again.",
        ),
        connecting: pick("connecting", "Connecting…"),
        failed: pick(
            "failed",
            "Could not connect. Make sure both devices are on the same network.",
        ),
        ended: pick("ended", "The sharing has ended."),
        fullscreen_hint: pick(
            "fullscreen_hint",
            "Tap or click the picture for full screen.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supplied_page_text_wins_and_the_rest_keeps_the_source_language() {
        let text = web_page_text("Sala".into(), &[("join".into(), "Assistir".into())]);
        assert_eq!(text.title, "Sala");
        assert_eq!(text.join, "Assistir");
        assert_eq!(text.ended, "The sharing has ended.");
    }
}
