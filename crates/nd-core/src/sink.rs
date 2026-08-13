//! The **sink** (receiver) abstraction: the lifecycle of a cast session.
//!
//! It mirrors the good design of the C project's `NdSink`, but with the state
//! machine spelled out in an `enum` (instead of loose integers) and an `async`
//! lifecycle (instead of the callback soup plus a hand-rolled `GCancellable`).

use std::net::SocketAddr;
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;

use crate::capture::CaptureSource;
use crate::Result;

/// The receiver's protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinkKind {
    /// Google Chromecast (mDNS + Cast + HTTP).
    Chromecast,
    /// Apple AirPlay (mDNS). Discovered, but casting is out of scope.
    AirPlay,
    /// Wi-Fi Display over Wi-Fi Direct / P2P.
    WfdP2p,
    /// Wi-Fi Display over infrastructure (MICE, ordinary LAN).
    WfdMice,
    /// Fake sink for testing (`NETWORK_DISPLAYS_DUMMY`).
    Dummy,
}

impl SinkKind {
    /// Whether streaming to this kind is implemented.
    ///
    /// The UI uses this so it never offers "Cast" where only an error awaits:
    /// receivers that are merely discovered still show up, but disabled.
    pub fn is_castable(self) -> bool {
        match self {
            SinkKind::Chromecast | SinkKind::WfdP2p | SinkKind::WfdMice => true,
            // AirPlay requires FairPlay/SAP; streaming is out of scope.
            SinkKind::AirPlay => false,
            SinkKind::Dummy => false,
        }
    }

    /// A short, stable label for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            SinkKind::Chromecast => "chromecast",
            SinkKind::AirPlay => "airplay",
            SinkKind::WfdP2p => "wfd-p2p",
            SinkKind::WfdMice => "wfd-mice",
            SinkKind::Dummy => "dummy",
        }
    }
}

/// The possible states of a session.
///
/// The transition into [`SinkState::Error`] is **terminal** and must not be
/// silently overwritten by `Disconnected` (a real bug in the C code, where
/// `closed_cb` masked the error in the UI). [`SinkStatus::set`] enforces that.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinkState {
    /// Idle, available to connect.
    Disconnected,
    /// Forming the Wi-Fi Direct group (WFD/P2P only).
    Connecting,
    /// Making sure of the firewall zone (WFD/P2P on the native build only).
    EnsuringFirewall,
    /// Waiting for the receiver's transport socket.
    WaitSocket,
    /// Connected; waiting for the media flow to start.
    WaitStreaming,
    /// Streaming.
    Streaming,
    /// Terminal error; the message describes the cause for the UI.
    Error,
}

impl SinkState {
    /// A session is under way (the UI shows progress).
    pub fn is_busy(self) -> bool {
        matches!(
            self,
            SinkState::Connecting
                | SinkState::EnsuringFirewall
                | SinkState::WaitSocket
                | SinkState::WaitStreaming
        )
    }
}

/// A sink's shared state, poisoning-proof.
///
/// `Mutex::lock().unwrap()` turned any single panic into a cascade of panics on
/// every later state read. The contents here are small and consistent by
/// construction, so recovering from poisoning is safe.
#[derive(Debug, Default)]
pub struct SinkStatus {
    inner: Mutex<StatusInner>,
}

/// What the session actually settled on, once it has settled on it.
///
/// Every field here is **negotiated, not requested**. A receiver that agreed to
/// 1280x720 while the preference asked for 1080p is described by what it
/// agreed to, because that is what is on the wall. Showing the request instead
/// would be a number the interface made up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamLink {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Where the receiver is reached, and on which control port.
    ///
    /// Kept because it is the only address that is certainly reachable: a
    /// Miracast receiver is announced by MAC and only acquires an IP once the
    /// Wi-Fi Direct group exists.
    pub endpoint: Option<SocketAddr>,
}

impl StreamLink {
    /// "1920 × 1080 · 60 Hz", for showing.
    ///
    /// The multiplication sign is the typographic one, not the letter x.
    pub fn describe(&self) -> String {
        format!("{} × {} · {} Hz", self.width, self.height, self.fps)
    }
}

#[derive(Clone, Debug)]
struct StatusInner {
    state: SinkState,
    message: Option<String>,
    link: Option<StreamLink>,
}

impl Default for StatusInner {
    fn default() -> Self {
        Self {
            state: SinkState::Disconnected,
            message: None,
            link: None,
        }
    }
}

impl SinkStatus {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current state.
    pub fn state(&self) -> SinkState {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .state
    }

    /// The message attached to the current state (usually the error's cause).
    pub fn message(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .message
            .clone()
    }

    /// What the running session settled on, if it has got that far.
    pub fn link(&self) -> Option<StreamLink> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .link
    }

    /// Records what the session negotiated.
    ///
    /// Called by the protocol once, after negotiation and before the first
    /// frame — the point at which the numbers stop being a request.
    pub fn set_link(&self, link: StreamLink) {
        tracing::info!(?link, "session negotiated");
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .link = Some(link);
    }

    /// Changes the state.
    ///
    /// An error state is **not** overwritten by `Disconnected`: that is how the
    /// reason for a failure survives long enough for the user to read it. Use
    /// [`Self::reset`] to clear it explicitly.
    pub fn set(&self, state: SinkState) {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.state == SinkState::Error && state == SinkState::Disconnected {
            return;
        }
        guard.state = state;
        if state != SinkState::Error {
            guard.message = None;
        }
        // The negotiated mode describes a *running* session. Keeping it past
        // the end would leave "1920 × 1080 · 60 Hz" on screen next to a
        // receiver that is no longer connected.
        if matches!(state, SinkState::Disconnected | SinkState::Error) {
            guard.link = None;
        }
    }

    /// Records a terminal error along with its cause.
    pub fn fail(&self, message: impl Into<String>) {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        guard.state = SinkState::Error;
        guard.message = Some(message.into());
        guard.link = None;
    }

    /// Returns to idle, clearing any previous error (a fresh attempt).
    pub fn reset(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        guard.state = SinkState::Disconnected;
        guard.message = None;
        guard.link = None;
    }
}

/// A sink's stable identifying data (for the GUI list).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SinkInfo {
    /// Unique, stable identifier for the instance.
    pub id: String,
    /// Friendly name announced by the receiver (already sanitised).
    pub display_name: String,
    /// The protocol.
    pub kind: SinkKind,
    /// Address/host where applicable (Chromecast IP, P2P MAC, …).
    pub address: Option<String>,
}

/// A connectable receiver. Implementations must be thread-safe.
#[async_trait]
pub trait Sink: Send + Sync {
    /// Stable identification for the UI.
    fn info(&self) -> SinkInfo;

    /// The state machine's current state.
    fn state(&self) -> SinkState;

    /// The message attached to [`SinkState::Error`], if any.
    fn error_message(&self) -> Option<String> {
        None
    }

    /// What the running session negotiated, once it has.
    ///
    /// `None` for a receiver that is idle, still connecting, or of a protocol
    /// that does not report it.
    fn link(&self) -> Option<StreamLink> {
        None
    }

    /// Starts casting from an already open capture source.
    async fn start_stream(&self, source: CaptureSource) -> Result<()>;

    /// Ends the cast and returns the sink to [`SinkState::Disconnected`].
    async fn stop_stream(&self) -> Result<()>;
}

/// Sanitises a name coming off the network (mDNS/P2P) before displaying it or
/// passing it as an argument to another process.
///
/// Debt carried over from the C code, where the announced name went raw into
/// the PulseAudio module's arguments — trivial injection from any machine on
/// the LAN.
pub fn sanitize_name(raw: &str) -> String {
    const MAX_CHARS: usize = 64;
    let cleaned: String = raw
        .chars()
        // Control characters (newlines included) and shell metacharacters.
        .filter(|c| !c.is_control() && !matches!(c, '"' | '\'' | '\\' | '`' | '$' | ';'))
        .take(MAX_CHARS)
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "Unnamed receiver".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_state_is_not_masked_by_disconnect() {
        // A real bug in the C code: `closed_cb` overwrote the error and the UI
        // only showed "disconnected", with no cause.
        let status = SinkStatus::new();
        status.fail("no route to host");
        status.set(SinkState::Disconnected);
        assert_eq!(status.state(), SinkState::Error);
        assert_eq!(status.message().as_deref(), Some("no route to host"));
    }

    #[test]
    fn reset_clears_the_error() {
        let status = SinkStatus::new();
        status.fail("it failed");
        status.reset();
        assert_eq!(status.state(), SinkState::Disconnected);
        assert!(status.message().is_none());
    }

    #[test]
    fn progress_clears_stale_message() {
        let status = SinkStatus::new();
        status.set(SinkState::Connecting);
        assert!(status.state().is_busy());
        assert!(status.message().is_none());
    }

    #[test]
    fn sanitize_strips_shell_metacharacters() {
        assert_eq!(sanitize_name("Living Room TV"), "Living Room TV");
        assert_eq!(sanitize_name("evil\"; rm -rf /"), "evil rm -rf /");
        assert_eq!(sanitize_name("line\nbreak"), "linebreak");
        assert_eq!(sanitize_name("   "), "Unnamed receiver");
        assert_eq!(sanitize_name(""), "Unnamed receiver");
        assert_eq!(sanitize_name(&"a".repeat(200)).chars().count(), 64);
    }

    #[test]
    fn airplay_is_discovered_but_not_castable() {
        assert!(!SinkKind::AirPlay.is_castable());
        assert!(SinkKind::Chromecast.is_castable());
        assert!(SinkKind::WfdP2p.is_castable());
    }
}
