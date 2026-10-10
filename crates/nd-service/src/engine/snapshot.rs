//! The published state: one value a client can draw from.

use nd_chromecast::media::MediaStatus;
use nd_core::settings::Settings;
use nd_core::sink::SinkState;

use super::{Engine, source_name};
use crate::wire::{self, Issue, Media, Receiver, Session, Status};

/// Everything a client needs to draw, in one value.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub receivers: Vec<Receiver>,
    pub session: Session,
    pub link_probe: String,
    pub status: Status,
    pub issues: Vec<Issue>,
    pub media: Media,
    pub media_owner: Option<MediaOwner>,
    pub media_volume: Option<f64>,
    pub media_muted: Option<bool>,
    pub virtual_available: bool,
    pub settings: Settings,
    pub last_client_activity: Option<tokio::time::Instant>,
}

impl Snapshot {
    /// The state before anything has happened. Takes the preferences rather
    /// than inventing them: `Settings` is read from disk, and a default here
    /// would publish "discovery is on" to a client whose file says otherwise.
    pub fn initial(settings: Settings) -> Self {
        Self {
            receivers: Vec::new(),
            session: Session::idle(),
            link_probe: "pending".into(),
            status: Status::of(if settings.auto_discovery {
                "searching"
            } else {
                "discovery-off"
            }),
            issues: Vec::new(),
            media: Media::idle(),
            media_owner: None,
            media_volume: None,
            media_muted: None,
            virtual_available: false,
            settings,
            last_client_activity: None,
        }
    }
}

/// The D-Bus connection and session that may issue player controls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaOwner {
    pub id: String,
    pub sender: String,
    pub receiver: String,
}

impl Engine {
    pub(super) fn publish(&mut self) {
        let receivers = self
            .order
            .iter()
            .filter_map(|id| {
                let sink = if self.active_cast.as_ref() == Some(id) {
                    self.active_sink.as_ref()
                } else {
                    self.registry.get(id)
                }?;
                let mut receiver = Receiver::of(sink.as_ref());
                if let Some((refused, why)) = &self.refused
                    && refused == id
                {
                    receiver.state = wire::state_name(SinkState::Error).to_string();
                    receiver.detail = why.clone();
                }
                Some(receiver)
            })
            .collect();
        let session = match &self.active_sink {
            Some(sink) => wire::session_of(
                sink.as_ref(),
                source_name(self.cast_source),
                self.measured
                    .flatten()
                    .map(|rtt| rtt.as_millis() as u64)
                    .unwrap_or(0),
            ),
            None => Session::idle(),
        };
        let media_status = self.media_session.as_ref().map(|s| s.status());
        let media_volume = media_status
            .as_ref()
            .and_then(|status| status.playback.volume);
        let media_muted = media_status
            .as_ref()
            .and_then(|status| status.playback.muted);
        let media = media_status.map_or_else(Media::idle, published_media);
        let next = Snapshot {
            receivers,
            session,
            link_probe: match self.measured {
                None => "pending",
                Some(None) => "unreachable",
                Some(Some(_)) => "available",
            }
            .into(),
            status: self.status.clone(),
            issues: self.issues.clone(),
            media,
            media_owner: self.media_owner.clone(),
            media_volume,
            media_muted,
            virtual_available: self.virtual_available,
            settings: self.settings.clone(),
            last_client_activity: self.last_client_activity,
        };
        // Only when something actually moved. This runs every poll tick, and
        // `send_replace` would wake every watcher two and a half times a second
        // to be told nothing changed — including the task that decides the
        // service has been idle long enough to leave.
        self.snapshots.send_if_modified(|current| {
            if *current == next {
                return false;
            }
            *current = next;
            true
        });
    }
}

/// The file session as [`Media`] carries it over D-Bus.
fn published_media(status: MediaStatus) -> Media {
    Media {
        active: true,
        title: status.title,
        queue: status
            .queue
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        index: status.position as u32,
        total: status.total as u32,
        position_ms: (status.playback.seconds * 1000.0) as u64,
        duration_ms: status
            .playback
            .duration
            .map(|d| (d * 1000.0) as u64)
            .unwrap_or(0),
        paused: status.playback.paused,
        can_pause: status.playback.can_pause,
        can_seek: status.playback.can_seek,
        finished: status.finished,
        detail: status.error.unwrap_or_default(),
        control_detail: status.control_error.unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::watch;

    use crate::engine::tests::sink;

    #[test]
    fn a_receiver_refused_while_sending_files_says_so_on_its_row() {
        let (tx, snapshots) = watch::channel(Snapshot::initial(Settings::default()));
        let mut engine = Engine::new(tx);
        engine.registry.insert("a".into(), sink("a"));
        engine.registry.insert("b".into(), sink("b"));
        engine.order = vec!["a".into(), "b".into()];
        let why = format!(
            "protocol: changed: {}",
            nd_chromecast::identity::IDENTITY_CHANGED
        );
        engine.refused = Some(("a".into(), why.clone()));
        engine.publish();
        let receivers = snapshots.borrow().receivers.clone();
        assert_eq!(
            (receivers[0].state.as_str(), receivers[0].detail.as_str()),
            ("error", why.as_str())
        );
        assert_eq!(
            receivers[1].state, "disconnected",
            "only the refused receiver"
        );
    }
}
