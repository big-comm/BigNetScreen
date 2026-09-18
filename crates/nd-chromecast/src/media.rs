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
use nd_core::media::{seek_target, FilePlaybackControl, MediaCommand, PlaybackState};
use nd_core::sink::Sink;
use nd_core::{NdError, Result};

use crate::cast::{CastChannel, LaunchedApp, DEFAULT_MEDIA_RECEIVER, NS_MEDIA};
use crate::file_server::{FileServer, MediaFile, MediaKind};

pub const PHOTO_SECONDS: u64 = 8;
const SILENCE_TIMEOUT: Duration = Duration::from_secs(60);

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
    fn update(&self, update: impl FnOnce(&mut MediaStatus)) {
        update(&mut self.status.lock().unwrap_or_else(|e| e.into_inner()));
    }

    fn finish(&self, outcome: Result<()>) {
        self.update(|status| {
            status.finished = true;
            if !self.stopping.load(Ordering::SeqCst) {
                status.error = outcome.err().map(|err| err.to_string());
            }
        });
    }
}

/// Immutable HTTP indexes survive removal of pending files.
struct Queue {
    pending: VecDeque<(usize, MediaFile)>,
    completed: usize,
}

impl Queue {
    fn new(files: Vec<MediaFile>) -> Self {
        Self {
            pending: files.into_iter().enumerate().collect(),
            completed: 0,
        }
    }

    fn publish(&self, shared: &Shared, new_item: bool) {
        shared.update(|status| {
            status.queue = self
                .pending
                .iter()
                .map(|(_, file)| file.path.clone())
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
                    .is_some_and(|(_, file)| &file.path == path);
                self.pending.retain(|(_, file)| &file.path != path);
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
        files: Vec<MediaFile>,
        port: u16,
        sender_name: String,
    ) -> Result<Self> {
        Self::spawn(
            files,
            move |files, shared, mut cancelled, commands| async move {
                let outcome = async {
                let server = FileServer::start(receiver.ip(), port, files.clone()).await?;
                let channel = tokio::select! {
                    result = CastChannel::connect_to(receiver.ip(), receiver.port()) => result?,
                    _ = cancelled.changed() => return Ok(()),
                };
                // Finish LAUNCH so cancellation can explicitly STOP its result.
                let app = channel.launch(DEFAULT_MEDIA_RECEIVER).await?;
                let outcome = if shared.stopping.load(Ordering::SeqCst) { Ok(()) } else {
                    tokio::select! {
                        result = play_queue(&channel, &app, &server, &shared, &sender_name, files, commands) => result,
                        _ = cancelled.changed() => Ok(()),
                    }
                };
                drop(server);
                let _ = tokio::time::timeout(Duration::from_secs(5), async {
                    let _ = channel.stop_app(&app).await;
                    channel.close().await;
                }).await;
                outcome
            }.await;
                shared.finish(outcome);
            },
        )
    }

    pub fn start_mirroring(sink: Arc<dyn Sink>, files: Vec<MediaFile>) -> Result<Self> {
        Self::spawn(
            files,
            move |files, shared, cancelled, commands| async move {
                let outcome = play_mirrored_queue(sink, files, &shared, cancelled, commands).await;
                shared.finish(outcome);
            },
        )
    }

    fn spawn<F, Fut>(files: Vec<MediaFile>, run: F) -> Result<Self>
    where
        F: FnOnce(
            Vec<MediaFile>,
            Arc<Shared>,
            watch::Receiver<bool>,
            mpsc::Receiver<MediaCommand>,
        ) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        if files.is_empty() {
            return Err(NdError::Protocol("no files to send".into()));
        }
        let shared = Arc::new(Shared {
            status: Mutex::new(MediaStatus {
                total: files.len(),
                queue: files.iter().map(|file| file.path.clone()).collect(),
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

async fn play_queue(
    channel: &CastChannel,
    app: &LaunchedApp,
    server: &FileServer,
    shared: &Shared,
    sender_name: &str,
    files: Vec<MediaFile>,
    mut commands: mpsc::Receiver<MediaCommand>,
) -> Result<()> {
    let mut queue = Queue::new(files);
    while let Some((index, file)) = queue.pending.front().cloned() {
        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }
        queue.publish(shared, true);
        let url = server.url(index);
        let loaded = channel
            .load_file(app, &url, &file, sender_name)
            .await
            .map_err(|e| {
                NdError::Protocol(format!("the receiver refused {}: {e}", file.title()))
            })?;
        let session_id = loaded
            .pointer("/status/0/mediaSessionId")
            .and_then(Value::as_i64)
            .ok_or_else(|| NdError::Protocol("LOAD returned no media session".into()))?;
        let photo = file.kind == MediaKind::Photo;
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
                    let Some(request) = control_request(&command, session_id, &playback) else { continue; };
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
        MediaCommand::SeekRelative(offset) if state.can_seek => {
            Some(json!({"type": "SEEK", "mediaSessionId": session_id,
            "currentTime": seek_target(state.seconds, *offset, state.duration)?}))
        }
        _ => None,
    }
}

async fn play_mirrored_queue(
    sink: Arc<dyn Sink>,
    files: Vec<MediaFile>,
    shared: &Shared,
    mut cancelled: watch::Receiver<bool>,
    mut commands: mpsc::Receiver<MediaCommand>,
) -> Result<()> {
    let mut queue = Queue::new(files);
    while let Some((_, file)) = queue.pending.front().cloned() {
        if *cancelled.borrow() {
            break;
        }
        queue.publish(shared, true);
        let control = FilePlaybackControl::default();
        let photo = file.kind == MediaKind::Photo;
        let source = CaptureSource::media_file(
            MediaPlayback {
                path: file.path.clone(),
                kind: file.kind,
                title: file.title(),
                control: Some(control.clone()),
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
    }
    Ok(())
}
fn active_status<'a>(payload: &'a Value, session_id: i64, url: &str) -> Option<&'a Value> {
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

    fn fixture(name: &str) -> MediaFile {
        MediaFile {
            path: name.into(),
            kind: MediaKind::Video,
            content_type: "video/mp4",
            size: 1,
        }
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

    struct QueueSink {
        started: Mutex<Vec<PathBuf>>,
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
            self.started
                .lock()
                .unwrap()
                .push(source.media.unwrap().path);
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
            cancel: watch::channel(false).0,
            ready: tokio::sync::Notify::new(),
        });
        let session = MediaSession::start_mirroring(
            sink.clone(),
            vec![fixture("a.mp4"), fixture("b.mp4"), fixture("c.mp4")],
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
