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

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

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
    LOCAL_RTP_PORT, WFD_AUDIO_PID, WFD_VIDEO_PID, WfdTransport, wfd_pipeline_description,
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
    /// Non-fatal warning. Not sent: warnings are logged where they arise, so
    /// a burst of them cannot crowd errors and end of stream out of the
    /// bounded channel.
    Warning { message: String },
    /// End of stream.
    Eos,
}

/// The channel through which bus events reach the caller.
pub type PipelineEvents = futures::channel::mpsc::Receiver<PipelineEvent>;

/// Ends the synthetic tracks of a file once every decoded track has ended.
///
/// A file's silence (`file-silence`) and drawn picture (`file-picture`) are
/// live sources that never end by themselves; without this a song or a film
/// with no audio track would never reach end of stream. A decoded file with no
/// audio leaves `file-audio-queue` unlinked, which is ended at once. A photo
/// is held on screen by `imagefreeze` and never ends.
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
    let remaining = Arc::new(AtomicUsize::new(0));
    let weak = pipeline.downgrade();
    decoder.connect_pad_added(move |_, pad| {
        remaining.fetch_add(1, Ordering::SeqCst);
        let remaining = remaining.clone();
        let weak = weak.clone();
        let ended = AtomicBool::new(false);
        pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
            if info
                .event()
                .is_some_and(|event| event.type_() == gst::EventType::Eos)
                && !ended.swap(true, Ordering::SeqCst)
                && remaining.fetch_sub(1, Ordering::SeqCst) == 1
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

/// Builds a pipeline from a `gst-launch` description, applies the latency,
/// installs the diagnostics and leaves it in READY.
///
/// `latency_ms` of [`PIPELINE_LATENCY_AUTO`] lets GStreamer use the minimum
/// the pipeline reports. Errors and end of stream arrive on the returned
/// channel.
pub fn build_pipeline(
    description: &str,
    latency_ms: u64,
) -> Result<(gst::Pipeline, PipelineEvents)> {
    build_configured_pipeline(description, latency_ms, |_| Ok(()))
}

/// [`build_pipeline`], with `configure` run on the parsed pipeline before
/// READY, when elements may open devices or sockets.
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

    // Below the minimum the pipeline reports, sinks drop every buffer as late
    // and the receiver shows black; above it is only delay. A fixed value is
    // for headroom over an encoder with latency spikes.
    if latency_ms > 0 {
        pipeline.set_latency(gst::ClockTime::from_mseconds(latency_ms));
    }
    instrument_framerate(&pipeline);
    if std::env::var("BIGNETSCREEN_LATENCY").is_ok() {
        instrument_latency(&pipeline);
    }
    finish_file_sources(&pipeline);

    // Bounded: a pipeline nobody drains must not grow without limit, and
    // `try_send` drops what does not fit rather than block a streaming thread.
    let (tx, rx) = futures::channel::mpsc::channel(32);
    let tx = Mutex::new(tx);
    let weak_pipeline = pipeline.downgrade();
    if let Some(bus) = pipeline.bus() {
        bus.set_sync_handler(move |_, message| {
            forward_bus_message(message, remote_media, &tx, &weak_pipeline);
            // There is no main loop guaranteed on the protocol threads to pop
            // the bus, so nothing is left queued on it.
            gst::BusSyncReply::Drop
        });
    }

    // `multisocketsink` accepts a receiver's socket only once out of NULL, and
    // the HTTP server hands it one before the pipeline plays.
    if let Err(err) = pipeline.set_state(gst::State::Ready) {
        let _ = pipeline.set_state(gst::State::Null);
        return Err(NdError::Gst(format!(
            "could not prepare the pipeline: {err}"
        )));
    }

    Ok((pipeline, rx))
}

/// Turns one bus message into a [`PipelineEvent`], from a streaming thread.
fn forward_bus_message(
    message: &gst::Message,
    remote_media: bool,
    events: &Mutex<futures::channel::mpsc::Sender<PipelineEvent>>,
    pipeline: &gst::glib::WeakRef<gst::Pipeline>,
) {
    let send = |event| {
        let _ = events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .try_send(event);
    };
    match message.view() {
        gst::MessageView::Error(err) => {
            // Errors from an HTTP source can repeat credentials or redirected
            // URLs, so online media reports error categories only.
            let (message, details) = if remote_media {
                (
                    "Could not read or decode the online media. Check the link and connection."
                        .to_string(),
                    format!(
                        "resource={:?}; stream={:?}",
                        err.error().kind::<gst::ResourceError>(),
                        err.error().kind::<gst::StreamError>()
                    ),
                )
            } else {
                (
                    err.error().to_string(),
                    err.debug().map(|d| d.to_string()).unwrap_or_default(),
                )
            };
            tracing::error!(%message, %details, "pipeline error");
            send(PipelineEvent::Error {
                message,
                debug: details,
            });
        }
        gst::MessageView::Warning(warning) => {
            let message = if remote_media {
                format!(
                    "online media warning (resource={:?}; stream={:?})",
                    warning.error().kind::<gst::ResourceError>(),
                    warning.error().kind::<gst::StreamError>()
                )
            } else {
                warning.error().to_string()
            };
            tracing::warn!(%message, "pipeline warning");
        }
        gst::MessageView::Eos(_) => send(PipelineEvent::Eos),
        gst::MessageView::Latency(_) => {
            // A bin does not redistribute latency by itself. Hardware encoders
            // often learn their delay from the first caps, after PLAYING, and
            // without a recalculation the synchronised sinks drop every frame
            // as late. The query walks the whole graph, so it leaves this
            // streaming thread.
            if let Some(pipeline) = pipeline.upgrade() {
                std::thread::spawn(move || match pipeline.recalculate_latency() {
                    Ok(()) => tracing::debug!("pipeline latency recalculated"),
                    Err(err) => tracing::debug!(%err, "latency could not be recalculated"),
                });
            }
        }
        _ => {}
    }
}

/// Takes the pipeline to `Null` when it goes out of scope.
///
/// A pipeline dropped while `Playing` logs a GStreamer critical warning and
/// leaves its elements running. Tied to a scope, the teardown also runs on an early `?`
/// return and when a cancelled future is dropped mid-await.
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

/// Runs a description that writes a test file, until it ends.
#[cfg(test)]
pub(crate) fn write_test_media(description: &str) {
    init().unwrap();
    let pipeline = gst::parse::launch(description)
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
    let guard = PipelineGuard::new(pipeline);
    guard.pipeline().set_state(gst::State::Playing).unwrap();
    let message = guard
        .pipeline()
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(20),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("the fixture pipeline must finish");
    assert_eq!(message.type_(), gst::MessageType::Eos, "{message:?}");
}

#[cfg(test)]
mod tests {
    use super::source::quoted;
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[tokio::test]
    async fn finite_music_ends_synthetic_tracks() {
        use futures::StreamExt;
        init().unwrap();
        let path = std::env::temp_dir().join(format!("nd-eos-{}.wav", std::process::id()));
        crate::pipeline::write_test_media(&format!(
            "audiotestsrc num-buffers=10 ! wavenc ! filesink location={}",
            quoted(&path.to_string_lossy())
        ));
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
                quoted(&path.to_string_lossy())
            );
            crate::pipeline::write_test_media(&description);
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
            let frames = Arc::new(AtomicUsize::new(0));
            let count = frames.clone();
            pipeline
                .by_name("video-check")
                .unwrap()
                .static_pad("sink")
                .unwrap()
                .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                    count.fetch_add(1, Ordering::Relaxed);
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
            assert_eq!(frames.load(Ordering::Relaxed), 15);
            assert!(start.elapsed() >= std::time::Duration::from_millis(400));
            drop(guard);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn a_pipeline_guard_leaves_the_pipeline_in_null() {
        // A pipeline dropped while playing leaves its elements running.
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
        // The three H.264 paths differ in what they do with the result (Cast
        // mirroring sends access units, Cast HTTP and Miracast mux) and not in
        // how the picture reaches the encoder, so they share that stage.
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
        // What differs: Wi-Fi Display agreed one mode with the sink in M3/M4.
        // Nothing was promised on either Cast path, so the capture's own size
        // stands there.
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
