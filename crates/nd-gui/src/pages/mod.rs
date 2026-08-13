//! The application's pages, and the little that is shared between them.
//!
//! Each page is a relm4 component of its own. The root component ([`crate::app`])
//! owns the state that outlives any single page — the receivers found, the
//! running session, the preferences — and hands each page the part it shows.
//! Pages never reach back into that state: they emit an output and the root
//! decides what it means.
//!
//! That split is what keeps the window honest. There is one place where a cast
//! starts, one place that knows whether a stream is running, and a page cannot
//! contradict another by holding a stale copy of the answer.

pub mod devices;
pub mod home;
pub mod media;
pub mod settings;

use nd_core::sink::{SinkInfo, SinkKind, SinkState};

use crate::tr;

/// Which page the window is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Page {
    #[default]
    Home,
    Devices,
    Media,
    Settings,
}

impl Page {
    /// The name of the page in the widget stack.
    pub fn id(self) -> &'static str {
        match self {
            Page::Home => "home",
            Page::Devices => "devices",
            Page::Media => "media",
            Page::Settings => "settings",
        }
    }

    /// The label in the sidebar.
    pub fn title(self) -> String {
        match self {
            Page::Home => tr!("Home"),
            Page::Devices => tr!("Devices"),
            Page::Media => tr!("Media"),
            Page::Settings => tr!("Settings"),
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Page::Home => "go-home-symbolic",
            Page::Devices => "video-display-symbolic",
            Page::Media => "folder-videos-symbolic",
            Page::Settings => "emblem-system-symbolic",
        }
    }

    /// The pages in the order the sidebar lists them.
    pub fn all() -> [Page; 4] {
        [Page::Home, Page::Devices, Page::Media, Page::Settings]
    }
}

/// One receiver, as the pages need to show it.
///
/// A plain snapshot rather than the `Arc<dyn Sink>` itself: a page that held
/// the sink could start or stop a stream behind the root component's back, and
/// then two parts of the window would disagree about what is running.
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
    /// merely available — nothing has been agreed with it yet.
    pub mode: String,
}

impl DeviceEntry {
    pub fn from_info(
        info: &SinkInfo,
        state: SinkState,
        detail: Option<String>,
        link: Option<nd_core::sink::StreamLink>,
    ) -> Self {
        Self {
            id: info.id.clone(),
            name: info.display_name.clone(),
            protocol: protocol_label(info.kind),
            address: info.address.clone().unwrap_or_default(),
            kind: info.kind,
            castable: info.kind.is_castable(),
            state,
            detail: detail.unwrap_or_default(),
            mode: link.map(|l| l.describe()).unwrap_or_default(),
        }
    }

    /// "Chromecast · 192.168.68.113", the line under the name.
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
            SinkState::Error => "warning",
            _ => "available",
        }
    }

    pub fn icon(&self) -> &'static str {
        icon_for(self.kind)
    }
}

pub fn protocol_label(kind: SinkKind) -> String {
    match kind {
        SinkKind::Chromecast => tr!("Chromecast"),
        SinkKind::AirPlay => tr!("AirPlay"),
        SinkKind::WfdP2p | SinkKind::WfdMice => tr!("Miracast"),
        SinkKind::Dummy => tr!("Test"),
    }
}

pub fn icon_for(kind: SinkKind) -> &'static str {
    match kind {
        SinkKind::Chromecast => "tv-symbolic",
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

/// The delay to expect, per protocol.
///
/// Not decoration: the two paths differ by an order of magnitude, and the
/// person choosing needs to know that **before** they choose, not after the
/// pointer starts lagging.
pub fn latency_hint(kind: SinkKind) -> Option<String> {
    match kind {
        SinkKind::WfdP2p | SinkKind::WfdMice => Some(tr!("instant response")),
        SinkKind::Chromecast => Some(tr!("quick response")),
        _ => None,
    }
}

/// What is being streamed right now, for the pages that show it.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    pub protocol: String,
    pub address: String,
    /// "1920 × 1080 · 60 Hz", empty until the session negotiates it.
    pub mode: String,
    pub state: SinkState,
    /// Can this protocol's link be measured at all?
    ///
    /// Cast receivers hold a control connection open on a known port; a
    /// Miracast receiver connects **to us** and listens on nothing we may
    /// count on. Rather than show a measurement that cannot be taken, the
    /// quality is left out entirely for the protocols where it cannot.
    pub measurable: bool,
    /// How the link measured, once it has been measured.
    pub quality: Option<nd_net::probe::Quality>,
    /// The measured round trip to the receiver, in milliseconds.
    pub round_trip_ms: Option<u64>,
}

/// The quality in words, with the number that produced it.
///
/// The number is always shown next to the word: "Excellent" on its own is an
/// opinion, "Excellent · 8 ms round trip" is a measurement someone can check.
pub fn quality_line(info: &SessionInfo) -> String {
    use nd_net::probe::Quality;
    let Some(quality) = info.quality else {
        return tr!("Measuring…");
    };
    let word = match quality {
        Quality::Excellent => tr!("Excellent"),
        Quality::Good => tr!("Good"),
        Quality::Weak => tr!("Weak signal"),
        Quality::Unreachable => return tr!("The receiver is not answering"),
    };
    match info.round_trip_ms {
        Some(ms) => format!("{word} · {ms} {}", tr!("ms round trip")),
        None => word,
    }
}

/// Signal strength drawn as text, so it needs nothing from the icon theme.
pub fn quality_bars(quality: Option<nd_net::probe::Quality>) -> String {
    let filled = quality.map(|q| q.bars()).unwrap_or(0);
    let mut bars = String::new();
    for step in 0..4 {
        bars.push(if step < filled { '▮' } else { '▯' });
    }
    bars
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_two_pages_answer_to_the_same_name() {
        // The stack addresses pages by string and the sidebar by position in
        // `all()`. A duplicate id would show one page while the sidebar
        // highlighted another, with nothing failing to build.
        let ids: std::collections::HashSet<&str> = Page::all().iter().map(|p| p.id()).collect();
        assert_eq!(ids.len(), Page::all().len());
    }

    #[test]
    fn the_sidebar_lists_every_page() {
        // The sidebar maps a row index straight onto `all()`, so a page missing
        // from it would be unreachable.
        for page in [Page::Home, Page::Devices, Page::Media, Page::Settings] {
            assert!(Page::all().contains(&page), "{page:?} has no way in");
        }
    }

    #[test]
    fn a_receiver_without_an_address_still_reads_well() {
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
        };
        assert_eq!(entry.subtitle(), "Miracast");
    }

    #[test]
    fn a_discovery_only_receiver_says_so_instead_of_available() {
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
        };
        assert_eq!(entry.badge(), tr!("Discovery only"));
    }
}
