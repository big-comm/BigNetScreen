//! A queue decoded here and streamed through a sink: each item becomes a
//! [`CaptureSource`] played over the sink's own transport (Cast mirroring,
//! DLNA, Miracast), for receivers that cannot fetch the file themselves.
//!
//! Pause, seek and volume act on the local decoder through
//! [`FilePlaybackControl`]; for a DLNA television the volume is the set's own.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use nd_core::Result;
use nd_core::capture::{CaptureSource, MediaPlayback};
use nd_core::media::{
    FilePlaybackControl, MediaCommand, PlaybackStart, PlaybackState, seek_target,
};
use nd_core::sink::Sink;
use nd_dlna::renderer::Renderer;

use super::{MediaItem, PHOTO_SECONDS, Queue, Shared, next_command};
use crate::file_server::MediaKind;

/// How an item handed to the sink stopped.
enum ItemEnd {
    /// Played out, skipped or removed: on to the next item.
    Next,
    /// A seek: the same item again from the new position.
    Restart,
    /// Cancelled, or nobody can send commands any more.
    Stop,
}

/// The queue's state across items.
pub(super) struct DecodedQueue<'a> {
    sink: Arc<dyn Sink>,
    /// A DLNA television: its own volume is the one the player's volume
    /// control moves and shows, not the level of our stream.
    tv: Option<&'a Renderer>,
    shared: &'a Shared,
    cancelled: watch::Receiver<bool>,
    commands: mpsc::Receiver<MediaCommand>,
    start: Option<PlaybackStart>,
}

impl<'a> DecodedQueue<'a> {
    pub(super) fn new(
        sink: Arc<dyn Sink>,
        tv: Option<&'a Renderer>,
        shared: &'a Shared,
        cancelled: watch::Receiver<bool>,
        commands: mpsc::Receiver<MediaCommand>,
        start: Option<PlaybackStart>,
    ) -> Self {
        Self {
            sink,
            tv,
            shared,
            cancelled,
            commands,
            start,
        }
    }

    pub(super) async fn play(mut self, files: Vec<MediaItem>) -> Result<()> {
        let mut queue = Queue::new(files);
        while let Some((_, file)) = queue.pending.front().cloned() {
            if *self.cancelled.borrow() {
                break;
            }
            queue.publish(self.shared, true);
            let end = match self.hold_while_paused(&mut queue).await {
                Some(held) => held,
                None => self.play_item(&file, &mut queue).await?,
            };
            match end {
                ItemEnd::Next => self.shared.next_start(&mut self.start),
                ItemEnd::Restart => {}
                ItemEnd::Stop => return Ok(()),
            }
        }
        Ok(())
    }

    /// Holds an item that starts paused until it is asked to play, keeping
    /// the volume and mute set meanwhile for when it does. `None` means play
    /// it now.
    ///
    /// DLNA renderers can fetch inside SetAVTransportURI and wait for data
    /// before accepting Pause, so the handshake waits for Play instead of
    /// briefly playing a paused item.
    async fn hold_while_paused(&mut self, queue: &mut Queue) -> Option<ItemEnd> {
        while let Some(mut initial) = self.start.filter(|initial| initial.paused) {
            self.shared.update(|status| {
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
                _ = self.cancelled.changed() => return Some(ItemEnd::Stop),
                command = next_command(&mut self.commands, self.shared) => command,
            };
            let Some(command) = command else {
                return Some(ItemEnd::Stop);
            };
            if queue.edit(&command, self.shared) {
                return Some(ItemEnd::Next);
            }
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
            self.start = Some(initial);
            self.shared.update(|status| {
                status.control_error = (!supported)
                    .then(|| "playback control is not available before starting this item".into())
            });
        }
        None
    }

    /// Streams one item until it ends, is skipped, sought or cancelled. The
    /// sink's session is always waited for, so its cleanup completes.
    async fn play_item(&mut self, file: &MediaItem, queue: &mut Queue) -> Result<ItemEnd> {
        let control = FilePlaybackControl::default();
        let frame = {
            let source = file.source();
            let height = self.start.map_or(0, |start| start.height);
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
                start: self.start,
            },
            frame,
        );
        let sink = self.sink.clone();
        let playing = sink.start_stream(source);
        tokio::pin!(playing);
        let mut item = ItemPlayback {
            control,
            photo: file.kind() == MediaKind::Photo,
            photo_started: None,
            tv_volume: match self.tv {
                Some(tv) => tv.volume().await,
                None => None,
            },
            polls: 0,
        };
        let mut poll = tokio::time::interval(Duration::from_millis(250));
        let end = loop {
            tokio::select! {
                biased;
                result = &mut playing => {
                    result?;
                    queue.advance();
                    return Ok(ItemEnd::Next);
                }
                _ = self.cancelled.changed() => break ItemEnd::Stop,
                command = next_command(&mut self.commands, self.shared) => {
                    let Some(command) = command else { break ItemEnd::Stop };
                    if queue.edit(&command, self.shared) {
                        break ItemEnd::Next;
                    }
                    if let Some(restart) = self.command(&command, &mut item).await {
                        self.start = Some(restart);
                        break ItemEnd::Restart;
                    }
                }
                _ = poll.tick() => {
                    if item.poll(self.tv, self.shared).await {
                        queue.advance();
                        break ItemEnd::Next;
                    }
                }
            }
        };
        let _ = self.sink.stop_stream().await;
        playing.await?;
        Ok(end)
    }

    /// Carries out a player command on a playing item. `Some` is the start
    /// to restart the item from, for a seek.
    async fn command(
        &self,
        command: &MediaCommand,
        item: &mut ItemPlayback,
    ) -> Option<PlaybackStart> {
        if matches!(command, MediaCommand::Remove(_)) {
            return None;
        }
        if let Some(tv) = self.tv
            && let Some(changed) = item.tv_volume_command(tv, command).await
        {
            self.shared
                .update(|status| status.control_error = changed.err().map(|err| err.to_string()));
            return None;
        }
        // A jump inside a stream that is already live sends the receiver
        // timestamps that run backwards or leap, and some receivers then
        // drop the stream. Starting the item again from there works as any
        // start does.
        if let Some(restart) = seek_restart(command, &item.control.state(), self.start) {
            return Some(restart);
        }
        let control = item.control.clone();
        let command = command.clone();
        let outcome = tokio::task::spawn_blocking(move || control.command(&command)).await;
        self.shared.update(|status| {
            status.control_error = match outcome {
                Ok(Ok(())) => None,
                Ok(Err(err)) => Some(err.to_string()),
                Err(err) => Some(err.to_string()),
            }
        });
        None
    }
}

/// One playing item's state between polls.
struct ItemPlayback {
    control: FilePlaybackControl,
    photo: bool,
    /// When a photo first showed; it stays [`PHOTO_SECONDS`].
    photo_started: Option<tokio::time::Instant>,
    /// The television's own volume and mute, when there is one.
    tv_volume: Option<(f64, bool)>,
    polls: u32,
}

impl ItemPlayback {
    /// Sets the television's volume or mute. `None` when the command is not
    /// one of those.
    async fn tv_volume_command(
        &mut self,
        tv: &Renderer,
        command: &MediaCommand,
    ) -> Option<Result<()>> {
        let muted = self.tv_volume.is_some_and(|(_, muted)| muted);
        let level = self.tv_volume.map_or(1.0, |(level, _)| level);
        let changed = match *command {
            MediaCommand::SetVolume(level) if level.is_finite() && (0.0..=1.0).contains(&level) => {
                tv.set_volume(level).await.map(|()| (level, muted))
            }
            MediaCommand::SetMute(muted) => tv.set_mute(muted).await.map(|()| (level, muted)),
            _ => return None,
        };
        Some(changed.map(|volume| self.tv_volume = Some(volume)))
    }

    /// Publishes the playback state. True when a photo has been shown long
    /// enough to move on.
    async fn poll(&mut self, tv: Option<&Renderer>, shared: &Shared) -> bool {
        let mut playback = self.control.state();
        // Read back every few seconds: the remote moves it too.
        self.polls += 1;
        if let Some(tv) = tv.filter(|_| self.polls.is_multiple_of(12)) {
            self.tv_volume = tv.volume().await.or(self.tv_volume);
        }
        if let Some((volume, muted)) = self.tv_volume {
            playback.volume = Some(volume);
            playback.muted = Some(muted);
        }
        if self.photo {
            if playback.can_pause {
                self.photo_started
                    .get_or_insert_with(tokio::time::Instant::now);
            }
            playback.can_pause = false;
            playback.can_seek = false;
            if self
                .photo_started
                .is_some_and(|start| start.elapsed() >= Duration::from_secs(PHOTO_SECONDS))
            {
                return true;
            }
        }
        shared.update(|status| status.playback = playback);
        false
    }
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

#[cfg(test)]
mod tests {
    use super::super::MediaSession;
    use super::super::recording_sink::RecordingSink;
    use super::super::tests::fixture;
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn seek_restarts_at_requested_position() {
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

    #[tokio::test]
    async fn removal_and_skip_update_running_queue() {
        // A cold plugin registry is scanned here, outside the deadlines below.
        nd_core::pipeline::init().unwrap();
        let sink = RecordingSink::new();
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
    async fn paused_renderer_waits_for_play() {
        // A cold plugin registry is scanned here, outside the deadlines below.
        nd_core::pipeline::init().unwrap();
        let sink = RecordingSink::new();
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
}
