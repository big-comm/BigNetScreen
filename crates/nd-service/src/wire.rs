//! What crosses D-Bus.
//!
//! These mirror the types in `nd_core::sink` rather than deriving from them.
//! Two reasons, and both would bite later:
//!
//! - D-Bus has no enums. `SinkKind` and `SinkState` have to become something,
//!   and a string an applet can read in QML or JavaScript is worth more than a
//!   number it has to keep a table for.
//! - Deriving on the originals would push `zvariant` into `nd-core`, which
//!   every crate depends on and none of the others speak D-Bus.
//!
//! ## No translated text crosses this boundary
//!
//! The service has no locale of its own worth trusting: it may be started by
//! D-Bus activation with a bare environment, and its clients — the window, the
//! command line, a panel applet — can each be running under a different one.
//! So the service reports *what is happening* and every client writes the
//! sentence itself. That is why [`Status`] is a shape and not a string, and why
//! the one free-text field left, [`Issue::reason`], carries text that was
//! already produced outside any interface.

use serde::{Deserialize, Serialize};
use zbus::zvariant::{OwnedValue, Type, Value};

use nd_core::sink::{Sink, SinkAccess, SinkKind, SinkState, StreamLink};

/// A receiver, as a client sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type, Value, OwnedValue)]
pub struct Receiver {
    pub id: String,
    pub display_name: String,
    /// `chromecast`, `airplay`, `wfd-p2p`, `wfd-mice`, `ndi`, `webrtc`, `dummy`.
    pub kind: String,
    /// IP or MAC, empty when the protocol announces neither.
    pub address: String,
    /// Can this be streamed to, or does it only show up in discovery?
    pub castable: bool,
    /// `disconnected`, `connecting`, `ensuring-firewall`, `wait-socket`,
    /// `wait-streaming`, `streaming`, `error`.
    pub state: String,
    /// Why it failed, empty when it has not.
    pub detail: String,
    /// What the two ends agreed on. Zeroed until they have.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Receiver {
    pub fn of(sink: &dyn Sink) -> Self {
        let info = sink.info();
        let link = sink.link();
        Self {
            id: info.id,
            display_name: info.display_name,
            kind: kind_name(info.kind).to_string(),
            address: info.address.unwrap_or_default(),
            castable: info.kind.is_castable(),
            state: state_name(sink.state()).to_string(),
            detail: sink.error_message().unwrap_or_default(),
            width: link.map(|l| l.width).unwrap_or(0),
            height: link.map(|l| l.height).unwrap_or(0),
            fps: link.map(|l| l.fps).unwrap_or(0),
        }
    }
}

/// The running session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type, Value, OwnedValue)]
pub struct Session {
    /// The receiver's id, empty when nothing is running.
    pub id: String,
    pub display_name: String,
    pub kind: String,
    pub address: String,
    pub state: String,
    /// What was captured: `monitor`, `window` or `virtual`.
    pub source: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// How many receivers are pulling the stream; negative when the protocol
    /// cannot tell. Only NDI publishes to whoever asks, so only NDI knows —
    /// on the point-to-point protocols "streaming" already means one.
    pub receivers: i32,
    /// Can this protocol's link be measured at all?
    pub measurable: bool,
    /// The round trip in milliseconds; `0` for not measured or no answer.
    pub round_trip_ms: u64,
    /// How a receiver joins a session this computer hosts (the web browser
    /// page). Empty for the protocols that push to a device instead.
    pub url: String,
    pub pin: String,
}

impl Session {
    pub fn idle() -> Self {
        Self {
            id: String::new(),
            display_name: String::new(),
            kind: String::new(),
            address: String::new(),
            state: state_name(SinkState::Disconnected).to_string(),
            source: String::new(),
            width: 0,
            height: 0,
            fps: 0,
            receivers: -1,
            measurable: false,
            round_trip_ms: 0,
            url: String::new(),
            pin: String::new(),
        }
    }

    pub fn is_idle(&self) -> bool {
        self.id.is_empty()
    }
}

/// A protocol that could not start, and why.
///
/// `reason` is the one string the service passes through: it is produced by the
/// provider that failed, not by an interface, and no client can reconstruct it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type, Value, OwnedValue)]
pub struct Issue {
    pub provider: String,
    pub reason: String,
}

/// What the service is doing, for a client to put into words.
///
/// The `kind` is the discriminant; the other fields carry whatever that kind
/// needs. A struct rather than a D-Bus variant because every client language
/// can read a struct, and only some can take a variant apart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type, Value, OwnedValue)]
pub struct Status {
    /// `searching`, `found`, `empty`, `discovery-off`, `connecting`,
    /// `streaming`, `sending`, `stopping`, `error`.
    pub kind: String,
    /// How many receivers are known, for `found`.
    pub count: u32,
    /// The receiver a `connecting`, `streaming` or `sending` refers to.
    pub display_name: String,
    /// The cause of an `error`, from whichever layer produced it.
    pub detail: String,
}

impl Status {
    pub fn of(kind: &str) -> Self {
        Self {
            kind: kind.to_string(),
            count: 0,
            display_name: String::new(),
            detail: String::new(),
        }
    }

    pub fn found(count: u32) -> Self {
        Self {
            count,
            ..Self::of("found")
        }
    }

    pub fn about(kind: &str, display_name: String) -> Self {
        Self {
            display_name,
            ..Self::of(kind)
        }
    }

    pub fn error(detail: String) -> Self {
        Self {
            detail,
            ..Self::of("error")
        }
    }
}

/// Files being sent to a receiver.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type, Value, OwnedValue)]
pub struct Media {
    /// Is anything being sent at all?
    pub active: bool,
    pub title: String,
    /// The files queued, in order, as absolute paths.
    pub queue: Vec<String>,
    /// Which item of the queue, one-based, and how many there are.
    pub index: u32,
    pub total: u32,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub paused: bool,
    /// Whether this receiver answers pause and seek at all. A Chromecast
    /// playing the file itself does; a Miracast screen being fed a picture
    /// cannot, and a client must not offer buttons that do nothing.
    pub can_pause: bool,
    pub can_seek: bool,
    pub finished: bool,
    /// What ended the sending, empty if nothing did.
    pub detail: String,
    /// A single control that failed, which does not end the sending.
    pub control_detail: String,
}

impl Media {
    pub fn idle() -> Self {
        Self {
            active: false,
            title: String::new(),
            queue: Vec::new(),
            index: 0,
            total: 0,
            position_ms: 0,
            duration_ms: 0,
            paused: false,
            can_pause: false,
            can_seek: false,
            finished: false,
            detail: String::new(),
            control_detail: String::new(),
        }
    }
}

pub fn kind_name(kind: SinkKind) -> &'static str {
    match kind {
        SinkKind::Ndi => "ndi",
        SinkKind::WebRtc => "webrtc",
        SinkKind::Chromecast => "chromecast",
        SinkKind::Dlna => "dlna",
        SinkKind::AirPlay => "airplay",
        SinkKind::WfdP2p => "wfd-p2p",
        SinkKind::WfdMice => "wfd-mice",
        SinkKind::Dummy => "dummy",
    }
}

pub fn state_name(state: SinkState) -> &'static str {
    match state {
        SinkState::Disconnected => "disconnected",
        SinkState::Connecting => "connecting",
        SinkState::EnsuringFirewall => "ensuring-firewall",
        SinkState::WaitSocket => "wait-socket",
        SinkState::WaitStreaming => "wait-streaming",
        SinkState::Streaming => "streaming",
        SinkState::Error => "error",
    }
}

/// The way back. A client draws icons and badges from the original enums, so
/// the names have to survive the trip in both directions.
pub fn kind_from_name(name: &str) -> Option<SinkKind> {
    Some(match name {
        "ndi" => SinkKind::Ndi,
        "webrtc" => SinkKind::WebRtc,
        "chromecast" => SinkKind::Chromecast,
        "dlna" => SinkKind::Dlna,
        "airplay" => SinkKind::AirPlay,
        "wfd-p2p" => SinkKind::WfdP2p,
        "wfd-mice" => SinkKind::WfdMice,
        "dummy" => SinkKind::Dummy,
        _ => return None,
    })
}

pub fn state_from_name(name: &str) -> Option<SinkState> {
    Some(match name {
        "disconnected" => SinkState::Disconnected,
        "connecting" => SinkState::Connecting,
        "ensuring-firewall" => SinkState::EnsuringFirewall,
        "wait-socket" => SinkState::WaitSocket,
        "wait-streaming" => SinkState::WaitStreaming,
        "streaming" => SinkState::Streaming,
        "error" => SinkState::Error,
        _ => return None,
    })
}

/// Fills in the half of [`Session`] that comes from the sink itself.
pub fn session_of(sink: &dyn Sink, source: &str, round_trip_ms: u64) -> Session {
    let info = sink.info();
    let link: Option<StreamLink> = sink.link();
    let access: Option<SinkAccess> = sink.access();
    Session {
        id: info.id,
        display_name: info.display_name,
        kind: kind_name(info.kind).to_string(),
        address: info.address.unwrap_or_default(),
        state: state_name(sink.state()).to_string(),
        source: source.to_string(),
        width: link.map(|l| l.width).unwrap_or(0),
        height: link.map(|l| l.height).unwrap_or(0),
        fps: link.map(|l| l.fps).unwrap_or(0),
        receivers: link
            .and_then(|l| l.receivers)
            .map(|count| count as i32)
            .unwrap_or(-1),
        measurable: link.and_then(|l| l.endpoint).is_some(),
        round_trip_ms,
        url: access.as_ref().map(|a| a.url.clone()).unwrap_or_default(),
        pin: access.map(|a| a.pin).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_and_state_has_a_name_a_client_can_switch_on() {
        // A name added to the enum and forgotten here would reach an applet as
        // a string it has no branch for, so the mapping is exhaustive by
        // construction (no `_` arm) and this checks the names stay distinct.
        let kinds = [
            SinkKind::Ndi,
            SinkKind::WebRtc,
            SinkKind::Chromecast,
            SinkKind::Dlna,
            SinkKind::AirPlay,
            SinkKind::WfdP2p,
            SinkKind::WfdMice,
            SinkKind::Dummy,
        ]
        .map(kind_name);
        let mut unique = kinds.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), kinds.len());

        let states = [
            SinkState::Disconnected,
            SinkState::Connecting,
            SinkState::EnsuringFirewall,
            SinkState::WaitSocket,
            SinkState::WaitStreaming,
            SinkState::Streaming,
            SinkState::Error,
        ]
        .map(state_name);
        let mut unique = states.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), states.len());
    }

    #[test]
    fn names_survive_the_trip_in_both_directions() {
        for kind in [
            SinkKind::Ndi,
            SinkKind::WebRtc,
            SinkKind::Chromecast,
            SinkKind::Dlna,
            SinkKind::AirPlay,
            SinkKind::WfdP2p,
            SinkKind::WfdMice,
            SinkKind::Dummy,
        ] {
            assert_eq!(kind_from_name(kind_name(kind)), Some(kind));
        }
        for state in [
            SinkState::Disconnected,
            SinkState::Connecting,
            SinkState::EnsuringFirewall,
            SinkState::WaitSocket,
            SinkState::WaitStreaming,
            SinkState::Streaming,
            SinkState::Error,
        ] {
            assert_eq!(state_from_name(state_name(state)), Some(state));
        }
        assert_eq!(kind_from_name("nonsense"), None);
        assert_eq!(state_from_name("nonsense"), None);
    }

    #[test]
    fn an_idle_session_is_told_apart_by_its_empty_id() {
        assert!(Session::idle().is_idle());
    }
}
