//! Files and URLs played to a receiver: what they contain, the frame a
//! television is sent them in, and control of a file pipeline once it plays.
//!
//! Shared by every protocol because the kind of file decides how it is
//! played: a Chromecast is handed the file and decodes it, a Miracast receiver
//! is a screen and is sent a picture (`crate::pipeline::VideoSource::Media`).
//! A photo has no sound, a song has no picture, and a film may lack either,
//! each of which stalls a pipeline built for the other.

use std::sync::{Arc, Mutex, PoisonError};

use gstreamer::{self as gst, prelude::*};

/// Maximum files in one playback queue, including additions to a selection.
pub const MAX_FILES: usize = 1000;

#[derive(Clone, PartialEq, Eq)]
pub enum MediaSource {
    File(std::path::PathBuf),
    Url(String),
}

impl std::fmt::Debug for MediaSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(path) => f.debug_tuple("File").field(path).finish(),
            // Stream URLs can contain credentials and temporary access tokens.
            Self::Url(_) => f.write_str("Url(<redacted>)"),
        }
    }
}

/// What a media file contains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Photo,
    Video,
    Music,
}

/// Playback controls, independent of the protocol carrying the file.
#[derive(Clone, Debug, PartialEq)]
pub enum MediaCommand {
    TogglePause,
    SetPaused(bool),
    SeekRelative(f64),
    SeekTo(f64),
    SetVolume(f64),
    SetMute(bool),
    Next,
    Remove(std::path::PathBuf),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlaybackState {
    pub paused: bool,
    pub seconds: f64,
    pub duration: Option<f64>,
    pub can_pause: bool,
    pub can_seek: bool,
    /// Per-media level, 0..=1. None means this output cannot report/control it.
    pub volume: Option<f64>,
    pub muted: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaybackStart {
    pub seconds: f64,
    pub paused: bool,
    pub volume: f64,
    pub muted: bool,
    /// The height of the frame a decoded file is sent in, 0 for the file's
    /// own size. See [`tv_frame`].
    pub height: u32,
}

impl PlaybackStart {
    pub fn validate(self) -> crate::Result<Self> {
        if !self.volume.is_finite()
            || !(0.0..=1.0).contains(&self.volume)
            || gst::ClockTime::try_from_seconds_f64(self.seconds).is_err()
            || !(self.height == 0 || (240..=TV_FRAME_MAX.1).contains(&self.height))
        {
            return Err(crate::NdError::Unsupported(
                "invalid initial playback position, volume or resolution".into(),
            ));
        }
        Ok(self)
    }
}

/// The largest frame a television is sent: 4K UHD.
const TV_FRAME_MAX: (u32, u32) = (3840, 2160);

/// The frame a television is sent this file in.
///
/// Always 16:9, black bars included: a television stretches whatever frame it
/// gets over its whole screen. `height` picks the frame; 0 asks for the file's
/// own size, the smallest 16:9 frame that holds its picture, so nothing is
/// scaled that did not need to be. `None` when the file has no picture or
/// could not be read in time. Blocking: it reads the file's headers.
pub fn tv_frame(source: &MediaSource, height: u32) -> Option<(u32, u32)> {
    if height > 0 {
        return Some(frame_of_height(height));
    }
    picture_size(source).map(frame_holding)
}

/// The smallest 16:9 frame that holds a picture of this size.
fn frame_holding((width, height): (u32, u32)) -> (u32, u32) {
    let holding = (u64::from(width) * 9).div_ceil(16);
    frame_of_height(height.max(u32::try_from(holding).unwrap_or(u32::MAX)))
}

fn frame_of_height(height: u32) -> (u32, u32) {
    let height = height.clamp(2, TV_FRAME_MAX.1) & !1;
    ((height * 16 / 9 + 1) & !1, height)
}

/// The size the file's picture is shown at, pixel shape included.
fn picture_size(source: &MediaSource) -> Option<(u32, u32)> {
    let discovered = discover(source)?;
    let video = discovered.video_streams().into_iter().next()?;
    let par = video.par();
    let (numer, denom) = (
        u64::try_from(par.numer()).ok().filter(|n| *n > 0)?,
        u64::try_from(par.denom()).ok().filter(|d| *d > 0)?,
    );
    let width = u32::try_from(u64::from(video.width()) * numer / denom).ok()?;
    Some((width, video.height())).filter(|&(w, h)| w > 0 && h > 0)
}

/// How long the file plays, read from its headers. Blocking.
pub fn duration(source: &MediaSource) -> Option<f64> {
    discover(source)?
        .duration()
        .map(|duration| duration.seconds_f64())
        .filter(|seconds| *seconds > 0.0)
}

/// What GStreamer can tell of a file without playing it; up to five seconds.
fn discover(source: &MediaSource) -> Option<gstreamer_pbutils::DiscovererInfo> {
    crate::pipeline::init().ok()?;
    let uri = match source {
        MediaSource::File(path) => gst::glib::filename_to_uri(path, None).ok()?.to_string(),
        MediaSource::Url(uri) => uri.clone(),
    };
    gstreamer_pbutils::Discoverer::new(gst::ClockTime::from_seconds(5))
        .ok()?
        .discover_uri(&uri)
        .ok()
}

/// `seconds + offset`, clamped to `0..=duration`; `None` for a non-finite
/// position or offset.
pub fn seek_target(seconds: f64, offset: f64, duration: Option<f64>) -> Option<f64> {
    if !seconds.is_finite() || !offset.is_finite() {
        return None;
    }
    let target = (seconds + offset).max(0.0);
    if !target.is_finite() {
        return None;
    }
    Some(
        match duration.filter(|value| value.is_finite() && *value >= 0.0) {
            Some(duration) => target.min(duration),
            None => target,
        },
    )
}

/// Where playback is, from the clock that paces it.
///
/// The decoder answers a position query with how far it has read, and its
/// queues read seconds ahead of the picture being sent. The `identity
/// sync=true` elements release each buffer at its running time, so the
/// segment they carry and the pipeline's running time give the position
/// actually leaving.
fn played_position(pipeline: &gst::Pipeline) -> Option<gst::ClockTime> {
    let running = if pipeline.current_state() == gst::State::Playing {
        pipeline
            .clock()?
            .time()
            .checked_sub(pipeline.base_time()?)?
    } else {
        pipeline.start_time()?
    };
    ["file-video-sync", "file-audio-sync"]
        .iter()
        .find_map(|name| {
            let pad = pipeline.by_name(name)?.static_pad("sink")?;
            let event = pad.sticky_event::<gst::event::Segment>(0)?;
            let segment = event.segment().downcast_ref::<gst::ClockTime>()?;
            segment.to_stream_time(segment.position_from_running_time(running)?)
        })
}

/// Weak access to a file pipeline; never keeps a stopped session alive.
#[derive(Clone, Debug, Default)]
pub struct FilePlaybackControl {
    pipeline: Arc<Mutex<gst::glib::WeakRef<gst::Pipeline>>>,
}

impl FilePlaybackControl {
    pub fn attach(&self, pipeline: &gst::Pipeline) {
        self.pipeline
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .set(Some(pipeline));
    }

    /// Prepare on a worker before a receiver can consume any buffers.
    pub fn prepare(&self, start: PlaybackStart) -> crate::Result<()> {
        let start = start.validate()?;
        self.command(&MediaCommand::SetVolume(start.volume))?;
        self.command(&MediaCommand::SetMute(start.muted))?;
        if start.seconds > 0.0 {
            let pipeline = self
                .pipeline()
                .ok_or_else(|| crate::NdError::Gst("media pipeline is not ready".into()))?;
            pipeline
                .set_state(gst::State::Paused)
                .map_err(|e| crate::NdError::Gst(e.to_string()))?;
            let decoder = pipeline
                .by_name("filedec")
                .ok_or_else(|| crate::NdError::Gst("media decoder is missing".into()))?;
            // Synthetic live tracks let the pipeline reach PAUSED while an
            // HTTP decoder is still discovering its streams.
            let (result, state, _) = decoder.state(gst::ClockTime::from_seconds(10));
            result.map_err(|e| crate::NdError::Gst(e.to_string()))?;
            if state != gst::State::Paused {
                return Err(crate::NdError::Gst("media preparation timed out".into()));
            }
            self.command(&MediaCommand::SeekTo(start.seconds))?;
        }
        Ok(())
    }

    fn pipeline(&self) -> Option<gst::Pipeline> {
        self.pipeline
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .upgrade()
    }

    pub fn state(&self) -> PlaybackState {
        let Some(pipeline) = self.pipeline() else {
            return PlaybackState::default();
        };
        let Some(decoder) = pipeline.by_name("filedec") else {
            return PlaybackState::default();
        };
        if pipeline.current_state() < gst::State::Paused {
            return PlaybackState::default();
        }
        let mut seeking = gst::query::Seeking::new(gst::Format::Time);
        let can_seek = decoder.query(&mut seeking) && seeking.result().0;
        PlaybackState {
            paused: pipeline.current_state() == gst::State::Paused,
            seconds: played_position(&pipeline)
                .or_else(|| decoder.query_position::<gst::ClockTime>())
                .map(|v| v.seconds_f64())
                .unwrap_or(0.0),
            duration: decoder
                .query_duration::<gst::ClockTime>()
                .map(|v| v.seconds_f64()),
            can_pause: true,
            can_seek,
            volume: pipeline
                .by_name("file-volume")
                .map(|v| v.property("volume")),
            muted: pipeline.by_name("file-volume").map(|v| v.property("mute")),
        }
    }

    /// Applies one control to the playing file. Blocking: call from the
    /// session worker, never the GTK main thread.
    pub fn command(&self, command: &MediaCommand) -> crate::Result<()> {
        let pipeline = self
            .pipeline()
            .ok_or_else(|| crate::NdError::Gst("media pipeline is not ready".into()))?;
        let state = self.state();
        match command {
            MediaCommand::TogglePause if state.can_pause => set_paused(&pipeline, !state.paused),
            MediaCommand::SetPaused(paused) if state.can_pause => set_paused(&pipeline, *paused),
            MediaCommand::SeekRelative(offset) if state.can_seek => seek(
                &pipeline,
                seek_target(state.seconds, *offset, state.duration),
                gst::SeekFlags::KEY_UNIT,
            ),
            MediaCommand::SeekTo(seconds) if state.can_seek => seek(
                &pipeline,
                seek_target(0.0, *seconds, state.duration),
                gst::SeekFlags::ACCURATE,
            ),
            MediaCommand::SetVolume(level) if level.is_finite() && (0.0..=1.0).contains(level) => {
                let volume = pipeline.by_name("file-volume").ok_or_else(|| {
                    crate::NdError::Unsupported("media volume is unavailable".into())
                })?;
                volume.set_property("volume", *level);
                Ok(())
            }
            MediaCommand::SetMute(muted) => {
                let volume = pipeline.by_name("file-volume").ok_or_else(|| {
                    crate::NdError::Unsupported("media mute is unavailable".into())
                })?;
                volume.set_property("mute", *muted);
                Ok(())
            }
            _ => Err(crate::NdError::Unsupported(
                "playback control is not available for this item".into(),
            )),
        }
    }
}

fn set_paused(pipeline: &gst::Pipeline, paused: bool) -> crate::Result<()> {
    let state = if paused {
        gst::State::Paused
    } else {
        gst::State::Playing
    };
    pipeline
        .set_state(state)
        .map(|_| ())
        .map_err(|err| crate::NdError::Gst(err.to_string()))
}

/// Flushing seek on the decoder, which keeps a paused pipeline paused.
fn seek(
    pipeline: &gst::Pipeline,
    target: Option<f64>,
    precision: gst::SeekFlags,
) -> crate::Result<()> {
    let target = target.ok_or_else(|| crate::NdError::Gst("invalid seek position".into()))?;
    let decoder = pipeline
        .by_name("filedec")
        .ok_or_else(|| crate::NdError::Gst("media decoder is missing".into()))?;
    let position = gst::ClockTime::try_from_seconds_f64(target)
        .map_err(|err| crate::NdError::Gst(err.to_string()))?;
    decoder
        .seek_simple(gst::SeekFlags::FLUSH | precision, position)
        .map_err(|err| crate::NdError::Gst(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Waits up to four seconds for both counters to advance by `frames`,
    /// failing on a pipeline error or a stall.
    fn wait_for_both_tracks(
        counts: &[Arc<AtomicUsize>],
        frames: usize,
        pipelines: &[&gst::Pipeline],
        context: impl Fn() -> String,
    ) {
        let now = || {
            counts
                .iter()
                .map(|count| count.load(Ordering::Relaxed))
                .collect::<Vec<_>>()
        };
        let before = now();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
        while now()
            .iter()
            .zip(&before)
            .any(|(count, before)| *count < before + frames)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "tracks stalled: before={before:?}, after={:?}, {}",
                now(),
                context()
            );
            for pipeline in pipelines {
                let error = pipeline
                    .bus()
                    .unwrap()
                    .pop_filtered(&[gst::MessageType::Error]);
                assert!(error.is_none(), "{error:?}");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Counts the buffers reaching the `video` and `audio` fakesinks.
    fn count_handoffs(pipeline: &gst::Pipeline) -> Vec<Arc<AtomicUsize>> {
        ["video", "audio"]
            .iter()
            .map(|name| {
                let count = Arc::new(AtomicUsize::new(0));
                let observed = count.clone();
                pipeline
                    .by_name(name)
                    .unwrap()
                    .connect("handoff", false, move |_| {
                        observed.fetch_add(1, Ordering::Relaxed);
                        None
                    });
                count
            })
            .collect()
    }

    /// The WFD sender for `source`, with its RTP packets pushed into the
    /// receiver's `wire` appsrc instead of a UDP socket.
    fn wfd_sender_into(
        receiver: &gst::Pipeline,
        source: &crate::pipeline::VideoSource,
        cfg: &crate::pipeline::StreamConfig,
    ) -> gst::Pipeline {
        use crate::pipeline::{self, WfdTransport};
        let transport = WfdTransport::new("127.0.0.1".parse().unwrap(), 19000);
        let description = pipeline::wfd_pipeline_description(cfg, source, &transport)
            .replace(
                &format!(
                    "udpsink host=127.0.0.1 port=19000 bind-port={} sync=true async=false",
                    transport.local_rtp_port
                ),
                "fakesink name=wire-out sync=true async=false signal-handoffs=true",
            )
            .replace(
                &format!(
                    "udpsink host=127.0.0.1 port=19001 bind-port={} sync=false async=false",
                    transport.local_rtp_port + 1
                ),
                "fakesink sync=false async=false",
            );
        assert!(!description.contains("udpsink"));
        let (sender, _) = pipeline::build_pipeline(&description, cfg.latency_ms()).unwrap();
        let appsrc = receiver.by_name("wire").unwrap();
        // Only packet bytes cross the bridge, as with UDP; no seek/flush events.
        sender
            .by_name("wire-out")
            .unwrap()
            .connect("handoff", false, move |values| {
                let mut packet = values[1].get::<gst::Buffer>().unwrap().copy();
                let packet_data = packet.make_mut();
                packet_data.set_pts(None);
                packet_data.set_dts(None);
                packet_data.set_duration(None);
                packet_data.unset_flags(gst::BufferFlags::DISCONT | gst::BufferFlags::RESYNC);
                let _ = appsrc.emit_by_name::<gst::FlowReturn>("push-buffer", &[&packet]);
                None
            });
        sender
    }

    /// A receiver for the WFD RTP stream, fed through the `wire` appsrc and
    /// decoding into the `video` and `audio` fakesinks.
    fn ts_receiver() -> gst::Pipeline {
        let receiver = gst::parse::launch(
            "appsrc name=wire is-live=true do-timestamp=true format=time \
             caps=\"application/x-rtp,media=video,clock-rate=90000,encoding-name=MP2T,payload=33\" \
             ! rtpjitterbuffer latency=30 ! rtpmp2tdepay ! tsdemux name=demux \
             queue name=video-input ! h264parse ! avdec_h264 max-threads=2 \
             ! fakesink name=video sync=false signal-handoffs=true \
             queue name=audio-input ! aacparse ! avdec_aac \
             ! fakesink name=audio sync=false signal-handoffs=true",
        )
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let weak_receiver = receiver.downgrade();
        receiver
            .by_name("demux")
            .unwrap()
            .connect_pad_added(move |_, pad| {
                let Some(receiver) = weak_receiver.upgrade() else {
                    return;
                };
                let name = if pad.name().starts_with("video") {
                    "video-input"
                } else if pad.name().starts_with("audio") {
                    "audio-input"
                } else {
                    return;
                };
                let sink = receiver.by_name(name).unwrap().static_pad("sink").unwrap();
                // MPEG-TS updates its program map when the audio track joins.
                if let Some(previous) = sink.peer() {
                    previous.unlink(&sink).unwrap();
                }
                pad.link(&sink).unwrap();
            });
        receiver
    }

    #[test]
    fn a_television_frame_is_16_by_9_and_holds_the_picture() {
        let file = MediaSource::File("/nonexistent.mkv".into());
        for (height, frame) in [
            (720, (1280, 720)),
            (1080, (1920, 1080)),
            (1440, (2560, 1440)),
            (2160, (3840, 2160)),
            (480, (854, 480)),
        ] {
            assert_eq!(tv_frame(&file, height), Some(frame), "{height}");
        }
        // A 2.39:1 film keeps its width; a 4:3 one keeps its height; nothing
        // grows past 4K.
        assert_eq!(frame_holding((1920, 802)), (1920, 1080));
        assert_eq!(frame_holding((1440, 1080)), (1920, 1080));
        assert_eq!(frame_holding((4096, 2160)), (3840, 2160));
        // A width read from a hostile header must not overflow the arithmetic.
        assert_eq!(frame_holding((u32::MAX, 1)), (3840, 2160));
        let start = |height| PlaybackStart {
            seconds: 0.0,
            paused: false,
            volume: 1.0,
            muted: false,
            height,
        };
        assert!(start(100).validate().is_err());
        assert!(start(4320).validate().is_err());
        assert!(start(1080).validate().is_ok());
        assert!(start(0).validate().is_ok());
    }

    #[test]
    fn media_volume_and_mute_change_encoded_input_samples() {
        use crate::pipeline::{AudioSource, PipelineGuard};
        gst::init().unwrap();
        let description = format!(
            "audiotestsrc is-live=true samplesperbuffer=480 ! audio/x-raw,rate=48000 ! identity name=filedec {} ! audio/x-raw,format=F32LE ! appsink name=measure sync=false max-buffers=1 drop=true",
            AudioSource::MediaFile.description()
        );
        let pipeline = gst::parse::launch(&description)
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
        let _guard = PipelineGuard::new(pipeline.clone());
        let control = FilePlaybackControl::default();
        control.attach(&pipeline);
        pipeline.set_state(gst::State::Playing).unwrap();
        pipeline.state(gst::ClockTime::from_seconds(2)).0.unwrap();
        let sink = pipeline.by_name("measure").unwrap();
        let rms = || {
            let mut result = 0.0;
            // Discard frames already in flight when the command was applied.
            for _ in 0..8 {
                let sample = sink
                    .emit_by_name::<Option<gst::Sample>>(
                        "try-pull-sample",
                        &[&gst::ClockTime::from_seconds(1)],
                    )
                    .expect("audio frame");
                let buffer = sample.buffer().unwrap().map_readable().unwrap();
                let samples: Vec<f64> = buffer
                    .as_slice()
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f64::from(f32::from_le_bytes(*b)))
                    .collect();
                result = (samples.iter().map(|v| v * v).sum::<f64>() / samples.len() as f64).sqrt();
            }
            result
        };
        let full = rms();
        assert!(full > 0.1);
        control.command(&MediaCommand::SetVolume(0.25)).unwrap();
        assert!((rms() / full - 0.25).abs() < 0.02);
        assert!(control.command(&MediaCommand::SetVolume(f64::NAN)).is_err());
        control.command(&MediaCommand::SetMute(true)).unwrap();
        assert_eq!(rms(), 0.0);
        control.command(&MediaCommand::SetMute(false)).unwrap();
        assert!((rms() / full - 0.25).abs() < 0.02);
        assert_eq!(control.state().volume, Some(0.25));
        assert_eq!(control.state().muted, Some(false));
    }

    #[test]
    fn seeking_clamps_and_rejects_invalid_positions() {
        assert_eq!(seek_target(3.0, -10.0, Some(60.0)), Some(0.0));
        assert_eq!(seek_target(57.0, 10.0, Some(60.0)), Some(60.0));
        assert_eq!(seek_target(57.0, 10.0, None), Some(67.0));
        assert_eq!(seek_target(f64::NAN, 10.0, None), None);
        assert_eq!(seek_target(0.0, f64::INFINITY, None), None);
    }

    #[test]
    fn mirrored_transport_keeps_decoding_after_pause_and_seeks() {
        use crate::pipeline::{
            self, AudioSource, H264Encoder, PipelineGuard, StreamConfig, VideoSource,
        };
        // The sender's latency follows the global profile.
        let _lock = crate::latency::exclusive();
        pipeline::init().unwrap();
        let path =
            std::env::temp_dir().join(format!("bns-transport-playback-{}.mkv", std::process::id()));
        crate::pipeline::write_test_media(&format!(
            "matroskamux name=mux ! filesink location=\"{}\" videotestsrc num-buffers=600 ! video/x-raw,width=160,height=90,framerate=30/1 ! vp8enc deadline=1 keyframe-max-dist=30 ! mux. audiotestsrc num-buffers=938 ! audio/x-raw,rate=48000 ! vorbisenc ! mux.",
            path.display()
        ));
        let receiver = ts_receiver();
        let _receiver_guard = PipelineGuard::new(receiver.clone());
        let counts = count_handoffs(&receiver);
        receiver.set_state(gst::State::Playing).unwrap();
        let source = VideoSource::Media {
            source: crate::media::MediaSource::File(path.clone()),
            kind: MediaKind::Video,
            title: "test".into(),
        };
        let cfg = StreamConfig {
            width: 320,
            height: 240,
            fps: 30,
            encoder: H264Encoder::X264,
            audio: AudioSource::MediaFile,
            ..Default::default()
        };
        let sender = wfd_sender_into(&receiver, &source, &cfg);
        let guard = PipelineGuard::new(sender.clone());
        sender.set_state(gst::State::Playing).unwrap();
        sender.state(gst::ClockTime::from_seconds(3)).0.unwrap();
        let encoded = Arc::new(AtomicUsize::new(0));
        let encoded_count = encoded.clone();
        sender
            .by_name("enc")
            .unwrap()
            .static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                encoded_count.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
        let control = FilePlaybackControl::default();
        control.attach(&sender);
        let flowing = || {
            wait_for_both_tracks(&counts, 4, &[&sender, &receiver], || {
                format!(
                    "encoded={}, playback={:?}",
                    encoded.load(Ordering::Relaxed),
                    control.state()
                )
            })
        };
        flowing();
        control.command(&MediaCommand::TogglePause).unwrap();
        sender.state(gst::ClockTime::from_seconds(2)).0.unwrap();
        assert!(control.state().paused);
        control.command(&MediaCommand::SeekRelative(5.0)).unwrap();
        sender.state(gst::ClockTime::from_seconds(2)).0.unwrap();
        control.command(&MediaCommand::TogglePause).unwrap();
        flowing();
        control.command(&MediaCommand::SeekRelative(-10.0)).unwrap();
        flowing();
        drop(guard);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn local_file_pause_seek_and_resume_preserve_audio_and_video() {
        use crate::pipeline::{self, AudioSource, PipelineGuard, VideoSource};
        use std::time::Duration;
        pipeline::init().unwrap();
        let path = std::env::temp_dir().join(format!("bns-playback-{}.mkv", std::process::id()));
        crate::pipeline::write_test_media(&format!(
            "matroskamux name=mux ! filesink location=\"{}\" videotestsrc num-buffers=360 ! video/x-raw,width=160,height=90,framerate=30/1 ! vp8enc deadline=1 ! mux. audiotestsrc num-buffers=563 ! audio/x-raw,rate=48000 ! vorbisenc ! mux.",
            path.display()
        ));
        let video = VideoSource::Media {
            source: crate::media::MediaSource::File(path.clone()),
            kind: MediaKind::Video,
            title: "test".into(),
        };
        let description = format!(
            "{} ! fakesink name=video sync=true signal-handoffs=true {} ! fakesink name=audio sync=true signal-handoffs=true",
            video.description(),
            AudioSource::MediaFile.description()
        );
        let (pipeline, _) = pipeline::build_pipeline(&description, 0).unwrap();
        let guard = PipelineGuard::new(pipeline.clone());
        let counts = count_handoffs(&pipeline);
        pipeline.set_state(gst::State::Playing).unwrap();
        pipeline.state(gst::ClockTime::from_seconds(3)).0.unwrap();
        let control = FilePlaybackControl::default();
        control.attach(&pipeline);
        std::thread::sleep(Duration::from_millis(300));
        assert!(control.state().can_seek, "{:?}", control.state());
        control.command(&MediaCommand::TogglePause).unwrap();
        pipeline.state(gst::ClockTime::from_seconds(2)).0.unwrap();
        assert!(control.state().paused);
        let before = control.state().seconds;
        control.command(&MediaCommand::SetPaused(true)).unwrap();
        pipeline.state(gst::ClockTime::from_seconds(2)).0.unwrap();
        assert!(control.state().paused, "explicit pause is idempotent");
        std::thread::sleep(Duration::from_millis(200));
        assert!((control.state().seconds - before).abs() < 0.1);
        control.command(&MediaCommand::SeekRelative(4.0)).unwrap();
        pipeline.state(gst::ClockTime::from_seconds(2)).0.unwrap();
        assert!(control.state().paused, "seek must preserve pause");
        control.command(&MediaCommand::SeekTo(4.0)).unwrap();
        pipeline.state(gst::ClockTime::from_seconds(2)).0.unwrap();
        assert!(control.state().paused, "absolute seek must preserve pause");
        control.command(&MediaCommand::TogglePause).unwrap();
        pipeline.state(gst::ClockTime::from_seconds(2)).0.unwrap();
        wait_for_both_tracks(&counts, 1, &[&pipeline], || {
            format!("playback={:?}", control.state())
        });
        assert!(control.state().seconds >= 3.0, "{:?}", control.state());
        control.command(&MediaCommand::SeekRelative(-10.0)).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            control.state().seconds < 3.0,
            "backward seek must reach start: {:?}",
            control.state()
        );
        drop(guard);
        assert!(!control.state().can_pause);
        std::fs::remove_file(path).unwrap();
    }
}
