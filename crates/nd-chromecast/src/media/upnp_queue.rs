//! A queue a DLNA renderer plays itself: each file is handed over as it is
//! through AVTransport, and pause, seek, volume and the position belong to
//! the renderer. Only music a television lists as playable takes this path;
//! [`super::MediaSession::start_upnp`] decides.

use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use nd_core::media::{MediaCommand, MediaSource, PlaybackStart, PlaybackState, seek_target};
use nd_core::{NdError, Result};
use nd_dlna::avtransport::{self, TransportState};
use nd_dlna::renderer::Renderer;

use super::{MediaItem, Queue, Shared, next_command};
use crate::file_server::{FileServer, MediaFile};

/// How long a television gets to start playing an item it was handed.
const UPNP_START_TIMEOUT: Duration = Duration::from_secs(30);
/// Polls between reads of the television's own volume.
const VOLUME_POLLS: u32 = 3;
/// Idle polls after playing that end the item.
const IDLE_POLLS: u32 = 2;
/// How close to the end an idle renderer counts as finished rather than
/// stopped from its own remote.
const END_SLACK_SECONDS: f64 = 5.0;

/// How an item handed to the renderer stopped.
enum ItemEnd {
    Next,
    Stop,
}

pub(super) async fn play_upnp_queue(
    tv: &Renderer,
    server: &FileServer,
    shared: &Shared,
    files: Vec<MediaItem>,
    mut commands: mpsc::Receiver<MediaCommand>,
    mut start: Option<PlaybackStart>,
) -> Result<()> {
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
        let mut item = hand_over(tv, server, index, file, start).await?;
        let mut poll = tokio::time::interval(Duration::from_secs(1));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let end = loop {
            tokio::select! {
                command = next_command(&mut commands, shared) => {
                    let Some(command) = command else { break ItemEnd::Stop };
                    if queue.edit(&command, shared) {
                        break ItemEnd::Next;
                    }
                    if matches!(command, MediaCommand::Remove(_)) {
                        continue;
                    }
                    let result = upnp_command(tv, &command, &mut item.playback).await;
                    // A pause or a seek moves the point the clock counts from.
                    if !matches!(command, MediaCommand::SetVolume(_) | MediaCommand::SetMute(_)) {
                        item.clock = None;
                    }
                    shared.update(|status| {
                        status.control_error = result.err().map(|err| err.to_string());
                        status.playback = item.playback.clone();
                    });
                }
                _ = poll.tick() => {
                    if let Some(end) = item.poll(tv, &file.title(), &mut queue).await? {
                        break end;
                    }
                    shared.update(|status| status.playback = item.playback.clone());
                }
            }
        };
        match end {
            ItemEnd::Next => shared.next_start(&mut start),
            ItemEnd::Stop => return Ok(()),
        }
    }
    Ok(())
}

/// Hands one file to the renderer and starts it.
async fn hand_over(
    tv: &Renderer,
    server: &FileServer,
    index: usize,
    file: &MediaFile,
    start: Option<PlaybackStart>,
) -> Result<UpnpItem> {
    let control = &tv.av_transport;
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
    // The TV's own volume, reported and left as it is: the player's level is
    // that of its own sound, and applied as the set's master volume it can
    // send a television to full volume.
    if let Some((volume, muted)) = tv.volume().await {
        playback.volume = Some(volume);
        playback.muted = Some(muted);
    }
    Ok(UpnpItem {
        playback,
        clock: None,
        polls: 0,
        pending_seek: start.map(|s| s.seconds).filter(|s| *s > 0.0),
        pending_pause: start.is_some_and(|s| s.paused),
        handed_over: Instant::now(),
        ever_played: false,
        idle_polls: 0,
    })
}

/// What is known about the item the renderer is playing.
struct UpnpItem {
    playback: PlaybackState,
    /// Since when, and from where, playback has been running unreported.
    clock: Option<(Instant, f64)>,
    polls: u32,
    /// Where to start and whether to hold, once the renderer is playing:
    /// before that it has nothing to seek in.
    pending_seek: Option<f64>,
    pending_pause: bool,
    handed_over: Instant,
    ever_played: bool,
    idle_polls: u32,
}

impl UpnpItem {
    /// Reads the renderer's state. `Some` when the item is over: played to
    /// the end, or stopped on the TV, which ends the session.
    async fn poll(
        &mut self,
        tv: &Renderer,
        title: &str,
        queue: &mut Queue,
    ) -> Result<Option<ItemEnd>> {
        let control = &tv.av_transport;
        self.polls += 1;
        if self.polls.is_multiple_of(VOLUME_POLLS)
            && let Some((volume, muted)) = tv.volume().await
        {
            self.playback.volume = Some(volume);
            self.playback.muted = Some(muted);
        }
        let state = avtransport::transport_state(control).await;
        match state {
            Ok(TransportState::Playing | TransportState::Paused) => {
                self.ever_played = true;
                self.idle_polls = 0;
                self.playback.paused = matches!(state, Ok(TransportState::Paused));
                self.apply_pending_start(tv).await;
                self.read_position(tv).await;
            }
            Ok(TransportState::Transitioning) => self.idle_polls = 0,
            Ok(TransportState::Idle) | Err(_) if self.ever_played => {
                self.idle_polls += 1;
                if self.idle_polls >= IDLE_POLLS {
                    let at_end = self.playback.duration.is_some_and(|duration| {
                        self.playback.seconds >= duration - END_SLACK_SECONDS
                    });
                    if !at_end {
                        return Ok(Some(ItemEnd::Stop));
                    }
                    queue.advance();
                    return Ok(Some(ItemEnd::Next));
                }
            }
            _ if self.handed_over.elapsed() > UPNP_START_TIMEOUT => {
                return Err(NdError::Protocol(format!(
                    "the TV did not start playing {title}"
                )));
            }
            _ => {}
        }
        Ok(None)
    }

    async fn apply_pending_start(&mut self, tv: &Renderer) {
        let control = &tv.av_transport;
        if let Some(seconds) = self.pending_seek.take()
            && avtransport::seek(control, seconds).await.is_ok()
        {
            self.playback.seconds = seconds;
            self.clock = None;
        }
        if std::mem::take(&mut self.pending_pause) && avtransport::pause(control).await.is_ok() {
            self.playback.paused = true;
        }
    }

    async fn read_position(&mut self, tv: &Renderer) {
        let reported = avtransport::position(&tv.av_transport).await.ok();
        if let Some((_, Some(duration))) = reported {
            self.playback.duration = Some(duration);
        }
        match reported.map(|(seconds, _)| seconds).filter(|s| *s > 0.0) {
            Some(seconds) => {
                self.playback.seconds = seconds;
                self.clock = None;
            }
            None if self.playback.paused => self.clock = None,
            // Some renderers play without ever saying where (a Panasonic
            // answers 0:00:00 throughout): count.
            None => {
                let (since, from) = *self
                    .clock
                    .get_or_insert((Instant::now(), self.playback.seconds));
                let seconds = from + since.elapsed().as_secs_f64();
                self.playback.seconds = self
                    .playback
                    .duration
                    .map_or(seconds, |duration| seconds.min(duration));
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::super::MediaSession;
    use super::super::recording_sink::{RecordingSink, read_request};
    use super::*;
    use nd_core::sink::UpnpRenderer;

    #[tokio::test]
    async fn dlna_tv_gets_music_as_audio() {
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
        let sink = RecordingSink::new();
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
    async fn unlisted_music_is_decoded() {
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
        let sink = RecordingSink::new();
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
}
