//! File queues and playback controls for media a player hands over: local
//! files and HTTP(S) URLs, played on a Cast receiver, a DLNA renderer, or
//! decoded here and streamed through any sink.
//!
//! [`MediaSession`] owns the queue and the commands; which device plays the
//! items, and how, is [`cast_queue`], [`upnp_queue`] or [`decoded_queue`].

mod cast_queue;
mod decoded_queue;
mod upnp_queue;

#[cfg(test)]
mod recording_sink;

pub(crate) use cast_queue::active_status;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, watch};

use nd_core::media::{MediaCommand, MediaSource, PlaybackStart, PlaybackState};
use nd_core::sink::{Sink, UpnpRenderer};
use nd_core::{NdError, Result};
use nd_dlna::avtransport;
use nd_dlna::renderer::{Renderer, accepts};

use crate::cast::{CastChannel, DEFAULT_MEDIA_RECEIVER};
use crate::file_server::{FileServer, MediaFile, MediaKind};
use decoded_queue::DecodedQueue;

/// How long a photo stays on screen before the queue moves on.
const PHOTO_SECONDS: u64 = 8;

/// One entry of a queue: a local file, or an HTTP(S) URL a player handed over.
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
    // A URL can carry an access token: only its kind is printed.
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
    /// A URL from another process, checked before it reaches a receiver or a
    /// decoder: HTTP(S) with a host, bounded, no control characters, and a
    /// plain `type/subtype` content type.
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

/// The items left to play, each with its index on the file server. Indexes
/// are fixed when the session starts, so removing an item does not move the
/// URL of another.
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

    /// Applies `Next` or `Remove`. True when the current item must stop.
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
    /// Plays the files on a Cast receiver, which fetches each from this
    /// computer's [`FileServer`] or from the web.
    pub fn start(
        receiver: SocketAddr,
        // The discovery identity its device key is remembered under.
        identity: String,
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
                let local_files: Vec<_> =
                    files.iter().filter_map(|item| item.file().cloned()).collect();
                let server = if local_files.is_empty() {
                    None
                } else {
                    Some(FileServer::start(receiver.ip(), port, local_files).await?)
                };
                let channel = tokio::select! {
                    result = CastChannel::connect_to(receiver.ip(), receiver.port(), &identity) => result?,
                    _ = cancelled.changed() => return Ok(()),
                };
                // LAUNCH is not raced with cancellation, so a cancelled launch
                // still gets its explicit STOP.
                let app = match channel.launch(DEFAULT_MEDIA_RECEIVER).await {
                    Ok(app) => app,
                    Err(err) => {
                        channel.close().await;
                        return Err(err);
                    }
                };
                let outcome = if shared.stopping.load(Ordering::SeqCst) {
                    Ok(())
                } else {
                    let queue = cast_queue::play_queue(
                        &channel,
                        &app,
                        server.as_ref(),
                        &shared,
                        &sender_name,
                        files,
                        commands,
                        start,
                    );
                    tokio::select! {
                        result = queue => result,
                        _ = cancelled.changed() => Ok(()),
                    }
                };
                drop(server);
                channel.finish_app(&app, outcome).await
            }
            .await;
                shared.finish(outcome);
            },
        )
    }

    /// Decodes the files here and streams them through `sink`, for a
    /// receiver that cannot fetch them.
    pub fn start_mirroring(
        sink: Arc<dyn Sink>,
        files: Vec<MediaItem>,
        start: Option<PlaybackStart>,
    ) -> Result<Self> {
        let start = start.map(PlaybackStart::validate).transpose()?;
        Self::spawn(
            files,
            move |files, shared, cancelled, commands| async move {
                let outcome = DecodedQueue::new(sink, None, &shared, cancelled, commands, start)
                    .play(files)
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
                        return DecodedQueue::new(sink, Some(&tv), &shared, cancelled, commands, start)
                            .play(files)
                            .await;
                    };
                    let server = FileServer::start(renderer.address, port, native).await?;
                    let result = tokio::select! {
                        result = upnp_queue::play_upnp_queue(&tv, &server, &shared, files, commands, start) => result,
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

#[cfg(test)]
mod tests {
    use super::*;
    use nd_core::media::FilePlaybackControl;
    use std::time::Duration;

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

    pub(super) fn fixture(name: &str) -> MediaItem {
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
                let bytes = sample.buffer().unwrap().map_readable().unwrap();
                let samples = bytes.as_slice().as_chunks::<4>().0;
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
}
