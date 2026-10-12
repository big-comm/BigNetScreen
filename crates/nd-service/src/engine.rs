//! The session, with nobody watching.
//!
//! Everything that outlives a window: the receivers discovery has found, the
//! running stream, the files being sent, and the preferences the next session
//! will obey. Closing the window stops drawing and nothing else.
//!
//! [`discovery`] finds receivers, [`session`] starts and reaps casts and file
//! sessions, [`snapshot`] builds what clients see; `crate::dbus` exports it.
//!
//! It runs as one task that owns its state outright. Callers send a
//! [`Command`]; the state is published as a [`Snapshot`] on a `watch` channel.
//! No locks are handed out: a client that could hold the state while it renders
//! could also hold it while a receiver disconnects.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};

use nd_chromecast::file_server::MediaFile;
use nd_chromecast::media::{MediaItem, MediaSession};
use nd_core::capture::SourceType;
use nd_core::media::MediaCommand;
use nd_core::provider::DiscoveryEvent;
use nd_core::settings::{self, Settings};
use nd_core::sink::{Sink, SinkState};

use crate::wire::{self, Issue, Receiver, Status, StatusKind};

mod discovery;
mod session;
mod snapshot;

pub use snapshot::{MediaOwner, Snapshot};

/// After this long with no receiver, the service stops saying "searching".
const EMPTY_HINT_AFTER: Duration = Duration::from_secs(12);
/// The prefix of every failure to change the virtual sound card, so a later
/// settings change knows to try again.
const SOUND_CARD_FAILED: &str = "could not change the virtual sound card";
/// How often receiver state is re-read and republished.
const STATE_POLL: Duration = Duration::from_millis(400);
/// How long between measurements of the link.
///
/// Long enough to be unnoticeable to the receiver, short enough that the
/// reading reflects a Wi-Fi that has just got worse.
const LINK_PROBE_INTERVAL: Duration = Duration::from_secs(3);

/// What a client asks the service to do.
#[derive(Debug)]
pub enum Command {
    Cast {
        id: String,
        source: SourceType,
        /// The connector of the screen to stream, when the compositor streams
        /// one directly and more than one is connected.
        screen: Option<String>,
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
    pub(crate) commands: mpsc::Sender<Command>,
    pub(crate) snapshots: watch::Receiver<Snapshot>,
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
        screen: Option<String>,
        page_text: Vec<(String, String)>,
    ) -> Result<(), String> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Cast {
            id,
            source,
            screen,
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
    /// The receiver files are being sent to.
    media_target: Option<String>,
    /// A receiver refused for proving another identity while files were sent
    /// to it, and why. Its row says so, like a refused screen does, until the
    /// next attempt to it: that is where the person can trust it again.
    refused: Option<(String, String)>,
    /// The last link measurement. The outer `None` means *not measured yet*,
    /// the inner one means *the receiver did not answer*.
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
            media_target: None,
            refused: None,
            media_owner: None,
            measured: None,
            probing: false,
            operation_generation: 0,
            status: if settings.auto_discovery {
                Status::of(StatusKind::Searching)
            } else {
                Status::of(StatusKind::DiscoveryOff)
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
                _ = poll.tick(), if self.active_cast.is_some() || self.media_session.is_some() || self.audio_pending => {
                    self.reap_finished_media();
                    self.measure_link();
                    // A session's progress raises no event of its own. Without
                    // this the status would only catch up when discovery
                    // chatters, and SSDP speaks only when something changed,
                    // so a DLNA session could read "connecting" for minutes.
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
        // The card goes with the service: left behind, a sink that discards
        // everything stays on the output list, and whoever picked it, or a
        // session manager that fell back to it, has a computer with no sound.
        // The next start puts it back (`reconcile_audio`, and every cast that
        // needs it), and the session manager remembers by name what was routed
        // to it, so the routing survives the gap.
        if let Err(err) = nd_core::virtual_sink::remove().await {
            tracing::warn!(%err, "could not remove the virtual sound card on the way out");
        }
    }

    async fn command(&mut self, command: Command) {
        match command {
            Command::Cast {
                id,
                source,
                screen,
                page_text,
                reply,
            } => {
                let _ = reply.send(self.begin_cast(id, source, screen, page_text));
            }
            Command::Stop => self.stop_everything(),
            Command::Rescan => self.rescan(),
            Command::KeepAlive => self.last_client_activity = Some(tokio::time::Instant::now()),
            Command::SetAutoDiscovery(on, reply) => {
                let _ = reply.send(self.set_auto_discovery(on).await);
            }
            Command::ApplySettings(new, reply) => {
                let _ = reply.send(self.apply_settings(*new).await);
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
                let _ = reply.send(self.start_player(sender, target, files, start));
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

    async fn set_auto_discovery(&mut self, on: bool) -> Result<(), String> {
        self.settings.auto_discovery = on;
        settings::set_in_memory(&self.settings);
        let persisted = tokio::task::spawn_blocking(settings::persist)
            .await
            .map_err(|err| err.to_string())
            .and_then(|result| result.map_err(|err| err.to_string()));
        self.apply_discovery_preference();
        persisted
    }

    /// Takes preferences a client has just saved, acting at once on the ones
    /// that cannot wait for the next session.
    async fn apply_settings(&mut self, new: Settings) -> Result<(), String> {
        let protocol_changed = new.protocol != self.settings.protocol;
        let discovery_changed = new.auto_discovery != self.settings.auto_discovery;
        let card_changed = (new.virtual_audio && new.system_audio)
            != (self.settings.virtual_audio && self.settings.system_audio);
        self.settings = new;
        settings::set_in_memory(&self.settings);
        let retry_audio =
            self.status.is(StatusKind::Error) && self.status.detail.starts_with(SOUND_CARD_FAILED);
        self.audio_pending |= card_changed || retry_audio;
        // The receiver list a client is showing is the result of the old
        // protocol choice, so discovery restarts now.
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
            self.status = Status::of(StatusKind::Idle);
            self.refresh_status();
        }
        result
    }

    /// Starts files for an external player and makes `sender` their owner.
    /// The returned id is what [`Command::ControlPlayer`] must quote.
    fn start_player(
        &mut self,
        sender: String,
        target: String,
        files: Vec<MediaItem>,
        start: nd_core::media::PlaybackStart,
    ) -> Result<String, String> {
        let id = nd_core::secret::random_token().map_err(|err| err.to_string())?;
        self.send_media(target.clone(), files, Some(start))?;
        self.media_owner = Some(MediaOwner {
            id: id.clone(),
            sender,
            receiver: target,
        });
        Ok(id)
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
        result.map_err(|err| format!("{SOUND_CARD_FAILED}: {err}"))
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
                // The session may have ended while the probe was in flight, and
                // a reading for a link that has gone is not published.
                if self.active_sink.is_some() {
                    self.measured = Some(round_trip);
                }
            }
            Event::CastFinished {
                id,
                error,
                ndi_runtime_unavailable,
            } => self.cast_finished(&id, error, ndi_runtime_unavailable),
        }
    }

    /// The only thing that clears a cast, so a client sees a session end only
    /// once the cast task has finished its teardown.
    fn cast_finished(&mut self, id: &str, error: Option<String>, ndi_runtime_unavailable: bool) {
        tracing::info!(%id, failed = error.is_some(), "the cast task returned");
        if self.active_cast.as_deref() != Some(id) {
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
                self.status = Status::of(StatusKind::NdiRuntimeMissing);
            }
            None => {
                tracing::info!(%id, "cast session ended");
                self.status = Status::of(StatusKind::Idle);
                self.refresh_status();
            }
        }
    }

    fn refresh_status(&mut self) {
        if matches!(
            self.status.kind(),
            Some(StatusKind::Error | StatusKind::NdiRuntimeMissing | StatusKind::Stopping)
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
                SinkState::Streaming => StatusKind::Streaming,
                _ => StatusKind::Connecting,
            };
            self.status = Status::about(kind, sink.info().display_name);
            return;
        }
        if self.media_session.is_some() {
            self.status = Status::of(StatusKind::Sending);
            return;
        }
        if !self.settings.auto_discovery && self.registry.is_empty() {
            self.status = Status::of(StatusKind::DiscoveryOff);
            return;
        }
        let count = self.registry.len() as u32;
        self.status = match (count, self.searching) {
            (0, true) => Status::of(StatusKind::Searching),
            (0, false) => Status::of(StatusKind::Empty),
            (count, _) => Status::found(count),
        };
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
            self.status = Status::of(StatusKind::Idle);
            self.refresh_status();
            return;
        }
        // Cancelling is enough. The cast task owns the orderly teardown (STOP
        // for the session, CLOSE for both virtual connections, then the
        // socket) and it reports back with `CastFinished`, which is
        // what clears the session here.
        //
        // Calling `stop_stream` here as well would race the task: whichever
        // closed the writer first could leave the other's STOP unsent, and a
        // receiver that never got a STOP keeps the session and refuses the
        // next connection.
        tracing::info!(id = ?self.active_cast, "stopping: cancelling the capture");
        self.status = Status::of(StatusKind::Stopping);
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
}

/// The receivers that can be sent a file, for a client to choose from.
///
/// There is no "best guess" anywhere above this: casting to a
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

/// A cast's source as clients spell it: a [`source_name`], and for a screen
/// optionally the connector of the one to stream, `monitor:DP-1`.
pub fn source_from_arg(arg: &str) -> Option<(SourceType, Option<String>)> {
    match arg.split_once(':') {
        Some((name, screen)) if !screen.is_empty() => match source_from_name(name)? {
            SourceType::Monitor => Some((SourceType::Monitor, Some(screen.to_string()))),
            _ => None,
        },
        Some(_) => None,
        None => Some((source_from_name(arg)?, None)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use nd_core::sink::{SinkInfo, SinkKind, SinkState};

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

    pub(super) fn sink(id: &str) -> Arc<dyn Sink> {
        Arc::new(TestSink(id.to_string(), SinkState::Disconnected))
    }

    pub(super) fn streaming_sink(id: &str) -> Arc<dyn Sink> {
        Arc::new(TestSink(id.to_string(), SinkState::Streaming))
    }

    pub(super) fn engine() -> Engine {
        let (tx, _rx) = watch::channel(Snapshot::initial(Settings::default()));
        Engine::new(tx)
    }

    #[test]
    fn running_session_outranks_receiver_count() {
        let mut engine = engine();
        engine.settings.auto_discovery = true;
        engine.searching = false;
        engine.refresh_status();
        assert!(engine.status.is(StatusKind::Empty));

        engine.registry.insert("a".into(), sink("a"));
        engine.refresh_status();
        assert_eq!(engine.status, Status::found(1));

        // Chosen but not yet on the wall. Saying "streaming" here would
        // announce a picture no receiver has fetched a byte of.
        engine.active_sink = Some(sink("a"));
        engine.refresh_status();
        assert!(engine.status.is(StatusKind::Connecting));

        engine.active_sink = Some(streaming_sink("a"));
        engine.refresh_status();
        assert!(engine.status.is(StatusKind::Streaming));
    }

    #[test]
    fn discovery_off_is_reported() {
        let mut engine = engine();
        engine.settings.auto_discovery = false;
        engine.searching = false;
        engine.refresh_status();
        assert!(engine.status.is(StatusKind::DiscoveryOff));
    }

    #[test]
    fn failure_persists_until_user_action() {
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
    fn stopping_persists_until_cast_finishes() {
        let mut engine = engine();
        engine.active_cast = Some("a".into());
        engine.active_sink = Some(streaming_sink("a"));
        engine.stop_everything();
        engine.refresh_status();
        engine.discovery_event(DiscoveryEvent::Added(sink("b")));
        assert!(engine.status.is(StatusKind::Stopping));
        engine.event(Event::CastFinished {
            id: "a".into(),
            error: None,
            ndi_runtime_unavailable: false,
        });
        assert_eq!(engine.status, Status::found(1));
        assert!(engine.active_cast.is_none());
    }

    #[test]
    fn source_names_survive_a_round_trip() {
        for source in [SourceType::Monitor, SourceType::Window, SourceType::Virtual] {
            assert_eq!(source_from_name(source_name(source)), Some(source));
        }
        assert_eq!(source_from_name("nonsense"), None);
    }

    #[test]
    fn screen_source_names_connector() {
        assert_eq!(
            source_from_arg("monitor"),
            Some((SourceType::Monitor, None))
        );
        assert_eq!(
            source_from_arg("screen:DP-1"),
            Some((SourceType::Monitor, Some("DP-1".into())))
        );
        assert_eq!(
            source_from_arg("virtual"),
            Some((SourceType::Virtual, None))
        );
        // Only a screen takes a name, and the name is not optional after `:`.
        assert_eq!(source_from_arg("window:DP-1"), None);
        assert_eq!(source_from_arg("monitor:"), None);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_waits_for_session_cleanup() {
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

    #[test]
    fn files_offered_to_capable_protocols() {
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
