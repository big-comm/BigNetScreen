//! GStreamer pipeline construction for every transmission path.
//!
//! The protocol crates ask this module for a `gst-launch` description and
//! build it with [`build_pipeline`]; capture, scaling, encoding and queueing
//! are decided here once instead of in each protocol. Latency is mostly a
//! property of the description: short queues, an encoder told to add no delay
//! (no lookahead, no B-frames, CBR) whichever element it is, and a pipeline
//! latency that is applied rather than only declared.
//!
//! - `source`: where frames and sound come from (PipeWire, a file, a pattern).
//! - `scale`: sizing the picture and reading back what was negotiated.
//! - `encoder`, `encoder_selection`: the H.264 element, its properties in each
//!   encoder's own units, and which one a machine can run.
//! - `stream_config`: one session's parameters and the stages derived from them.
//! - `wfd`, `mirror`, `ts_http`: the complete descriptions for Miracast RTP,
//!   Cast mirroring access units, and MPEG-TS over HTTP for Cast and DLNA.
//! - `diagnostics`: probes that count and time buffers in a running pipeline.

mod diagnostics;
mod encoder;
mod encoder_selection;
mod mirror;
mod scale;
mod source;
mod stream_config;
mod ts_http;
mod wfd;

use std::sync::OnceLock;

use gst::prelude::*;
use gstreamer as gst;

use crate::{NdError, Result};

pub use diagnostics::{
    capture_memory, count_buffers, instrument_framerate, instrument_latency, query_min_latency_ms,
};
pub use encoder::{
    ENCODER_NAME, H264Encoder, H264Profile, OPENH264_PIPELINE_LATENCY_MS, PIPELINE_LATENCY_AUTO,
    set_encoder_bitrate, software_encode_threads,
};
pub use encoder_selection::{
    Acceleration, GpuDriver, MonitoredPipeline, best_encoder, build_monitored, encoder_candidates,
    probe_encoders, select_encoder, working_encoder,
};
pub use mirror::{
    CAST_VBV_FRAMES, MIRROR_AUDIO_SINK, MIRROR_VIDEO_SINK, mirror_pipeline_description,
};
pub use scale::{
    SCALE_CAPS, VideoTarget, delivered_capture_size, gpu_path_requested, negotiated_video_size,
    pin_video_size,
};
pub use source::{AudioSource, CAPTURE_SOURCE_NAME, Monitor, SQUARE_PIXELS, VideoSource};
pub use stream_config::{
    AUDIO_QUEUE_BUFFERS, AUDIO_QUEUE_MS, MIRROR_AUDIO_QUEUE_BUFFERS, MIRROR_AUDIO_QUEUE_MS,
    StreamConfig, VIDEO_QUEUE_BUFFERS, VIDEO_QUEUE_MS,
};
pub use ts_http::{
    CHROMECAST_MAX_RESOLUTION, DLNA_MAX_RESOLUTION, TS_HTTP_SINK_NAME, ts_http_pipeline_description,
};
pub use wfd::{
    LOCAL_RTP_PORT, RTP_LATENCY_MS, WFD_AUDIO_PID, WFD_VIDEO_PID, WfdTransport, rtp_latency_ms,
    wfd_pipeline_description,
};

/// The memoised result of `gst::init()`.
static GST_INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();

/// Initialises GStreamer exactly once per process.
pub fn init() -> Result<()> {
    GST_INIT
        .get_or_init(|| gst::init().map_err(|e| e.to_string()))
        .clone()
        .map_err(NdError::Gst)
}

/// An event coming off a running pipeline's bus.
#[derive(Clone, Debug)]
pub enum PipelineEvent {
    /// Fatal error: the pipeline stopped producing media.
    Error { message: String, debug: String },
    /// Non-fatal warning.
    Warning { message: String },
    /// End of stream.
    Eos,
}

/// The channel through which bus events reach the caller.
pub type PipelineEvents = futures::channel::mpsc::Receiver<PipelineEvent>;

/// End synthetic tracks when all finite decoder tracks reach EOS.
fn finish_file_sources(pipeline: &gst::Pipeline) {
    let Some(decoder) = pipeline.by_name("filedec") else {
        return;
    };
    if let Some(queue) = pipeline.by_name("file-audio-queue") {
        let weak = queue.downgrade();
        decoder.connect_no_more_pads(move |_| {
            if let Some(queue) = weak.upgrade()
                && let Some(pad) = queue.static_pad("sink")
                && !pad.is_linked()
            {
                pad.send_event(gst::event::Eos::new());
            }
        });
    }
    if pipeline.by_name("file-photo").is_some() {
        return;
    }
    let remaining = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let weak = pipeline.downgrade();
    decoder.connect_pad_added(move |_, pad| {
        remaining.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let remaining = remaining.clone();
        let weak = weak.clone();
        let ended = std::sync::atomic::AtomicBool::new(false);
        pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
            if info
                .event()
                .is_some_and(|event| event.type_() == gst::EventType::Eos)
                && !ended.swap(true, std::sync::atomic::Ordering::SeqCst)
                && remaining.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 1
                && let Some(pipeline) = weak.upgrade()
            {
                pipeline.call_async(|pipeline| {
                    for name in ["file-silence", "file-picture"] {
                        if let Some(source) = pipeline.by_name(name) {
                            source.send_event(gst::event::Eos::new());
                        }
                    }
                });
            }
            gst::PadProbeReturn::Ok
        });
    });
}

pub fn build_pipeline(
    description: &str,
    latency_ms: u64,
) -> Result<(gst::Pipeline, PipelineEvents)> {
    build_configured_pipeline(description, latency_ms, |_| Ok(()))
}

/// Set properties before READY can create external resources.
pub fn build_configured_pipeline(
    description: &str,
    latency_ms: u64,
    configure: impl FnOnce(&gst::Pipeline) -> Result<()>,
) -> Result<(gst::Pipeline, PipelineEvents)> {
    init()?;
    tracing::debug!(latency_ms, "building the pipeline");

    let remote_media = description.contains("uridecodebin ");
    let element = gst::parse::launch(description).map_err(|e| {
        NdError::Gst(if remote_media {
            format!(
                "could not build online media pipeline ({:?})",
                e.kind::<gst::ParseError>()
            )
        } else {
            e.to_string()
        })
    })?;
    let pipeline = element
        .downcast::<gst::Pipeline>()
        .map_err(|_| NdError::Gst("the description did not produce a Pipeline".into()))?;
    configure(&pipeline)?;

    // `0` = automatic: let GStreamer use the minimum latency the pipeline
    // itself reports. That is the right default — forcing a value **below**
    // that minimum makes sinks drop late buffers (the sink receives megabits
    // and shows a black screen), and forcing one far **above** it only adds
    // lag. Setting it explicitly is for giving headroom to spiky encoders.
    if latency_ms > 0 {
        pipeline.set_latency(gst::ClockTime::from_mseconds(latency_ms));
    }
    // Every pipeline, because the question "were those frames ever made?" is
    // asked of whichever one is misbehaving. Costs nothing unless asked for.
    instrument_framerate(&pipeline);

    // Optional diagnostic: how long a frame spends in here. Kept behind an
    // environment variable because it installs a probe on every buffer.
    if std::env::var("BIGNETSCREEN_LATENCY").is_ok() {
        instrument_latency(&pipeline);
    }
    finish_file_sources(&pipeline);

    let (tx, rx) = futures::channel::mpsc::channel(32);
    let tx = std::sync::Mutex::new(tx);
    let weak_pipeline = pipeline.downgrade();
    if let Some(bus) = pipeline.bus() {
        bus.set_sync_handler(move |_, msg| {
            match msg.view() {
                gst::MessageView::Error(err) => {
                    // HTTP diagnostics can repeat credentials or redirected
                    // URLs. Publish categories, not the upstream strings.
                    let (message, details) = if remote_media {
                        ("Could not read or decode the online media. Check the link and connection.".into(),
                         format!("resource={:?}; stream={:?}", err.error().kind::<gst::ResourceError>(), err.error().kind::<gst::StreamError>()))
                    } else {
                        (err.error().to_string(), err.debug().map(|d| d.to_string()).unwrap_or_default())
                    };
                    tracing::error!(%message, %details, "pipeline error");
                    let _ = tx
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .try_send(PipelineEvent::Error {
                            message,
                            debug: details,
                        });
                }
                gst::MessageView::Warning(w) => {
                    let message = if remote_media {
                        format!("online media warning (resource={:?}; stream={:?})", w.error().kind::<gst::ResourceError>(), w.error().kind::<gst::StreamError>())
                    } else { w.error().to_string() };
                    tracing::warn!(%message, "pipeline warning");
                }
                gst::MessageView::Eos(_) => {
                    let _ = tx
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .try_send(PipelineEvent::Eos);
                }
                gst::MessageView::Latency(_) => {
                    // GstBin does not act on this by itself: the application
                    // has to redistribute the latency. Hardware encoders often
                    // learn their delay only once the first caps arrive, after
                    // `Playing`; ignoring the message leaves the synchronised
                    // sinks dropping every frame as "late". The query walks
                    // the whole graph and this is a streaming thread, so it
                    // runs on its own thread instead.
                    if let Some(pipeline) = weak_pipeline.upgrade() {
                        std::thread::spawn(move || {
                            if let Err(err) = pipeline.recalculate_latency() {
                                tracing::debug!(%err, "latency could not be recalculated");
                            } else {
                                tracing::debug!("pipeline latency recalculated");
                            }
                        });
                    }
                }
                _ => {}
            }
            // `Drop` keeps the bus queue empty: there is no guaranteed main
            // loop on the protocol threads to consume it.
            gst::BusSyncReply::Drop
        });
    }

    // See the note in the documentation: leaving `Null` is a prerequisite for
    // sinks to accept clients before `Playing`.
    if let Err(err) = pipeline.set_state(gst::State::Ready) {
        let _ = pipeline.set_state(gst::State::Null);
        return Err(NdError::Gst(format!(
            "could not prepare the pipeline: {err}"
        )));
    }

    Ok((pipeline, rx))
}

/// A pipeline under observation: it knows whether the encoder really produced
/// frames.
///
/// This is what makes it possible to discover at runtime that a hardware
/// encoder "exists, accepts the configuration and encodes nothing" — the silent
/// failure the driver blacklist was trying to guess at.
/// Takes the pipeline to `NULL` when it goes out of scope.
///
/// Dropping a pipeline in `PLAYING` makes GStreamer complain with a `CRITICAL`
/// and leaves elements uncleaned — in practice, the app closed when Stop was
/// pressed. Calling `set_state(Null)` at the end of the function was not
/// enough: a `?` midway, or cancellation itself (which **drops the future**),
/// skipped that line. Tied to a lifetime, every exit path goes through here.
#[derive(Debug)]
pub struct PipelineGuard {
    pipeline: gst::Pipeline,
    armed: bool,
}

impl PipelineGuard {
    pub fn new(pipeline: gst::Pipeline) -> Self {
        Self {
            pipeline,
            armed: true,
        }
    }

    /// Transfer teardown to an owner that already retains the pipeline.
    pub fn disarm(mut self) {
        self.armed = false;
    }

    pub fn pipeline(&self) -> &gst::Pipeline {
        &self.pipeline
    }
}

impl Drop for PipelineGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Err(err) = self.pipeline.set_state(gst::State::Null) {
            tracing::debug!(%err, "failed to take the pipeline to NULL");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::source::escape_location;
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[tokio::test]
    async fn finite_music_ends_synthetic_tracks() {
        use futures::StreamExt;
        init().unwrap();
        let path = std::env::temp_dir().join(format!("nd-eos-{}.wav", std::process::id()));
        let make = gst::parse::launch(&format!(
            "audiotestsrc num-buffers=10 ! wavenc ! filesink location={}",
            escape_location(&path)
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let fixture_guard = PipelineGuard::new(make.clone());
        make.set_state(gst::State::Playing).unwrap();
        let msg = make
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(3),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .unwrap();
        assert_eq!(msg.type_(), gst::MessageType::Eos, "{msg:?}");
        drop(fixture_guard);
        let source = VideoSource::Media {
            source: crate::media::MediaSource::File(path.clone()),
            kind: crate::media::MediaKind::Music,
            title: "Regression".into(),
        };
        // Exercise the finite decoder, silence mixer and synthetic picture without network or encoders.
        let description = format!(
            "{} ! video/x-raw,width=320,height=240 ! fakesink sync=true {} ! audioconvert ! fakesink sync=true",
            source.description(),
            AudioSource::MediaFile.description()
        );
        let (pipeline, mut events) = build_pipeline(&description, 0).unwrap();
        let guard = PipelineGuard::new(pipeline.clone());
        pipeline.set_state(gst::State::Playing).unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(4), events.next())
            .await
            .expect("finite media must finish")
            .unwrap();
        assert!(matches!(event, PipelineEvent::Eos), "{event:?}");
        drop(guard);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn finite_video_with_or_without_audio_keeps_frames_and_ends() {
        use futures::StreamExt;
        init().unwrap();
        for with_audio in [false, true] {
            let path = std::env::temp_dir().join(format!(
                "nd-video-eos-{}-{with_audio}.mkv",
                std::process::id()
            ));
            let audio = if with_audio {
                "audiotestsrc num-buffers=22 ! audio/x-raw,rate=48000 ! vorbisenc ! mux."
            } else {
                ""
            };
            let description = format!(
                "videotestsrc num-buffers=15 ! video/x-raw,width=320,height=240,framerate=30/1 ! vp8enc deadline=1 ! matroskamux name=mux ! filesink location={} {audio}",
                escape_location(&path)
            );
            let make = gst::parse::launch(&description)
                .unwrap()
                .downcast::<gst::Pipeline>()
                .unwrap();
            let fixture = PipelineGuard::new(make.clone());
            make.set_state(gst::State::Playing).unwrap();
            let msg = make
                .bus()
                .unwrap()
                .timed_pop_filtered(
                    gst::ClockTime::from_seconds(5),
                    &[gst::MessageType::Eos, gst::MessageType::Error],
                )
                .unwrap();
            assert_eq!(msg.type_(), gst::MessageType::Eos, "{msg:?}");
            drop(fixture);
            let video = VideoSource::Media {
                source: crate::media::MediaSource::File(path.clone()),
                kind: crate::media::MediaKind::Video,
                title: "Test".into(),
            };
            let description = format!(
                "{} ! fakesink name=video-check sync=true {} ! audioconvert ! fakesink sync=true",
                video.description(),
                AudioSource::MediaFile.description()
            );
            let (pipeline, mut events) = build_pipeline(&description, 0).unwrap();
            let guard = PipelineGuard::new(pipeline.clone());
            let frames = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let count = frames.clone();
            pipeline
                .by_name("video-check")
                .unwrap()
                .static_pad("sink")
                .unwrap()
                .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                    count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    gst::PadProbeReturn::Ok
                });
            let start = std::time::Instant::now();
            pipeline.set_state(gst::State::Playing).unwrap();
            let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.next())
                .await
                .expect("video must finish")
                .unwrap();
            assert!(
                matches!(event, PipelineEvent::Eos),
                "audio={with_audio}: {event:?}"
            );
            assert_eq!(frames.load(std::sync::atomic::Ordering::Relaxed), 15);
            assert!(start.elapsed() >= std::time::Duration::from_millis(400));
            drop(guard);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn a_pipeline_guard_leaves_the_pipeline_in_null() {
        // Dropping a pipeline in PLAYING used to crash the app when pressing Stop.
        init().expect("gstreamer");
        let pipeline = gst::parse::launch("fakesrc num-buffers=1 ! fakesink")
            .expect("test pipeline")
            .downcast::<gst::Pipeline>()
            .expect("it is a Pipeline");
        pipeline.set_state(gst::State::Playing).expect("playing");

        let copia = pipeline.clone();
        {
            let _guard = PipelineGuard::new(pipeline);
        }

        assert_eq!(
            copia.current_state(),
            gst::State::Null,
            "o guarda precisa levar o pipeline a NULL ao sair de escopo"
        );
    }

    #[test]
    fn the_profile_travels_with_the_path_and_takes_cabac_with_it() {
        // Constrained baseline forbids CABAC, so the two must never be set
        // apart: a "high" profile encoded without CABAC would throw away most
        // of what raising it was for, and nothing would report the mistake.
        for encoder in [
            H264Encoder::X264,
            H264Encoder::VaH264,
            H264Encoder::VaapiH264,
        ] {
            let base = StreamConfig {
                encoder,
                ..Default::default()
            };
            assert!(
                encoder.encoder_description(&base).contains("cabac=false"),
                "{encoder:?} must not use CABAC on constrained baseline"
            );
            let high = StreamConfig {
                profile: H264Profile::High,
                ..base
            };
            assert!(
                encoder.encoder_description(&high).contains("cabac=true"),
                "{encoder:?} should use CABAC once the profile allows it"
            );
        }

        // Miracast pins the profile itself: the Wi-Fi Display specification
        // requires it, so it is not the caller's to raise.
        let high = StreamConfig {
            profile: H264Profile::High,
            ..Default::default()
        };
        let wfd = wfd_pipeline_description(
            &high,
            &VideoSource::Test,
            &WfdTransport::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 19000),
        );
        assert!(wfd.contains("profile=constrained-baseline"), "{wfd}");
        assert!(wfd.contains("cabac=false"), "{wfd}");

        // Cast mirroring takes whatever the session decided.
        assert!(
            mirror_pipeline_description(
                &high,
                &VideoSource::Test,
                CHROMECAST_MAX_RESOLUTION,
                false
            )
            .contains("profile=high"),
            "the mirroring caps have to follow the configuration"
        );
        let base = StreamConfig::default();
        assert!(
            mirror_pipeline_description(
                &base,
                &VideoSource::Test,
                CHROMECAST_MAX_RESOLUTION,
                false
            )
            .contains("profile=constrained-baseline"),
            "and default to what every path assumed before there was a choice"
        );
    }

    #[test]
    fn every_h264_path_gets_the_same_capture_and_encode_stage() {
        // The three paths that encode H.264 differ in what they do with the
        // result — Cast mirroring sends access units, Cast HTTP and Miracast
        // mux — and not in how the picture reaches the encoder. Keeping that
        // stage in one place is what stops a measured gain from landing on one
        // path and leaving the other two on the old code, which is exactly what
        // happened while this was two functions.
        let cfg = StreamConfig {
            width: 1920,
            height: 1080,
            encoder: H264Encoder::NvH264,
            ..Default::default()
        };
        let mirror = mirror_pipeline_description(&cfg, &VideoSource::Test, (2560, 1440), false);
        let http = ts_http_pipeline_description(
            &cfg,
            &VideoSource::Test,
            VideoTarget::UpTo((2560, 1440)),
            None,
        );
        let wfd = wfd_pipeline_description(
            &cfg,
            &VideoSource::Test,
            &WfdTransport::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 19000),
        );
        for desc in [&mirror, &http, &wfd] {
            assert!(
                desc.contains("format={ BGRA, BGRx, RGBA, RGBx, NV12 }"),
                "{desc}"
            );
            assert!(desc.contains("cudaupload ! nvh264enc"), "{desc}");
            assert!(desc.contains("capsfilter name=scale-caps"), "{desc}");
        }
        // What genuinely differs: Wi-Fi Display agreed one mode with the sink
        // in M3/M4 and may not be sent another. Nothing was promised on either
        // Cast path, so there the capture's own size stands.
        assert!(wfd.contains("width=1920,height=1080"), "{wfd}");
        for desc in [&mirror, &http] {
            assert!(
                desc.contains("width=[2,2560,2],height=[2,1440,2]"),
                "{desc}"
            );
        }
    }

    /// Validates the descriptions against GStreamer's real parser: it catches
    /// syntax errors and non-existent properties at test time rather than in
    /// front of the user. Encoders/muxers missing from the machine are
    /// tolerated.
    #[test]
    fn descriptions_parse_in_gstreamer() {
        if init().is_err() {
            return;
        }
        for enc in probe_encoders() {
            let cfg = StreamConfig {
                encoder: enc,
                ..Default::default()
            };
            for desc in [
                ts_http_pipeline_description(
                    &cfg,
                    &VideoSource::Test,
                    VideoTarget::Exact((cfg.width, cfg.height)),
                    None,
                ),
                wfd_pipeline_description(
                    &cfg,
                    &VideoSource::Test,
                    &WfdTransport::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 19000),
                ),
            ] {
                if let Err(err) = gst::parse::launch(&desc) {
                    let msg = err.to_string();
                    // "no element" = a plugin missing from the test machine,
                    // not our error. Anything else is a bug in the description.
                    assert!(
                        msg.contains("no element") || msg.contains("elemento"),
                        "{enc:?}: {msg}\n{desc}"
                    );
                }
            }
        }
    }
}
