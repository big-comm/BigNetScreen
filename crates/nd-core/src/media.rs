//! What kind of thing a media file is.
//!
//! Lives here, not in the Cast crate, because the answer decides how a file is
//! **played** on either protocol: a Chromecast is handed the file and decodes
//! it, a Miracast receiver is a screen and has to be shown a picture. A photo
//! has no sound, a song has no picture, and a film may have either missing —
//! each of which stalls a pipeline built for the other.

/// What a media file contains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Photo,
    Video,
    Music,
}

impl MediaKind {
    /// Does the file carry a picture of its own?
    ///
    /// A song does not, so something has to be drawn for the screen that is
    /// showing it.
    pub fn has_picture(self) -> bool {
        matches!(self, MediaKind::Photo | MediaKind::Video)
    }

    /// Does the picture stand still?
    ///
    /// A photo has exactly one frame, and a receiver expecting a video stream
    /// needs that frame repeated rather than sent once and never again.
    pub fn is_still(self) -> bool {
        self == MediaKind::Photo
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_song_has_nothing_to_show() {
        assert!(!MediaKind::Music.has_picture());
        assert!(MediaKind::Video.has_picture());
        assert!(MediaKind::Photo.has_picture());
    }

    #[test]
    fn only_a_photo_stands_still() {
        assert!(MediaKind::Photo.is_still());
        assert!(!MediaKind::Video.is_still());
        assert!(!MediaKind::Music.is_still());
    }
}

/// Transport-independent controls exposed by the media page.
#[derive(Clone, Debug, PartialEq)]
pub enum MediaCommand {
    TogglePause,
    SeekRelative(f64),
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
}

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

/// Weak access to a file pipeline; never keeps a stopped session alive.
#[derive(Clone, Debug, Default)]
pub struct FilePlaybackControl {
    pipeline: std::sync::Arc<std::sync::Mutex<gstreamer::glib::WeakRef<gstreamer::Pipeline>>>,
}

impl FilePlaybackControl {
    pub fn attach(&self, pipeline: &gstreamer::Pipeline) {
        self.pipeline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set(Some(pipeline));
    }

    fn pipeline(&self) -> Option<gstreamer::Pipeline> {
        self.pipeline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .upgrade()
    }

    pub fn state(&self) -> PlaybackState {
        use gstreamer::{self as gst, prelude::*};
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
            seconds: decoder
                .query_position::<gst::ClockTime>()
                .map(|v| v.seconds_f64())
                .unwrap_or(0.0),
            duration: decoder
                .query_duration::<gst::ClockTime>()
                .map(|v| v.seconds_f64()),
            can_pause: true,
            can_seek,
        }
    }

    /// Call from the session worker, never the GTK main thread.
    pub fn command(&self, command: &MediaCommand) -> crate::Result<()> {
        use gstreamer::{self as gst, prelude::*};
        let pipeline = self
            .pipeline()
            .ok_or_else(|| crate::NdError::Gst("media pipeline is not ready".into()))?;
        let state = self.state();
        match command {
            MediaCommand::TogglePause if state.can_pause => {
                pipeline
                    .set_state(if state.paused {
                        gst::State::Playing
                    } else {
                        gst::State::Paused
                    })
                    .map_err(|err| crate::NdError::Gst(err.to_string()))?;
            }
            MediaCommand::SeekRelative(offset) if state.can_seek => {
                let target = seek_target(state.seconds, *offset, state.duration)
                    .ok_or_else(|| crate::NdError::Gst("invalid seek position".into()))?;
                let decoder = pipeline
                    .by_name("filedec")
                    .ok_or_else(|| crate::NdError::Gst("media decoder is missing".into()))?;
                decoder
                    .seek_simple(
                        gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                        gst::ClockTime::from_seconds_f64(target),
                    )
                    .map_err(|err| crate::NdError::Gst(err.to_string()))?;
            }
            _ => {
                return Err(crate::NdError::Unsupported(
                    "playback control is not available for this item".into(),
                ))
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod playback_tests {
    use super::*;
    use gstreamer::{self as gst, prelude::*};

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
            self, AudioSource, H264Encoder, PipelineGuard, StreamConfig, VideoSource, WfdTransport,
        };
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use std::time::{Duration, Instant};
        pipeline::init().unwrap();
        let path =
            std::env::temp_dir().join(format!("bns-transport-playback-{}.mkv", std::process::id()));
        let make = gst::parse::launch(&format!(
            "matroskamux name=mux ! filesink location=\"{}\" videotestsrc num-buffers=600 ! video/x-raw,width=160,height=90,framerate=30/1 ! vp8enc deadline=1 keyframe-max-dist=30 ! mux. audiotestsrc num-buffers=938 ! audio/x-raw,rate=48000 ! vorbisenc ! mux.", path.display()
        )).unwrap().downcast::<gst::Pipeline>().unwrap();
        let fixture = PipelineGuard::new(make.clone());
        make.set_state(gst::State::Playing).unwrap();
        let message = make
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .unwrap();
        assert_eq!(message.type_(), gst::MessageType::Eos, "{message:?}");
        drop(fixture);
        let receiver = gst::parse::launch(
            "appsrc name=wire is-live=true do-timestamp=true format=time caps=\"application/x-rtp,media=video,clock-rate=90000,encoding-name=MP2T,payload=33\" ! rtpjitterbuffer latency=30 ! rtpmp2tdepay ! tsdemux name=demux queue name=video-input ! h264parse ! avdec_h264 max-threads=2 ! fakesink name=video sync=false signal-handoffs=true queue name=audio-input ! aacparse ! avdec_aac ! fakesink name=audio sync=false signal-handoffs=true"
        ).unwrap().downcast::<gst::Pipeline>().unwrap();
        let _receiver_guard = PipelineGuard::new(receiver.clone());
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
        let counts: Vec<_> = ["video", "audio"]
            .iter()
            .map(|name| {
                let count = Arc::new(AtomicUsize::new(0));
                let observed = count.clone();
                receiver
                    .by_name(name)
                    .unwrap()
                    .connect("handoff", false, move |_| {
                        observed.fetch_add(1, Ordering::Relaxed);
                        None
                    });
                count
            })
            .collect();
        receiver.set_state(gst::State::Playing).unwrap();
        let source = VideoSource::MediaFile {
            path: path.clone(),
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
        let transport = WfdTransport::new("127.0.0.1".parse().unwrap(), 19000);
        let description = pipeline::wfd_pipeline_description(&cfg, &source, &transport)
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
        let guard = PipelineGuard::new(sender.clone());
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
            let before: Vec<_> = counts
                .iter()
                .map(|count| count.load(Ordering::Relaxed))
                .collect();
            let deadline = Instant::now() + Duration::from_secs(4);
            while counts
                .iter()
                .zip(&before)
                .any(|(count, before)| count.load(Ordering::Relaxed) < before + 4)
            {
                assert!(
                    Instant::now() < deadline,
                    "receiver stalled: before={before:?}, encoded={}, playback={:?}, after={:?}",
                    encoded.load(Ordering::Relaxed),
                    control.state(),
                    counts
                        .iter()
                        .map(|count| count.load(Ordering::Relaxed))
                        .collect::<Vec<_>>()
                );
                for pipeline in [&sender, &receiver] {
                    assert!(pipeline
                        .bus()
                        .unwrap()
                        .pop_filtered(&[gst::MessageType::Error])
                        .is_none());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
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
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use std::time::Duration;
        pipeline::init().unwrap();
        let path = std::env::temp_dir().join(format!("bns-playback-{}.mkv", std::process::id()));
        let fixture = gst::parse::launch(&format!(
            "matroskamux name=mux ! filesink location=\"{}\" videotestsrc num-buffers=360 ! video/x-raw,width=160,height=90,framerate=30/1 ! vp8enc deadline=1 ! mux. audiotestsrc num-buffers=563 ! audio/x-raw,rate=48000 ! vorbisenc ! mux.", path.display()
        )).unwrap().downcast::<gst::Pipeline>().unwrap();
        let guard = PipelineGuard::new(fixture.clone());
        fixture.set_state(gst::State::Playing).unwrap();
        let message = fixture
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .unwrap();
        assert_eq!(message.type_(), gst::MessageType::Eos, "{message:?}");
        drop(guard);
        let video = VideoSource::MediaFile {
            path: path.clone(),
            kind: MediaKind::Video,
            title: "test".into(),
        };
        let description = format!("{} ! fakesink name=video sync=true signal-handoffs=true {} ! fakesink name=audio sync=true signal-handoffs=true", video.description(), AudioSource::MediaFile.description());
        let (pipeline, _) = pipeline::build_pipeline(&description, 0).unwrap();
        let guard = PipelineGuard::new(pipeline.clone());
        let counts: Vec<_> = ["video", "audio"]
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
            .collect();
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
        std::thread::sleep(Duration::from_millis(200));
        assert!((control.state().seconds - before).abs() < 0.1);
        control.command(&MediaCommand::SeekRelative(4.0)).unwrap();
        pipeline.state(gst::ClockTime::from_seconds(2)).0.unwrap();
        assert!(control.state().paused, "seek must preserve pause");
        control.command(&MediaCommand::TogglePause).unwrap();
        pipeline.state(gst::ClockTime::from_seconds(2)).0.unwrap();
        let before_counts: Vec<_> = counts.iter().map(|c| c.load(Ordering::Relaxed)).collect();
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        while counts
            .iter()
            .zip(&before_counts)
            .any(|(count, before)| count.load(Ordering::Relaxed) <= *before)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "both tracks must resume: before={before_counts:?}, after={:?}, playback={:?}",
                counts
                    .iter()
                    .map(|c| c.load(Ordering::Relaxed))
                    .collect::<Vec<_>>(),
                control.state(),
            );
            std::thread::sleep(Duration::from_millis(50));
        }
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
