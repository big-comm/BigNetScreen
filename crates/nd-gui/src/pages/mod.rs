//! The parts of the window, and the little that is shared between them.
//!
//! Each part is a relm4 component of its own. The root component ([`crate::app`])
//! owns the state that outlives any single part (the receivers found, the
//! running session, the preferences) and hands each the piece it shows.
//! Parts never reach back into that state: they emit an output and the root
//! decides what it means.
//!
//! So there is one place where a cast starts and one place that knows whether
//! a stream is running, and no page holds a stale copy of that answer.

mod device_row;
pub(crate) mod hover_cards;
pub mod media;
pub mod options;
mod preview;
pub mod qr;
pub mod settings;
pub mod share;

use relm4::gtk;
use relm4::gtk::prelude::*;

use nd_core::sink::{SinkKind, SinkState};

use crate::tr;

/// One receiver, as the window shows it: a snapshot of what the service
/// publishes, so pages can only ask the root component to act on it.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceEntry {
    pub id: String,
    pub name: String,
    pub protocol: String,
    /// IP or MAC address, empty when the protocol does not give one.
    pub address: String,
    pub kind: SinkKind,
    /// Can this receiver be streamed to, or is it only discovered?
    pub castable: bool,
    pub state: SinkState,
    /// The reason for a failure, when there is one.
    pub detail: String,
    /// "1920 × 1080 · 60 Hz" while a session is running on this receiver.
    ///
    /// What the two ends negotiated, so it is empty for a receiver that is
    /// merely available.
    pub mode: String,
    /// Why the row's Share button cannot act yet, such as no file chosen.
    pub blocked: Option<String>,
}

impl DeviceEntry {
    /// The same row, built from what the service publishes.
    ///
    /// An unrecognised protocol keeps its raw name as the label rather than
    /// being dropped or relabelled: a name this build has no word for still
    /// belongs to a device somebody can see, and calling it "Test" would be a
    /// lie. It can only happen when the window and the service are from
    /// different builds, which is already a broken install.
    pub fn from_wire(receiver: &nd_service::wire::Receiver) -> Self {
        let kind = nd_service::wire::kind_from_name(&receiver.kind);
        Self {
            id: receiver.id.clone(),
            name: receiver.display_name.clone(),
            protocol: kind
                .map(protocol_label)
                .unwrap_or_else(|| receiver.kind.clone()),
            address: receiver.address.clone(),
            kind: kind.unwrap_or(SinkKind::Dummy),
            castable: receiver.castable,
            state: nd_service::wire::state_from_name(&receiver.state)
                .unwrap_or(SinkState::Disconnected),
            detail: receiver.detail.clone(),
            // A row never shows an audience: the count belongs to the session
            // that is running, not to a receiver sitting in a list.
            mode: describe_mode(receiver.width, receiver.height, receiver.fps, -1),
            blocked: None,
        }
    }

    /// "Chromecast · 192.168.1.20", the line under the name.
    pub fn subtitle(&self) -> String {
        if self.address.is_empty() {
            self.protocol.clone()
        } else {
            format!("{} · {}", self.protocol, self.address)
        }
    }

    /// The word in the badge at the end of the row.
    pub fn badge(&self) -> String {
        match self.state {
            SinkState::Streaming => tr!("Connected"),
            SinkState::Error => tr!("Failed"),
            _ if self.state.is_busy() => state_label(self.state),
            _ if self.castable => tr!("Available"),
            _ => tr!("Discovery only"),
        }
    }

    /// The CSS class that colours that badge.
    pub fn badge_class(&self) -> &'static str {
        match self.state {
            SinkState::Streaming => "connected",
            SinkState::Error => "failed",
            state if state.is_busy() => "busy",
            _ => "available",
        }
    }

    pub fn icon(&self) -> &'static str {
        icon_for(self.kind)
    }
}

/// "1280 × 720 · 60 Hz", with the audience when the protocol can count it.
///
/// `receivers` is negative for the protocols that cannot tell, which is every
/// one but NDI: on a point-to-point link "streaming" already means one, while
/// NDI publishes to whoever asks, so zero means sharing with nobody.
pub fn describe_mode(width: u32, height: u32, fps: u32, receivers: i32) -> String {
    if width == 0 {
        return String::new();
    }
    let mode = format!("{width} × {height} · {fps} Hz");
    match receivers {
        n if n < 0 => mode,
        0 => format!("{mode} · {}", tr!("No receivers yet")),
        count => format!(
            "{mode} · {}",
            crate::tr_n!("{} receiver", "{} receivers", count as usize)
                .replace("{}", &count.to_string())
        ),
    }
}

pub fn protocol_label(kind: SinkKind) -> String {
    match kind {
        SinkKind::Chromecast => tr!("Chromecast"),
        // A standard's name, like NDI: it is what the television's own menu
        // calls it, so translating it would make the two disagree.
        SinkKind::Dlna => "DLNA".into(),
        SinkKind::Ndi => "NDI".into(),
        SinkKind::WebRtc => tr!("Web browser"),
        SinkKind::AirPlay => tr!("AirPlay"),
        SinkKind::WfdP2p | SinkKind::WfdMice => tr!("Miracast"),
        SinkKind::Dummy => tr!("Test"),
    }
}

pub fn icon_for(kind: SinkKind) -> &'static str {
    match kind {
        SinkKind::Chromecast | SinkKind::Dlna => "tv-symbolic",
        SinkKind::Ndi => "network-transmit-symbolic",
        SinkKind::WebRtc => "web-browser-symbolic",
        SinkKind::AirPlay => "display-projector-symbolic",
        SinkKind::WfdP2p | SinkKind::WfdMice => "video-display-symbolic",
        SinkKind::Dummy => "applications-system-symbolic",
    }
}

pub fn state_label(state: SinkState) -> String {
    match state {
        SinkState::Disconnected => String::new(),
        SinkState::Connecting => tr!("Connecting…"),
        SinkState::EnsuringFirewall => tr!("Opening the firewall port…"),
        SinkState::WaitSocket => tr!("Waiting for the receiver…"),
        SinkState::WaitStreaming => tr!("Preparing the video…"),
        SinkState::Streaming => tr!("Streaming"),
        SinkState::Error => tr!("Failed"),
    }
}

/// Warns about receiver-controlled buffering without promising unmeasured latency.
pub fn latency_hint(kind: SinkKind) -> Option<String> {
    match kind {
        // A renderer sets its own prebuffer, and DLNA gives the sender no way
        // to ask for a smaller one.
        SinkKind::Dlna => Some(tr!("delayed a few seconds")),
        _ => None,
    }
}

/// What is being streamed right now, for the pages that show it.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    /// `None` when this build has no kind for the service's protocol name.
    pub kind: Option<SinkKind>,
    pub protocol: String,
    pub address: String,
    /// "1920 × 1080 · 60 Hz", empty until the session negotiates it.
    pub mode: String,
    pub state: SinkState,
    /// Can this protocol's link be measured at all?
    ///
    /// Cast receivers hold a control connection open on a known port; a
    /// Miracast receiver connects to this computer and listens on nothing we may
    /// count on. Rather than show a measurement that cannot be taken, the
    /// quality is left out entirely for the protocols where it cannot.
    pub measurable: bool,
    pub probe_failed: bool,
    /// The measured round trip to the receiver, in milliseconds.
    pub round_trip_ms: Option<u64>,
    /// How receivers join, when this session is hosted here (web browsers).
    pub access: Option<nd_core::sink::SinkAccess>,
}

/// A TCP handshake measures network response, not signal or display latency.
pub fn quality_line(info: &SessionInfo) -> String {
    if info.probe_failed {
        return tr!("The receiver did not answer the connection check");
    }
    match info.round_trip_ms {
        Some(ms) => tr!("Network response: {} ms. This is not the picture delay.")
            .replace("{}", &ms.to_string()),
        None => tr!("Measuring…"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addressless_receiver_reads_well() {
        // Miracast before the group is formed has no address to show. The
        // subtitle must not come out as "Miracast · ".
        let entry = DeviceEntry {
            id: "x".into(),
            name: "Projector".into(),
            protocol: "Miracast".into(),
            address: String::new(),
            kind: SinkKind::WfdP2p,
            castable: true,
            state: SinkState::Disconnected,
            detail: String::new(),
            mode: String::new(),
            blocked: None,
        };
        assert_eq!(entry.subtitle(), "Miracast");
    }

    #[test]
    fn discovery_only_receiver_is_labelled() {
        // Claiming "Available" for something that cannot be streamed to sends
        // the person clicking at a row that will never work.
        let entry = DeviceEntry {
            id: "x".into(),
            name: "Apple TV".into(),
            protocol: "AirPlay".into(),
            address: "192.168.1.5".into(),
            kind: SinkKind::AirPlay,
            castable: false,
            state: SinkState::Disconnected,
            detail: String::new(),
            mode: String::new(),
            blocked: None,
        };
        assert_eq!(entry.badge(), tr!("Discovery only"));
    }
}

/// Where discovery stands, so an empty receiver list can say why it is empty.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Discovery {
    #[default]
    Searching,
    Finished,
    Off,
    /// The sharing service is not answering; the window keeps reconnecting.
    Unreachable,
}

/// The placeholder of an empty receiver list.
pub fn discovery_placeholder(discovery: Discovery) -> gtk::Box {
    let (title, body) = match discovery {
        Discovery::Searching => (tr!("Looking for receivers…"), None),
        Discovery::Finished => (
            tr!("No receivers found"),
            Some(tr!(
                "Turn on your TV or projector and open screen mirroring, or share with a web browser."
            )),
        ),
        Discovery::Off => (
            tr!("Automatic discovery is off"),
            Some(tr!("Choose Search to look once.")),
        ),
        Discovery::Unreachable => (tr!("Reconnecting to the sharing service…"), None),
    };
    placeholder(&title, body.as_deref())
}

/// A centered heading with an optional line under it, for an empty list.
pub fn placeholder(title: &str, body: Option<&str>) -> gtk::Box {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(18)
        .margin_end(18)
        .build();
    content.append(
        &gtk::Label::builder()
            .label(title)
            .wrap(true)
            .css_classes(["heading"])
            .build(),
    );
    if let Some(body) = body {
        content.append(
            &gtk::Label::builder()
                .label(body)
                .wrap(true)
                .max_width_chars(70)
                .justify(gtk::Justification::Center)
                .css_classes(["dim-label"])
                .build(),
        );
    }
    content
}

#[cfg(test)]
mod layout_tests;
