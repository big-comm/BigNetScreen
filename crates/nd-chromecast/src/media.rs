//! File queues and playback controls for Cast and mirrored media.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use nd_core::capture::{CaptureSource, MediaPlayback};
use nd_core::media::{
    FilePlaybackControl, MediaCommand, MediaSource, PlaybackStart, PlaybackState, seek_target,
};
use nd_core::sink::{Sink, UpnpRenderer};
use nd_core::{NdError, Result};
use nd_dlna::avtransport::{self, TransportState};
use nd_dlna::renderer::{Renderer, accepts};

use crate::cast::{CastChannel, DEFAULT_MEDIA_RECEIVER, LaunchedApp, NS_MEDIA};
use crate::file_server::{FileServer, MediaFile, MediaKind};

pub const PHOTO_SECONDS: u64 = 8;
const SILENCE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub enum MediaItem {
    File(MediaFile),
    Url {
        uri: String,
        content_type: String,
        title: String,
        kind: MediaKind,
    },
}

impl std::fmt::Debug for MediaItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(file) => f.debug_tuple("File").field(file).finish(),
            Self::Url { kind, .. } => f
                .debug_struct("Url")
                .field("kind", kind)
                .finish_non_exhaustive(),
        }
    }
}

impl MediaItem {
    pub fn url(uri: String, content_type: String, title: String, audio: bool) -> Result<Self> {
        use gstreamer::glib::{Uri, UriFlags};
        if uri.len() > 8192 || uri.chars().any(char::is_control) {
            return Err(NdError::Unsupported("invalid stream URL".into()));
        }
        let parsed = Uri::parse(&uri, UriFlags::ENCODED)
            .map_err(|_| NdError::Unsupported("invalid stream URL".into()))?;
        if !matches!(parsed.scheme().as_str(), "http" | "https")
            || parsed.host().is_none_or(|host| host.is_empty())
        {
            return Err(NdError::Unsupported(
                "only HTTP and HTTPS streams are supported".into(),
            ));
        }
        if content_type.len() > 128
            || !content_type.is_ascii()
            || !content_type
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"/.-+".contains(&c))
            || !content_type.contains('/')
            || title.len() > 1024
            || title.chars().any(char::is_control)
        {
            return Err(NdError::Unsupported("invalid stream metadata".into()));
        }
        Ok(Self::Url {
            uri,
            content_type,
            title,
            kind: if audio {
                MediaKind::Music
            } else {
                MediaKind::Video
            },
        })
    }

    fn file(&self) -> Option<&MediaFile> {
        match self {
            Self::File(file) => Some(file),
            Self::Url { .. } => None,
        }
    }

    pub(crate) fn kind(&self) -> MediaKind {
        match self {
            Self::File(file) => file.kind,
            Self::Url { kind, .. } => *kind,
        }
    }

    pub(crate) fn content_type(&self) -> &str {
        match self {
            Self::File(file) => file.content_type,
            Self::Url { content_type, .. } => content_type,
        }
    }

    pub(crate) fn title(&self) -> String {
        match self {
            Self::File(file) => file.title(),
            Self::Url { title, .. } => title.clone(),
        }
    }

    fn source(&self) -> MediaSource {
        match self {
            Self::File(file) => MediaSource::File(file.path.clone()),
            Self::Url { uri, .. } => MediaSource::Url(uri.clone()),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaStatus {
    pub position: usize,
    pub total: usize,
    pub title: String,
    pub queue: Vec<PathBuf>,
    pub playback: PlaybackState,
    pub finished: bool,
    pub error: Option<String>,
    pub control_error: Option<String>,
}

struct Shared {
    status: Mutex<MediaStatus>,
    stopping: AtomicBool,
    latest: Mutex<Latest>,
}

/// The seek and the volume waiting for the session. A slider sends them far
/// faster than a TV follows (a seek restarts the item there), so the last
/// one asked waits here and the queue holds one place for it, not hundreds.
#[derive(Default)]
struct Latest {
    seek: Option<MediaCommand>,
    volume: Option<MediaCommand>,
}

impl Latest {
    fn slot(&mut self, command: &MediaCommand) -> Option<&mut Option<MediaCommand>> {
        match command {
            MediaCommand::SeekTo(_) | MediaCommand::SeekRelative(_) => Some(&mut self.seek),
            MediaCommand::SetVolume(_) => Some(&mut self.volume),
            _ => None,
        }
    }
}

/// `later` asked while `earlier` still waited: relative steps add up.
fn merge(earlier: Option<MediaCommand>, later: MediaCommand) -> MediaCommand {
    match (earlier, later) {
        (Some(MediaCommand::SeekTo(at)), MediaCommand::SeekRelative(by)) => {
            MediaCommand::SeekTo(at + by)
        }
        (Some(MediaCommand::SeekRelative(a)), MediaCommand::SeekRelative(b)) => {
            MediaCommand::SeekRelative(a + b)
        }
        (_, later) => later,
    }
}

/// The next command, a seek or a volume standing for the latest one asked.
async fn next_command(
    commands: &mut mpsc::Receiver<MediaCommand>,
    shared: &Shared,
) -> Option<MediaCommand> {
    let command = commands.recv().await?;
    let mut latest = shared.latest.lock().unwrap_or_else(|e| e.into_inner());
    Some(
        latest
            .slot(&command)
            .and_then(Option::take)
            .unwrap_or(command),
    )
}

impl Shared {
    fn next_start(&self, start: &mut Option<PlaybackStart>) {
        if let Some(start) = start {
            let status = self.status.lock().unwrap_or_else(|e| e.into_inner());
            start.seconds = 0.0;
            start.paused = status.playback.paused;
            start.volume = status.playback.volume.unwrap_or(start.volume);
            start.muted = status.playback.muted.unwrap_or(start.muted);
        }
    }
    fn update(&self, update: impl FnOnce(&mut MediaStatus)) {
        update(&mut self.status.lock().unwrap_or_else(|e| e.into_inner()));
    }

    fn finish(&self, outcome: Result<()>) {
        self.update(|status| {
            status.finished = true;
            // User cancellation is already represented by Ok in orchestration.
            // A failed STOP is still an error and must not be hidden by Stop.
            status.error = outcome.err().map(|err| err.to_string());
        });
    }
}

/// Immutable HTTP indexes survive removal of pending files.
struct Queue {
    pending: VecDeque<(usize, MediaItem)>,
    completed: usize,
}

impl Queue {
    fn new(files: Vec<MediaItem>) -> Self {
        let mut file_index = 0;
        Self {
            pending: files
                .into_iter()
                .map(|item| {
                    let index = file_index;
                    file_index += usize::from(item.file().is_some());
                    (index, item)
                })
                .collect(),
            completed: 0,
        }
    }

    fn publish(&self, shared: &Shared, new_item: bool) {
        shared.update(|status| {
            status.queue = self
                .pending
                .iter()
                .filter_map(|(_, item)| item.file().map(|file| file.path.clone()))
                .collect();
            status.total = self.completed + self.pending.len();
            if new_item {
                status.position = self.completed + usize::from(!self.pending.is_empty());
                status.title = self
                    .pending
                    .front()
                    .map(|(_, file)| file.title())
                    .unwrap_or_default();
                status.playback = PlaybackState::default();
                status.control_error = None;
            }
        });
    }

    fn advance(&mut self) {
        if self.pending.pop_front().is_some() {
            self.completed += 1;
        }
    }

    /// Returns true if the current item was removed and must stop.
    fn edit(&mut self, command: &MediaCommand, shared: &Shared) -> bool {
        let advance = match command {
            MediaCommand::Next => {
                self.advance();
                true
            }
            MediaCommand::Remove(path) => {
                let current = self
                    .pending
                    .front()
                    .is_some_and(|(_, item)| item.file().is_some_and(|file| &file.path == path));
                self.pending
                    .retain(|(_, item)| item.file().is_none_or(|file| &file.path != path));
                current
            }
            _ => false,
        };
        self.publish(shared, false);
        advance
    }
}

/// Dropping a session stops playback and closes its file server.
pub struct MediaSession {
    shared: Arc<Shared>,
    _task: tokio::task::JoinHandle<()>,
    cancel: watch::Sender<bool>,
    commands: mpsc::Sender<MediaCommand>,
}

impl Drop for MediaSession {
    fn drop(&mut self) {
        self.stop();
    }
}

impl MediaSession {
    pub fn start(
        receiver: SocketAddr,
        files: Vec<MediaItem>,
        port: u16,
        sender_name: String,
        start: Option<PlaybackStart>,
    ) -> Result<Self> {
        let start = start.map(PlaybackStart::validate).transpose()?;
        Self::spawn(
            files,
            move |files, shared, mut cancelled, commands| async move {
                let outcome = async {
                let local_files: Vec<_> = files.iter().filter_map(|item| item.file().cloned()).collect();
                let server = if local_files.is_empty() { None } else {
                    Some(FileServer::start(receiver.ip(), port, local_files).await?)
                };
                let channel = tokio::select! {
                    result = CastChannel::connect_to(receiver.ip(), receiver.port()) => result?,
                    _ = cancelled.changed() => return Ok(()),
                };
                // Finish LAUNCH so cancellation can explicitly STOP its result.
                let app = match channel.launch(DEFAULT_MEDIA_RECEIVER).await {
                    Ok(app) => app,
                    Err(err) => { channel.close().await; return Err(err); }
                };
                let outcome = if shared.stopping.load(Ordering::SeqCst) { Ok(()) } else {
                    tokio::select! {
                        result = play_queue(&channel, &app, server.as_ref(), &shared, &sender_name, files, commands, start) => result,
                        _ = cancelled.changed() => Ok(()),
                    }
                };
                drop(server);
                channel.finish_app(&app, outcome).await
            }.await;
                shared.finish(outcome);
            },
        )
    }

    pub fn start_mirroring(
        sink: Arc<dyn Sink>,
        files: Vec<MediaItem>,
        start: Option<PlaybackStart>,
    ) -> Result<Self> {
        let start = start.map(PlaybackStart::validate).transpose()?;
        Self::spawn(
            files,
            move |files, shared, cancelled, commands| async move {
                let outcome =
                    play_mirrored_queue(sink, None, files, &shared, cancelled, commands, start)
                        .await;
                shared.finish(outcome);
            },
        )
    }

    /// A DLNA television plays the files itself when it lists their format
    /// — music goes as music, not as a black picture with sound — and gets
    /// them decoded otherwise, as [`Self::start_mirroring`] does.
    pub fn start_upnp(
        sink: Arc<dyn Sink>,
        renderer: UpnpRenderer,
        files: Vec<MediaItem>,
        port: u16,
        start: Option<PlaybackStart>,
    ) -> Result<Self> {
        let start = start.map(PlaybackStart::validate).transpose()?;
        Self::spawn(
            files,
            move |files, shared, mut cancelled, commands| async move {
                let outcome = async {
                    let tv = Renderer::new(&renderer)?;
                    let accepted = tv.accepted_types().await.unwrap_or_default();
                    let native: Option<Vec<MediaFile>> = files
                        .iter()
                        .map(|item| match item {
                            MediaItem::File(file)
                                if file.kind == MediaKind::Music
                                    && file.plays_natively()
                                    && accepts(&accepted, file.content_type) =>
                            {
                                Some(file.clone())
                            }
                            _ => None,
                        })
                        .collect();
                    let Some(native) = native else {
                        return play_mirrored_queue(
                            sink,
                            Some(&tv),
                            files,
                            &shared,
                            cancelled,
                            commands,
                            start,
                        )
                        .await;
                    };
                    let server = FileServer::start(renderer.address, port, native).await?;
                    let result = tokio::select! {
                        result = play_upnp_queue(&tv, &server, &shared, files, commands, start) => result,
                        _ = cancelled.changed() => Ok(()),
                    };
                    // Told even on the way out of an error, as the stream is.
                    let _ = avtransport::stop(&tv.av_transport).await;
                    result
                }
                .await;
                shared.finish(outcome);
            },
        )
    }

    fn spawn<F, Fut>(files: Vec<MediaItem>, run: F) -> Result<Self>
    where
        F: FnOnce(
            Vec<MediaItem>,
            Arc<Shared>,
            watch::Receiver<bool>,
            mpsc::Receiver<MediaCommand>,
        ) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        if files.is_empty() {
            return Err(NdError::Protocol("no files to send".into()));
        }
        if files.len() > nd_core::media::MAX_FILES {
            return Err(NdError::Protocol("select up to 1000 files".into()));
        }
        let shared = Arc::new(Shared {
            status: Mutex::new(MediaStatus {
                total: files.len(),
                queue: files
                    .iter()
                    .filter_map(|item| item.file().map(|file| file.path.clone()))
                    .collect(),
                ..Default::default()
            }),
            stopping: AtomicBool::new(false),
            latest: Mutex::default(),
        });
        let (cancel, cancelled) = watch::channel(false);
        let (commands, incoming) = mpsc::channel(32);
        let task = tokio::spawn(run(files, shared.clone(), cancelled, incoming));
        Ok(Self {
            shared,
            _task: task,
            cancel,
            commands,
        })
    }

    pub fn stop(&self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        self.cancel.send_replace(true);
    }

    pub fn command(&self, command: MediaCommand) -> Result<()> {
        let busy = || NdError::Protocol("media control is busy or playback has ended".into());
        let mut latest = self.shared.latest.lock().unwrap_or_else(|e| e.into_inner());
        let Some(slot) = latest.slot(&command) else {
            drop(latest);
            return self.commands.try_send(command).map_err(|_| busy());
        };
        let waiting = slot.is_some();
        *slot = Some(merge(slot.take(), command.clone()));
        if waiting {
            return Ok(());
        }
        self.commands.try_send(command).map_err(|_| {
            *slot = None;
            busy()
        })
    }

    pub fn status(&self) -> MediaStatus {
        self.shared
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn is_finished(&self) -> bool {
        self.status().finished
    }
}

#[allow(clippy::too_many_arguments)]
async fn play_queue(
    channel: &CastChannel,
    app: &LaunchedApp,
    server: Option<&FileServer>,
    shared: &Shared,
    sender_name: &str,
    files: Vec<MediaItem>,
    mut commands: mpsc::Receiver<MediaCommand>,
    mut start: Option<PlaybackStart>,
) -> Result<()> {
    let mut queue = Queue::new(files);
    while let Some((index, file)) = queue.pending.front().cloned() {
        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }
        queue.publish(shared, true);
        let url = match &file {
            MediaItem::Url { uri, .. } => uri.clone(),
            MediaItem::File(_) => server
                .ok_or_else(|| NdError::Protocol("file server unavailable".into()))?
                .url(index),
        };
        let loaded = channel
            .load_item(app, &url, &file, sender_name, start)
            .await
            .map_err(|e| {
                NdError::Protocol(format!("the receiver refused {}: {e}", file.title()))
            })?;
        let session_id = loaded
            .pointer("/status/0/mediaSessionId")
            .and_then(Value::as_i64)
            .ok_or_else(|| NdError::Protocol("LOAD returned no media session".into()))?;
        let photo = file.kind() == MediaKind::Photo;
        if let Some(entry) = active_status(&loaded, session_id, &url) {
            shared.update(|status| update_playback(&mut status.playback, entry, photo));
        }
        let mut poll = tokio::time::interval(Duration::from_secs(1));
        let mut last_status = tokio::time::Instant::now();
        let photo_end = last_status + Duration::from_secs(PHOTO_SECONDS);
        loop {
            let payload = tokio::select! {
                command = next_command(&mut commands, shared) => {
                    let Some(command) = command else { return Ok(()); };
                    if queue.edit(&command, shared) { break; }
                    if matches!(command, MediaCommand::Remove(_)) { continue; }
                    let playback = shared.status.lock().unwrap_or_else(|e| e.into_inner()).playback.clone();
                    let Some(request) = control_request(&command, session_id, &playback) else {
                        shared.update(|status| status.control_error = Some("playback control is not available for this item".into()));
                        continue;
                    };
                    match channel.request(NS_MEDIA, &app.transport_id, request).await {
                        Ok(reply) if active_status(&reply, session_id, &url).is_some() => {
                            shared.update(|status| status.control_error = None);
                            reply
                        }
                        result => {
                            let error = match result { Ok(_) => "receiver rejected the playback control".to_string(), Err(err) => err.to_string() };
                            shared.update(|status| status.control_error = Some(error));
                            continue;
                        }
                    }
                }
                _ = tokio::time::sleep_until(photo_end), if photo => { queue.advance(); break; }
                _ = poll.tick() => channel.request(NS_MEDIA, &app.transport_id,
                    json!({"type": "GET_STATUS", "mediaSessionId": session_id})).await?,
                event = channel.next_event() => {
                    let event = event.ok_or_else(|| NdError::Protocol("media channel closed".into()))?;
                    if event.namespace != NS_MEDIA { continue; }
                    event.payload
                }
                _ = tokio::time::sleep_until(last_status + SILENCE_TIMEOUT) => {
                    return Err(NdError::Protocol("media receiver stopped reporting status".into()));
                }
            };
            let Some(entry) = active_status(&payload, session_id, &url) else {
                continue;
            };
            last_status = tokio::time::Instant::now();
            shared.update(|status| update_playback(&mut status.playback, entry, photo));
            if !photo && item_has_ended(&json!({"type": "MEDIA_STATUS", "status": [entry]})) {
                if entry.get("idleReason").and_then(Value::as_str) != Some("FINISHED") {
                    return Err(NdError::Protocol(
                        "receiver cancelled or failed to play the item".into(),
                    ));
                }
                queue.advance();
                break;
            }
        }
        shared.next_start(&mut start);
    }
    Ok(())
}

fn update_playback(playback: &mut PlaybackState, entry: &Value, photo: bool) {
    if let Some(seconds) = entry
        .get("currentTime")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v >= 0.0)
    {
        playback.seconds = seconds;
    }
    if let Some(duration) = entry
        .pointer("/media/duration")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v > 0.0)
    {
        playback.duration = Some(duration);
    }
    if let Some(state) = entry.get("playerState").and_then(Value::as_str) {
        playback.paused = state == "PAUSED";
    }
    if let Some(flags) = entry.get("supportedMediaCommands").and_then(Value::as_u64) {
        playback.can_pause = !photo && flags & 1 != 0;
        playback.can_seek = !photo && flags & 2 != 0;
        playback.volume = if !photo && flags & 4 != 0 {
            entry
                .pointer("/volume/level")
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
                .or(playback.volume)
        } else {
            None
        };
        playback.muted = if !photo && flags & 8 != 0 {
            entry
                .pointer("/volume/muted")
                .and_then(Value::as_bool)
                .or(playback.muted)
        } else {
            None
        };
    } else {
        if playback.volume.is_some() {
            playback.volume = entry
                .pointer("/volume/level")
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
                .or(playback.volume);
        }
        if playback.muted.is_some() {
            playback.muted = entry
                .pointer("/volume/muted")
                .and_then(Value::as_bool)
                .or(playback.muted);
        }
    }
}

fn control_request(
    command: &MediaCommand,
    session_id: i64,
    state: &PlaybackState,
) -> Option<Value> {
    match command {
        MediaCommand::TogglePause if state.can_pause => Some(
            json!({"type": if state.paused {"PLAY"} else {"PAUSE"}, "mediaSessionId": session_id}),
        ),
        MediaCommand::SetPaused(paused) if state.can_pause => {
            Some(json!({"type": if *paused {"PAUSE"} else {"PLAY"}, "mediaSessionId": session_id}))
        }
        MediaCommand::SeekTo(seconds) if state.can_seek => {
            Some(json!({"type": "SEEK", "mediaSessionId": session_id,
            "currentTime": seek_target(0.0, *seconds, state.duration)?}))
        }
        MediaCommand::SetVolume(level)
            if state.volume.is_some() && level.is_finite() && (0.0..=1.0).contains(level) =>
        {
            Some(
                json!({"type": "SET_VOLUME", "mediaSessionId": session_id, "volume": {"level": level}}),
            )
        }
        MediaCommand::SetMute(muted) if state.muted.is_some() => Some(
            json!({"type": "SET_VOLUME", "mediaSessionId": session_id, "volume": {"muted": muted}}),
        ),
        MediaCommand::SeekRelative(offset) if state.can_seek => {
            Some(json!({"type": "SEEK", "mediaSessionId": session_id,
            "currentTime": seek_target(state.seconds, *offset, state.duration)?}))
        }
        _ => None,
    }
}

/// `tv`, for a DLNA television: its own volume is the one the player's
/// volume control moves and shows, not the level of our stream.
async fn play_mirrored_queue(
    sink: Arc<dyn Sink>,
    tv: Option<&Renderer>,
    files: Vec<MediaItem>,
    shared: &Shared,
    mut cancelled: watch::Receiver<bool>,
    mut commands: mpsc::Receiver<MediaCommand>,
    mut start: Option<PlaybackStart>,
) -> Result<()> {
    let mut queue = Queue::new(files);
    'items: while let Some((_, file)) = queue.pending.front().cloned() {
        if *cancelled.borrow() {
            break;
        }
        queue.publish(shared, true);
        // DLNA renderers can fetch inside SetAVTransportURI and wait for data
        // before accepting Pause. Defer that handshake until Play instead of
        // briefly playing an initially paused item to make it finish.
        while let Some(initial) = start.filter(|initial| initial.paused) {
            shared.update(|status| {
                status.playback = PlaybackState {
                    paused: true,
                    seconds: initial.seconds,
                    can_pause: true,
                    volume: Some(initial.volume),
                    muted: Some(initial.muted),
                    ..Default::default()
                }
            });
            let command = tokio::select! {
                _ = cancelled.changed() => return Ok(()),
                command = next_command(&mut commands, shared) => command,
            };
            let Some(command) = command else {
                return Ok(());
            };
            if queue.edit(&command, shared) {
                shared.next_start(&mut start);
                continue 'items;
            }
            let mut initial = initial;
            let supported = match command {
                MediaCommand::TogglePause => {
                    initial.paused = false;
                    true
                }
                MediaCommand::SetPaused(paused) => {
                    initial.paused = paused;
                    true
                }
                MediaCommand::SetVolume(volume)
                    if volume.is_finite() && (0.0..=1.0).contains(&volume) =>
                {
                    initial.volume = volume;
                    true
                }
                MediaCommand::SetMute(muted) => {
                    initial.muted = muted;
                    true
                }
                MediaCommand::Remove(_) => true,
                _ => false,
            };
            start = Some(initial);
            shared.update(|status| {
                status.control_error = (!supported)
                    .then(|| "playback control is not available before starting this item".into())
            });
        }
        let control = FilePlaybackControl::default();
        let photo = file.kind() == MediaKind::Photo;
        let frame = {
            let source = file.source();
            let height = start.map_or(0, |start| start.height);
            tokio::task::spawn_blocking(move || nd_core::media::tv_frame(&source, height))
                .await
                .ok()
                .flatten()
                .unwrap_or((1920, 1080))
        };
        let source = CaptureSource::media_file(
            MediaPlayback {
                source: file.source(),
                kind: file.kind(),
                title: file.title(),
                control: Some(control.clone()),
                start,
            },
            frame,
        );
        let playing = sink.start_stream(source);
        tokio::pin!(playing);
        let mut poll = tokio::time::interval(Duration::from_millis(250));
        let mut photo_started = None;
        let mut tv_volume = match tv {
            Some(tv) => tv.volume().await,
            None => None,
        };
        let mut polls = 0_u32;
        loop {
            tokio::select! {
                biased;
                result = &mut playing => { result?; queue.advance(); break; }
                _ = cancelled.changed() => {
                    let _ = sink.stop_stream().await;
                    playing.await?;
                    return Ok(());
                }
                command = next_command(&mut commands, shared) => {
                    let Some(command) = command else {
                        let _ = sink.stop_stream().await;
                        playing.await?;
                        return Ok(());
                    };
                    if queue.edit(&command, shared) {
                        let _ = sink.stop_stream().await;
                        playing.await?;
                        break;
                    }
                    if matches!(command, MediaCommand::Remove(_)) { continue; }
                    if let Some(tv) = tv {
                        let changed = match command {
                            MediaCommand::SetVolume(level) if level.is_finite() && (0.0..=1.0).contains(&level) => {
                                Some(tv.set_volume(level).await.map(|()| (level, tv_volume.is_some_and(|(_, muted)| muted))))
                            }
                            MediaCommand::SetMute(muted) => Some(
                                tv.set_mute(muted).await.map(|()| (tv_volume.map_or(1.0, |(level, _)| level), muted)),
                            ),
                            _ => None,
                        };
                        if let Some(changed) = changed {
                            if let Ok(volume) = &changed {
                                tv_volume = Some(*volume);
                            }
                            shared.update(|status| status.control_error = changed.err().map(|err| err.to_string()));
                            continue;
                        }
                    }
                    // A jump inside a stream that is already live sends the
                    // receiver timestamps that run backwards or leap: a
                    // Panasonic reconnects and then drops it. Start the item
                    // again from there instead, as a start already works.
                    if let Some(restart) = seek_restart(&command, &control.state(), start) {
                        let _ = sink.stop_stream().await;
                        playing.await?;
                        start = Some(restart);
                        continue 'items;
                    }
                    let control = control.clone();
                    let outcome = tokio::task::spawn_blocking(move || control.command(&command)).await;
                    shared.update(|status| status.control_error = match outcome {
                        Ok(Ok(())) => None,
                        Ok(Err(err)) => Some(err.to_string()),
                        Err(err) => Some(err.to_string()),
                    });
                }
                _ = poll.tick() => {
                    let mut playback = control.state();
                    // Read back every few seconds: the remote moves it too.
                    polls += 1;
                    if let Some(tv) = tv.filter(|_| polls.is_multiple_of(12)) {
                        tv_volume = tv.volume().await.or(tv_volume);
                    }
                    if let Some((volume, muted)) = tv_volume {
                        playback.volume = Some(volume);
                        playback.muted = Some(muted);
                    }
                    if photo {
                        if playback.can_pause { photo_started.get_or_insert_with(tokio::time::Instant::now); }
                        playback.can_pause = false;
                        playback.can_seek = false;
                        if photo_started.is_some_and(|start| start.elapsed() >= Duration::from_secs(PHOTO_SECONDS)) {
                            let _ = sink.stop_stream().await;
                            playing.await?;
                            queue.advance();
                            break;
                        }
                    }
                    shared.update(|status| status.playback = playback);
                }
            }
        }
        shared.next_start(&mut start);
    }
    Ok(())
}
/// How long a television gets to start playing an item it was handed.
const UPNP_START_TIMEOUT: Duration = Duration::from_secs(30);

/// A queue the renderer plays itself: each file is handed over as it is, and
/// pause, seek, volume and the position belong to the renderer.
async fn play_upnp_queue(
    tv: &Renderer,
    server: &FileServer,
    shared: &Shared,
    files: Vec<MediaItem>,
    mut commands: mpsc::Receiver<MediaCommand>,
    mut start: Option<PlaybackStart>,
) -> Result<()> {
    let control = &tv.av_transport;
    let mut queue = Queue::new(files);
    while let Some((index, item)) = queue.pending.front().cloned() {
        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }
        queue.publish(shared, true);
        let MediaItem::File(file) = &item else {
            queue.advance();
            continue;
        };
        let duration = {
            let source = MediaSource::File(file.path.clone());
            tokio::task::spawn_blocking(move || nd_core::media::duration(&source))
                .await
                .ok()
                .flatten()
        };
        // A renderer holding something else may refuse a new URI.
        let _ = avtransport::stop(control).await;
        avtransport::set_file(
            control,
            &server.url(index),
            &file.title(),
            "object.item.audioItem.musicTrack",
            file.content_type,
            file.size,
            duration,
        )
        .await
        .map_err(|e| NdError::Protocol(format!("the TV refused {}: {e}", file.title())))?;
        avtransport::play(control).await?;

        let mut playback = PlaybackState {
            can_pause: true,
            can_seek: true,
            duration,
            ..Default::default()
        };
        // Since when, and from where, playback has been running unreported.
        let mut clock: Option<(tokio::time::Instant, f64)> = None;
        let mut polls = 0_u32;
        // The TV's own volume, reported and left as it is. The player's level
        // is the level of its own sound, not of the television: applied here
        // as the set's master volume it sent a Panasonic to full volume.
        if let Some((volume, muted)) = tv.volume().await {
            playback.volume = Some(volume);
            playback.muted = Some(muted);
        }
        // Where to start and whether to hold, once the renderer is playing:
        // before that it has nothing to seek in.
        let mut pending_seek = start.map(|s| s.seconds).filter(|s| *s > 0.0);
        let mut pending_pause = start.is_some_and(|s| s.paused);
        let handed_over = tokio::time::Instant::now();
        let mut ever_played = false;
        let mut idle_polls = 0;
        let mut poll = tokio::time::interval(Duration::from_secs(1));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                command = next_command(&mut commands, shared) => {
                    let Some(command) = command else { return Ok(()); };
                    if queue.edit(&command, shared) { break; }
                    if matches!(command, MediaCommand::Remove(_)) { continue; }
                    let result = upnp_command(tv, &command, &mut playback).await;
                    // A pause or a seek moves the point the clock counts from.
                    if !matches!(command, MediaCommand::SetVolume(_) | MediaCommand::SetMute(_)) {
                        clock = None;
                    }
                    shared.update(|status| {
                        status.control_error = result.err().map(|err| err.to_string());
                        status.playback = playback.clone();
                    });
                }
                _ = poll.tick() => {
                    polls += 1;
                    if polls.is_multiple_of(3)
                        && let Some((volume, muted)) = tv.volume().await {
                            playback.volume = Some(volume);
                            playback.muted = Some(muted);
                        }
                    let state = avtransport::transport_state(control).await;
                    match state {
                        Ok(TransportState::Playing | TransportState::Paused) => {
                            ever_played = true;
                            idle_polls = 0;
                            playback.paused = matches!(state, Ok(TransportState::Paused));
                            if let Some(seconds) = pending_seek.take()
                                && avtransport::seek(control, seconds).await.is_ok() {
                                    playback.seconds = seconds;
                                    clock = None;
                                }
                            if std::mem::take(&mut pending_pause)
                                && avtransport::pause(control).await.is_ok()
                            {
                                playback.paused = true;
                            }
                            let reported = avtransport::position(control).await.ok();
                            if let Some((_, Some(duration))) = reported {
                                playback.duration = Some(duration);
                            }
                            match reported.map(|(seconds, _)| seconds).filter(|s| *s > 0.0) {
                                Some(seconds) => {
                                    playback.seconds = seconds;
                                    clock = None;
                                }
                                None if playback.paused => clock = None,
                                // Some renderers play without ever saying where
                                // (a Panasonic answers 0:00:00 throughout): count.
                                None => {
                                    let (since, from) = *clock
                                        .get_or_insert((tokio::time::Instant::now(), playback.seconds));
                                    let seconds = from + since.elapsed().as_secs_f64();
                                    playback.seconds = playback
                                        .duration
                                        .map_or(seconds, |duration| seconds.min(duration));
                                }
                            }
                        }
                        Ok(TransportState::Transitioning) => idle_polls = 0,
                        Ok(TransportState::Idle) | Err(_) if ever_played => {
                            idle_polls += 1;
                            if idle_polls >= 2 {
                                // At the end it moves on; before it, the viewer
                                // stopped it on the TV and the session is over.
                                let at_end = playback
                                    .duration
                                    .is_some_and(|duration| playback.seconds >= duration - 5.0);
                                if !at_end {
                                    return Ok(());
                                }
                                queue.advance();
                                break;
                            }
                        }
                        _ if handed_over.elapsed() > UPNP_START_TIMEOUT => {
                            return Err(NdError::Protocol(format!(
                                "the TV did not start playing {}",
                                file.title()
                            )));
                        }
                        _ => {}
                    }
                    shared.update(|status| status.playback = playback.clone());
                }
            }
        }
        shared.next_start(&mut start);
    }
    Ok(())
}

/// One player command, carried out by the renderer.
async fn upnp_command(
    tv: &Renderer,
    command: &MediaCommand,
    playback: &mut PlaybackState,
) -> Result<()> {
    let control = &tv.av_transport;
    match *command {
        MediaCommand::TogglePause | MediaCommand::SetPaused(_) => {
            let pause = match *command {
                MediaCommand::SetPaused(pause) => pause,
                _ => !playback.paused,
            };
            if pause {
                avtransport::pause(control).await?;
            } else {
                avtransport::play(control).await?;
            }
            playback.paused = pause;
        }
        MediaCommand::SeekTo(_) | MediaCommand::SeekRelative(_) => {
            let target = match *command {
                MediaCommand::SeekTo(seconds) => seek_target(0.0, seconds, playback.duration),
                MediaCommand::SeekRelative(offset) => {
                    seek_target(playback.seconds, offset, playback.duration)
                }
                _ => None,
            }
            .ok_or_else(|| NdError::Unsupported("invalid seek position".into()))?;
            avtransport::seek(control, target).await?;
            playback.seconds = target;
        }
        MediaCommand::SetVolume(level) if level.is_finite() && (0.0..=1.0).contains(&level) => {
            tv.set_volume(level).await?;
            playback.volume = Some(level);
        }
        MediaCommand::SetMute(muted) => {
            tv.set_mute(muted).await?;
            playback.muted = Some(muted);
        }
        _ => {
            return Err(NdError::Unsupported(
                "playback control is not available for this item".into(),
            ));
        }
    }
    Ok(())
}

/// Where a seek on a decoded item restarts it, keeping its pause, volume and
/// frame. None for any other command, or a seek the item cannot take.
fn seek_restart(
    command: &MediaCommand,
    state: &PlaybackState,
    start: Option<PlaybackStart>,
) -> Option<PlaybackStart> {
    if !state.can_seek {
        return None;
    }
    let seconds = match command {
        MediaCommand::SeekTo(seconds) => seek_target(0.0, *seconds, state.duration),
        MediaCommand::SeekRelative(offset) => seek_target(state.seconds, *offset, state.duration),
        _ => return None,
    }?;
    Some(PlaybackStart {
        seconds,
        paused: state.paused,
        volume: state
            .volume
            .or(start.map(|start| start.volume))
            .unwrap_or(1.0),
        muted: state
            .muted
            .or(start.map(|start| start.muted))
            .unwrap_or(false),
        height: start.map_or(0, |start| start.height),
    })
}

pub(crate) fn active_status<'a>(
    payload: &'a Value,
    session_id: i64,
    url: &str,
) -> Option<&'a Value> {
    if payload.get("type").and_then(Value::as_str) != Some("MEDIA_STATUS") {
        return None;
    }
    payload.get("status")?.as_array()?.iter().find(|entry| {
        entry.get("mediaSessionId").and_then(Value::as_i64) == Some(session_id)
            && !entry
                .pointer("/media/contentId")
                .and_then(Value::as_str)
                .is_some_and(|id| id != url)
    })
}

/// Does this `MEDIA_STATUS` say the item has ended?
///
/// The receiver reports the end as `playerState: IDLE` with an `idleReason`.
/// `IDLE` on its own is not enough — it is also what is reported *before* the
/// first item starts, and treating that as "finished" would skip straight past
/// the file the person asked for.
fn item_has_ended(payload: &Value) -> bool {
    if payload.get("type").and_then(Value::as_str) != Some("MEDIA_STATUS") {
        return false;
    }
    let Some(entries) = payload.get("status").and_then(Value::as_array) else {
        return false;
    };
    entries.iter().any(|entry| {
        let idle = entry.get("playerState").and_then(Value::as_str) == Some("IDLE");
        let reason = entry.get("idleReason").and_then(Value::as_str);
        // `FINISHED`: played to the end. `ERROR`/`CANCELLED`: it will not play,
        // and waiting longer will not change that.
        idle && matches!(reason, Some("FINISHED") | Some("ERROR") | Some("CANCELLED"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_dragged_slider_is_one_seek_not_a_full_queue() {
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let (seen_tx, seen) = tokio::sync::oneshot::channel();
        let file = MediaItem::Url {
            uri: "http://example.invalid/a.mp4".into(),
            content_type: "video/mp4".into(),
            title: "a".into(),
            kind: MediaKind::Video,
        };
        let session =
            MediaSession::spawn(vec![file], move |_, shared, _, mut commands| async move {
                // Busy restarting the item while the slider moves.
                let _ = released.await;
                let mut seen = Vec::new();
                while let Ok(Some(command)) = tokio::time::timeout(
                    Duration::from_millis(50),
                    next_command(&mut commands, &shared),
                )
                .await
                {
                    seen.push(command);
                }
                let _ = seen_tx.send(seen);
            })
            .unwrap();
        for step in 0..200 {
            session
                .command(MediaCommand::SeekTo(f64::from(step)))
                .unwrap();
            session
                .command(MediaCommand::SetVolume(f64::from(step) / 200.0))
                .unwrap();
        }
        session.command(MediaCommand::SeekRelative(-9.0)).unwrap();
        session.command(MediaCommand::TogglePause).unwrap();
        release.send(()).unwrap();
        assert_eq!(
            seen.await.unwrap(),
            [
                MediaCommand::SeekTo(190.0),
                MediaCommand::SetVolume(199.0 / 200.0),
                MediaCommand::TogglePause,
            ]
        );
    }

    #[test]
    fn a_seek_restarts_the_item_where_it_was_asked() {
        let state = PlaybackState {
            paused: true,
            seconds: 40.0,
            duration: Some(100.0),
            can_pause: true,
            can_seek: true,
            volume: Some(0.3),
            muted: Some(true),
        };
        let start = Some(PlaybackStart {
            seconds: 0.0,
            paused: false,
            volume: 1.0,
            muted: false,
            height: 720,
        });
        let to = seek_restart(&MediaCommand::SeekTo(90.0), &state, start).unwrap();
        assert_eq!(
            (to.seconds, to.paused, to.volume, to.muted, to.height),
            (90.0, true, 0.3, true, 720)
        );
        let back = seek_restart(&MediaCommand::SeekRelative(-50.0), &state, start).unwrap();
        assert_eq!(back.seconds, 0.0);
        // Past the end is the end; not a seek, or not seekable, is no restart.
        assert_eq!(
            seek_restart(&MediaCommand::SeekTo(500.0), &state, start)
                .unwrap()
                .seconds,
            100.0
        );
        assert!(seek_restart(&MediaCommand::SetPaused(true), &state, start).is_none());
        let fixed = PlaybackState {
            can_seek: false,
            ..state
        };
        assert!(seek_restart(&MediaCommand::SeekTo(10.0), &fixed, start).is_none());
    }
    use serde_json::json;

    fn fixture(name: &str) -> MediaItem {
        MediaItem::File(MediaFile {
            path: name.into(),
            kind: MediaKind::Video,
            content_type: "video/mp4",
            size: 1,
        })
    }

    #[test]
    fn stop_failure_is_not_reported_as_success_after_user_cancellation() {
        let shared = Shared {
            status: Mutex::new(MediaStatus::default()),
            stopping: AtomicBool::new(true),
            latest: Mutex::default(),
        };
        shared.finish(Err(NdError::Protocol("STOP unconfirmed".into())));
        let status = shared.status.lock().unwrap();
        assert!(status.finished);
        assert!(status.error.as_ref().unwrap().contains("STOP unconfirmed"));
    }

    #[test]
    fn remote_items_preserve_file_indexes_without_publishing_url_credentials() {
        let remote = MediaItem::url(
            "https://example.invalid/radio?token=private".into(),
            "audio/mpeg".into(),
            "Radio".into(),
            true,
        )
        .unwrap();
        assert!(!format!("{remote:?}").contains("private"));
        assert!(!format!("{:?}", remote.source()).contains("private"));
        let mut queue = Queue::new(vec![fixture("a.mp4"), remote, fixture("b.mp4")]);
        let shared = Shared {
            status: Mutex::new(MediaStatus::default()),
            stopping: AtomicBool::new(false),
            latest: Mutex::default(),
        };
        assert_eq!(queue.pending.back().unwrap().0, 1);
        queue.advance();
        queue.publish(&shared, true);
        let status = shared.status.lock().unwrap();
        assert_eq!(status.title, "Radio");
        assert_eq!(status.queue, [PathBuf::from("b.mp4")]);
        assert_eq!(status.total, 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_http_audio_source_decodes_and_obeys_playback_controls() {
        use gstreamer::{self as gst, prelude::*};
        use nd_core::pipeline::{self, AudioSource, PipelineGuard, VideoSource};
        pipeline::init().unwrap();
        let path = std::env::temp_dir().join(format!("bns-url-audio-{}.wav", std::process::id()));
        let fixture = gst::parse::launch(
            "audiotestsrc num-buffers=240 ! audio/x-raw,rate=48000 ! wavenc ! filesink name=output",
        )
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let guard = PipelineGuard::new(fixture.clone());
        fixture
            .by_name("output")
            .unwrap()
            .set_property("location", path.to_str().unwrap());
        fixture.set_state(gst::State::Playing).unwrap();
        let message = fixture
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(5),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .unwrap();
        assert_eq!(message.type_(), gst::MessageType::Eos, "{message:?}");
        drop(guard);
        let server = FileServer::start(
            "127.0.0.1".parse().unwrap(),
            0,
            vec![MediaFile::inspect(&path).unwrap()],
        )
        .await
        .unwrap();
        let item =
            MediaItem::url(server.url(0), "audio/wav".into(), "Test audio".into(), true).unwrap();
        tokio::task::spawn_blocking(move || {
            let source = VideoSource::Media { source: item.source(), kind: item.kind(), title: item.title() };
            let description = format!("{} ! fakesink sync=false {} ! audioconvert ! audio/x-raw,format=F32LE,channels=1,rate=48000 ! appsink name=samples sync=false max-buffers=1 drop=true", source.description(), AudioSource::MediaFile.description());
            let (pipeline, _) = pipeline::build_pipeline(&description, 0).unwrap();
            let _guard = PipelineGuard::new(pipeline.clone());
            let control = FilePlaybackControl::default();
            control.attach(&pipeline);
            control.prepare(nd_core::media::PlaybackStart {
                seconds: 1.0, paused: false, volume: 0.25, muted: true, height: 0,
            }).unwrap();
            pipeline.set_state(gst::State::Playing).unwrap();
            let sink = pipeline.by_name("samples").unwrap();
            let rms = || {
                let sample = sink.emit_by_name::<Option<gst::Sample>>("try-pull-sample", &[&gst::ClockTime::from_seconds(2)]).expect("decoded HTTP audio");
                let data = sample.buffer().unwrap().map_readable().unwrap();
                let samples = data.as_slice().as_chunks::<4>().0;
                (samples.iter().map(|b| f64::from(f32::from_le_bytes(*b)).powi(2)).sum::<f64>() / samples.len() as f64).sqrt()
            };
            for _ in 0..8 { assert!(rms() < 0.000_001, "initial mute must precede decoded samples"); }
            assert_eq!(control.state().muted, Some(true));
            assert!((control.state().volume.unwrap() - 0.25).abs() < 0.000_001);
            assert!(control.state().seconds >= 0.9);
            control.command(&MediaCommand::SetMute(false)).unwrap();
            control.command(&MediaCommand::SetVolume(1.0)).unwrap();
            let mut audible = false;
            for _ in 0..80 { if rms() > 0.1 { audible = true; break; } }
            assert!(audible, "URL decoder must supply the audio branch");
            control.command(&MediaCommand::SetMute(true)).unwrap();
            for _ in 0..8 { rms(); }
            assert!(rms() < 0.000_001);
            control.command(&MediaCommand::SetMute(false)).unwrap();
            let mut restored = false;
            for _ in 0..40 { if rms() > 0.1 { restored = true; break; } }
            assert!(restored, "unmute must restore decoded HTTP audio");
            assert!(control.state().can_seek);
            control.command(&MediaCommand::SetPaused(true)).unwrap();
            pipeline.state(gst::ClockTime::from_seconds(2)).0.unwrap();
            assert!(control.state().paused);
            control.command(&MediaCommand::SeekTo(1.0)).unwrap();
            pipeline.state(gst::ClockTime::from_seconds(2)).0.unwrap();
            control.command(&MediaCommand::SetPaused(false)).unwrap();
            let mut resumed = false;
            for _ in 0..40 { if rms() > 0.1 { resumed = true; break; } }
            assert!(resumed, "seek and resume must preserve HTTP audio");
        }).await.unwrap();
        drop(server);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn removing_files_keeps_http_indexes_and_current_item_consistent() {
        let shared = Shared {
            status: Mutex::new(MediaStatus::default()),
            stopping: AtomicBool::new(false),
            latest: Mutex::default(),
        };
        let mut queue = Queue::new(vec![fixture("a.mp4"), fixture("b.mp4"), fixture("c.mp4")]);
        assert!(!queue.edit(&MediaCommand::Remove("b.mp4".into()), &shared));
        assert_eq!(
            queue
                .pending
                .iter()
                .map(|(index, _)| *index)
                .collect::<Vec<_>>(),
            vec![0, 2]
        );
        assert!(queue.edit(&MediaCommand::Remove("a.mp4".into()), &shared));
        assert_eq!(queue.pending.front().unwrap().0, 2);
        assert!(queue.edit(&MediaCommand::Next, &shared));
        assert!(queue.pending.is_empty());
    }

    #[test]
    fn receiver_updates_preserve_duration_and_use_capabilities() {
        let mut playback = PlaybackState::default();
        update_playback(
            &mut playback,
            &json!({"currentTime": 8.0, "media": {"duration": 50.0}, "playerState": "PLAYING", "supportedMediaCommands": 3}),
            false,
        );
        assert!(playback.can_seek && playback.can_pause);
        update_playback(
            &mut playback,
            &json!({"currentTime": 9.0, "playerState": "PAUSED"}),
            false,
        );
        assert_eq!(playback.duration, Some(50.0));
        assert!(playback.paused);
        let request = control_request(&MediaCommand::TogglePause, 7, &playback).unwrap();
        assert_eq!(request, json!({"type": "PLAY", "mediaSessionId": 7}));
        let request = control_request(&MediaCommand::SeekRelative(-10.0), 7, &playback).unwrap();
        assert_eq!(request["currentTime"], 0.0);
        assert!(
            request.get("resumeState").is_none(),
            "seeking preserves pause"
        );
        update_playback(&mut playback, &json!({"supportedMediaCommands": 0}), false);
        assert!(control_request(&MediaCommand::TogglePause, 7, &playback).is_none());
    }

    #[test]
    fn explicit_controls_target_only_the_owned_media_and_validate_levels() {
        let mut state = PlaybackState::default();
        update_playback(
            &mut state,
            &json!({"supportedMediaCommands": 15,
            "volume": {"level": 0.5, "muted": false}, "media": {"duration": 60.0}}),
            false,
        );
        for (command, expected) in [
            (
                MediaCommand::SetPaused(true),
                json!({"type":"PAUSE", "mediaSessionId":7}),
            ),
            (
                MediaCommand::SetPaused(false),
                json!({"type":"PLAY", "mediaSessionId":7}),
            ),
            (
                MediaCommand::SeekTo(90.0),
                json!({"type":"SEEK", "mediaSessionId":7, "currentTime":60.0}),
            ),
            (
                MediaCommand::SetVolume(0.37),
                json!({"type":"SET_VOLUME", "mediaSessionId":7, "volume":{"level":0.37}}),
            ),
            (
                MediaCommand::SetMute(true),
                json!({"type":"SET_VOLUME", "mediaSessionId":7, "volume":{"muted":true}}),
            ),
        ] {
            assert_eq!(control_request(&command, 7, &state), Some(expected));
        }
        for level in [f64::NAN, f64::INFINITY, -0.1, 1.01] {
            assert!(control_request(&MediaCommand::SetVolume(level), 7, &state).is_none());
        }
        update_playback(
            &mut state,
            &json!({"volume":{"level":0.83, "muted":true}}),
            false,
        );
        assert_eq!(state.volume, Some(0.83));
        assert_eq!(state.muted, Some(true));
        update_playback(&mut state, &json!({"supportedMediaCommands":0}), false);
        assert!(control_request(&MediaCommand::SetVolume(0.5), 7, &state).is_none());
        assert!(control_request(&MediaCommand::SetMute(false), 7, &state).is_none());
    }

    struct QueueSink {
        started: Mutex<Vec<PathBuf>>,
        initial: Mutex<Vec<Option<PlaybackStart>>>,
        cancel: watch::Sender<bool>,
        ready: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl Sink for QueueSink {
        fn info(&self) -> nd_core::sink::SinkInfo {
            nd_core::sink::SinkInfo {
                id: "test".into(),
                display_name: "test".into(),
                kind: nd_core::sink::SinkKind::WfdP2p,
                address: None,
            }
        }
        fn state(&self) -> nd_core::sink::SinkState {
            nd_core::sink::SinkState::Streaming
        }
        async fn start_stream(&self, source: CaptureSource) -> Result<()> {
            self.cancel.send_replace(false);
            let mut cancelled = self.cancel.subscribe();
            let media = source.media.unwrap();
            self.initial.lock().unwrap().push(media.start);
            let MediaSource::File(path) = media.source else {
                panic!("expected a local test file");
            };
            self.started.lock().unwrap().push(path);
            self.ready.notify_one();
            cancelled.changed().await.unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
            Ok(())
        }
        async fn stop_stream(&self) -> Result<()> {
            self.cancel.send_replace(true);
            Ok(())
        }
    }

    /// Reads one HTTP request: its head and its body.
    async fn read_request(socket: &mut tokio::net::TcpStream) -> (String, String) {
        use tokio::io::AsyncReadExt;
        let mut data = Vec::new();
        let mut chunk = [0; 4096];
        let head_end = loop {
            let read = socket.read(&mut chunk).await.unwrap();
            assert!(read > 0, "request ended early");
            data.extend_from_slice(&chunk[..read]);
            if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let head = String::from_utf8_lossy(&data[..head_end]).to_string();
        let length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        while data.len() < head_end + length {
            let read = socket.read(&mut chunk).await.unwrap();
            data.extend_from_slice(&chunk[..read]);
        }
        let body = String::from_utf8_lossy(&data[head_end..head_end + length]).to_string();
        (head, body)
    }

    #[tokio::test]
    async fn a_dlna_tv_is_handed_music_as_music() {
        use tokio::io::AsyncWriteExt;
        let dir = std::env::temp_dir().join(format!("nd-upnp-music-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let song = dir.join("song.wav");
        std::fs::write(&song, vec![0u8; 4096]).unwrap();
        let file = MediaFile::inspect(&song).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let renderer = UpnpRenderer {
            address: "127.0.0.1".parse().unwrap(),
            av_transport: format!("{base}/avt"),
            rendering_control: Some(format!("{base}/rc")),
            connection_manager: Some(format!("{base}/cm")),
        };
        // Would be used by the decoding fallback, which must not happen here.
        let sink = Arc::new(QueueSink {
            started: Mutex::new(Vec::new()),
            initial: Mutex::new(Vec::new()),
            cancel: watch::channel(false).0,
            ready: tokio::sync::Notify::new(),
        });
        // What a player sends: its own level, full. The TV must not follow it.
        let start = PlaybackStart {
            seconds: 0.0,
            paused: false,
            volume: 1.0,
            muted: false,
            height: 0,
        };
        let session = MediaSession::start_upnp(
            sink.clone(),
            renderer,
            vec![MediaItem::File(file)],
            0,
            Some(start),
        )
        .unwrap();

        let tv = async {
            let mut state = "NO_MEDIA_PRESENT";
            let mut metadata = String::new();
            let mut served = String::new();
            let mut polls = 0;
            let mut actions = Vec::new();
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (head, body) = read_request(&mut socket).await;
                let action = head
                    .lines()
                    .find_map(|line| {
                        line.split_once('#')
                            .map(|(_, action)| action.trim_end_matches('"').to_string())
                    })
                    .unwrap_or_default();
                actions.push(action.clone());
                let reply = match action.as_str() {
                    "GetProtocolInfo" => {
                        "<Sink>http-get:*:audio/wav:*,http-get:*:video/mpeg:*</Sink>".to_string()
                    }
                    "SetAVTransportURI" => {
                        metadata = nd_dlna::upnp::tag_text(&body, "CurrentURIMetaData")
                            .unwrap()
                            .replace("&lt;", "<")
                            .replace("&gt;", ">")
                            .replace("&quot;", "\"")
                            .replace("&amp;", "&");
                        // Fetch the item as a television would.
                        let url = nd_dlna::upnp::tag_text(&body, "CurrentURI")
                            .unwrap()
                            .to_string();
                        let endpoint = nd_dlna::upnp::Endpoint::parse(&url).unwrap();
                        let mut client =
                            tokio::net::TcpStream::connect(endpoint.addr).await.unwrap();
                        client
                            .write_all(format!("GET {} HTTP/1.1\r\nHost: {}\r\ngetcontentFeatures.dlna.org: 1\r\n\r\n", endpoint.path, endpoint.authority).as_bytes())
                            .await
                            .unwrap();
                        served = read_request(&mut client).await.0;
                        String::new()
                    }
                    "Play" => {
                        state = "PLAYING";
                        String::new()
                    }
                    "GetTransportInfo" => {
                        polls += 1;
                        format!("<CurrentTransportState>{state}</CurrentTransportState>")
                    }
                    "GetPositionInfo" => {
                        "<RelTime>0:00:01</RelTime><TrackDuration>0:03:00</TrackDuration>"
                            .to_string()
                    }
                    "GetVolume" => "<CurrentVolume>40</CurrentVolume>".to_string(),
                    "GetMute" => "<CurrentMute>0</CurrentMute>".to_string(),
                    "Stop" if !metadata.is_empty() => break,
                    _ => String::new(),
                };
                let body = format!("<s:Envelope><s:Body>{reply}</s:Body></s:Envelope>");
                socket
                    .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes())
                    .await
                    .unwrap();
                if polls == 2 {
                    let status = session.status();
                    assert_eq!(status.playback.seconds, 1.0);
                    assert_eq!(status.playback.duration, Some(180.0));
                    assert_eq!(status.playback.volume, Some(0.4));
                    session.stop();
                }
            }
            (metadata, served, actions)
        };
        let (metadata, served, actions) = tokio::time::timeout(Duration::from_secs(15), tv)
            .await
            .expect("the session must drive the TV and stop it");
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            metadata.contains("object.item.audioItem.musicTrack"),
            "{metadata}"
        );
        assert!(
            metadata.contains("http-get:*:audio/wav:DLNA.ORG_OP=01"),
            "{metadata}"
        );
        assert!(served.starts_with("HTTP/1.1 200"), "{served}");
        assert!(served.contains("Content-Type: audio/wav"), "{served}");
        assert!(
            served.contains("contentFeatures.dlna.org: DLNA.ORG_OP=01"),
            "{served}"
        );
        assert!(
            sink.started.lock().unwrap().is_empty(),
            "no decoding for a format the TV plays"
        );
        // The TV's volume is its own: starting never sets it.
        assert!(
            !actions.iter().any(|a| a == "SetVolume" || a == "SetMute"),
            "{actions:?}"
        );
    }

    #[tokio::test]
    async fn music_a_tv_does_not_list_is_decoded_for_it() {
        use tokio::io::AsyncWriteExt;
        let dir = std::env::temp_dir().join(format!("nd-upnp-ogg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let song = dir.join("song.ogg");
        std::fs::write(&song, vec![0u8; 16]).unwrap();
        let file = MediaFile::inspect(&song).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let renderer = UpnpRenderer {
            address: "127.0.0.1".parse().unwrap(),
            av_transport: format!("{base}/avt"),
            rendering_control: None,
            connection_manager: Some(format!("{base}/cm")),
        };
        let sink = Arc::new(QueueSink {
            started: Mutex::new(Vec::new()),
            initial: Mutex::new(Vec::new()),
            cancel: watch::channel(false).0,
            ready: tokio::sync::Notify::new(),
        });
        let session =
            MediaSession::start_upnp(sink.clone(), renderer, vec![MediaItem::File(file)], 0, None)
                .unwrap();
        // It asks what the TV plays, and the answer has no Ogg in it.
        let (mut socket, _) = listener.accept().await.unwrap();
        let (head, _) = read_request(&mut socket).await;
        assert!(head.contains("#GetProtocolInfo"), "{head}");
        let body = "<s:Envelope><s:Body><Sink>http-get:*:audio/mpeg:*</Sink></s:Body></s:Envelope>";
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), sink.ready.notified())
            .await
            .expect("decoded instead");
        assert_eq!(*sink.started.lock().unwrap(), [song]);
        session.stop();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn removing_and_skipping_files_updates_the_running_mirrored_queue() {
        // A cold plugin registry is scanned here, outside the deadlines below.
        nd_core::pipeline::init().unwrap();
        let sink = Arc::new(QueueSink {
            started: Mutex::new(Vec::new()),
            initial: Mutex::new(Vec::new()),
            cancel: watch::channel(false).0,
            ready: tokio::sync::Notify::new(),
        });
        let session = MediaSession::start_mirroring(
            sink.clone(),
            vec![fixture("a.mp4"), fixture("b.mp4"), fixture("c.mp4")],
            None,
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), sink.ready.notified())
            .await
            .unwrap();
        session
            .command(MediaCommand::Remove("b.mp4".into()))
            .unwrap();
        session
            .command(MediaCommand::Remove("a.mp4".into()))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), sink.ready.notified())
            .await
            .unwrap();
        assert_eq!(
            *sink.started.lock().unwrap(),
            vec![PathBuf::from("a.mp4"), PathBuf::from("c.mp4")]
        );
        assert_eq!(session.status().queue, vec![PathBuf::from("c.mp4")]);
        session.command(MediaCommand::Next).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !session.is_finished() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(session.status().error.is_none());
    }

    #[tokio::test]
    async fn an_initially_paused_renderer_waits_for_play_and_keeps_pending_settings() {
        // A cold plugin registry is scanned here, outside the deadlines below.
        nd_core::pipeline::init().unwrap();
        let sink = Arc::new(QueueSink {
            started: Mutex::new(Vec::new()),
            initial: Mutex::new(Vec::new()),
            cancel: watch::channel(false).0,
            ready: tokio::sync::Notify::new(),
        });
        let session = MediaSession::start_mirroring(
            sink.clone(),
            vec![fixture("a.mp4")],
            Some(PlaybackStart {
                seconds: 12.0,
                paused: true,
                volume: 0.25,
                muted: true,
                height: 0,
            }),
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !session.status().playback.can_pause {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(session.status().playback.paused);
        assert!(sink.started.lock().unwrap().is_empty());
        session.command(MediaCommand::SetVolume(0.37)).unwrap();
        session.command(MediaCommand::SetMute(false)).unwrap();
        session.command(MediaCommand::SetPaused(false)).unwrap();
        tokio::time::timeout(Duration::from_secs(2), sink.ready.notified())
            .await
            .unwrap();
        assert_eq!(
            *sink.initial.lock().unwrap(),
            [Some(PlaybackStart {
                seconds: 12.0,
                paused: false,
                volume: 0.37,
                muted: false,
                height: 0,
            })]
        );
        session.stop();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !session.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(session.status().error.is_none());
    }

    #[test]
    fn stale_status_cannot_finish_the_current_item() {
        let status = json!({"type": "MEDIA_STATUS", "status": [{"mediaSessionId": 5, "media": {"contentId": "file-one"}, "playerState": "IDLE", "idleReason": "FINISHED"}]});
        assert!(active_status(&status, 6, "file-two").is_none());
        assert!(active_status(&status, 5, "file-two").is_none());
        assert!(active_status(&status, 5, "file-one").is_some());
        assert!(active_status(&json!({"type": "PING"}), 5, "file-one").is_none());
    }

    #[test]
    fn the_end_of_an_item_is_idle_with_a_reason() {
        assert!(item_has_ended(&json!({
            "type": "MEDIA_STATUS",
            "status": [{"playerState": "IDLE", "idleReason": "FINISHED"}]
        })));
    }

    #[test]
    fn idle_before_the_first_item_is_not_the_end() {
        // The receiver is IDLE when the app has just started. Reading that as
        // "finished" skipped the file the person asked for.
        assert!(!item_has_ended(&json!({
            "type": "MEDIA_STATUS",
            "status": [{"playerState": "IDLE"}]
        })));
    }

    #[test]
    fn playing_and_buffering_are_not_the_end() {
        for state in ["PLAYING", "BUFFERING", "PAUSED"] {
            assert!(
                !item_has_ended(&json!({
                    "type": "MEDIA_STATUS",
                    "status": [{"playerState": state}]
                })),
                "{state} must not advance the queue"
            );
        }
    }

    #[test]
    fn an_item_that_cannot_be_played_does_not_stall_the_queue() {
        // A file the receiver refuses mid-play reports ERROR. Waiting for a
        // FINISHED that will never come would hang the rest of the queue.
        assert!(item_has_ended(&json!({
            "type": "MEDIA_STATUS",
            "status": [{"playerState": "IDLE", "idleReason": "ERROR"}]
        })));
    }

    #[test]
    fn other_messages_are_ignored() {
        assert!(!item_has_ended(&json!({"type": "PING"})));
        assert!(!item_has_ended(&json!({"type": "MEDIA_STATUS"})));
        assert!(!item_has_ended(&Value::Null));
    }
}
