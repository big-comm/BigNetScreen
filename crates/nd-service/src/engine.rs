//! The session, with nobody watching.
//!
//! Everything the window used to own that outlives a window: the receivers
//! discovery has found, the running stream, the files being sent, and the
//! preferences the next session will obey. Lifted out of `nd_gui::app` so that
//! closing the window stops drawing and nothing else.
//!
//! It runs as one task that owns its state outright. Callers send a
//! [`Command`]; the state is published as a [`Snapshot`] on a `watch` channel.
//! No locks are handed out: a client that could hold the state while it renders
//! could also hold it while a receiver disconnects.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot, watch};

use nd_chromecast::MdnsProvider;
use nd_chromecast::file_server::MediaFile;
use nd_chromecast::media::{MediaItem, MediaSession};
use nd_core::capture::SourceType;
use nd_core::media::MediaCommand;
use nd_core::meta::MetaProvider;
use nd_core::provider::{DiscoveryEvent, Provider};
use nd_core::settings::{self, Protocol, Settings};
use nd_core::sink::{Sink, SinkKind, SinkState};
use nd_wfd::WfdP2pProvider;

use crate::wire::{self, Issue, Media, Receiver, Session, Status};

/// After this long with no receiver, the service stops saying "searching".
const EMPTY_HINT_AFTER: Duration = Duration::from_secs(12);
/// How often receiver state is re-read and republished.
const STATE_POLL: Duration = Duration::from_millis(400);
/// How long between measurements of the link.
///
/// Long enough to be unnoticeable to the receiver, short enough that the
/// reading reflects a Wi-Fi that has just got worse.
const LINK_PROBE_INTERVAL: Duration = Duration::from_secs(3);

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

/// What a client asks the service to do.
#[derive(Debug)]
pub enum Command {
    Cast {
        id: String,
        source: SourceType,
        /// Text for the page a web browser sees, already in the client's
        /// language. Empty leaves the source-language wording: see
        /// [`crate::web_page_text`].
        page_text: Vec<(String, String)>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Stop,
    Rescan,
    KeepAlive,
    SetAutoDiscovery(bool, oneshot::Sender<Result<(), String>>),
    ApplySettings(Box<Settings>, oneshot::Sender<Result<(), String>>),
    SendMedia {
        target: String,
        files: Vec<MediaFile>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    ControlMedia(MediaCommand),
    StartPlayer {
        sender: String,
        target: String,
        files: Vec<MediaItem>,
        start: nd_core::media::PlaybackStart,
        reply: oneshot::Sender<Result<String, String>>,
    },
    ControlPlayer {
        sender: String,
        id: String,
        /// None stops only this player's media session.
        command: Option<MediaCommand>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    PlayerGone(String),
    /// End everything and let the task finish.
    Shutdown,
}

/// What the engine tells itself.
enum Event {
    Discovery(DiscoveryEvent, u64),
    SearchTimedOut(u64),
    VirtualSupported(bool),
    LinkMeasured(u64, Option<Duration>),
    CastFinished {
        id: String,
        error: Option<String>,
        ndi_runtime_unavailable: bool,
    },
}

/// A handle on the running engine.
#[derive(Clone)]
pub struct Handle {
    commands: mpsc::Sender<Command>,
    snapshots: watch::Receiver<Snapshot>,
}

impl Handle {
    pub fn snapshot(&self) -> watch::Ref<'_, Snapshot> {
        self.snapshots.borrow()
    }

    /// A receiver that fires whenever the snapshot changes.
    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.snapshots.clone()
    }

    pub async fn send(&self, command: Command) -> Result<(), String> {
        self.commands
            .send(command)
            .await
            .map_err(|_| "the session service is not running".to_string())
    }

    pub async fn cast(
        &self,
        id: String,
        source: SourceType,
        page_text: Vec<(String, String)>,
    ) -> Result<(), String> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Cast {
            id,
            source,
            page_text,
            reply,
        })
        .await?;
        answer
            .await
            .map_err(|_| "the session service stopped answering".to_string())?
    }

    pub async fn send_media(&self, target: String, files: Vec<MediaFile>) -> Result<(), String> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::SendMedia {
            target,
            files,
            reply,
        })
        .await?;
        answer
            .await
            .map_err(|_| "the session service stopped answering".to_string())?
    }
}

/// Starts the engine and returns a handle on it.
pub fn start() -> Handle {
    let (commands, command_rx) = mpsc::channel(32);
    let (snapshots_tx, snapshots) = watch::channel(Snapshot::initial(settings::current()));
    let handle = Handle {
        commands,
        snapshots,
    };
    tokio::spawn(Engine::new(snapshots_tx).run(command_rx));
    handle
}

struct Engine {
    registry: HashMap<String, Arc<dyn Sink>>,
    order: Vec<String>,
    issues: Vec<Issue>,
    searching: bool,
    active_cast: Option<String>,
    active_sink: Option<Arc<dyn Sink>>,
    cast_cancel: Option<watch::Sender<bool>>,
    cast_source: SourceType,
    discovery: Option<futures::future::AbortHandle>,
    virtual_available: bool,
    generation: u64,
    settings: Settings,
    audio_pending: bool,
    last_client_activity: Option<tokio::time::Instant>,
    media_session: Option<MediaSession>,
    media_owner: Option<MediaOwner>,
    /// The last link measurement. The outer `None` means *not measured yet*,
    /// the inner one means *the receiver did not answer* — one `Option` would
    /// have to call one of those the other.
    measured: Option<Option<Duration>>,
    probing: bool,
    operation_generation: u64,
    status: Status,
    events: mpsc::Sender<Event>,
    snapshots: watch::Sender<Snapshot>,
}

impl Engine {
    fn new(snapshots: watch::Sender<Snapshot>) -> Self {
        // Placeholder: `run` replaces it with the live sender before anything
        // can emit. Kept non-optional so no call site has to unwrap it.
        let (events, _) = mpsc::channel(1);
        let settings = settings::current();
        let audio_pending = settings.virtual_audio && settings.system_audio;
        Self {
            registry: HashMap::new(),
            order: Vec::new(),
            issues: Vec::new(),
            searching: settings.auto_discovery,
            active_cast: None,
            active_sink: None,
            cast_cancel: None,
            cast_source: SourceType::Monitor,
            discovery: None,
            virtual_available: false,
            generation: 0,
            media_session: None,
            media_owner: None,
            measured: None,
            probing: false,
            operation_generation: 0,
            status: if settings.auto_discovery {
                Status::of("searching")
            } else {
                Status::of("discovery-off")
            },
            settings,
            audio_pending,
            last_client_activity: None,
            events,
            snapshots,
        }
    }

    async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        let (events, mut event_rx) = mpsc::channel(256);
        self.events = events;

        // Ports a crashed run left open in firewalld are closed before
        // anything else. Under Flatpak the system bus is out of reach anyway.
        if !nd_capture::is_sandboxed() {
            tokio::spawn(nd_net::firewall::release_stale());
        }
        self.probe_virtual_support();
        if self.settings.auto_discovery {
            self.start_discovery();
        }
        self.publish();

        let mut poll = tokio::time::interval(STATE_POLL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(Command::Shutdown) | None => break,
                    Some(command) => self.command(command).await,
                },
                Some(event) = event_rx.recv() => self.event(event),
                _ = poll.tick() => {
                    self.reap_finished_media();
                    self.measure_link();
                    // The session's own progress moves no event of its own, so
                    // without this the status only catches up when discovery
                    // happens to chatter. An mDNS provider re-announces often
                    // enough to hide that; an SSDP one, which only speaks when
                    // something changed, left the status reading "connecting"
                    // for a session that had been on the wall for a minute.
                    self.refresh_status();
                }
            }
            if let Err(err) = self.reconcile_audio().await {
                self.status = Status::error(err);
            }
            self.publish();
        }
        self.finish_shutdown(&mut event_rx).await;
    }

    async fn finish_shutdown(&mut self, events: &mut mpsc::Receiver<Event>) {
        self.stop_everything();
        self.abort_discovery();
        let mut poll = tokio::time::interval(STATE_POLL);
        let cleanup = async {
            while self.media_session.is_some() || self.active_cast.is_some() {
                tokio::select! {
                    Some(event) = events.recv() => self.event(event),
                    _ = poll.tick() => self.reap_finished_media(),
                }
                self.publish();
            }
        };
        // A Cast LAUNCH can take 45 seconds; allow it to finish so its app can
        // be explicitly stopped. Bound shutdown if a protocol never returns.
        if tokio::time::timeout(Duration::from_secs(60), cleanup)
            .await
            .is_err()
        {
            tracing::warn!("session cleanup did not finish before service shutdown");
        }
        // The card goes with the service. Left behind, a sink that discards
        // everything stayed on the machine's output list after the last
        // transmission, and whoever had picked it to transmit — or a session
        // manager that fell back to it — kept a computer with no sound. The
        // next start puts it back (`reconcile_audio`, and every cast that
        // needs it), and the session manager remembers what was routed to it
        // by name, so the routing survives the gap.
        if let Err(err) = nd_core::virtual_sink::remove().await {
            tracing::warn!(%err, "could not remove the virtual sound card on the way out");
        }
    }

    async fn command(&mut self, command: Command) {
        match command {
            Command::Cast {
                id,
                source,
                page_text,
                reply,
            } => {
                let _ = reply.send(self.begin_cast(id, source, page_text));
            }
            Command::Stop => self.stop_everything(),
            Command::Rescan => self.rescan(),
            Command::KeepAlive => self.last_client_activity = Some(tokio::time::Instant::now()),
            Command::SetAutoDiscovery(on, reply) => {
                self.settings.auto_discovery = on;
                settings::set_in_memory(&self.settings);
                let result = tokio::task::spawn_blocking(settings::persist)
                    .await
                    .map_err(|err| err.to_string())
                    .and_then(|result| result.map_err(|err| err.to_string()));
                self.apply_discovery_preference();
                let _ = reply.send(result);
            }
            Command::ApplySettings(new, reply) => {
                let protocol_changed = new.protocol != self.settings.protocol;
                let discovery_changed = new.auto_discovery != self.settings.auto_discovery;
                let card_changed = (new.virtual_audio && new.system_audio)
                    != (self.settings.virtual_audio && self.settings.system_audio);
                self.settings = *new;
                settings::set_in_memory(&self.settings);
                let retry_audio = self.status.kind == "error"
                    && self
                        .status
                        .detail
                        .starts_with("could not change the virtual sound card:");
                self.audio_pending |= card_changed || retry_audio;
                // Which protocols to look for is the one preference that cannot
                // wait for the next session: the list a client is showing is
                // the result of the old choice.
                if discovery_changed {
                    self.apply_discovery_preference();
                } else if protocol_changed && self.settings.auto_discovery {
                    self.rescan();
                } else if protocol_changed {
                    self.abort_discovery();
                }
                let result = self.reconcile_audio().await;
                if let Err(err) = &result {
                    self.status = Status::error(err.clone());
                } else if retry_audio && !self.audio_pending {
                    self.status = Status::of("idle");
                    self.refresh_status();
                }
                let _ = reply.send(result);
            }
            Command::SendMedia {
                target,
                files,
                reply,
            } => {
                let _ = reply.send(self.send_media(
                    target,
                    files.into_iter().map(MediaItem::File).collect(),
                    None,
                ));
            }
            Command::ControlMedia(command) => {
                if let Some(session) = &self.media_session
                    && let Err(err) = session.command(command)
                {
                    self.status = Status::error(err.to_string());
                }
            }
            Command::StartPlayer {
                sender,
                target,
                files,
                start,
                reply,
            } => {
                let result = nd_core::stream_server::random_token()
                    .map_err(|err| err.to_string())
                    .and_then(|id| {
                        self.send_media(target.clone(), files, Some(start))?;
                        self.media_owner = Some(MediaOwner {
                            id: id.clone(),
                            sender,
                            receiver: target,
                        });
                        Ok(id)
                    });
                let _ = reply.send(result);
            }
            Command::ControlPlayer {
                sender,
                id,
                command,
                reply,
            } => {
                let result = self.control_player(&sender, &id, command);
                let _ = reply.send(result);
            }
            Command::PlayerGone(sender) => {
                if self
                    .media_owner
                    .as_ref()
                    .is_some_and(|owner| owner.sender == sender)
                {
                    self.stop_everything();
                }
            }
            Command::Shutdown => {}
        }
    }

    async fn reconcile_audio(&mut self) -> Result<(), String> {
        if !self.audio_pending || self.active_cast.is_some() || self.media_session.is_some() {
            return Ok(());
        }
        self.audio_pending = false;
        let result = if self.settings.virtual_audio && self.settings.system_audio {
            nd_core::virtual_sink::ensure().await
        } else {
            nd_core::virtual_sink::remove().await
        };
        result.map_err(|err| format!("could not change the virtual sound card: {err}"))
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Discovery(event, generation) => {
                if generation != self.generation {
                    return;
                }
                self.discovery_event(event);
            }
            Event::SearchTimedOut(generation) => {
                if generation == self.generation && self.registry.is_empty() {
                    self.searching = false;
                    self.refresh_status();
                }
            }
            Event::VirtualSupported(available) => {
                tracing::info!(available, "virtual monitor support");
                self.virtual_available = available;
            }
            Event::LinkMeasured(generation, round_trip) => {
                if generation != self.operation_generation {
                    // A newer session owns `probing` now; leave it alone.
                    return;
                }
                self.probing = false;
                // The session may have ended while the probe was in flight: a
                // reading for a link that no longer exists is not published.
                if self.active_sink.is_some() {
                    self.measured = Some(round_trip);
                }
            }
            Event::CastFinished {
                id,
                error,
                ndi_runtime_unavailable,
            } => {
                // The only thing that clears the session, so its absence is
                // the difference between a receiver that stopped and a window
                // that knows it did.
                tracing::info!(%id, failed = error.is_some(), "the cast task returned");
                if self.active_cast.as_deref() != Some(id.as_str()) {
                    tracing::warn!(%id, active = ?self.active_cast, "for a session that is not the current one");
                    return;
                }
                self.active_cast = None;
                self.active_sink = None;
                self.cast_cancel = None;
                self.measured = None;
                self.probing = false;
                match error {
                    Some(err) => {
                        tracing::warn!(%id, %err, "cast session ended with an error");
                        self.status = Status::error(err);
                    }
                    None if ndi_runtime_unavailable => {
                        self.status = Status::of("ndi-runtime-missing");
                    }
                    None => {
                        tracing::info!(%id, "cast session ended");
                        self.status = Status::of("idle");
                        self.refresh_status();
                    }
                }
            }
        }
    }

    fn discovery_event(&mut self, event: DiscoveryEvent) {
        match event {
            DiscoveryEvent::Added(sink) | DiscoveryEvent::Updated(sink) => {
                let id = sink.info().id;
                if !self.registry.contains_key(&id)
                    && self.registry.len() >= nd_core::provider::MAX_RECEIVERS
                {
                    return;
                }
                if !self.order.contains(&id) {
                    self.order.push(id.clone());
                }
                // An existing receiver is updated, never replaced: swapping the
                // `Arc` mid-session would lose the connection running on it.
                if self.active_cast.as_ref() != Some(&id) {
                    self.registry.insert(id, sink);
                }
                self.searching = false;
                self.refresh_status();
            }
            DiscoveryEvent::Removed(id) => {
                // Never remove the receiver that is streaming: a momentary mDNS
                // dropout would take the running session's row away.
                if self.active_cast.as_deref() == Some(id.as_str()) {
                    return;
                }
                self.registry.remove(&id);
                self.order.retain(|listed| listed != &id);
                self.refresh_status();
            }
            DiscoveryEvent::ProviderUnavailable { provider, reason } => {
                let issue = Issue {
                    provider: provider.to_string(),
                    reason,
                };
                self.issues.retain(|old| old.provider != issue.provider);
                self.issues.push(issue);
            }
            DiscoveryEvent::ProviderReady { provider } => {
                self.issues.retain(|issue| issue.provider != provider);
            }
        }
    }

    fn refresh_status(&mut self) {
        if matches!(
            self.status.kind.as_str(),
            "error" | "ndi-runtime-missing" | "stopping"
        ) {
            return;
        }
        // A running session outranks the receiver count: "3 receivers found"
        // while one of them is on the wall is true and useless.
        //
        // Which of the two it is comes from the sink, not from the mere
        // existence of one. Reporting "streaming" the moment a cast is
        // requested says the screen is on the wall before any receiver has
        // fetched a byte of it.
        if let Some(sink) = &self.active_sink {
            let kind = match sink.state() {
                SinkState::Streaming => "streaming",
                _ => "connecting",
            };
            self.status = Status::about(kind, sink.info().display_name);
            return;
        }
        if self.media_session.is_some() {
            self.status = Status::of("sending");
            return;
        }
        if !self.settings.auto_discovery && self.registry.is_empty() {
            self.status = Status::of("discovery-off");
            return;
        }
        let count = self.registry.len() as u32;
        self.status = match (count, self.searching) {
            (0, true) => Status::of("searching"),
            (0, false) => Status::of("empty"),
            (count, _) => Status::found(count),
        };
    }

    fn publish(&mut self) {
        let receivers = self
            .order
            .iter()
            .filter_map(|id| {
                let sink = if self.active_cast.as_ref() == Some(id) {
                    self.active_sink.as_ref()
                } else {
                    self.registry.get(id)
                }?;
                Some(Receiver::of(sink.as_ref()))
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
        let media = match media_status {
            Some(status) => Media {
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
            },
            None => Media::idle(),
        };
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

    /// A finished queue clears itself: leaving "playing 3 of 3" published after
    /// the last file ended would be untrue within a second.
    fn reap_finished_media(&mut self) {
        let Some(status) = self.media_session.as_ref().map(|s| s.status()) else {
            return;
        };
        if !status.finished {
            return;
        }
        self.media_session = None;
        self.media_owner = None;
        self.active_cast = None;
        self.active_sink = None;
        self.cast_cancel = None;
        self.measured = None;
        self.probing = false;
        match status.error {
            Some(error) => self.status = Status::error(error),
            None => {
                self.status = Status::of("idle");
                self.refresh_status();
            }
        }
    }

    /// Measures the link, at most one probe at a time.
    ///
    /// Only while something is streaming, and only when the protocol has said
    /// where the receiver is: a Miracast receiver is announced by MAC and has
    /// no address at all until the Wi-Fi Direct group exists.
    fn measure_link(&mut self) {
        if self.probing {
            return;
        }
        let Some(endpoint) = self
            .active_sink
            .as_ref()
            .and_then(|sink| sink.link())
            .and_then(|link| link.endpoint)
        else {
            return;
        };
        self.probing = true;
        let generation = self.operation_generation;
        let events = self.events.clone();
        tokio::spawn(async move {
            // Spaced out rather than run on every poll: the poll keeps the list
            // fresh, and opening a connection to the receiver two and a half
            // times a second would be rude to its firmware.
            tokio::time::sleep(LINK_PROBE_INTERVAL).await;
            let _ = events
                .send(Event::LinkMeasured(
                    generation,
                    nd_net::probe::round_trip(endpoint).await,
                ))
                .await;
        });
    }

    fn abort_discovery(&mut self) {
        if let Some(task) = self.discovery.take() {
            task.abort();
        }
        self.generation += 1;
    }

    fn apply_discovery_preference(&mut self) {
        if self.settings.auto_discovery {
            self.rescan();
        } else {
            self.abort_discovery();
            self.searching = false;
            self.refresh_status();
        }
    }

    fn rescan(&mut self) {
        if matches!(self.status.kind.as_str(), "error" | "ndi-runtime-missing") {
            self.status = Status::of("idle");
        }
        self.abort_discovery();
        self.issues.clear();
        self.searching = true;
        self.registry
            .retain(|id, _| self.active_cast.as_ref() == Some(id));
        self.order
            .retain(|id| self.active_cast.as_ref() == Some(id));
        self.refresh_status();
        self.start_discovery();
    }

    fn start_discovery(&mut self) {
        let (handle, registration) = futures::future::AbortHandle::new_pair();
        let generation = self.generation;
        let protocol = self.settings.protocol;
        let events = self.events.clone();
        tokio::spawn(async move {
            let _ = futures::future::Abortable::new(
                run_discovery(events, generation, protocol),
                registration,
            )
            .await;
        });
        let events = self.events.clone();
        tokio::spawn(async move {
            tokio::time::sleep(EMPTY_HINT_AFTER).await;
            let _ = events.send(Event::SearchTimedOut(generation)).await;
        });
        self.discovery = Some(handle);
    }

    fn probe_virtual_support(&mut self) {
        let events = self.events.clone();
        tokio::spawn(async move {
            // No sink yet: this only asks whether a virtual screen is possible.
            let backend = nd_capture::select_backend_for(SourceType::Virtual, None).await;
            let supported = backend.supported_sources().await;
            let _ = events
                .send(Event::VirtualSupported(
                    supported.contains(&SourceType::Virtual),
                ))
                .await;
        });
    }

    /// Ends whatever is running: the stream, the file sending, or both.
    fn stop_everything(&mut self) {
        if let Some(session) = &self.media_session {
            session.stop();
        }
        if let Some(cancel) = &self.cast_cancel {
            cancel.send_replace(true);
        }
        if self.active_sink.is_none() && self.media_session.is_none() {
            self.status = Status::of("idle");
            self.refresh_status();
            return;
        }
        // Cancelling is the whole of it. The cast task owns the orderly
        // teardown — STOP for the session, CLOSE for both virtual connections,
        // then the socket — and it reports back with `CastFinished`, which is
        // what clears the session here.
        //
        // This used to also call `stop_stream` itself, concurrently with the
        // task already doing it. Whichever closed the writer first could leave
        // the other's STOP unsent, and a receiver that never got a STOP keeps
        // the session and refuses the next connection.
        tracing::info!(id = ?self.active_cast, "stopping: cancelling the capture");
        self.status = Status::of("stopping");
    }

    fn control_player(
        &mut self,
        sender: &str,
        id: &str,
        command: Option<MediaCommand>,
    ) -> Result<(), String> {
        if !self
            .media_owner
            .as_ref()
            .is_some_and(|owner| owner.sender == sender && owner.id == id)
        {
            return Err("this player does not own the media session".into());
        }
        let session = self
            .media_session
            .as_ref()
            .ok_or("the media session has ended")?;
        match command {
            Some(command) => session.command(command).map_err(|err| err.to_string()),
            None => {
                self.stop_everything();
                Ok(())
            }
        }
    }

    fn begin_cast(
        &mut self,
        id: String,
        source: SourceType,
        page_text: Vec<(String, String)>,
    ) -> Result<(), String> {
        if self.active_cast.is_some() || self.media_session.is_some() {
            return Err("a stream is already running".into());
        }

        self.media_owner = None;
        // From the file, every time. The window writes it and this process
        // cached its own copy at start-up; a session built from that copy ran
        // on whatever was configured whenever the service happened to be
        // activated — resolution, frame rate and latency profile included.
        self.settings = settings::reload();

        let sink: Arc<dyn Sink> = if id == nd_ndi::ID {
            if !nd_ndi::available() {
                return Err("ndi-unavailable".into());
            }
            Arc::new(nd_ndi::NdiPublisher::new(self.settings.display_name()))
        } else if id == nd_webrtc::ID {
            if !nd_webrtc::available() {
                return Err("webrtc-unavailable".into());
            }
            let name = self.settings.display_name();
            Arc::new(nd_webrtc::WebRtcPublisher::new(
                name.clone(),
                nd_net::detect_gpu_driver(),
                crate::web_page_text(name, &page_text),
            ))
        } else if let Some(sink) = self.registry.get(&id).cloned() {
            sink
        } else {
            return Err("unknown-receiver".into());
        };

        let info = sink.info();
        if !info.kind.is_castable() {
            return Err("not-castable".into());
        }

        tracing::info!(name = %info.display_name, ?source, "starting the stream");
        self.status = Status::about("connecting", info.display_name.clone());
        self.operation_generation += 1;
        self.probing = false;
        self.measured = None;
        self.cast_source = source;
        self.active_cast = Some(id.clone());
        self.active_sink = Some(sink.clone());
        let (cancel, cancelled) = watch::channel(false);
        self.cast_cancel = Some(cancel);

        let events = self.events.clone();
        tokio::spawn(async move {
            let outcome = crate::cast::run(sink, id.clone(), source, cancelled).await;
            let _ = events
                .send(Event::CastFinished {
                    id,
                    error: outcome.error,
                    ndi_runtime_unavailable: outcome.ndi_runtime_unavailable,
                })
                .await;
        });
        Ok(())
    }

    /// Starts sending files to the receiver a client chose.
    ///
    /// The two protocols do genuinely different things here: a **Chromecast**
    /// is given a URL and plays the file itself, so this computer only serves
    /// bytes; a **Miracast** receiver is a screen and nothing else, so the file
    /// is decoded here and streamed as the picture.
    fn send_media(
        &mut self,
        target: String,
        files: Vec<MediaItem>,
        start: Option<nd_core::media::PlaybackStart>,
    ) -> Result<(), String> {
        self.settings = settings::reload();
        let Some(sink) = self.registry.get(&target).cloned() else {
            return Err("unknown-receiver".into());
        };
        // Sending files and mirroring the screen are two things the receiver
        // cannot do at once.
        if self.active_cast.is_some() || self.media_session.is_some() {
            return Err("a stream is already running".into());
        }

        let info = sink.info();
        if start.is_some() && !matches!(info.kind, SinkKind::Chromecast | SinkKind::Dlna) {
            return Err("player controls require a Chromecast or DLNA receiver".into());
        }
        tracing::info!(name = %info.display_name, count = files.len(), "sending files");
        self.media_owner = None;

        // A Chromecast fetches its own formats directly; anything else is
        // decoded here and streamed, the way DLNA always is.
        let fetchable = files.iter().all(|item| match item {
            MediaItem::File(file) => file.plays_natively(),
            MediaItem::Url { .. } => true,
        });
        if info.kind == SinkKind::Chromecast && fetchable {
            let endpoint = sink.control_endpoint().ok_or("unknown-receiver")?;
            let session = MediaSession::start(
                endpoint,
                files,
                self.settings.port,
                self.settings.display_name(),
                start,
            )
            .map_err(|err| err.to_string())?;
            self.media_session = Some(session);
            self.status = Status::of("sending");
            self.refresh_status();
            return Ok(());
        }

        self.operation_generation += 1;
        self.probing = false;
        self.measured = None;
        // A DLNA television is asked first whether it plays the files itself.
        let session = match sink.upnp_renderer() {
            Some(renderer) => {
                MediaSession::start_upnp(sink.clone(), renderer, files, self.settings.port, start)
            }
            None => MediaSession::start_mirroring(sink.clone(), files, start),
        }
        .map_err(|err| err.to_string())?;
        self.active_cast = Some(target);
        self.active_sink = Some(sink);
        self.media_session = Some(session);
        self.status = Status::of("sending");
        self.refresh_status();
        Ok(())
    }
}

/// The receivers that can be sent a file, for a client to choose from.
///
/// There is deliberately **no** "best guess" anywhere above this: casting to a
/// device is visible to whoever is standing in front of it, so it takes an
/// explicit choice every time.
pub fn can_receive_files(receiver: &Receiver) -> bool {
    wire::kind_from_name(&receiver.kind)
        .is_some_and(|kind| kind.can_receive_files(&receiver.address))
}

pub fn source_name(source: SourceType) -> &'static str {
    match source {
        SourceType::Monitor => "monitor",
        SourceType::Window => "window",
        SourceType::Virtual => "virtual",
    }
}

pub fn source_from_name(name: &str) -> Option<SourceType> {
    match name {
        "monitor" | "screen" => Some(SourceType::Monitor),
        "window" => Some(SourceType::Window),
        "virtual" => Some(SourceType::Virtual),
        _ => None,
    }
}

/// Runs discovery across the chosen providers and reports to the engine.
async fn run_discovery(events: mpsc::Sender<Event>, generation: u64, protocol: Protocol) {
    let mut providers: Vec<Arc<dyn Provider>> = Vec::new();

    // A preference for one protocol is honoured by **not starting** the other.
    // Filtering the list afterwards would leave the radio scanning for Wi-Fi
    // Direct peers during a Cast session, which is the contention the
    // preference exists to avoid.
    if matches!(protocol, Protocol::Auto | Protocol::Cast) {
        // A single mDNS daemon serves both Chromecast and AirPlay.
        match MdnsProvider::all_media_receivers() {
            Ok(provider) => providers.push(Arc::new(provider)),
            Err(err) => {
                let _ = events
                    .send(Event::Discovery(
                        DiscoveryEvent::ProviderUnavailable {
                            provider: "mdns",
                            reason: err.to_string(),
                        },
                        generation,
                    ))
                    .await;
            }
        }
    }
    if matches!(protocol, Protocol::Auto | Protocol::Cast) {
        // SSDP, not mDNS: the two discovery protocols share nothing, so a
        // DLNA television is invisible to the browse above and this one is
        // invisible to a Chromecast. Both have to run.
        providers.push(Arc::new(nd_dlna::DlnaProvider));
    }
    if matches!(protocol, Protocol::Auto | Protocol::Miracast) {
        providers.push(Arc::new(WfdP2pProvider));
    }
    if std::env::var_os("NETWORK_DISPLAYS_DUMMY").is_some() {
        providers.push(Arc::new(nd_core::dummy::DummyProvider));
    }

    let meta = MetaProvider::new(providers);
    let mut stream = meta.discover().await;
    while let Some(event) = stream.next().await {
        if events
            .send(Event::Discovery(event, generation))
            .await
            .is_err()
        {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nd_core::sink::{SinkInfo, SinkState};

    /// The smallest thing that satisfies `Sink`. `nd_core::dummy` keeps its own
    /// private, and the tests here ask it its name and what it is doing.
    struct TestSink(String, SinkState);

    #[async_trait::async_trait]
    impl Sink for TestSink {
        fn info(&self) -> SinkInfo {
            SinkInfo {
                id: self.0.clone(),
                display_name: self.0.clone(),
                kind: SinkKind::Dummy,
                address: None,
            }
        }
        fn state(&self) -> SinkState {
            self.1
        }
        async fn start_stream(&self, _: nd_core::capture::CaptureSource) -> nd_core::Result<()> {
            Ok(())
        }
        async fn stop_stream(&self) -> nd_core::Result<()> {
            Ok(())
        }
    }

    fn sink(id: &str) -> Arc<dyn Sink> {
        Arc::new(TestSink(id.to_string(), SinkState::Disconnected))
    }

    fn streaming_sink(id: &str) -> Arc<dyn Sink> {
        Arc::new(TestSink(id.to_string(), SinkState::Streaming))
    }

    fn engine() -> Engine {
        let (tx, _rx) = watch::channel(Snapshot::initial(Settings::default()));
        Engine::new(tx)
    }

    #[test]
    fn a_running_session_outranks_the_receiver_count_in_the_status() {
        let mut engine = engine();
        engine.settings.auto_discovery = true;
        engine.searching = false;
        engine.refresh_status();
        assert_eq!(engine.status.kind, "empty");

        engine.registry.insert("a".into(), sink("a"));
        engine.refresh_status();
        assert_eq!(engine.status, Status::found(1));

        // Chosen but not yet on the wall. Saying "streaming" here would
        // announce a picture no receiver has fetched a byte of.
        engine.active_sink = Some(sink("a"));
        engine.refresh_status();
        assert_eq!(engine.status.kind, "connecting");

        engine.active_sink = Some(streaming_sink("a"));
        engine.refresh_status();
        assert_eq!(engine.status.kind, "streaming");
    }

    #[test]
    fn discovery_off_with_nothing_found_says_so_rather_than_empty() {
        let mut engine = engine();
        engine.settings.auto_discovery = false;
        engine.searching = false;
        engine.refresh_status();
        assert_eq!(engine.status.kind, "discovery-off");
    }

    #[test]
    fn a_failure_survives_polling_and_discovery_until_an_explicit_action() {
        let mut engine = engine();
        engine.active_cast = Some("a".into());
        engine.active_sink = Some(sink("a"));
        engine.event(Event::CastFinished {
            id: "a".into(),
            error: Some("audio unavailable".into()),
            ndi_runtime_unavailable: false,
        });
        engine.refresh_status();
        engine.discovery_event(DiscoveryEvent::Added(sink("b")));
        engine.event(Event::SearchTimedOut(engine.generation));
        assert_eq!(engine.status, Status::error("audio unavailable".into()));
        engine.stop_everything();
        assert_eq!(engine.status, Status::found(1));
    }

    #[test]
    fn stopping_survives_polling_until_the_cast_has_finished() {
        let mut engine = engine();
        engine.active_cast = Some("a".into());
        engine.active_sink = Some(streaming_sink("a"));
        engine.stop_everything();
        engine.refresh_status();
        engine.discovery_event(DiscoveryEvent::Added(sink("b")));
        assert_eq!(engine.status.kind, "stopping");
        engine.event(Event::CastFinished {
            id: "a".into(),
            error: None,
            ndi_runtime_unavailable: false,
        });
        assert_eq!(engine.status, Status::found(1));
        assert!(engine.active_cast.is_none());
    }

    #[test]
    fn the_streaming_receiver_survives_a_discovery_dropout() {
        let mut engine = engine();
        engine.order.push("a".into());
        engine.registry.insert("a".into(), sink("a"));
        engine.active_cast = Some("a".into());
        engine.active_sink = Some(sink("a"));
        engine.discovery_event(DiscoveryEvent::Removed("a".into()));
        assert!(engine.order.contains(&"a".to_string()));
    }

    #[test]
    fn source_names_survive_a_round_trip() {
        for source in [SourceType::Monitor, SourceType::Window, SourceType::Virtual] {
            assert_eq!(source_from_name(source_name(source)), Some(source));
        }
        assert_eq!(source_from_name("nonsense"), None);
    }

    #[test]
    fn discovery_bounds_new_receivers_and_replaces_provider_diagnostics() {
        let mut engine = engine();
        for index in 0..nd_core::provider::MAX_RECEIVERS + 10 {
            engine.discovery_event(DiscoveryEvent::Added(sink(&index.to_string())));
            engine.discovery_event(DiscoveryEvent::ProviderUnavailable {
                provider: "test",
                reason: index.to_string(),
            });
        }
        assert_eq!(engine.registry.len(), nd_core::provider::MAX_RECEIVERS);
        assert_eq!(engine.order.len(), nd_core::provider::MAX_RECEIVERS);
        assert_eq!(engine.issues.len(), 1);
        engine.discovery_event(DiscoveryEvent::Removed("0".into()));
        engine.discovery_event(DiscoveryEvent::Added(sink("new")));
        assert!(engine.registry.contains_key("new"));
    }

    #[tokio::test]
    async fn cancelling_file_playback_retains_the_session_until_cleanup_finishes() {
        let mut engine = engine();
        let file = MediaFile {
            path: "/nonexistent/cancellation-test.png".into(),
            kind: nd_core::media::MediaKind::Photo,
            content_type: "image/png",
            size: 0,
        };
        engine.media_session = Some(
            MediaSession::start(
                "127.0.0.1:9".parse().unwrap(),
                vec![MediaItem::File(file)],
                0,
                "test".into(),
                None,
            )
            .unwrap(),
        );
        engine.media_owner = Some(MediaOwner {
            id: "current-session".into(),
            sender: ":1.10".into(),
            receiver: "test".into(),
        });
        for (sender, id) in [(":1.11", "current-session"), (":1.10", "previous-session")] {
            assert!(engine.control_player(sender, id, None).is_err());
            assert!(
                engine
                    .control_player(sender, id, Some(MediaCommand::SetPaused(true)))
                    .is_err()
            );
        }
        assert_ne!(engine.status.kind, "stopping");
        engine
            .control_player(":1.10", "current-session", None)
            .unwrap();
        engine.refresh_status();
        assert!(engine.media_session.is_some());
        assert_eq!(engine.status.kind, "stopping");
        tokio::time::timeout(Duration::from_secs(2), async {
            while engine.media_session.is_some() {
                tokio::task::yield_now().await;
                engine.reap_finished_media();
            }
        })
        .await
        .expect("cancelled file session must finish cleanup");
        assert_ne!(engine.status.kind, "stopping");
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_waits_for_the_session_to_finish_cleanup() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut engine = engine();
        engine.active_cast = Some("test".into());
        engine.active_sink = Some(streaming_sink("test"));
        let (cancel, mut cancelled) = watch::channel(false);
        engine.cast_cancel = Some(cancel);
        let (events, mut incoming) = mpsc::channel(1);
        let finished = AtomicBool::new(false);
        tokio::join!(
            async {
                engine.finish_shutdown(&mut incoming).await;
                assert!(
                    finished.load(Ordering::SeqCst),
                    "engine left before protocol cleanup"
                );
                assert!(engine.active_cast.is_none());
            },
            async {
                cancelled.changed().await.unwrap();
                assert!(*cancelled.borrow());
                tokio::time::sleep(Duration::from_secs(2)).await;
                finished.store(true, Ordering::SeqCst);
                events
                    .send(Event::CastFinished {
                        id: "test".into(),
                        error: None,
                        ndi_runtime_unavailable: false,
                    })
                    .await
                    .unwrap();
            }
        );
    }

    #[tokio::test]
    #[ignore = "requires BNS_TEST_PRIVATE_BUS=1 and a private dbus-run-session"]
    async fn player_dbus_methods_use_the_actual_calling_connection() {
        assert_eq!(std::env::var("BNS_TEST_PRIVATE_BUS").as_deref(), Ok("1"));
        let path = std::env::temp_dir().join(format!("bns-player-api-{}.mp3", std::process::id()));
        std::fs::write(&path, []).unwrap();
        let (commands, mut incoming) = mpsc::channel(32);
        let (_snapshots, receiver) = watch::channel(Snapshot::initial(Settings::default()));
        let service = crate::dbus::serve(Handle {
            commands,
            snapshots: receiver,
        })
        .await
        .unwrap();
        let client = zbus::Connection::session().await.unwrap();
        let proxy = crate::dbus::ServiceProxy::new(&client).await.unwrap();
        let expected_sender = client.unique_name().unwrap().to_string();
        let engine = tokio::spawn(async move {
            match incoming.recv().await.unwrap() {
                Command::StartPlayer {
                    sender,
                    target,
                    files,
                    start,
                    reply,
                } => {
                    assert_eq!(sender, expected_sender);
                    assert_eq!(target, "living-room");
                    assert_eq!(files.len(), 1);
                    assert_eq!(
                        start,
                        nd_core::media::PlaybackStart {
                            seconds: 1.0,
                            paused: true,
                            volume: 0.25,
                            muted: true,
                            height: 720,
                        }
                    );
                    reply.send(Ok("session-a".into())).unwrap();
                }
                other => panic!("unexpected {other:?}"),
            }
            match incoming.recv().await.unwrap() {
                Command::ControlPlayer {
                    sender,
                    id,
                    command,
                    reply,
                } => {
                    assert_eq!(sender, expected_sender);
                    assert_eq!(id, "session-a");
                    assert_eq!(command, Some(MediaCommand::SetVolume(0.37)));
                    reply.send(Ok(())).unwrap();
                }
                other => panic!("unexpected {other:?}"),
            }
            match incoming.recv().await.unwrap() {
                Command::StartPlayer {
                    sender,
                    target,
                    files,
                    start,
                    reply,
                } => {
                    assert_eq!(sender, expected_sender);
                    assert_eq!(target, "living-room");
                    assert_eq!(start.seconds, 0.0);
                    assert!(matches!(
                        &files[..],
                        [MediaItem::Url {
                            kind: nd_core::media::MediaKind::Music,
                            ..
                        }]
                    ));
                    reply.send(Ok("session-url".into())).unwrap();
                }
                other => panic!("unexpected {other:?}"),
            }
            match incoming.recv().await.unwrap() {
                Command::PlayerGone(sender) => assert_eq!(sender, expected_sender),
                other => panic!("unexpected {other:?}"),
            }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            assert_eq!(proxy.player_api().await.unwrap(), 2);
            assert!(proxy.player_session().await.unwrap().id.is_empty());
            let id = proxy
                .start_player(
                    "living-room",
                    &[path.display().to_string()],
                    (1.0, true, 0.25, true, 720),
                )
                .await
                .unwrap();
            assert_eq!(id, "session-a");
            assert!(
                proxy
                    .control_player(&id, "volume", f64::NAN, "")
                    .await
                    .is_err()
            );
            proxy.control_player(&id, "volume", 0.37, "").await.unwrap();
            let id = proxy
                .start_player_url(
                    "living-room",
                    "https://example.invalid/radio",
                    "audio/mpeg",
                    "Test radio",
                    true,
                    (0.0, false, 1.0, false, 0),
                )
                .await
                .unwrap();
            assert_eq!(id, "session-url");
            drop(proxy);
            drop(client);
            engine.await.unwrap();
        })
        .await
        .unwrap();
        drop(service);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn only_the_protocols_that_can_play_a_file_are_offered_one() {
        let receiver = |kind: &str, address: &str| Receiver {
            id: "x".into(),
            display_name: "x".into(),
            kind: kind.into(),
            address: address.into(),
            castable: true,
            state: "disconnected".into(),
            detail: String::new(),
            width: 0,
            height: 0,
            fps: 0,
        };
        assert!(can_receive_files(&receiver("chromecast", "192.168.1.2")));
        // A Chromecast announced without a usable address has nothing to send to.
        assert!(!can_receive_files(&receiver("chromecast", "")));
        assert!(can_receive_files(&receiver("wfd-p2p", "")));
        assert!(!can_receive_files(&receiver("airplay", "192.168.1.2")));
        assert!(!can_receive_files(&receiver("ndi", "")));
    }
}
