//! File queues and playback controls for Cast and mirrored media.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};

use nd_core::capture::{CaptureSource, MediaPlayback};
use nd_core::media::{
    seek_target, FilePlaybackControl, MediaCommand, MediaSource, PlaybackStart, PlaybackState,
};
use nd_core::sink::Sink;
use nd_core::{NdError, Result};

use crate::cast::{CastChannel, LaunchedApp, DEFAULT_MEDIA_RECEIVER, NS_MEDIA};
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
                    play_mirrored_queue(sink, files, &shared, cancelled, commands, start).await;
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
        self.commands
            .try_send(command)
            .map_err(|_| NdError::Protocol("media control is busy or playback has ended".into()))
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
                command = commands.recv() => {
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

async fn play_mirrored_queue(
    sink: Arc<dyn Sink>,
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
                command = commands.recv() => command,
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
        let source = CaptureSource::media_file(
            MediaPlayback {
                source: file.source(),
                kind: file.kind(),
                title: file.title(),
                control: Some(control.clone()),
                start,
            },
            (1920, 1080),
        );
        let playing = sink.start_stream(source);
        tokio::pin!(playing);
        let mut poll = tokio::time::interval(Duration::from_millis(250));
        let mut photo_started = None;
        loop {
            tokio::select! {
                biased;
                result = &mut playing => { result?; queue.advance(); break; }
                _ = cancelled.changed() => {
                    let _ = sink.stop_stream().await;
                    playing.await?;
                    return Ok(());
                }
                command = commands.recv() => {
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
                seconds: 1.0, paused: false, volume: 0.25, muted: true,
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

    #[tokio::test]
    async fn removing_and_skipping_files_updates_the_running_mirrored_queue() {
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
