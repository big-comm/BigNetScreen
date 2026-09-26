//! Building the GStreamer pipelines.
//!
//! **This module is the most valuable asset of the port.** Latency ("no lag")
//! is ~90% pipeline configuration plus hardware encoding.
//!
//! Principles:
//! - short queues (a few ms) and a pipeline latency that is **actually
//!   applied** (`gst_pipeline_set_latency`, not just declared constants);
//! - a zero-latency encoder (no lookahead, no B-frames, CBR) — configured for
//!   **every** encoder, hardware ones included;
//! - prefer hardware encoding when it is stable, falling back to software when
//!   the hardware encoder stops producing frames at runtime;
//! - on the VA-API path, convert/scale on the GPU (`vapostproc`) so frames are
//!   not copied out to RAM and back.
//!
//! ## Bitrate units (a real source of bugs)
//!
//! `x264enc`, `vah264enc`, `vaapih264enc` and `nvh264enc` express `bitrate` in
//! **kbit/s**; `openh264enc` expresses it in **bit/s**. That is why each
//! encoder builds its own description in
//! [`H264Encoder::encoder_description`], instead of a generic
//! `format!("{} bitrate={}")` that fed the wrong unit to the software
//! fallback.

use std::net::IpAddr;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use gst::prelude::*;
use gstreamer as gst;

use crate::{NdError, Result};

// ------------------------------------------------------------------------
// Initialisation and construction
// ------------------------------------------------------------------------

/// The memoised result of `gst::init()`.
static GST_INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();

/// Initialises GStreamer exactly once per process.
pub fn init() -> Result<()> {
    GST_INIT
        .get_or_init(|| gst::init().map_err(|e| e.to_string()))
        .clone()
        .map_err(NdError::Gst)
}

/// Creates a `gst::Bin` from a `gst-launch`-style description.
pub fn build_bin(description: &str) -> Result<gst::Element> {
    init()?;
    gst::parse::bin_from_description(description, true)
        .map(|bin| bin.upcast())
        .map_err(|e| NdError::Gst(e.to_string()))
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

/// Asks the pipeline for the **minimum** latency it can sustain.
///
/// Only meaningful once `Playing`. This is the number that separates
/// "aggressive tuning" from "the sink goes black": setting anything below this
/// minimum makes sinks drop late buffers. Querying it and logging the value is
/// what keeps latency from being picked by guesswork again.
pub fn query_min_latency_ms(pipeline: &gst::Pipeline) -> Option<u64> {
    let mut query = gst::query::Latency::new();
    if !pipeline.query(&mut query) {
        return None;
    }
    let (live, min, max) = query.result();
    let min_ms = min.mseconds();
    tracing::info!(
        live,
        min_ms,
        max_ms = max.map(|m| m.mseconds()),
        "minimum latency the pipeline can sustain"
    );
    Some(min_ms)
}

/// Counts the frames the encoder actually produces, once a second.
///
/// The question it answers cannot be answered from the other end: a receiver
/// showing a stuttering picture may be losing frames on the network, or may
/// never have been sent them. This says which — it counts what left the
/// encoder, before anything can be lost.
///
/// Enabled by `BIGNETSCREEN_FPS_LOG=1`, because it is a diagnostic and its
/// place is in a bug report, not in every session's log.
pub fn instrument_framerate(pipeline: &gst::Pipeline) {
    use gst::prelude::*;

    if std::env::var("BIGNETSCREEN_FPS_LOG").as_deref() != Ok("1") {
        return;
    }
    // Both ends of the pipeline, because they answer different questions. A
    // compositor only repaints what changed, so a still screen legitimately
    // produces almost no capture buffers — and `pipewiresrc`'s keepalive then
    // resends the last one, which is a real frame rate of one per second. Told
    // apart from an encoder that cannot keep up only by counting both.
    for (element, pad, label) in [
        (
            CAPTURE_SOURCE_NAME,
            "src",
            "frames arriving from the compositor",
        ),
        (ENCODER_NAME, "src", "frames leaving the encoder"),
    ] {
        let Some(pad) = pipeline
            .by_name(element)
            .and_then(|element| element.static_pad(pad))
        else {
            tracing::debug!(element, "not in this pipeline; frame rate not instrumented");
            continue;
        };
        let frames = std::sync::atomic::AtomicU64::new(0);
        let since = std::sync::Mutex::new(std::time::Instant::now());
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                let bytes = buffer.size() as u64;
                let count = frames.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                let mut mark = since.lock().unwrap_or_else(|e| e.into_inner());
                let elapsed = mark.elapsed();
                if elapsed >= std::time::Duration::from_secs(1) {
                    tracing::info!(
                        fps = format!("{:.1}", count as f64 / elapsed.as_secs_f64()),
                        last_frame_bytes = bytes,
                        "{label}"
                    );
                    frames.store(0, std::sync::atomic::Ordering::Relaxed);
                    *mark = std::time::Instant::now();
                }
            }
            gst::PadProbeReturn::Ok
        });
    }
}

/// Changes a running encoder's bitrate, in kbit/s.
///
/// Lives here because the unit is the encoder's and they disagree — the same
/// disagreement the module header warns about, which once fed kbit/s to an
/// element measuring bit/s. A caller adapting the rate should not have to know
/// which is which.
///
/// `false` when this encoder cannot be retuned while it runs: `v4l2h264enc`
/// carries its bitrate inside `extra-controls`, which is not a property to be
/// set on a playing pipeline. The session keeps the rate it started with,
/// which is what it did before any of this existed.
pub fn set_encoder_bitrate(pipeline: &gst::Pipeline, encoder: H264Encoder, kbps: u32) -> bool {
    use gst::prelude::*;

    let Some(element) = pipeline.by_name(ENCODER_NAME) else {
        return false;
    };
    match encoder {
        H264Encoder::X264 | H264Encoder::VaH264 | H264Encoder::VaapiH264 | H264Encoder::NvH264 => {
            element.set_property_from_str("bitrate", &kbps.to_string());
            true
        }
        H264Encoder::OpenH264 => {
            element.set_property_from_str("bitrate", &(kbps as u64 * 1000).to_string());
            true
        }
        H264Encoder::V4l2H264 => false,
    }
}

/// Counts buffers leaving a named element, for as long as the pipeline lives.
///
/// Two atomics, installed unconditionally, because the one question every
/// report of a slow picture turns on — were those frames ever made? — took a
/// special run with `BIGNETSCREEN_FPS_LOG=1` to answer, three times over. A
/// compositor delivering 7 frames a second and an encoder keeping up with it
/// look identical from the far end of the pipeline.
///
/// `None` when the element is not in this pipeline, which is normal: not every
/// path has a capture.
pub fn count_buffers(pipeline: &gst::Pipeline, element: &str) -> Option<Arc<AtomicU64>> {
    use gst::prelude::*;

    let pad = pipeline.by_name(element)?.static_pad("src")?;
    let count = Arc::new(AtomicU64::new(0));
    let counter = count.clone();
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        counter.fetch_add(1, Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    });
    Some(count)
}

/// The device buffer asked of `pulsesrc`, in microseconds.
///
/// The element's own default is 200 ms, documented as "the maximum latency
/// that the source reports". A pipeline adopts the largest latency any of its
/// sources reports and the screen capture reports almost none, so that default
/// made the audio branch decide how late the picture was — reported from use
/// as the screen arriving noticeably sooner with audio switched off, and
/// confirmed by asking for less and watching the delay go.
///
/// Forty milliseconds, not the ten that was measured working. Ten is one read
/// period — `latency-time` asks for ten and gets one buffer's worth of room,
/// which is no headroom at all, and this already logs an overrun at startup
/// with two hundred. It held on one fast machine; this ships to slow ones,
/// where the cost is not latency but audio breaking up. Four periods is the
/// usual floor for low-latency capture and still cuts the old default by five.
///
/// `BIGNETSCREEN_AUDIO_BUFFER_MS` overrides it either way.
const CAPTURE_AUDIO_BUFFER_MS: u32 = 40;

fn capture_audio_buffer_us() -> u32 {
    std::env::var("BIGNETSCREEN_AUDIO_BUFFER_MS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(CAPTURE_AUDIO_BUFFER_MS)
        .clamp(5, 500)
        * 1_000
}

/// The `buffer-time=` property as `pulsesrc` spells it.
fn audio_buffer_property() -> String {
    format!(" buffer-time={}", capture_audio_buffer_us())
}

/// The capture element's name, so its negotiated caps can be read back.
pub const CAPTURE_SOURCE_NAME: &str = "capture";

/// How long `pipewiresrc` waits before resending the last frame, in ms.
///
/// One frame at 30 Hz, two at 60. It is the frame rate a still screen gets,
/// because nothing else produces buffers when the compositor has nothing to
/// repaint — at the 1000 ms it used to be, typing in a terminal updated the
/// picture about once a second.
///
/// `BIGNETSCREEN_CAPTURE_KEEPALIVE_MS` overrides it, and exists for an open
/// question rather than for tuning. A resent frame is not a copy of the real
/// one: `gstpipewiresrc` rewrites its timestamp to the clock, while a real
/// frame keeps the one the compositor gave it. If those two disagree, a real
/// frame arriving between resends can look older than what already went out —
/// which would show up as exactly the reported symptom, a keystroke taking far
/// too long to appear while a moving pointer stays fluid. Comparing 33 against
/// a much larger value while typing is what tells that apart from the
/// compositor simply not sending the damage.
fn capture_keepalive_ms() -> u32 {
    std::env::var("BIGNETSCREEN_CAPTURE_KEEPALIVE_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map(|ms: u32| ms.clamp(1, 5_000))
        .unwrap_or(CAPTURE_KEEPALIVE_MS)
}

const CAPTURE_KEEPALIVE_MS: u32 = 33;

/// The video capsfilter's name, so its size can be read back or narrowed after
/// the pipeline has negotiated.
pub const SCALE_CAPS: &str = "scale-caps";

/// What size to encode at.
///
/// Two shapes because two situations, and telling them apart is the whole
/// reason this is a type rather than a pair of numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoTarget {
    /// Encode at exactly this. For a link that agreed on a mode and would
    /// refuse anything else — Miracast negotiates one with the sink, and a Cast
    /// receiver whose ANSWER declares a smaller decoder has done the same.
    Exact((u32, u32)),
    /// Encode at whatever the capture is, up to this ceiling.
    ///
    /// The right shape wherever nothing was promised, because the portal
    /// announces a size in the compositor's coordinate space rather than
    /// pixels: measured, 2328x1310 announced for a 2560x1440 panel at 110%
    /// scale. Pinning the announced number resamples every frame away from the
    /// size it arrived in. A range lets the scaler stand aside.
    UpTo((u32, u32)),
}

impl VideoTarget {
    /// The `width`/`height` fields, fixed or as a range.
    fn caps_dimensions(self) -> String {
        match self {
            VideoTarget::Exact((w, h)) => format!("width={w},height={h}"),
            VideoTarget::UpTo((w, h)) => {
                // 4:2:0 H.264 needs even dimensions, including odd-sized windows.
                format!(
                    "width=[2,{},2],height=[2,{},2]",
                    w.max(2) & !1,
                    h.max(2) & !1
                )
            }
        }
    }
}

/// Whether to keep mirroring frames on the graphics card end to end.
///
/// Off unless `BIGNETSCREEN_GPU_PATH=1`, and measured to be the right default.
///
/// The path works: KWin hands over a real DMA-BUF (`drm-format=AR24` with
/// NVIDIA's block-linear modifier `0x0300000000606014`), `glupload` imports it
/// and NVENC takes the texture, so the picture never crosses to the CPU. It is
/// also **slower**. On a GeForce with that modifier, against the system-memory
/// path carrying BGRx:
///
/// | | system memory | GPU |
/// | --- | --- | --- |
/// | frames from the compositor | 45-53/s | 16-26/s |
/// | frames leaving the encoder | ~60/s | 21-41/s |
///
/// The cost is not the import and not our handling of it. Taking 300 frames at
/// 2560x1440 apart stage by stage:
///
/// | pipeline | wall clock |
/// | --- | --- |
/// | generate only | 0.55s |
/// | + `glupload` | 0.72s |
/// | + `nvh264enc` from system memory | 1.44s |
/// | + `nvh264enc` from GL memory | 4.74s |
/// | `glupload ! cudaupload ! nvh264enc` | 3.74s |
///
/// The upload costs 0.17s and the encode 0.89s, but encoding *from a GL
/// texture* costs 4.02s — four and a half times as much, and routing through
/// `cudaupload` instead of letting the encoder do it saves only a third of that.
/// The expense is the GL/CUDA bridge itself, wherever it is crossed.
///
/// There is no way around it here: GStreamer 1.28.6 ships no nvcodec element
/// that accepts `memory:DMABuf`, so GL is the only import route, and the
/// import is what makes the frame worth having.
///
/// Kept, switch and all, because the result is hardware-specific: another card,
/// another compositor or a linear modifier could invert it, and the next person
/// to wonder should be able to measure in one run rather than build this again.
/// Promoting it to a default would also need a fallback, since a machine
/// without the DMA-BUF or the interop does not start at all.
pub fn gpu_path_requested() -> bool {
    std::env::var("BIGNETSCREEN_GPU_PATH").as_deref() == Ok("1")
}

/// The longest input gap `videorate` will fill with repeats, in nanoseconds.
///
/// Comfortably more than the keepalive interval, so ordinary jitter is still
/// smoothed. Anything past it is the capture having stopped, and repeating a
/// frozen frame at full resolution is the most expensive way to display
/// nothing.
const MAX_DUPLICATION_NS: u64 = 500_000_000;

/// The size the encoder will really be fed, once the pipeline has negotiated.
///
/// This is the number the Cast OFFER has to carry, and it cannot be known any
/// earlier: the portal announces a compositor-space size that is not pixels,
/// and the only other way to ask — a second PipeWire stream — is refused by
/// the session manager ("target not found"), because the portal's node has no
/// session item to look up. So the pipeline is built first and asked here.
pub fn negotiated_video_size(
    pipeline: &gst::Pipeline,
    deadline: std::time::Instant,
) -> Option<(u32, u32)> {
    use gst::prelude::*;

    let pad = pipeline
        .by_name(SCALE_CAPS)
        .and_then(|caps| caps.static_pad("src"))?;
    loop {
        if let Some(caps) = pad.current_caps() {
            let s = caps.structure(0)?;
            let (w, h) = (s.get::<i32>("width").ok()?, s.get::<i32>("height").ok()?);
            if w > 0 && h > 0 {
                return Some((w as u32, h as u32));
            }
        }
        if std::time::Instant::now() >= deadline {
            tracing::info!("the capture did not negotiate a size in time");
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Narrows the mirroring capsfilter to one exact size.
///
/// Only for a receiver whose ANSWER declares a decoder smaller than the screen.
/// Left alone otherwise, so the capture reaches the encoder untouched.
pub fn pin_video_size(pipeline: &gst::Pipeline, cfg: &StreamConfig) -> Result<()> {
    use gst::prelude::*;

    let filter = pipeline
        .by_name(SCALE_CAPS)
        .ok_or_else(|| NdError::Gst("the mirroring capsfilter is missing".into()))?;
    // Narrow the size and nothing else: the format stays a list so the picture
    // still reaches the encoder in whatever it was captured as, even when the
    // receiver has forced a scale.
    let caps = format!(
        "video/x-raw,format={fmt},width={w},height={h},framerate={fps}/1",
        fmt = cfg.encoder.accepted_formats(),
        w = cfg.width,
        h = cfg.height,
        fps = cfg.fps,
    );
    let caps = caps
        .parse::<gst::Caps>()
        .map_err(|err| NdError::Gst(format!("mirroring caps: {err}")))?;
    filter.set_property("caps", &caps);
    Ok(())
}

/// The size PipeWire is really delivering, from the capture's own pad.
///
/// Read alongside [`negotiated_video_size`] so one line can say whether the
/// scaler is doing anything: if the two agree, the capture reaches the encoder
/// untouched.
pub fn delivered_capture_size(pipeline: &gst::Pipeline) -> Option<(u32, u32)> {
    use gst::prelude::*;

    let caps = pipeline
        .by_name(CAPTURE_SOURCE_NAME)?
        .static_pad("src")?
        .current_caps()?;
    let s = caps.structure(0)?;
    let (w, h) = (s.get::<i32>("width").ok()?, s.get::<i32>("height").ok()?);
    (w > 0 && h > 0).then_some((w as u32, h as u32))
}

/// What the capture settled on, and whether it could have done better.
///
/// Returns the negotiated caps and what the source says when asked for a GPU
/// buffer specifically.
///
/// The two answer different questions. Negotiated caps of plain `video/x-raw`
/// mean the frame is in system memory, but not why: the compositor may offer
/// nothing else, or our own scaler and converter may have forced the download
/// by being unable to accept a GPU buffer. Asking with a `memory:DMABuf` filter
/// separates those — `pipewiresrc` builds that feature from the modifiers the
/// producer advertises (`gstpipewireformat.c`), so an empty answer means there
/// is nothing to import and a non-empty one means importing it is available to
/// be taken.
pub fn capture_memory(pipeline: &gst::Pipeline) -> Option<(String, String)> {
    use gst::prelude::*;

    let pad = pipeline.by_name(CAPTURE_SOURCE_NAME)?.static_pad("src")?;
    let negotiated = pad.current_caps()?.to_string();
    let dmabuf = gst::Caps::builder("video/x-raw")
        .features(["memory:DMABuf"])
        .build();
    let offered = pad.query_caps(Some(&dmabuf));
    let dmabuf = if offered.is_empty() {
        "none".to_string()
    } else {
        offered.to_string()
    };
    Some((negotiated, dmabuf))
}

fn pad_running_time(pad: &gst::Pad, pts: gst::ClockTime) -> Option<gst::ClockTime> {
    let event = pad.sticky_event::<gst::event::Segment>(0)?;
    event
        .segment()
        .downcast_ref::<gst::ClockTime>()?
        .to_running_time(pts)
}

/// Measure sender-side frame age using each pad's SEGMENT, not raw encoder PTS.
/// This diagnostic excludes network transit and receiver buffering/decoding.
pub fn instrument_latency(pipeline: &gst::Pipeline) {
    use gst::prelude::*;

    // A probe on **every** element, not just the sink: knowing the total is
    // 83 ms says nothing about where those milliseconds are born. With the
    // frame's age at each stage's output, the step between two neighbours is
    // the cost of that stage.
    let mut found = 0;
    for element in pipeline.iterate_elements().into_iter().flatten() {
        let Some(pad) = element
            .static_pad("src")
            .or_else(|| element.static_pad("sink"))
        else {
            continue;
        };
        let name = element.name().to_string();
        let state = std::sync::Mutex::new((std::time::Instant::now(), 0u64, 0u64, 0u64));

        pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            let Some(buffer) = info.buffer() else {
                return gst::PadProbeReturn::Ok;
            };
            let Some(pts) = buffer.pts() else {
                return gst::PadProbeReturn::Ok;
            };
            // Frame age = now (running time) − the frame's running time.
            let Some(parent) = pad.parent_element() else {
                return gst::PadProbeReturn::Ok;
            };
            let (Some(clock), Some(base)) = (parent.clock(), parent.base_time()) else {
                return gst::PadProbeReturn::Ok;
            };
            let Some(now) = clock.time().checked_sub(base) else {
                return gst::PadProbeReturn::Ok;
            };
            let Some(running_time) = pad_running_time(pad, pts) else {
                return gst::PadProbeReturn::Ok;
            };
            let Some(age) = now.checked_sub(running_time) else {
                return gst::PadProbeReturn::Ok;
            };

            let ms = age.mseconds();
            if let Ok(mut guard) = state.lock() {
                let (ref mut last, ref mut n, ref mut sum, ref mut worst) = *guard;
                *n += 1;
                *sum += ms;
                *worst = (*worst).max(ms);
                if last.elapsed() >= std::time::Duration::from_secs(5) {
                    tracing::info!(
                        sink = %name,
                        mean_ms = *sum / (*n).max(1),
                        worst_ms = *worst,
                        frames = *n,
                        "latency inside the pipeline (no network, no receiver)"
                    );
                    *last = std::time::Instant::now();
                    *n = 0;
                    *sum = 0;
                    *worst = 0;
                }
            }
            gst::PadProbeReturn::Ok
        });
        found += 1;
    }
    tracing::debug!(sinks = found, "latency probes installed");
}

/// End synthetic tracks when all finite decoder tracks reach EOS.
fn finish_file_sources(pipeline: &gst::Pipeline) {
    let Some(decoder) = pipeline.by_name("filedec") else {
        return;
    };
    if let Some(queue) = pipeline.by_name("file-audio-queue") {
        let weak = queue.downgrade();
        decoder.connect_no_more_pads(move |_| {
            if let Some(queue) = weak.upgrade() {
                if let Some(pad) = queue.static_pad("sink") {
                    if !pad.is_linked() {
                        pad.send_event(gst::event::Eos::new());
                    }
                }
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
            {
                if let Some(pipeline) = weak.upgrade() {
                    pipeline.call_async(|pipeline| {
                        for name in ["file-silence", "file-picture"] {
                            if let Some(source) = pipeline.by_name(name) {
                                source.send_event(gst::event::Eos::new());
                            }
                        }
                    });
                }
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

// ------------------------------------------------------------------------
// Low-latency tuning
// ------------------------------------------------------------------------

/// Automatic latency: the pipeline uses the minimum it reports itself.
///
/// Measured in the field on a real 1080p60 Miracast link: **41 ms**. The 20 ms
/// this module once forced sat *below* that minimum — and the effect was not
/// "faster", it was the sink receiving data and displaying nothing.
pub const PIPELINE_LATENCY_AUTO: u64 = 0;
/// For compatibility: "low" latency now means automatic.
pub const PIPELINE_LATENCY_MS: u64 = PIPELINE_LATENCY_AUTO;
/// `openh264enc` with `usage-type=screen` has latency spikes after scene
/// changes; it alone justifies fixed headroom (the reference C project's
/// value).
pub const OPENH264_PIPELINE_LATENCY_MS: u64 = 500;
/// The RTP jitter buffer's latency (`rtpbin`), when responding quickly.
pub const RTP_LATENCY_MS: u64 = 20;

/// The RTP jitter buffer to use, given the latency profile.
///
/// This is the buffer that actually absorbs network variance on the Miracast
/// path: frames arriving unevenly are held here and released on schedule. It
/// costs exactly its own size in delay, pointer included.
pub fn rtp_latency_ms() -> u64 {
    if crate::latency::is_film() {
        crate::latency::FILM_RTP_LATENCY_MS
    } else {
        RTP_LATENCY_MS
    }
}
/// The video queue before the encoder: few buffers, ~1 frame.
pub const VIDEO_QUEUE_BUFFERS: u32 = 3;
/// The same, in milliseconds.
pub const VIDEO_QUEUE_MS: u64 = 30;
/// The audio queue on the muxed path (the reference C used 100000 — a bug).
///
/// Small on purpose, and here leaking is the lesser evil: `mpegtsmux`
/// interleaves the tracks by timestamp, so **video waits for audio**. Every bit
/// of slack given to this queue becomes picture delay — at 200 ms Miracast left
/// the measured ~40 ms behind and the lag became visible when moving the
/// mouse.
pub const AUDIO_QUEUE_BUFFERS: u32 = 4;
/// The same, in milliseconds.
pub const AUDIO_QUEUE_MS: u64 = 40;
/// The Cast mirroring audio queue, in frames.
///
/// There is no muxer here: each track leaves through its own `appsink`, and the
/// receiver aligns the two by their timestamps. With nobody waiting on the
/// audio, the slack costs no picture latency — so this queue can be generous
/// and, above all, **does not leak**: dropping samples does not bring the sound
/// forward, it opens a hole in it.
pub const MIRROR_AUDIO_QUEUE_BUFFERS: u32 = 64;
/// The same, in milliseconds.
pub const MIRROR_AUDIO_QUEUE_MS: u64 = 200;
/// Local RTP port used as the source of the WFD stream.
pub const LOCAL_RTP_PORT: u16 = 16384;

/// The WFD path's pipeline latency: automatic.
///
/// The C project pinned 500 ms for every encoder because of openh264's spikes.
/// Measured in the field, the real minimum with x264/VA is **41 ms** — pinning
/// 500 ms adds nearly half a second of lag, visible when moving the mouse. The
/// fixed headroom now lives only where it is needed
/// ([`OPENH264_PIPELINE_LATENCY_MS`]).
pub const WFD_PIPELINE_LATENCY_MS: u64 = PIPELINE_LATENCY_AUTO;

/// The video PES PID the Wi-Fi Display specification requires (0x1011).
///
/// `mpegtsmux` assigns PIDs automatically when the generic `mux.` pad is used;
/// WFD sinks look for video on **this** PID. It is the difference between the
/// projector showing the picture and sitting black while receiving data.
pub const WFD_VIDEO_PID: u16 = 0x1011;
/// The audio PES PID the specification requires (0x1100).
pub const WFD_AUDIO_PID: u16 = 0x1100;

// ------------------------------------------------------------------------
// Encoders
// ------------------------------------------------------------------------

/// The H.264 feature set a path's receiver is known to decode.
///
/// One decision with two consequences — the profile written into the caps and
/// whether the encoder may use CABAC — so it is one value rather than two that
/// can disagree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum H264Profile {
    /// No CABAC, no 8x8 transform. Required by the Wi-Fi Display profile, and
    /// the safe assumption for a receiver that declared nothing.
    #[default]
    ConstrainedBaseline,
    /// CABAC and the 8x8 transform. Measured at a fixed QP 26 against
    /// constrained baseline, on the same NVENC and the same two clips:
    /// **13.5% fewer bits** on screen-like content, **12.6%** on dense detail.
    /// At a fixed bitrate that is the same 13% spent on the picture instead.
    High,
}

impl H264Profile {
    /// The name `video/x-h264,profile=…` expects.
    pub fn caps_name(self) -> &'static str {
        match self {
            H264Profile::ConstrainedBaseline => "constrained-baseline",
            H264Profile::High => "high",
        }
    }

    /// May the encoder use CABAC? Constrained baseline forbids it.
    pub fn cabac(self) -> bool {
        matches!(self, H264Profile::High)
    }
}

/// Candidate H.264 encoders, in order of preference.
///
/// ## Why `vulkanh264enc` is not one of them
///
/// GStreamer 1.28.6 ships it, and it works. Measured here against this list's
/// own software encoder, 1280x720, same content and target bitrate:
///
/// | | x264 | vulkanh264enc |
/// |---|---|---|
/// | encoder latency | 0 ms | 4 frames — 133 ms at 30 Hz, 66 ms at 60 Hz |
/// | slices per frame | one per thread | 1 |
/// | input it accepts | the capture's own BGRx | `VulkanImage` NV12 only |
///
/// The four-frame depth is fixed: `num-ref-frames=1` and `b-frames=0` do not
/// move it. That is the latency this project spent a week taking out of the
/// path, and no amount of encoder quality buys it back.
///
/// Converting the capture for it is worse than it looks. `vulkancolorconvert`
/// would do BGRx to NV12 on the card, and **segfaults** — reproducibly, on
/// this driver, within five frames. So the conversion has to happen on the
/// CPU, which is the cost the hardware path exists to avoid.
///
/// And a crash is not a fallback. [`working_encoder`] probes each candidate
/// and moves on when one produces no frame; it cannot survive one that takes
/// the process down, and since the session now lives in a service, that ends
/// somebody's cast rather than one pipeline.
///
/// Worth revisiting when `vulkancolorconvert` stops crashing *and* the
/// encoder's four-frame depth is gone. Either alone is not enough.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum H264Encoder {
    /// `vah264enc` (the modern `va` plugin) — the best option on Intel/AMD.
    VaH264,
    /// `vaapih264enc` (plugin `vaapi` legado).
    VaapiH264,
    /// `nvh264enc` (NVENC, placas NVIDIA).
    NvH264,
    /// `v4l2h264enc` (encoders V4L2 stateful: RPi, Rockchip, Amlogic…).
    V4l2H264,
    /// `x264enc` software, `tune=zerolatency`.
    X264,
    /// `openh264enc` in software (a last resort; permissive licence).
    OpenH264,
}

impl H264Encoder {
    /// Every known encoder (for scanning the registry).
    pub const ALL: [H264Encoder; 6] = [
        H264Encoder::VaH264,
        H264Encoder::VaapiH264,
        H264Encoder::NvH264,
        H264Encoder::V4l2H264,
        H264Encoder::X264,
        H264Encoder::OpenH264,
    ];

    /// Nome do elemento GStreamer.
    pub fn element(self) -> &'static str {
        match self {
            H264Encoder::VaH264 => "vah264enc",
            H264Encoder::VaapiH264 => "vaapih264enc",
            H264Encoder::NvH264 => "nvh264enc",
            H264Encoder::V4l2H264 => "v4l2h264enc",
            H264Encoder::X264 => "x264enc",
            H264Encoder::OpenH264 => "openh264enc",
        }
    }

    /// Peso de prioridade (maior = preferido).
    pub fn priority(self) -> u32 {
        match self {
            H264Encoder::VaH264 => 100,
            H264Encoder::NvH264 => 95,
            H264Encoder::VaapiH264 => 90,
            H264Encoder::V4l2H264 => 80,
            H264Encoder::X264 => 50,
            H264Encoder::OpenH264 => 40,
        }
    }

    /// Hardware encoding?
    pub fn is_hardware(self) -> bool {
        !matches!(self, H264Encoder::X264 | H264Encoder::OpenH264)
    }

    /// Uses the VA-API stack (allowing conversion/scaling on the GPU with `vapostproc`).
    pub fn is_va(self) -> bool {
        matches!(self, H264Encoder::VaH264 | H264Encoder::VaapiH264)
    }

    /// The raw video format this encoder prefers on its input.
    ///
    /// Hardware encoders consume NV12 natively; handing them I420 forces an
    /// extra conversion inside the driver.
    pub fn preferred_format(self) -> &'static str {
        if self.is_hardware() {
            "NV12"
        } else {
            "I420"
        }
    }

    /// Every raw format this encoder takes, as a caps list.
    ///
    /// A list rather than one choice, so negotiation can settle on whatever
    /// the capture already is and leave the converter a passthrough. The
    /// portal delivers BGRA and NVENC accepts BGRA, converting on the GPU as
    /// part of encoding it — asking for NV12 first buys nothing and spends a
    /// full-frame pass over the CPU to do it. Measured over 300 frames at
    /// 2560x1440: 2.76s of CPU against 1.21s, and twice the wall clock.
    ///
    /// Only NVENC is listed from measurement, because it is the only hardware
    /// encoder installed on the machine this was verified on. The others keep
    /// the single format they had: a list nobody checked is a guess, and the
    /// cost of guessing wrong here is a receiver that gets nothing.
    pub fn accepted_formats(self) -> &'static str {
        match self {
            H264Encoder::NvH264 => "{ BGRA, BGRx, RGBA, RGBx, NV12 }",
            _ => self.preferred_format(),
        }
    }

    /// The pipeline latency suited to this encoder.
    pub fn pipeline_latency_ms(self) -> u64 {
        match self {
            H264Encoder::OpenH264 => OPENH264_PIPELINE_LATENCY_MS,
            _ => PIPELINE_LATENCY_MS,
        }
    }

    /// The encoder element's full description, already carrying every
    /// low-latency property and the bitrate **in each encoder's correct
    /// unit**.
    pub fn encoder_description(self, cfg: &StreamConfig) -> String {
        let kbps = cfg.scaled_bitrate_kbps();
        // `0` means the caller wants no schedule at all: key frames come when
        // they are asked for. Each encoder spells that differently, and only
        // the two verified on this machine are told directly — the rest keep a
        // finite distance, because a driver that reads `0` as "every frame is
        // an I-frame" would ruin a session nobody here can test.
        let gop = match (cfg.gop(), self) {
            (0, H264Encoder::NvH264) => "-1".to_string(),
            (0, H264Encoder::X264) => "0".to_string(),
            (0, _) => (cfg.fps.max(1) * 10).to_string(),
            (frames, _) => frames.to_string(),
        };
        let cabac = cfg.profile.cabac();
        match self {
            H264Encoder::X264 => {
                let intra = if cfg.intra_refresh { "true" } else { "false" };
                // x264 asks for the buffer in milliseconds. The 50 ms this
                // path has always used is a little under two frames at 30 Hz,
                // which is why the oversized key frame never showed up here.
                let vbv = match cfg.vbv_ms() {
                    0 => 50,
                    ms => ms,
                };
                format!(
                    // `threads` decides how many slices each frame is cut
                    // into, one per thread, and `tune=zerolatency` slices
                    // whatever this says. `0` means one per core, so a
                    // sixteen-core machine was sending eleven slices a frame.
                    //
                    // Measured at 1280x720, same content and bitrate:
                    //
                    // | threads | slices | bytes  | encoder latency |
                    // |---------|--------|--------|-----------------|
                    // | 1       | 1      | 42 418 | 0 ms            |
                    // | 2       | 2      | 43 904 | 0 ms            |
                    // | 4       | 4      | 46 000 | 0 ms            |
                    // | 0 (11)  | 11     | 49 883 | 0 ms            |
                    //
                    // and at 2560x1440 the throughput is 60 fps on one thread,
                    // 68 on three, 69 on all sixteen. So past a handful of
                    // threads every extra slice costs bits and buys nothing:
                    // four of them reach 68 of those 69 frames while spending
                    // 8% fewer bits than eleven.
                    //
                    // `sliced-threads=false` would give one slice a frame and
                    // is the trap here: it puts x264 back on frame threading,
                    // which the same measurement showed declaring **700 ms**
                    // of latency. One slice is not worth that, and the comment
                    // this replaces claimed we already had it.
                    "x264enc name=enc tune=zerolatency speed-preset=ultrafast \
                     rc-lookahead=0 sync-lookahead=0 bframes=0 b-adapt=false \
                     threads={threads} aud=true cabac={cabac} ref=1 \
                     pass=cbr vbv-buf-capacity={vbv} intra-refresh={intra} \
                     key-int-max={gop} bitrate={kbps}",
                    threads = software_encode_threads()
                )
            }
            // `bitrate` in kbps. No B-frames and CBR: the VA-API defaults
            // (VBR + B-frames) reorder frames and leave the "fast" path with
            // more latency than x264.
            H264Encoder::VaH264 => format!(
                "vah264enc name=enc rate-control=cbr bitrate={kbps} key-int-max={gop} \
                 b-frames=0 ref-frames=1 num-slices=1 target-usage=6 \
                 aud=true cabac={cabac}"
            ),
            H264Encoder::VaapiH264 => format!(
                "vaapih264enc name=enc rate-control=cbr bitrate={kbps} keyframe-period={gop} \
                 max-bframes=0 refs=1 num-slices=1 quality-level=7 cabac={cabac} aud=true"
            ),
            // NVENC asks for the buffer in kbits. Its own default is a whole
            // second, which is what let a key frame reach 908 KB on a 10 Mbit
            // stream (see [`StreamConfig::vbv_frames`]).
            //
            // The property is only written when a path asks for it: it is
            // "conditionally available" on this element, so a GPU that does
            // not offer it would fail to build — and a path that never needed
            // the limit should not lose its hardware encoder over it.
            H264Encoder::NvH264 => {
                let vbv = match cfg.vbv_frames {
                    0 => String::new(),
                    frames => format!(
                        "vbv-buffer-size={} ",
                        (u64::from(kbps) * u64::from(frames) / u64::from(cfg.fps.max(1))).max(1)
                    ),
                };
                format!(
                    "nvh264enc name=enc preset=low-latency-hq rc-mode=cbr bitrate={kbps} \
                     {vbv}gop-size={gop} bframes=0 zerolatency=true aud=true"
                )
            }
            // V4L2 stateful: as propriedades ficam em `extra-controls`.
            H264Encoder::V4l2H264 => format!(
                "v4l2h264enc name=enc extra-controls=\"controls,h264_profile={v4l2_profile},\
                 h264_i_frame_period={gop},video_bitrate={bps},\
                 repeat_sequence_header=1\"",
                bps = kbps as u64 * 1000,
                v4l2_profile = match cfg.profile {
                    H264Profile::ConstrainedBaseline => 0, // Preserve the legacy WFD control.
                    H264Profile::High => 4,                // V4L2_MPEG_VIDEO_H264_PROFILE_HIGH.
                },
            ),
            // CAREFUL: `openh264enc` measures `bitrate` in **bit/s**, not kbit/s.
            H264Encoder::OpenH264 => format!(
                "openh264enc name=enc usage-type=screen rate-control=bitrate \
                 complexity=low scene-change-detection=false \
                 enable-frame-skip=true multi-thread=0 \
                 gop-size={gop} bitrate={bps} max-bitrate={maxbps}",
                bps = kbps as u64 * 1000,
                maxbps = kbps as u64 * 1200,
            ),
        }
    }
}

/// The detected KMS driver (used to decide whether HW encoding is trustworthy).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuDriver {
    I915,
    /// Intel `xe`: encode VA-API **trava** — preferir software.
    Xe,
    Amdgpu,
    Nvidia,
    Nouveau,
    Unknown,
}

impl GpuDriver {
    /// Builds from the module name in `/sys/class/drm/*/device/driver`.
    pub fn from_kernel_module(name: &str) -> Self {
        match name {
            "i915" => GpuDriver::I915,
            "xe" => GpuDriver::Xe,
            "amdgpu" | "radeon" => GpuDriver::Amdgpu,
            "nvidia" | "nvidia-drm" => GpuDriver::Nvidia,
            "nouveau" => GpuDriver::Nouveau,
            _ => GpuDriver::Unknown,
        }
    }

    /// Is hardware encoding considered trustworthy on this driver?
    ///
    /// The C project blocked VA-API on Intel's `xe` driver because it hung
    /// during encoding. **Verified in the field on 2026-08-07** on a Core
    /// Ultra 200H with `xe`: `vah264enc` encodes 1080p60 stably, at ~14% CPU
    /// (against software) and with less perceived delay. The block became cost
    /// without benefit, so it went — anyone with problematic hardware can
    /// force software with `BIGNETSCREEN_ENCODER=x264enc`.
    pub fn hardware_encode_is_reliable(self) -> bool {
        match self {
            // `nouveau` exposes no usable NVENC.
            GpuDriver::Nouveau => false,
            _ => true,
        }
    }
}

/// Scans GStreamer's registry and returns the encoders actually installed.
///
/// Without this, [`select_encoder`] never received a candidate list and the
/// project always fell back to the fixed encoder in
/// `StreamConfig::default()`.
pub fn probe_encoders() -> Vec<H264Encoder> {
    if init().is_err() {
        return Vec::new();
    }
    H264Encoder::ALL
        .into_iter()
        .filter(|enc| gst::ElementFactory::find(enc.element()).is_some())
        .collect()
}

/// Picks the best encoder from a list of candidates.
///
/// The rules: discard hardware encoding when the driver is known to be
/// problematic (`nouveau`), and never cross stacks (NVENC only on NVIDIA;
/// VA-API does not work under NVIDIA's proprietary driver).
pub fn select_encoder(available: &[H264Encoder], driver: GpuDriver) -> Option<H264Encoder> {
    available
        .iter()
        .copied()
        .filter(|enc| encoder_fits_driver(*enc, driver))
        .max_by_key(|enc| enc.priority())
}

/// Whether an encoder can run at all on top of the given driver.
///
/// Software encoders always can. Hardware ones need a driver whose encoding
/// is known to be reliable, and must not cross stacks: NVENC only exists on
/// NVIDIA, and VA-API does not work under NVIDIA's proprietary driver.
fn encoder_fits_driver(enc: H264Encoder, driver: GpuDriver) -> bool {
    if !enc.is_hardware() {
        return true;
    }
    if !driver.hardware_encode_is_reliable() {
        return false;
    }
    match (enc, driver) {
        (H264Encoder::NvH264, GpuDriver::Nvidia) => true,
        (H264Encoder::NvH264, _) => false,
        (e, GpuDriver::Nvidia) if e.is_va() => false,
        _ => true,
    }
}

/// How many threads the software encoder gets.
///
/// One slice per thread, so this is also how finely each frame is cut. Capped
/// because the two pull apart: throughput stops improving after about three
/// threads while every extra slice keeps costing bits, and a hardware decoder
/// in a television has fewer pieces to reassemble. On a machine with four
/// cores or fewer it changes nothing — that is already all there is.
pub fn software_encode_threads() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(2)
        .clamp(1, 4)
}

/// Whether a session may encode on the graphics card.
///
/// A parameter rather than a read of the preferences from in here: this module
/// is also where the tests for encoder selection live, and a function that
/// consults the machine's settings file gives a different answer on the
/// machine of whoever runs them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Acceleration {
    Allowed,
    Off,
}

impl Acceleration {
    pub fn from_settings(settings: &crate::settings::Settings) -> Self {
        if settings.hardware_encoding {
            Acceleration::Allowed
        } else {
            Acceleration::Off
        }
    }

    /// What the preferences currently say. For the call sites that have no
    /// `Settings` to hand and want the person's choice anyway.
    pub fn preferred() -> Self {
        Self::from_settings(&crate::settings::current())
    }

    fn permits(self, encoder: H264Encoder) -> bool {
        self == Acceleration::Allowed || !encoder.is_hardware()
    }
}

/// The name given to the encoder element in every description.
pub const ENCODER_NAME: &str = "enc";

/// The order in which encoders are tried: hardware first, software in reserve.
///
/// This replaces the driver blacklist. A blacklist gets the hardware that
/// existed when it was written right and everything else wrong — Intel's `xe`
/// driver, for instance, hung during encoding in 2022 and works fine today.
/// Here the decision is made **at runtime**: the GPU is tried and, if it
/// produces no frames, we fall back to software (see [`MonitoredPipeline`]).
///
/// Only the structural filtering stays static: NVENC does not run without an
/// NVIDIA card, and VA-API does not run under NVIDIA's proprietary driver.
pub fn encoder_candidates(driver: GpuDriver, acceleration: Acceleration) -> Vec<H264Encoder> {
    let available = probe_encoders();

    if let Ok(name) = std::env::var("BIGNETSCREEN_ENCODER") {
        if let Some(forced) = available.iter().copied().find(|e| e.element() == name) {
            tracing::warn!(?forced, "encoder forced by BIGNETSCREEN_ENCODER");
            return vec![forced];
        }
        tracing::warn!(%name, ?available, "BIGNETSCREEN_ENCODER not available; ignoring it");
    }

    let mut candidates: Vec<H264Encoder> = available
        .into_iter()
        .filter(|enc| acceleration.permits(*enc) && encoder_fits_driver(*enc, driver))
        .collect();
    candidates.sort_by_key(|enc| std::cmp::Reverse(enc.priority()));
    tracing::info!(?candidates, ?driver, ?acceleration, "encoder attempt order");
    candidates
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

pub struct MonitoredPipeline {
    pub pipeline: gst::Pipeline,
    pub events: PipelineEvents,
    pub encoder: H264Encoder,
    frames: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl MonitoredPipeline {
    /// How many frames have left the encoder so far.
    pub fn frames_encoded(&self) -> u64 {
        self.frames.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Shuts the pipeline down (used when this encoder is discarded).
    pub fn shutdown(self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Builds a pipeline and installs the probe that counts encoded frames.
pub fn build_monitored(
    description: &str,
    latency_ms: u64,
    encoder: H264Encoder,
) -> Result<MonitoredPipeline> {
    let (pipeline, events) = build_pipeline(description, latency_ms)?;

    let frames = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    if let Some(element) = pipeline.by_name(ENCODER_NAME) {
        if let Some(pad) = element.static_pad("src") {
            let counter = frames.clone();
            pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
        }
    } else {
        tracing::warn!("element `{ENCODER_NAME}` not found; no encoder output verification");
    }

    Ok(MonitoredPipeline {
        pipeline,
        events,
        encoder,
        frames,
    })
}

/// Select an encoder that produces frames at the requested mode.
///
/// `session` is the configuration the caller is about to stream with, its
/// `encoder` field ignored — this replaces it with each candidate in turn. It
/// takes the whole thing rather than a size and a frame rate because the probe
/// is only worth anything if it builds the *same* encoder the session will:
/// with a partial configuration it once validated a pipeline that differed
/// from the real one in every property but two, so a property the hardware
/// rejects would have passed here and failed on the screen.
pub async fn working_encoder(
    driver: GpuDriver,
    acceleration: Acceleration,
    session: StreamConfig,
) -> Result<H264Encoder> {
    for encoder in encoder_candidates(driver, acceleration) {
        let cfg = StreamConfig { encoder, ..session };
        let description = format!(
            "videotestsrc is-live=true num-buffers=2 ! {} ! {} ! fakesink sync=false",
            cfg.convert_scale(VideoTarget::Exact((cfg.width, cfg.height))),
            cfg.encoder_stage(false)
        );
        let Ok(monitored) = build_monitored(&description, cfg.latency_ms(), encoder) else {
            continue;
        };
        let _guard = PipelineGuard::new(monitored.pipeline.clone());
        if monitored.pipeline.set_state(gst::State::Playing).is_err() {
            continue;
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(1500);
        while tokio::time::Instant::now() < deadline {
            if monitored.frames_encoded() > 0 {
                return Ok(encoder);
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        tracing::warn!(
            ?encoder,
            "encoder startup probe failed; trying the next candidate"
        );
    }
    Err(NdError::Unsupported(
        "no working H.264 encoder for the requested resolution".into(),
    ))
}

/// Convenience: scans the registry and picks the best encoder for the driver.
pub fn best_encoder(driver: GpuDriver, acceleration: Acceleration) -> Result<H264Encoder> {
    let installed = probe_encoders();

    // `BIGNETSCREEN_ENCODER=vah264enc` forces a specific encoder. It is there
    // to measure in the field whether a quirk (such as Intel `xe`'s) still
    // holds on current hardware, rather than carrying it forever out of
    // inertia.
    // Read against everything installed, before the preference is applied.
    // This variable exists to measure a specific encoder in the field, and a
    // switch in the preferences silently overruling it makes it useless for
    // exactly the session somebody is trying to diagnose — which cost one
    // measurement in the session that wrote this.
    if let Ok(name) = std::env::var("BIGNETSCREEN_ENCODER") {
        if let Some(forced) = installed.iter().copied().find(|e| e.element() == name) {
            tracing::warn!(?forced, "encoder forced by BIGNETSCREEN_ENCODER");
            return Ok(forced);
        }
        tracing::warn!(%name, ?installed, "BIGNETSCREEN_ENCODER not available; ignoring it");
    }
    let available: Vec<H264Encoder> = installed
        .into_iter()
        .filter(|enc| acceleration.permits(*enc))
        .collect();
    let chosen = select_encoder(&available, driver);
    tracing::info!(?available, ?driver, ?chosen, "H.264 encoder selection");
    chosen.ok_or_else(|| {
        NdError::Unsupported(
            "no H.264 encoder found — install gst-plugins-ugly (x264) \
             ou gst-plugins-bad (openh264/va)"
                .into(),
        )
    })
}

// ------------------------------------------------------------------------
// Sources
// ------------------------------------------------------------------------

/// Where the video frames come from.
// Not `Copy`: a media file carries its path. Cloning a source is rare (once
// per session) and cheap next to what the session then does.
#[derive(Clone, Debug)]
pub enum VideoSource {
    /// The PipeWire stream handed over by the portal/Mutter.
    ///
    /// `node_id` may **never** be missing: a `pipewiresrc` without `path`
    /// picks an arbitrary node from the daemon instead of the one the capture
    /// session authorised. `fd` is the portal's remote descriptor (mandatory
    /// under Flatpak); with Mutter directly the node lives in the session's own
    /// daemon and `fd` is `None`.
    PipeWire {
        fd: Option<RawFd>,
        node_id: u32,
        /// Prefer the non-reusable portal serial over the legacy node ID.
        serial: Option<u64>,
        /// The smallest buffer pool to accept from the producer.
        ///
        /// `None` leaves `pipewiresrc`'s own default of one, which lets the
        /// producer settle anywhere in the range it offers. KWin offers two to
        /// four and prefers three (`screencaststream.cpp`), and where it lands
        /// is decided once per stream and kept — which is why a new virtual
        /// screen came out fluid or stuttering at random and stayed that way,
        /// with no load to blame: measured at 9 frames a second on a bad one
        /// and 36 on a good one, with the compositor at 7% of a core both
        /// times.
        ///
        /// A small pool starves the compositor: it can only paint into a
        /// buffer we have given back, and this pipeline holds one in
        /// `videorate` by design. With two, that is half the pool.
        ///
        /// Only set where the producer's range is known from its source.
        /// Asking for more than it offers is not clamped — it fails the
        /// allocation outright, and a capture that will not start is worse
        /// than one that sometimes stutters.
        min_buffers: Option<u32>,
        /// The size to demand from the producer, when it must not be left open.
        ///
        /// A **virtual monitor** has no panel to take a resolution from, so
        /// Mutter leaves it to PipeWire to negotiate one — and creates the
        /// screen at whatever comes out of that negotiation. With the size only
        /// fixed downstream of the scaler, `pipewiresrc` accepted anything and
        /// the monitor was created at **16x16**, then stretched to fill the
        /// receiver. That is what a blurry picture looks like from here.
        ///
        /// `None` for a real monitor: its resolution is its own.
        size: Option<(u32, u32)>,
    },
    /// A media file played **to** the receiver.
    ///
    /// Miracast has no notion of "play this file": the receiver is a screen and
    /// nothing else. So the file is decoded here and takes the place of the
    /// screen capture — full screen on the receiver, without the desktop
    /// showing. It costs a re-encode, which is the price of a protocol that
    /// only knows how to be a display.
    ///
    /// The element is named `filedec` because [`AudioSource::MediaFile`] takes
    /// the sound from the **same** decoder: decoding a film twice to split its
    /// two tracks would double the cost of the thing that is already the most
    /// expensive part.
    Media {
        source: crate::media::MediaSource,
        kind: crate::media::MediaKind,
        /// What to write across a file that has no picture of its own.
        title: String,
    },
    /// A test pattern (diagnostic, needing no capture session).
    Test,
    /// An **animated** pattern with a clock overlaid.
    ///
    /// Colour bars are static: on a remote screen there is no way to tell "live
    /// picture" from "frozen picture". A moving ball plus a running clock
    /// settle that at a glance — and the clock also allows measuring the delay
    /// by comparing against the local screen.
    Diagnostic,
}

impl VideoSource {
    /// Is this local or online media, rather than a captured desktop?
    ///
    /// The distinction decides pacing: a capture arrives at the pace of the
    /// thing it captures, and a file arrives as fast as it can be read.
    pub fn is_media(&self) -> bool {
        matches!(self, VideoSource::Media { .. })
    }

    /// The GStreamer fragment that produces this source's frames.
    ///
    /// Public so diagnostics can build a pipeline of their own — the
    /// `cursor_probe` example needs raw video, where the production pipelines
    /// all end in an encoder.
    pub fn description(&self) -> String {
        match self {
            // `keepalive-time`/`resend-last` make the source re-emit the last
            // frame when the screen is still. Without them the encoder starves
            // and the receiver drops the session for lack of data.
            VideoSource::PipeWire {
                fd,
                node_id,
                serial,
                size,
                min_buffers,
            } => {
                let pool = match min_buffers {
                    Some(n) => format!(" min-buffers={n}"),
                    None => String::new(),
                };
                let fd_prop = match fd {
                    Some(fd) => format!("fd={fd} "),
                    None => String::new(),
                };
                // No caps are forced on the producer, and that is deliberate.
                //
                // Demanding a size here looked like the way to stop Mutter
                // creating a 16x16 virtual monitor. What it actually did was
                // make the authorised node unable to satisfy the negotiation —
                // and `pipewiresrc`, whose `autoconnect` defaults to true,
                // then went looking for another video peer and found the
                // laptop's **webcam**, which went out to the projector.
                //
                // Turning `autoconnect` off is not the fix either: with it off
                // the element never connects at all (measured: zero frames,
                // the monitor is never created). So the size stays negotiated,
                // and sizing the virtual monitor properly is still open.
                let _ = size;
                // Do not fall back to a node ID when a serial was supplied:
                // a recycled ID could point at a different producer. A plugin
                // without target-object must fail, not capture another node.
                let target = match serial {
                    Some(serial) => format!("target-object={serial}"),
                    None => format!("path={node_id}"),
                };
                // `keepalive-time` is the floor on the capture's frame rate: a
                // compositor only repaints what changed, so when the screen is
                // still this is the only thing producing buffers at all.
                //
                // It used to be 1000 ms, which is a floor of one frame per
                // second — measured, and visible as exactly that: typing in a
                // terminal changes a character cell, the compositor sends
                // almost nothing, and the picture updated about once a second.
                // One frame interval keeps the encoder fed and the receiver's
                // clock moving, at the price of duplicate frames that a P-frame
                // codes in a handful of bytes. The encoder was already emitting
                // 60 fps against 13 arriving, so the duplicates are not new —
                // only their timing is, and regular beats bursty.
                format!(
                    "pipewiresrc name={CAPTURE_SOURCE_NAME} {fd_prop}{target} \
                     do-timestamp=true keepalive-time={keepalive} resend-last=true{pool}",
                    keepalive = capture_keepalive_ms()
                )
            }
            VideoSource::Test => "videotestsrc is-live=true".to_string(),
            VideoSource::Diagnostic => "videotestsrc is-live=true pattern=ball ! \
                 timeoverlay halignment=center valignment=center font-desc=\"Sans 48\" \
                 time-mode=running-time"
                .to_string(),
            VideoSource::Media {
                source,
                kind,
                title,
            } => {
                let decoder = match source {
                    crate::media::MediaSource::File(path) => format!(
                        "filesrc location={} ! decodebin name=filedec",
                        escape_location(path)
                    ),
                    crate::media::MediaSource::Url(uri) => format!(
                        "uridecodebin uri={} name=filedec",
                        escape_location(std::path::Path::new(uri))
                    ),
                };
                match kind {
                    // A photo is one frame. Sent once, a receiver expecting a
                    // video stream shows nothing at all, so `imagefreeze`
                    // repeats it for as long as the session lasts.
                    crate::media::MediaKind::Photo => format!(
                        "{decoder} \
                         filedec. ! queue ! imagefreeze name=file-photo ! videoconvert"
                    ),
                    crate::media::MediaKind::Video => format!(
                        "{decoder} \
                         filedec. ! queue ! identity name=file-video-sync sync=true ! videoconvert"
                    ),
                    // A song has no picture. The decoder is still declared —
                    // the audio branch takes its sound from it — and the screen
                    // gets the one thing worth showing: what is playing.
                    crate::media::MediaKind::Music => format!(
                        "{decoder} \
                         videotestsrc name=file-picture is-live=true pattern=black \
                         ! textoverlay text={title} halignment=center valignment=center \
                           font-desc=\"Sans 32\" ! videoconvert",
                        title = escape_location(std::path::Path::new(title)),
                    ),
                }
            }
        }
    }
}

/// Where the streamed programme's audio comes from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AudioSource {
    /// Synthesised silence.
    ///
    /// **Not optional on WFD:** the sink declares an A/V session and discards
    /// the programme if it arrives with video alone.
    Silence,
    /// System audio: an output's *monitor*, that is, what is playing through
    /// it. Which output is [`Monitor`].
    System(Monitor),
    /// The microphone alone, at the given gain (`1.0` = as captured).
    ///
    /// For narrating over what is on screen without sending the computer's own
    /// sound back through the receiver's speakers.
    Mic { volume: f64 },
    /// The computer's sound and the microphone, mixed.
    SystemAndMic { monitor: Monitor, volume: f64 },
    /// The sound of the media file being played, from the same decoder that
    /// produces its picture.
    MediaFile,
}

/// Which output's monitor "the computer's sound" means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Monitor {
    /// The default output. Resolved by the server, so it follows the person
    /// when they switch device mid-session, and it carries **everything** the
    /// computer is playing.
    Default,
    /// The virtual card's monitor: only what an application was routed to it.
    Virtual,
}

impl Monitor {
    fn device(self) -> &'static str {
        match self {
            // Resolved by the server (PulseAudio, or PipeWire's compatibility
            // mode) — there is nothing for us to query.
            Monitor::Default => "@DEFAULT_MONITOR@",
            Monitor::Virtual => crate::virtual_sink::MONITOR,
        }
    }

    pub fn from_settings(settings: &crate::settings::Settings) -> Self {
        if settings.virtual_audio {
            Monitor::Virtual
        } else {
            Monitor::Default
        }
    }
}

impl AudioSource {
    /// Picks the best source available on this machine.
    ///
    /// Falls back to silence if there is no way to capture system audio — the
    /// audio branch is **not optional** on WFD (the sink discards a video-only
    /// programme), so sending silence beats sending nothing.
    pub fn detect() -> Self {
        Self::for_settings(&crate::settings::current())
    }

    /// The source the person asked for, or the closest thing this machine can
    /// actually produce.
    ///
    /// Turning both off is a legitimate choice — sharing a screen in a meeting
    /// where the sound would echo — and it still yields silence rather than no
    /// audio branch at all.
    pub fn for_settings(settings: &crate::settings::Settings) -> Self {
        if init().is_err() || gst::ElementFactory::find("pulsesrc").is_none() {
            tracing::info!("no `pulsesrc`; the cast will go without audio");
            return AudioSource::Silence;
        }
        let volume = settings.mic_volume as f64 / 100.0;
        // Mixing needs an element that may not be installed (`audiomixer` is in
        // gst-plugins-base, but a minimal image can still lack it). Without it
        // the honest answer is the system audio, not a pipeline that fails to
        // build at the moment the person hits cast.
        let can_mix = gst::ElementFactory::find("audiomixer").is_some();
        let monitor = Monitor::from_settings(settings);
        match (settings.system_audio, settings.microphone) {
            (true, true) if can_mix => AudioSource::SystemAndMic { monitor, volume },
            (true, true) => {
                tracing::warn!("no `audiomixer`; sending system audio without the microphone");
                AudioSource::System(monitor)
            }
            (true, false) => AudioSource::System(monitor),
            (false, true) => AudioSource::Mic { volume },
            (false, false) => AudioSource::Silence,
        }
    }
}

impl AudioSource {
    pub fn description(&self) -> String {
        match self {
            // `samplesperbuffer=480` = 10 ms at 48 kHz (the default, 1024, is
            // 21 ms). The audio branch is what sets the pipeline's latency
            // floor: measured in the field, 41 ms with the default.
            AudioSource::Silence => {
                "audiotestsrc is-live=true wave=silence samplesperbuffer=480".to_string()
            }
            // `provide-clock=false`: the screen capture sets the pace; a second
            // clock in the pipeline fights with it.
            //
            // `buffer-time`: see `CAPTURE_AUDIO_BUFFER_MS`. This branch reports
            // the pipeline's largest latency, so it decides when the picture
            // arrives.
            AudioSource::System(monitor) => format!(
                "pulsesrc device={device} provide-clock=false do-timestamp=true{buffer}",
                device = monitor.device(),
                buffer = audio_buffer_property()
            ),
            // `@DEFAULT_SOURCE@`, the counterpart of `@DEFAULT_MONITOR@`: the
            // input the person selected in their sound settings, following
            // them when they plug a headset in mid-session.
            AudioSource::Mic { volume } => format!(
                "pulsesrc device=@DEFAULT_SOURCE@ provide-clock=false do-timestamp=true{buffer} \
                 ! audioconvert ! audioresample ! volume volume={volume:.2}",
                buffer = audio_buffer_property()
            ),
            // Two live sources into one branch. What matters here:
            //
            // - `audiomixer` (not the deprecated `audiomixer`-less `adder`)
            //   resamples and aligns by timestamp, so the two sources need not
            //   agree on format or arrive in step;
            // - both are converted to a common rate **before** the mixer, or it
            //   refuses to link;
            // - `latency=20000000` (20 ms) is how long the mixer waits for a
            //   late source before outputting without it. The default, 0, makes
            //   a microphone that hiccups drop the system audio with it.
            AudioSource::SystemAndMic { monitor, volume } => format!(
                "audiomixer name=micmix latency=20000000 \
                 pulsesrc device={device} provide-clock=false do-timestamp=true{buffer} \
                 ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! micmix. \
                 pulsesrc device=@DEFAULT_SOURCE@ provide-clock=false do-timestamp=true{buffer} \
                 ! audioconvert ! audioresample ! volume volume={volume:.2} \
                 ! audio/x-raw,rate=48000,channels=2 ! micmix. \
                 micmix.",
                device = monitor.device(),
                buffer = audio_buffer_property()
            ),
            // Mixed with silence, and that is not belt and braces: a film with
            // no audio track, and every photo, leaves this branch with nothing
            // linked to it. The muxer would then wait for a track that never
            // comes and the picture would never leave the machine. The mixer
            // always has the silence to fall back on.
            AudioSource::MediaFile => "audiomixer name=filemix latency=20000000 ignore-inactive-pads=true \
                 audiotestsrc name=file-silence is-live=true wave=silence samplesperbuffer=480 \
                 ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! filemix. \
                 filedec. ! queue name=file-audio-queue ! identity name=file-audio-sync sync=true ! audioconvert ! audioresample \
                 ! audio/x-raw,rate=48000,channels=2 ! filemix. \
                 filemix. ! volume name=file-volume"
                .to_string(),
        }
    }
}

/// Quotes a path for `gst_parse_launch`.
///
/// A file called `My Holiday - Côte d'Azur.mp4` would otherwise end the
/// property at the first space and take the rest as elements. Backslashes and
/// quotes are escaped first, so a name containing one cannot close the string
/// and start writing pipeline of its own.
fn escape_location(path: &std::path::Path) -> String {
    let text = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("\"{text}\"")
}

// ------------------------------------------------------------------------
// Session configuration
// ------------------------------------------------------------------------

/// A cast session's parameters (negotiated resolution/fps/bitrate).
#[derive(Clone, Copy, Debug)]
pub struct StreamConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// `0` = derive it from the cap scaled by resolution and frame rate.
    pub bitrate_kbps: u32,
    pub encoder: H264Encoder,
    pub audio: AudioSource,
    /// Periodic Intra Refresh on x264 (a flat bitrate curve, quick recovery
    /// from loss). Off by default: without periodic IDRs, older receivers
    /// cannot join the stream after it has started.
    pub intra_refresh: bool,
    /// How many frames of slack the rate control gets, or `0` to leave the
    /// encoder's own default alone.
    ///
    /// This is the VBV (the "leaky bucket" H.264 rate control drains into),
    /// expressed in frames because that is what bounds the size of a single
    /// one; each encoder is told in whatever unit it wants.
    ///
    /// It matters on a packet network and almost nowhere else. Measured at
    /// 1080p30, CBR 10 Mbit, `videotestsrc pattern=snow`:
    ///
    /// | VBV | key frame, mean | key frame, largest | delivered |
    /// | --- | --- | --- | --- |
    /// | NVENC default | 483 KB | 908 KB | 12.7 Mbit/s |
    /// | 1 frame | 403 KB | 1414 KB | 12.5 Mbit/s |
    /// | **2 frames** | **92 KB** | **154 KB** | **10.3 Mbit/s** |
    /// | 3 frames | 116 KB | 175 KB | 10.1 Mbit/s |
    ///
    /// Two lessons in that table. NVENC's default buffer holds a whole second,
    /// so a key frame is free to eat most of the second's budget — and the
    /// stream then overran the CBR ceiling it was given by 27%. And one frame
    /// is *worse* than no limit at all: the rate control cannot fit an IDR in
    /// that bucket and overshoots wildly instead.
    pub vbv_frames: u32,
    /// Seconds between forced key frames, or `0` for none at all.
    ///
    /// One by default, which is a direct link's answer: a periodic IDR bounds
    /// recovery after loss where the receiver has no way to ask for one.
    ///
    /// Cast mirroring has two ways to ask. The receiver sends a picture loss
    /// indication, which the sender turns into a key frame, and lost packets
    /// are retransmitted on NACK before it comes to that — so scheduling them
    /// as well buys nothing, and it is not free. An IDR is a whole picture
    /// squeezed into a rate-control budget a few frames wide: it lands softer
    /// than its neighbours and they sharpen it again, which at one per second
    /// is the image visibly pulsing. Reported, repeatedly, as a blur every few
    /// seconds.
    pub gop_seconds: u32,
    /// The H.264 feature set the receiver on this path is known to decode.
    ///
    /// Constrained baseline by default, which is what every path assumed
    /// before there was a choice — and what the Wi-Fi Display profile
    /// requires. Raising it is a per-path statement about the device on the
    /// other end, never a global default.
    pub profile: H264Profile,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 30,
            bitrate_kbps: 0,
            encoder: H264Encoder::X264,
            audio: AudioSource::Silence,
            intra_refresh: false,
            vbv_frames: 0,
            gop_seconds: 1,
            profile: H264Profile::ConstrainedBaseline,
        }
    }
}

impl StreamConfig {
    /// The bitrate cap (CBR), scaled by resolution **and frame rate**.
    ///
    /// The original table (6/9/10/14/20 Mbit) was calibrated for 30 Hz; at
    /// 60 Hz the same cap undersizes the stream and the picture blurs in
    /// motion.
    pub fn scaled_bitrate_kbps(&self) -> u32 {
        if self.bitrate_kbps != 0 {
            return self.bitrate_kbps;
        }
        let base = match self.width as u64 * self.height as u64 {
            n if n <= 640 * 480 => 6_000u32,
            n if n <= 1280 * 720 => 9_000,
            n if n <= 1920 * 1080 => 10_000,
            n if n <= 2560 * 1440 => 14_000,
            _ => 20_000,
        };
        if self.fps > 30 {
            // Linear interpolation between 30 and 60 Hz (a 1.5× ceiling).
            let factor = 1.0 + 0.5 * ((self.fps.min(60) - 30) as f32 / 30.0);
            (base as f32 * factor) as u32
        } else {
            base
        }
    }

    /// Shrinks a resolution to fit a cap **while preserving the aspect ratio**.
    ///
    /// It returns even dimensions (H.264 requires them) and never enlarges: a
    /// screen smaller than the cap is streamed at its original size, without
    /// spending bits on upscaling the receiver would undo.
    pub fn fit_within(source: (u32, u32), max: (u32, u32)) -> (u32, u32) {
        // A valid H.264 image has at least two pixels on each axis. Sanitise
        // degenerate portal/preference values before division or even rounding.
        let (max_w, max_h) = (max.0.max(2) & !1, max.1.max(2) & !1);
        if source.0 == 0 || source.1 == 0 {
            return (max_w, max_h);
        }
        let (w, h) = (source.0.max(2), source.1.max(2));
        if w <= max_w && h <= max_h {
            return (w & !1, h & !1);
        }
        // Scale by the most restrictive axis, using integer arithmetic.
        let scaled_h = (h as u64 * max_w as u64 / w as u64) as u32;
        let (fit_w, fit_h) = if scaled_h <= max_h {
            (max_w, scaled_h)
        } else {
            ((w as u64 * max_h as u64 / h as u64) as u32, max_h)
        };
        (fit_w.max(2) & !1, fit_h.max(2) & !1)
    }

    /// The receiver's limit and the person's preference, whichever is lower.
    ///
    /// Both are ceilings and they compose in only one direction: choosing
    /// "medium quality" must be able to send *less* than the receiver can take,
    /// and must never talk a receiver into a size it did not offer.
    pub fn capped_by_preference(receiver_max: (u32, u32)) -> (u32, u32) {
        let (pw, ph) = crate::settings::current().resolution_limit();
        (receiver_max.0.min(pw), receiver_max.1.min(ph))
    }

    /// The size to send when the protocol never states a maximum.
    ///
    /// Cast is the case: the sender declares a resolution in its OFFER and the
    /// receiver takes it or refuses it — nothing is announced beforehand. So
    /// the 1080p written here is a **safe default**, not a limit discovered
    /// from the device, and a person who asks for more must be able to get it.
    /// Reported from the field by someone with a 1440p screen watching it
    /// arrive at 1080p with no way to say otherwise.
    ///
    /// Contrast with [`Self::capped_by_preference`], for Miracast, where the
    /// sink lists the modes it accepts and going past them is not a choice we
    /// are allowed to make.
    pub fn preferred_or(default_max: (u32, u32)) -> (u32, u32) {
        let chosen = crate::settings::current().resolution_limit();
        if chosen == crate::settings::Quality::default().resolution() {
            default_max
        } else {
            chosen
        }
    }

    /// The frame rate to use, given what the link negotiated.
    ///
    /// The same rule: a preference of 60 cannot raise a link that agreed on 30,
    /// because the other end would be sent frames it never asked for.
    pub fn capped_fps(negotiated: u32) -> u32 {
        negotiated.min(crate::settings::current().fps).max(1)
    }

    /// [`Self::vbv_frames`] in milliseconds, for encoders that ask in time.
    ///
    /// `0` stays `0` — it means "leave the encoder's default alone", and a
    /// frame count that rounds to nothing must not turn into a real limit.
    pub fn vbv_ms(&self) -> u32 {
        match self.vbv_frames {
            0 => 0,
            frames => (frames * 1000 / self.fps.max(1)).max(1),
        }
    }

    /// The maximum distance between keyframes.
    ///
    /// One second (not two): over an unstable Wi-Fi Direct link the GOP sets
    /// the recovery time after packet loss, and more frequent IDRs keep the
    /// bitrate curve flatter (less VBV jitter).
    ///
    /// That reasoning is a direct link's. [`Self::gop_seconds`] carries the
    /// path's own answer, because Cast asks for a key frame rather than
    /// scheduling one.
    pub fn gop(&self) -> u32 {
        if let Ok(value) = std::env::var("BIGNETSCREEN_GOP") {
            if let Ok(gop) = value.parse::<u32>() {
                return gop.max(1);
            }
        }
        if self.gop_seconds == 0 {
            return 0;
        }
        self.fps.max(1) * self.gop_seconds
    }

    /// The pipeline latency suited to the chosen encoder.
    ///
    /// `BIGNETSCREEN_PIPELINE_LATENCY_MS` overrides it, for bisecting
    /// interoperability problems in the field.
    pub fn latency_ms(&self) -> u64 {
        if let Ok(value) = std::env::var("BIGNETSCREEN_PIPELINE_LATENCY_MS") {
            if let Ok(ms) = value.parse::<u64>() {
                return ms;
            }
        }
        let encoder_latency = self.encoder.pipeline_latency_ms();
        if crate::latency::is_film() {
            // Never *below* what the encoder needs: buffering is added on top,
            // and going under the pipeline's own minimum makes sinks drop late
            // buffers instead of playing them.
            return encoder_latency.max(crate::latency::FILM_PIPELINE_LATENCY_MS);
        }
        encoder_latency
    }

    /// The `rate → scale → convert` fragment, tuned per encoder.
    ///
    /// Two performance decisions:
    /// 1. `videorate` comes **before** scaling: frames that will be dropped pay
    ///    for neither conversion nor scaling (which matters when the source is
    ///    60 Hz and the sink negotiated 30);
    /// 2. on the VA-API path `vapostproc` is used, converting and scaling **on
    ///    the GPU** — at 4K that avoids hundreds of MB/s of GPU→RAM→GPU
    ///    copying.
    fn convert_scale(&self, target: VideoTarget) -> String {
        let fmt = self.encoder.accepted_formats();
        let fps = self.fps;
        // `videorate` fills a gap in the input by repeating the last frame
        // until the timeline catches up. Over a short gap that is what keeps
        // the output steady. Over a long one it is a flood: measured during a
        // ten-second freeze, capture stalled at 0.5 fps while the encoder ran
        // at 78 — above the 60 it was asked for — because every frame that did
        // arrive carried a timestamp two seconds ahead and bought a hundred
        // duplicates. Each of those is a full-size scale and colour convert, so
        // the stall pays for its own continuation.
        //
        // Cap the gap it will fill. Anything longer is a stall, and a stalled
        // picture should cost nothing until it recovers.
        //
        // This element is also the most expensive thing in the path, and it is
        // staying. Instrumented at 2560x1440 it holds each frame **35 ms**
        // waiting for the next, against 12 ms for the entire capture and 4 ms
        // for upload and encode together — removing it measured 57 ms down to
        // 22. It has been removed twice and put back twice, by two different
        // routes:
        //
        // - `drop-only=true`, which stops it duplicating and therefore holding;
        // - skipping it for `nvh264enc`, which negotiates `framerate=0/1`
        //   where `x264enc` and `openh264enc` refuse to link at all.
        //
        // Both produce the same failure against a real receiver, and it is not
        // subtle: `encoded frame has no valid running-time timestamp`, with a
        // black picture on a shared screen and a session that drops seconds
        // after a virtual one appears. Turning the compositor's variable
        // cadence into a fixed one is what lets the Cast sender map an encoded
        // frame through its segment, and nothing downstream reconstructs that.
        //
        // So the 35 ms is the price of the sender working. Anyone going after
        // it again needs to fix the mapping first, not the cadence.
        let rate = format!("videorate max-duplication-time={MAX_DUPLICATION_NS}");
        // `add-borders=true` is **not** vapostproc's default (unlike
        // videoscale's): without it, a 16:10 screen sent to a 16:9 panel
        // comes out stretched vertically.
        let scale = if self.encoder.is_va() {
            "vapostproc add-borders=true".to_string()
        } else {
            // Converted first: `videoscale` fills its borders wrongly in the
            // 10-bit formats a HEVC film decodes to, and a film's black bars
            // reached the television pink.
            "videoconvert n-threads=0 ! videoscale add-borders=true".to_string()
        };
        // Square pixels, or the borders never appear: with the aspect ratio
        // left open the scaler fills the frame and records the stretch as a
        // pixel shape, which receivers ignore — a 2.39:1 film then fills a
        // 16:9 television, stretched vertically.
        let caps = format!(
            "video/x-raw,format={fmt},{dims},pixel-aspect-ratio=1/1,framerate={fps}/1",
            dims = target.caps_dimensions()
        );
        format!("{rate} ! {scale} ! capsfilter name={SCALE_CAPS} caps=\"{caps}\"")
    }

    /// The same stage, keeping the frame on the GPU from end to end.
    ///
    /// The capture is imported as a GL texture and handed to NVENC through GL
    /// interop, so the picture is never read back to the CPU and never uploaded
    /// again. What it cannot do is scale: `glcolorscale` will not link to this
    /// encoder here (measured), so this shape only fits a screen that already
    /// sits inside the person's ceiling — which is why the caller decides.
    ///
    /// Requesting `memory:DMABuf` explicitly is the point. Without it `glupload`
    /// would happily accept system memory and upload it, which is what already
    /// happens and would make this path a rename rather than a saving.
    fn gpu_convert(&self, ceiling: (u32, u32)) -> String {
        let fps = self.fps;
        let dims = VideoTarget::UpTo(ceiling).caps_dimensions();
        format!(
            "video/x-raw(memory:DMABuf) ! \
             videorate max-duplication-time={MAX_DUPLICATION_NS} ! glupload ! \
             capsfilter name={SCALE_CAPS} \
             caps=\"video/x-raw(memory:GLMemory),{dims},framerate={fps}/1\""
        )
    }

    /// The encoder, with the transfer to the card done by the element that is
    /// best at it.
    ///
    /// NVENC uploads its own input when handed system memory, and `cudaupload`
    /// does the same job measurably faster — 300 frames at 2560x1440 encode in
    /// 1.15s through it against 1.30s without, repeatably, which is about a
    /// fifth off the encoding stage. On the GPU path the frame is already on the
    /// card and there is nothing to upload.
    fn encoder_stage(&self, gpu: bool) -> String {
        let encoder = self.encoder.encoder_description(self);
        if gpu || self.encoder != H264Encoder::NvH264 {
            encoder
        } else {
            format!("cudaupload ! {encoder}")
        }
    }

    fn video_queue(&self) -> String {
        format!(
            "queue max-size-buffers={b} max-size-time={t}000000 max-size-bytes=0 \
             leaky=downstream",
            b = VIDEO_QUEUE_BUFFERS,
            t = VIDEO_QUEUE_MS,
        )
    }

    /// The audio branch's queue on the muxed path.
    fn audio_queue(&self) -> String {
        format!(
            "queue max-size-buffers={b} max-size-time={t}000000 max-size-bytes=0 \
             leaky=downstream",
            b = AUDIO_QUEUE_BUFFERS,
            t = AUDIO_QUEUE_MS,
        )
    }

    /// The Cast mirroring audio branch's queue — **without** `leaky`.
    ///
    /// The video queue leaks on purpose: an old frame is of no interest, the
    /// next one arrives whole. Audio is the opposite — dropping samples does
    /// not "advance" the sound, it opens a hole in the middle of it. Not
    /// leaking is possible here because nothing waits on this track; on the
    /// muxed path, video would.
    fn mirror_audio_queue(&self) -> String {
        format!(
            "queue max-size-buffers={b} max-size-time={t}000000 max-size-bytes=0",
            b = MIRROR_AUDIO_QUEUE_BUFFERS,
            t = MIRROR_AUDIO_QUEUE_MS,
        )
    }

    /// Is an audio track configured?
    pub fn audio_enabled(&self) -> bool {
        true
    }

    /// The AAC audio branch attached to the given pad of the muxer named
    /// `mux`.
    ///
    /// `mux_pad` is `"mux."` for Chromecast (automatic PID) and
    /// `"mux.sink_4352"` for WFD, which requires PID 0x1100.
    fn audio_branch(&self, mux_pad: &str) -> String {
        format!(
            "{src} ! audioconvert ! audioresample ! \
             audio/x-raw,rate=48000,channels=2 ! {queue} ! \
             avenc_aac bitrate=128000 ! aacparse ! {mux_pad}",
            src = self.audio.description(),
            queue = self.audio_queue(),
        )
    }
}

// ------------------------------------------------------------------------
// Pipelines
// ------------------------------------------------------------------------

/// Should the RTP `udpsink` wait on the clock before sending each packet?
///
/// With `sync=true` (the element's default) a frame captured at `T` only hits
/// the network at `T + pipeline-latency`. Since the source is already live —
/// it produces at the screen's pace — that wait is **pure delay** in mirroring:
/// the RTP timestamps remain correct and the sink does its own scheduling.
///
/// `BIGNETSCREEN_RTP_SYNC=1` restores the old behaviour for comparison.
fn rtp_sink_syncs_to_clock() -> bool {
    std::env::var("BIGNETSCREEN_RTP_SYNC")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// A WFD session's RTP destination.
#[derive(Clone, Copy, Debug)]
pub struct WfdTransport {
    /// IP do sink (TV/projetor) no link P2P.
    pub sink_ip: IpAddr,
    /// The RTP port the sink asked for (`wfd_client_rtp_ports` / `client_port=`).
    pub rtp_port: u16,
    /// Local source RTP port.
    pub local_rtp_port: u16,
}

impl WfdTransport {
    pub fn new(sink_ip: IpAddr, rtp_port: u16) -> Self {
        Self {
            sink_ip,
            rtp_port,
            local_rtp_port: LOCAL_RTP_PORT,
        }
    }
}

/// Builds the WFD (Miracast) pipeline description.
///
/// `source → videorate → scale/convert → encoder → h264parse → mpegtsmux →
///  rtpmp2tpay → rtpbin → udpsink`, with a **mandatory AAC audio branch**.
///
/// Correctness notes (all verified against the reference C project, which is
/// this protocol's interoperability oracle):
/// - the MPEG-TS programme has to contain audio **and** video: a WFD sink
///   advertises an A/V session and discards an incomplete programme;
/// - **the PIDs are fixed**: video on 0x1011 and audio on 0x1100. Using the
///   generic `mux.` pad lets `mpegtsmux` choose arbitrary PIDs, and the sink
///   simply cannot find the video — it receives megabits and shows a black
///   screen;
/// - **`rtpmp2tpay perfect-rtptime=false`**: the default (`true`) derives the
///   RTP timestamp from the buffer count rather than the send clock, and the
///   sink uses that timestamp to schedule display;
/// - `video/x-h264,profile=constrained-baseline` is required by the WFD
///   profile;
/// - `h264parse config-interval=-1` reinserts SPS/PPS at every IDR, so the sink
///   can join the stream at any moment;
/// - `rtpbin` generates the RTCP Sender Reports the sink uses to synchronise;
/// - the queue before the payloader is **short and non-leaky**: dropping a
///   piece of a TS packet would corrupt the flow.
pub fn wfd_pipeline_description(
    cfg: &StreamConfig,
    source: &VideoSource,
    transport: &WfdTransport,
) -> String {
    let rtcp_port = transport.rtp_port.wrapping_add(1);
    let local_rtcp = transport.local_rtp_port.wrapping_add(1);
    format!(
        "rtpbin name=rtpbin rtp-profile=avp latency={rtp_latency} \
         {src} ! {convert} ! {vqueue} ! \
         {enc} ! video/x-h264,profile=constrained-baseline ! \
         h264parse config-interval=-1 ! mux.sink_{video_pid} \
         mpegtsmux name=mux alignment=7 ! \
         queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 silent=true ! \
         rtpmp2tpay ssrc=1 perfect-rtptime=false timestamp-offset=0 seqnum-offset=0 ! \
         rtpbin.send_rtp_sink_0 \
         rtpbin.send_rtp_src_0 ! \
         udpsink host={ip} port={port} bind-port={local} sync={sync} async=false \
         rtpbin.send_rtcp_src_0 ! \
         udpsink host={ip} port={rtcp_port} bind-port={local_rtcp} sync=false async=false \
         {audio}",
        rtp_latency = rtp_latency_ms(),
        src = source.description(),
        // Wi-Fi Display agreed a mode with the sink in M3/M4; sending any
        // other size is a mode it never accepted.
        convert = cfg.convert_scale(VideoTarget::Exact((cfg.width, cfg.height))),
        vqueue = if source.is_media() {
            cfg.video_queue().replace("leaky=downstream", "leaky=no")
        } else {
            cfg.video_queue()
        },
        enc = cfg.encoder_stage(false),
        video_pid = WFD_VIDEO_PID,
        // A screen produces frames at the pace of the screen, so waiting on
        // the clock before sending is pure delay. **A file does not**: with
        // `sync=false` a decoder reads a film as fast as the disc allows and
        // the receiver is sent an hour of video in a few seconds. So the file
        // is the one case that has to be paced.
        sync = if rtp_sink_syncs_to_clock() || source.is_media() {
            "true"
        } else {
            "false"
        },
        ip = transport.sink_ip,
        port = transport.rtp_port,
        local = transport.local_rtp_port,
        audio = if source.is_media() {
            cfg.audio_branch(&format!("mux.sink_{}", WFD_AUDIO_PID))
                .replace("leaky=downstream", "leaky=no")
        } else {
            cfg.audio_branch(&format!("mux.sink_{}", WFD_AUDIO_PID))
        },
    )
}

/// How `multisocketsink` positions a client that has just connected.
///
/// `latest-keyframe` (the historical default) places it at the **last keyframe
/// already gone by** — that is, it hands over up to a whole GOP of stale data
/// straight away, which becomes permanent delay because the receiver keeps
/// whatever it buffered. `next-keyframe` makes the client wait for the next
/// keyframe and start clean: it costs a few extra frames before the first
/// picture, and saves that time forever afterwards.
///
/// `BIGNETSCREEN_CC_SYNC_METHOD` allows comparing in the field.
fn chromecast_sync_method() -> String {
    std::env::var("BIGNETSCREEN_CC_SYNC_METHOD").unwrap_or_else(|_| "next-keyframe".to_string())
}

/// The name of the `appsink` that hands encoded frames to Cast Streaming.
pub const MIRROR_VIDEO_SINK: &str = "mirror-video";
/// The same, for the audio track.
pub const MIRROR_AUDIO_SINK: &str = "mirror-audio";

/// The Cast **mirroring** pipeline: it delivers encoded frames, no container.
///
/// Cast Streaming receives *access units* and handles transport itself, so
/// there is no muxer, no HTTP server and no media player on the other end —
/// which is precisely what removes the Default Media Receiver path's seconds
/// of pre-buffering.
///
/// - video as H.264 byte-stream with SPS/PPS at every IDR
///   (`config-interval=-1`), so the receiver can join the stream;
/// - audio in Opus, which is what the mirroring app expects;
/// - the video `appsink` with `max-buffers=1`: the frame goes straight out to
///   the network, with no queue;
/// - the audio `appsink` with a little more slack: Opus frames go out every
///   10 ms and a minimal amount of slack absorbs thread scheduling without
///   becoming perceptible delay (8 frames = 80 ms in the worst case).
///
/// `ceiling` is the largest picture the person is willing to send. The scaler
/// stays out of the way below it, so the capture reaches the encoder at the
/// size it was captured in rather than at the portal's guess about it.
///
/// `gpu` keeps the frame on the graphics card from capture to encoder. It is
/// off unless [`gpu_path_requested`] says otherwise, because it demands a
/// DMA-BUF the compositor may not offer and cannot scale — a screen larger than
/// the ceiling has no path through it.
pub fn mirror_pipeline_description(
    cfg: &StreamConfig,
    source: &VideoSource,
    ceiling: (u32, u32),
    gpu: bool,
) -> String {
    let audio = if cfg.audio_enabled() {
        format!(
            " {src} ! audioconvert ! audioresample ! \
             audio/x-raw,rate=48000,channels=2 ! {queue} ! \
             opusenc bitrate=128000 frame-size=10 ! \
             appsink name={audio_sink} emit-signals=false sync=false \
             max-buffers=8 drop=false",
            src = cfg.audio.description(),
            queue = cfg.mirror_audio_queue(),
            audio_sink = MIRROR_AUDIO_SINK,
        )
    } else {
        String::new()
    };

    format!(
        // The profile comes from the configuration here, unlike the WFD path
        // where the specification pins it: a Cast receiver decodes H.264 High
        // (it is what every streaming service sends it), and constrained
        // baseline was inherited from Miracast rather than required.
        "{src} ! {convert} ! {vqueue} ! \
         {enc} ! video/x-h264,profile={profile},stream-format=byte-stream,\
alignment=au ! \
         h264parse config-interval=-1 ! \
         appsink name={video_sink} emit-signals=false sync=false \
         max-buffers=1 drop=false{audio}",
        profile = cfg.profile.caps_name(),
        src = source.description(),
        convert = if gpu {
            cfg.gpu_convert(ceiling)
        } else {
            cfg.convert_scale(VideoTarget::UpTo(ceiling))
        },
        vqueue = if source.is_media() {
            cfg.video_queue().replace("leaky=downstream", "leaky=no")
        } else {
            cfg.video_queue()
        },
        enc = cfg.encoder_stage(gpu),
        video_sink = MIRROR_VIDEO_SINK,
        audio = audio,
    )
}

/// The maximum resolution sent to a Chromecast.
///
/// The Default Media Receiver decodes H.264 up to 1080p; anything above would
/// need VP9/HEVC, which this pipeline does not produce. Sending the raw screen
/// (a 16:10 1920x1200 one, say) makes the device rescale — or refuse.
pub const CHROMECAST_MAX_RESOLUTION: (u32, u32) = (1920, 1080);

/// The Cast bitrate at the mode Open Screen calibrates against, in kbit/s.
///
/// Open Screen describes 1080p30 as "playable at good quality around 10mbps",
/// and [`StreamConfig::scaled_bitrate_kbps`] returns exactly this at that mode.
/// It is the anchor the rest of the table is read against, not a ceiling: a
/// receiver that declares `maxBitRate` caps the stream, and a receiver that
/// declares nothing gets what the mode needs.
///
/// A flat 10 Mbit ceiling used to sit here for every Cast mode. It could not
/// protect anybody: at 1080p30 and below the table already returns this number
/// or less, so the ceiling only ever fired against a bigger picture, handing a
/// 1440p60 screen the bitrate of a 1080p30 one. The field report behind it — a
/// Google TV Stick losing frames from the first second, with a backlog that
/// grew until it ended the session — was measured before the sender bounded
/// its own backlog at all (see `flow.rs`). Re-measure before restoring any
/// fixed ceiling, and if one is needed, pin it to a number that was observed.
pub const CAST_REFERENCE_BITRATE_KBPS: u32 = 10_000;

/// The rate-control budget the Cast **mirroring** encoder is held to, in
/// frames. See [`StreamConfig::vbv_frames`] for the table behind the number.
///
/// Every other path hands its frames to a muxer or to a stack that paces on
/// its behalf. Mirroring puts each frame on the wire as its own burst of UDP
/// datagrams, so the size of one frame is something the network sees directly,
/// and an encoder given no budget will spend it: switching a browser tab
/// changes the whole screen and produces one enormous frame, several hundred
/// datagrams the pacer then releases a burst at a time. The receiver cannot
/// show any of that change until the last packet of it arrives.
///
/// This was removed once, on the reasoning that an encoder knows its own job.
/// It does — about pictures. It cannot know that here a frame is a burst on a
/// paced link, which is a property of this transport and not of H.264. That is
/// the line: configure what the component has no way to know, and nothing else.
pub const CAST_VBV_FRAMES: u32 = 3;

/// The `multisocketsink`'s name in the transport-stream pipeline description.
///
/// The HTTP server looks the element up by this name in order to hand it the
/// receiver's socket with the headers already written.
pub const TS_HTTP_SINK_NAME: &str = "cc-sink";

/// Conservative DLNA default when the user has not selected a higher ceiling.
/// This is not a capability negotiated with the receiver.
pub const DLNA_MAX_RESOLUTION: (u32, u32) = (1920, 1080);

/// How long the transport-stream multiplex waits for a file's first picture.
const MEDIA_MUX_WAIT_NS: u64 = 1_000_000_000;

/// Builds an H.264 + AAC transport stream for a receiver that fetches it over
/// HTTP — the Cast fallback and every DLNA renderer.
///
/// MP2T is an officially supported Cast container and the one DLNA profile
/// family a television is certain to have. Explicit byte-stream access units
/// and short mux/queue batches bound sender buffering; the receiver may still
/// prebuffer seconds. This is not the raw-RTP mirroring path.
/// `multisocketsink` takes the receiver socket after HTTP headers are written.
///
/// `mux_bitrate_bps` pads the multiplex with null packets to a constant rate.
/// `None` sends only real data, which is what Cast gets. **A DLNA renderer
/// needs the padding**, and the reason is measured rather than assumed: a
/// Panasonic VIErA prebuffers a fixed number of *bytes*, so the delay it adds
/// is that buffer divided by the bitrate. Streaming content that happened to
/// compress to 320 kbit/s put the picture **6 seconds** behind; padding the
/// same stream to 8 Mbit/s brought it to 2 s, and 20 Mbit/s to 1.5 s. Padding
/// to what the encoder already targets captures nearly all of that without
/// spending the network on null packets.
pub fn ts_http_pipeline_description(
    cfg: &StreamConfig,
    source: &VideoSource,
    target: VideoTarget,
    mux_bitrate_bps: Option<u32>,
) -> String {
    // MPEG-TS buffers do not preserve H.264 DELTA_UNIT flags. A soft-limit
    // "keyframe" resync would cut arbitrary TS/PES packets, corrupting decode.
    // Bound DLNA backlog by bytes instead: about two seconds at the target
    // rate, at most 8 MiB. Disconnect a stalled reader rather than grow forever.
    let sink_policy = match mux_bitrate_bps {
        Some(bps) => format!(
            "sync-method=latest recover-policy=none unit-format=bytes units-max={}",
            (u64::from(bps) / 4).clamp(1_048_576, 8_388_608)
        ),
        None => format!(
            "sync-method={} recover-policy=keyframe",
            chromecast_sync_method()
        ),
    };
    format!(
        "{src} ! {convert} ! {vqueue} ! \
         {enc} ! h264parse config-interval=-1 ! \
         video/x-h264,stream-format=byte-stream,alignment=au ! \
         mpegtsmux name=mux alignment=7{padding}{wait_for_video} ! \
         queue max-size-buffers=0 max-size-bytes=0 max-size-time=50000000 silent=true ! \
         multisocketsink name={sink} sync=false async=false blocksize=8192 \
         burst-format=buffers {sink_policy} \
         {audio}",
        padding = match mux_bitrate_bps {
            Some(bps) => format!(" bitrate={bps}"),
            None => String::new(),
        },
        // A file's picture comes out of the decoder a few hundred milliseconds
        // after the silence that backs its sound, and without a wait the
        // multiplex starts with a program table that lists audio alone. A
        // Panasonic VIErA takes that first table as the whole program, shows
        // "waiting" and gives up; a session that happened to seek first won
        // the race and played. The wait costs nothing once both are flowing.
        wait_for_video = if source.is_media() {
            format!(" latency={MEDIA_MUX_WAIT_NS}")
        } else {
            String::new()
        },
        src = source.description(),
        convert = cfg.convert_scale(target),
        vqueue = if source.is_media() {
            cfg.video_queue().replace("leaky=downstream", "leaky=no")
        } else {
            cfg.video_queue()
        },
        enc = cfg.encoder_stage(false),
        sink = TS_HTTP_SINK_NAME,
        audio = if source.is_media() {
            cfg.audio_branch("mux.")
                .replace("leaky=downstream", "leaky=no")
        } else {
            cfg.audio_branch("mux.")
        },
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn dlna_ts_negotiates_physical_pixels_within_the_ceiling() {
        use gst::prelude::*;

        for (source, ceiling, expected) in [
            ((2560, 1440), (2560, 1440), (2560, 1440)),
            ((1280, 720), (2560, 1440), (1280, 720)),
            ((2560, 1440), (1920, 1080), (1920, 1080)),
            ((931, 601), (2560, 1440), (930, 600)),
        ] {
            let cfg = StreamConfig {
                width: ceiling.0,
                height: ceiling.1,
                ..Default::default()
            };
            let description = ts_http_pipeline_description(
                &cfg,
                &VideoSource::Test,
                VideoTarget::UpTo(ceiling),
                Some(8_000_000),
            )
            .replacen(
                "videotestsrc is-live=true",
                &format!(
                    "videotestsrc is-live=true ! video/x-raw,width={},height={}",
                    source.0, source.1
                ),
                1,
            );
            let (pipeline, _events) = build_pipeline(&description, cfg.latency_ms()).unwrap();
            let frames = count_buffers(&pipeline, ENCODER_NAME).unwrap();
            let guard = PipelineGuard::new(pipeline);
            guard.pipeline().set_state(gst::State::Playing).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while frames.load(std::sync::atomic::Ordering::Relaxed) == 0
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(
                frames.load(std::sync::atomic::Ordering::Relaxed) > 0,
                "encoder must produce H.264 for {source:?}"
            );
            assert_eq!(
                negotiated_video_size(
                    guard.pipeline(),
                    std::time::Instant::now() + std::time::Duration::from_secs(5)
                ),
                Some(expected),
                "source={source:?}, ceiling={ceiling:?}"
            );
        }
    }

    #[test]
    fn a_ten_bit_film_gets_black_bars() {
        use gst::prelude::*;
        init().unwrap();
        let cfg = StreamConfig::default();
        let description = format!(
            "videotestsrc num-buffers=1 pattern=white \
             ! video/x-raw,format=P010_10LE,width=320,height=134,pixel-aspect-ratio=1/1,framerate=30/1 \
             ! {} ! videoconvert ! video/x-raw,format=RGB ! appsink name=out",
            cfg.convert_scale(VideoTarget::Exact((320, 180)))
        );
        let pipeline = gst::parse::launch(&description)
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
        let guard = PipelineGuard::new(pipeline);
        guard.pipeline().set_state(gst::State::Playing).unwrap();
        let sample = guard
            .pipeline()
            .by_name("out")
            .unwrap()
            .emit_by_name::<Option<gst::Sample>>(
                "try-pull-sample",
                &[&gst::ClockTime::from_seconds(5)],
            )
            .expect("one frame");
        let frame = sample.buffer().unwrap().map_readable().unwrap();
        // Top-left: inside the upper bar. Centre: the white picture.
        assert!(frame[..3].iter().all(|&c| c < 24), "bar {:?}", &frame[..3]);
        let centre = (90 * 320 + 160) * 3;
        assert!(frame[centre..centre + 3].iter().all(|&c| c > 200));
    }

    #[test]
    fn diagnostics_map_the_encoder_pts_offset_through_its_segment() {
        use gst::prelude::*;
        init().unwrap();
        let pad = gst::Pad::builder(gst::PadDirection::Src).build();
        pad.set_active(true).unwrap();
        let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
        let offset = gst::ClockTime::from_seconds(1000 * 3600);
        segment.set_start(offset);
        pad.store_sticky_event(&gst::event::Segment::new(&segment))
            .unwrap();
        assert_eq!(pad_running_time(&pad, offset), Some(gst::ClockTime::ZERO));
        assert_eq!(
            pad_running_time(&pad, offset + gst::ClockTime::from_mseconds(50)),
            Some(gst::ClockTime::from_mseconds(50))
        );
    }
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
        let description = format!("{} ! video/x-raw,width=320,height=240 ! fakesink sync=true {} ! audioconvert ! fakesink sync=true", source.description(), AudioSource::MediaFile.description());
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
            let description = format!("videotestsrc num-buffers=15 ! video/x-raw,width=320,height=240,framerate=30/1 ! vp8enc deadline=1 ! matroskamux name=mux ! filesink location={} {audio}", escape_location(&path));
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

    use super::*;
    use std::net::Ipv4Addr;

    fn cfg_at(width: u32, height: u32, fps: u32) -> StreamConfig {
        StreamConfig {
            width,
            height,
            fps,
            ..Default::default()
        }
    }

    #[test]
    fn a_preference_may_exceed_the_cast_default_but_never_a_negotiated_mode() {
        // The two caps mean different things, and treating them alike is what
        // sent a 1440p screen to a Cast receiver at 1080p with no way to ask
        // for more.
        //
        // Cast: 1080p is our own safe guess, so a person who chooses 1440p
        // gets 1440p. Miracast: the sink listed the modes it accepts, and
        // exceeding them is not ours to decide.
        //
        // The preference is set here rather than read from the machine running
        // the tests. It used to be read, and the assertion then only held for
        // someone whose `settings.conf` still said "high": choosing any other
        // quality failed a test about a code path that was working correctly.
        let restore = crate::settings::current();

        let mut chosen = restore.clone();
        chosen.quality = crate::settings::Quality::Ultra;
        crate::settings::set_in_memory(&chosen);
        assert_eq!(
            StreamConfig::preferred_or(CHROMECAST_MAX_RESOLUTION),
            crate::settings::Quality::Ultra.resolution(),
            "the Cast default is our guess, and a stated preference outranks it"
        );
        let sink_mode = (1280, 720);
        assert_eq!(
            StreamConfig::capped_by_preference(sink_mode),
            sink_mode,
            "a negotiated mode is never exceeded"
        );

        let mut untouched = restore.clone();
        untouched.quality = crate::settings::Quality::default();
        crate::settings::set_in_memory(&untouched);
        assert_eq!(
            StreamConfig::preferred_or(CHROMECAST_MAX_RESOLUTION),
            CHROMECAST_MAX_RESOLUTION,
            "an untouched preference must not change what was sent before"
        );

        crate::settings::set_in_memory(&restore);
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
    fn a_still_screen_is_not_left_at_one_frame_per_second() {
        // A compositor repaints what changed, so on a still screen the only
        // thing producing buffers is this timeout — it *is* the frame rate.
        // At the 1000 ms it used to be, typing in a terminal changed one
        // character cell and the picture updated about once a second.
        const _: () = assert!(CAPTURE_KEEPALIVE_MS <= 1000 / 30);
        let description = VideoSource::PipeWire {
            fd: Some(7),
            node_id: 42,
            serial: None,
            size: None,
            min_buffers: None,
        }
        .description();
        assert!(
            description.contains("keepalive-time=33") && description.contains("resend-last=true"),
            "{description}"
        );
    }

    #[test]
    fn only_the_path_that_asks_for_a_vbv_limit_gets_one() {
        // The whole point of the setting being per-path: Miracast, NDI and the
        // browser path were never measured here, so their encoder line has to
        // come out exactly as it did before.
        let untouched = StreamConfig {
            encoder: H264Encoder::NvH264,
            ..Default::default()
        };
        let desc = untouched.encoder.encoder_description(&untouched);
        assert!(!desc.contains("vbv-buffer-size"), "{desc}");
        // Not even as an explicit zero: the property is conditionally
        // available, and naming it can cost a GPU its hardware encoder.
        assert!(
            desc.contains("rc-mode=cbr bitrate=10000 gop-size=30"),
            "{desc}"
        );

        let x264 = StreamConfig::default();
        assert!(
            x264.encoder
                .encoder_description(&x264)
                .contains("vbv-buf-capacity=50"),
            "x264 keeps the buffer it has always used"
        );
    }

    #[test]
    fn a_link_that_can_ask_for_a_key_frame_is_not_sent_one_every_second() {
        // A periodic IDR bounds recovery where the receiver cannot ask. Cast
        // can: it sends a picture loss indication and the sender answers, and
        // NACK repairs loss before that. Scheduling one a second as well only
        // makes the picture pulse — each IDR is a whole picture inside a
        // rate-control budget a few frames wide, so it lands softer than its
        // neighbours and they sharpen it again.
        let direct = StreamConfig {
            fps: 60,
            ..Default::default()
        };
        assert_eq!(direct.gop(), 60, "a direct link keeps its one second");

        // Nought means no schedule: the encoder decides, and a key frame is
        // produced when the receiver asks. Each encoder spells it its own way,
        // and only the two that could be checked here are told directly.
        let cast = StreamConfig {
            fps: 60,
            gop_seconds: 0,
            encoder: H264Encoder::NvH264,
            ..Default::default()
        };
        assert_eq!(cast.gop(), 0);
        assert!(
            cast.encoder
                .encoder_description(&cast)
                .contains("gop-size=-1"),
            "{}",
            cast.encoder.encoder_description(&cast)
        );
        let x264 = StreamConfig {
            encoder: H264Encoder::X264,
            ..cast
        };
        assert!(
            x264.encoder
                .encoder_description(&x264)
                .contains("key-int-max=0"),
            "{}",
            x264.encoder.encoder_description(&x264)
        );
        // An encoder nobody here can test keeps a finite distance rather than
        // a value a driver might read as "every frame is an I-frame".
        let va = StreamConfig {
            encoder: H264Encoder::VaH264,
            ..cast
        };
        assert!(
            va.encoder
                .encoder_description(&va)
                .contains("key-int-max=600"),
            "{}",
            va.encoder.encoder_description(&va)
        );
    }

    #[test]
    fn a_rate_control_buffer_is_asked_for_or_left_alone() {
        // The arithmetic is pinned because it crosses two units and getting it
        // wrong shows up only as a picture that takes too long to change.
        let cfg = StreamConfig {
            fps: 30,
            bitrate_kbps: CAST_REFERENCE_BITRATE_KBPS,
            encoder: H264Encoder::NvH264,
            vbv_frames: CAST_VBV_FRAMES,
            ..Default::default()
        };
        assert_eq!(cfg.vbv_ms(), 100);
        assert!(
            cfg.encoder
                .encoder_description(&cfg)
                .contains("vbv-buffer-size=1000"),
            "{}",
            cfg.encoder.encoder_description(&cfg)
        );

        // And at 60 fps it is still three frames, not twice as much buffer.
        let fast = StreamConfig { fps: 60, ..cfg };
        assert_eq!(fast.vbv_ms(), 50);
        assert!(
            fast.encoder
                .encoder_description(&fast)
                .contains("vbv-buffer-size=500"),
            "{}",
            fast.encoder.encoder_description(&fast)
        );

        // Nought is the default and means the property is never written, which
        // is what every path that hands its frames to a muxer gets. Removing it
        // from Cast mirroring was tried and reverted: a tab switch became one
        // enormous frame and the receiver could show none of the change until
        // its last datagram arrived.
        const _: () = assert!(
            CAST_VBV_FRAMES >= 2,
            "one frame measured worse than no limit"
        );
        let untouched = StreamConfig {
            encoder: H264Encoder::NvH264,
            ..Default::default()
        };
        assert_eq!(untouched.vbv_frames, 0);
        assert!(
            !untouched
                .encoder
                .encoder_description(&untouched)
                .contains("vbv-buffer-size"),
            "{}",
            untouched.encoder.encoder_description(&untouched)
        );
    }

    #[test]
    fn bitrate_scales_with_resolution() {
        assert_eq!(cfg_at(1280, 720, 30).scaled_bitrate_kbps(), 9_000);
        assert_eq!(cfg_at(1920, 1080, 30).scaled_bitrate_kbps(), 10_000);
        assert_eq!(cfg_at(3840, 2160, 30).scaled_bitrate_kbps(), 20_000);
    }

    #[test]
    fn bitrate_scales_with_framerate() {
        assert_eq!(cfg_at(1920, 1080, 30).scaled_bitrate_kbps(), 10_000);
        assert_eq!(cfg_at(1920, 1080, 60).scaled_bitrate_kbps(), 15_000);
    }

    #[test]
    fn explicit_bitrate_wins() {
        let cfg = StreamConfig {
            bitrate_kbps: 4_200,
            ..Default::default()
        };
        assert_eq!(cfg.scaled_bitrate_kbps(), 4_200);
    }

    #[test]
    fn openh264_bitrate_is_in_bits_per_second() {
        // A regression: `openh264enc` measures bitrate in bit/s. Feeding it
        // kbit/s produced 10 kbit/s — an unrecognisable picture on the software
        // fallback.
        let cfg = StreamConfig {
            encoder: H264Encoder::OpenH264,
            ..Default::default()
        };
        let desc = cfg.encoder.encoder_description(&cfg);
        assert!(desc.contains("bitrate=10000000"), "{desc}");
    }

    #[test]
    fn kbps_encoders_keep_kilobits() {
        for enc in [
            H264Encoder::X264,
            H264Encoder::VaH264,
            H264Encoder::VaapiH264,
            H264Encoder::NvH264,
        ] {
            let cfg = StreamConfig {
                encoder: enc,
                ..Default::default()
            };
            let desc = cfg.encoder.encoder_description(&cfg);
            assert!(desc.contains("bitrate=10000"), "{enc:?}: {desc}");
        }
    }

    #[test]
    fn hardware_encoders_disable_b_frames() {
        let cfg = StreamConfig {
            encoder: H264Encoder::VaH264,
            ..Default::default()
        };
        let desc = cfg.encoder.encoder_description(&cfg);
        assert!(desc.contains("b-frames=0"), "{desc}");
        assert!(desc.contains("rate-control=cbr"), "{desc}");
    }

    #[test]
    fn hardware_is_preferred_on_every_intel_driver() {
        // The Intel `xe` driver block is gone: it hung during encoding in 2022
        // and works fine in 2026 (verified in the field on a Core Ultra 200H).
        // Problematic hardware is now detected at runtime, by the absence of
        // frames — see [`MonitoredPipeline`].
        let avail = [H264Encoder::VaH264, H264Encoder::X264];
        for driver in [GpuDriver::I915, GpuDriver::Xe, GpuDriver::Amdgpu] {
            assert_eq!(
                select_encoder(&avail, driver),
                Some(H264Encoder::VaH264),
                "{driver:?}"
            );
        }
    }

    #[test]
    fn nouveau_still_avoids_hardware() {
        // This block stays: `nouveau` exposes no usable NVENC, and that is
        // structural rather than a driver bug that could be fixed.
        let avail = [H264Encoder::NvH264, H264Encoder::X264];
        assert_eq!(
            select_encoder(&avail, GpuDriver::Nouveau),
            Some(H264Encoder::X264)
        );
    }

    #[test]
    fn what_each_driver_is_allowed_to_encode_with() {
        use H264Encoder::*;

        // The machines this ships to mostly have no usable hardware encoder,
        // so the rule that decides what they get is worth pinning. Reads as a
        // table on purpose: a change to `encoder_fits_driver` should have to
        // change a row here and say why.
        let allowed = |driver: GpuDriver| {
            H264Encoder::ALL
                .into_iter()
                .filter(|enc| encoder_fits_driver(*enc, driver))
                .collect::<Vec<_>>()
        };

        // Software is never filtered out, whatever the machine has. It is the
        // only thing keeping a session possible on hardware we cannot use.
        for driver in [
            GpuDriver::I915,
            GpuDriver::Xe,
            GpuDriver::Amdgpu,
            GpuDriver::Nvidia,
            GpuDriver::Nouveau,
            GpuDriver::Unknown,
        ] {
            let allowed = allowed(driver);
            assert!(allowed.contains(&X264), "{driver:?} lost x264");
            assert!(allowed.contains(&OpenH264), "{driver:?} lost openh264");
        }

        // NVENC only where NVIDIA's own driver is loaded, and VA-API never
        // alongside it: the NVIDIA stack exposes no usable VA-API encoder.
        assert!(encoder_fits_driver(NvH264, GpuDriver::Nvidia));
        assert!(!encoder_fits_driver(VaH264, GpuDriver::Nvidia));
        assert!(!encoder_fits_driver(VaapiH264, GpuDriver::Nvidia));
        for driver in [GpuDriver::I915, GpuDriver::Amdgpu, GpuDriver::Unknown] {
            assert!(!encoder_fits_driver(NvH264, driver), "NVENC on {driver:?}");
            assert!(
                encoder_fits_driver(VaH264, driver),
                "no VA-API on {driver:?}"
            );
        }

        // `nouveau` exposes no usable NVENC, so it gets nothing in hardware.
        assert_eq!(allowed(GpuDriver::Nouveau), vec![X264, OpenH264]);

        // Intel `xe` is the odd one: the enum says VA-API encode hangs there
        // and software is preferred, and the filter still offers VA-API. That
        // is deliberate — `working_encoder` probes each candidate and moves on
        // when it produces no frame — but it costs the probe's timeout on
        // every session that starts on such a machine, and it is the only
        // driver where the comment and the filter say different things.
        assert!(
            encoder_fits_driver(VaH264, GpuDriver::Xe),
            "if this ever changes, the probe is no longer what saves an xe machine"
        );
    }

    #[test]
    fn turning_acceleration_off_leaves_only_software() {
        use H264Encoder::*;

        for enc in [VaH264, VaapiH264, NvH264, V4l2H264] {
            assert!(Acceleration::Allowed.permits(enc));
            assert!(
                !Acceleration::Off.permits(enc),
                "{enc:?} survived the switch"
            );
        }
        // Turning it off must never leave a session with nothing to encode
        // with, so software is permitted either way.
        for enc in [X264, OpenH264] {
            assert!(Acceleration::Allowed.permits(enc));
            assert!(Acceleration::Off.permits(enc), "{enc:?} went with it");
        }

        let settings = crate::settings::Settings {
            hardware_encoding: false,
            ..Default::default()
        };
        assert_eq!(Acceleration::from_settings(&settings), Acceleration::Off);
    }

    #[test]
    fn candidates_keep_a_software_backup() {
        // If the GPU produces no frames, the software path has to be in the
        // list to take over — without it the fallback would have nowhere to
        // go.
        let candidates = encoder_candidates(GpuDriver::Xe, Acceleration::Allowed);
        if candidates.len() < 2 {
            return; // a test machine with only one encoder
        }
        assert!(
            candidates.iter().any(|e| !e.is_hardware()),
            "no software fallback: {candidates:?}"
        );
        let mut sorted = candidates.clone();
        sorted.sort_by_key(|e| std::cmp::Reverse(e.priority()));
        assert_eq!(candidates, sorted, "it must come ordered by priority");
    }

    #[test]
    fn every_encoder_is_findable_for_monitoring() {
        // The probe that counts frames locates the encoder by name; without
        // the name in the description, the evidence-based fallback could not
        // work.
        for enc in H264Encoder::ALL {
            let cfg = StreamConfig {
                encoder: enc,
                ..Default::default()
            };
            assert!(
                cfg.encoder
                    .encoder_description(&cfg)
                    .contains(&format!("name={ENCODER_NAME}")),
                "{enc:?}"
            );
        }
    }

    #[test]
    fn monitor_counts_frames_leaving_the_encoder() {
        if init().is_err() || gst::ElementFactory::find("videotestsrc").is_none() {
            return;
        }
        let desc = format!(
            "videotestsrc is-live=false num-buffers=10 ! identity name={ENCODER_NAME} ! fakesink"
        );
        let monitored =
            build_monitored(&desc, PIPELINE_LATENCY_AUTO, H264Encoder::X264).expect("pipeline");
        monitored
            .pipeline
            .set_state(gst::State::Playing)
            .expect("playing");
        std::thread::sleep(std::time::Duration::from_millis(600));
        let frames = monitored.frames_encoded();
        monitored.shutdown();
        assert!(frames > 0, "the probe counted no frames at all");
    }

    #[test]
    fn monitor_reports_zero_when_the_encoder_stays_silent() {
        // This is the case that triggers the fall back to software: the
        // encoder exists, accepts the configuration and encodes nothing — the
        // silent failure the driver blacklist was trying to guess at.
        if init().is_err() || gst::ElementFactory::find("fakesrc").is_none() {
            return;
        }
        let desc =
            format!("fakesrc num-buffers=0 ! identity name={ENCODER_NAME} ! fakesink sync=false");
        let monitored =
            build_monitored(&desc, PIPELINE_LATENCY_AUTO, H264Encoder::VaH264).expect("pipeline");
        monitored
            .pipeline
            .set_state(gst::State::Playing)
            .expect("playing");
        std::thread::sleep(std::time::Duration::from_millis(300));
        let frames = monitored.frames_encoded();
        monitored.shutdown();
        assert_eq!(frames, 0, "there should be no frames");
    }

    #[test]
    fn nvenc_only_on_nvidia() {
        let avail = [H264Encoder::NvH264, H264Encoder::X264];
        assert_eq!(
            select_encoder(&avail, GpuDriver::Nvidia),
            Some(H264Encoder::NvH264)
        );
        assert_eq!(
            select_encoder(&avail, GpuDriver::I915),
            Some(H264Encoder::X264)
        );
    }

    #[test]
    fn va_not_used_on_proprietary_nvidia() {
        let avail = [H264Encoder::VaH264, H264Encoder::X264];
        assert_eq!(
            select_encoder(&avail, GpuDriver::Nvidia),
            Some(H264Encoder::X264)
        );
    }

    #[test]
    fn no_encoder_available() {
        assert_eq!(select_encoder(&[], GpuDriver::I915), None);
    }

    #[test]
    fn driver_from_kernel_module() {
        assert_eq!(GpuDriver::from_kernel_module("xe"), GpuDriver::Xe);
        assert_eq!(GpuDriver::from_kernel_module("i915"), GpuDriver::I915);
        assert_eq!(GpuDriver::from_kernel_module("amdgpu"), GpuDriver::Amdgpu);
        assert_eq!(GpuDriver::from_kernel_module("zzz"), GpuDriver::Unknown);
        assert!(GpuDriver::Xe.hardware_encode_is_reliable());
        assert!(GpuDriver::I915.hardware_encode_is_reliable());
        assert!(!GpuDriver::Nouveau.hardware_encode_is_reliable());
    }

    fn wfd_desc(cfg: &StreamConfig) -> String {
        let src = VideoSource::PipeWire {
            fd: Some(7),
            node_id: 42,
            serial: None,
            size: None,
            min_buffers: None,
        };
        let transport = WfdTransport::new(IpAddr::V4(Ipv4Addr::new(192, 168, 49, 1)), 19000);
        wfd_pipeline_description(cfg, &src, &transport)
    }

    #[test]
    fn mutter_source_omits_the_fd_but_keeps_the_node() {
        // Mutter publishes into the session's PipeWire daemon: there is no fd
        // to pass, but `path` remains mandatory.
        let cfg = StreamConfig::default();
        let src = VideoSource::PipeWire {
            fd: None,
            node_id: 42,
            serial: None,
            size: None,
            min_buffers: None,
        };
        let desc = ts_http_pipeline_description(
            &cfg,
            &src,
            VideoTarget::Exact((cfg.width, cfg.height)),
            None,
        );
        assert!(desc.contains("pipewiresrc name=capture path=42"), "{desc}");
        assert!(!desc.contains("fd="), "{desc}");
    }

    #[test]
    fn pipewire_source_carries_fd_and_node() {
        // A regression: without `fd=`/`path=` pipewiresrc does not capture the
        // stream the portal opened — it captured an arbitrary node from the
        // daemon.
        let desc = wfd_desc(&StreamConfig::default());
        assert!(
            desc.contains("pipewiresrc name=capture fd=7 path=42"),
            "{desc}"
        );
    }

    #[test]
    fn videorate_precedes_scaling() {
        let desc = wfd_desc(&StreamConfig::default());
        let rate = desc.find("videorate").expect("videorate is mandatory");
        let scale = desc.find("videoscale").expect("videoscale");
        assert!(rate < scale, "videorate must precede videoscale: {desc}");
    }

    #[test]
    fn the_virtual_card_replaces_the_default_monitor_rather_than_adding_to_it() {
        // The whole point of the option is what is *not* captured. A branch
        // that read both would send the private call the person switched the
        // card on to avoid, and would look perfectly healthy doing it.
        let virtual_only = AudioSource::System(Monitor::Virtual).description();
        assert!(
            virtual_only.contains(crate::virtual_sink::MONITOR),
            "{virtual_only}"
        );
        assert!(
            !virtual_only.contains("@DEFAULT_MONITOR@"),
            "the default monitor is still being captured: {virtual_only}"
        );

        // And the microphone still mixes on top, which is the answer to
        // "only what is routed to it": a person's own voice is not something
        // an application routes anywhere.
        let with_mic = AudioSource::SystemAndMic {
            monitor: Monitor::Virtual,
            volume: 0.8,
        }
        .description();
        assert!(
            with_mic.contains(crate::virtual_sink::MONITOR),
            "{with_mic}"
        );
        assert!(with_mic.contains("@DEFAULT_SOURCE@"), "{with_mic}");
        assert!(
            !with_mic.contains("@DEFAULT_MONITOR@"),
            "the default monitor leaked into the mix: {with_mic}"
        );
    }

    #[test]
    fn the_switch_is_what_chooses_the_monitor() {
        let off = crate::settings::Settings {
            virtual_audio: false,
            ..Default::default()
        };
        assert_eq!(Monitor::from_settings(&off), Monitor::Default);
        let on = crate::settings::Settings {
            virtual_audio: true,
            ..Default::default()
        };
        assert_eq!(Monitor::from_settings(&on), Monitor::Virtual);
    }

    #[test]
    fn wfd_pipeline_has_audio_and_baseline_profile() {
        let desc = wfd_desc(&StreamConfig::default());
        assert!(
            desc.contains("avenc_aac"),
            "audio is mandatory on WFD: {desc}"
        );
        assert!(desc.contains("profile=constrained-baseline"), "{desc}");
        assert!(desc.contains("mux."), "{desc}");
    }

    #[test]
    fn wfd_pipeline_targets_the_negotiated_port() {
        let desc = wfd_desc(&StreamConfig::default());
        assert!(desc.contains("host=192.168.49.1 port=19000"), "{desc}");
        // RTCP goes to port+1.
        assert!(desc.contains("port=19001"), "{desc}");
    }

    #[test]
    fn chromecast_pipeline_has_audio_track() {
        let cfg = StreamConfig::default();
        let desc = ts_http_pipeline_description(
            &cfg,
            &VideoSource::Test,
            VideoTarget::Exact((cfg.width, cfg.height)),
            None,
        );
        assert!(desc.contains("avenc_aac"), "{desc}");
        assert!(desc.contains("mpegtsmux"), "{desc}");
    }

    #[test]
    fn chromecast_sink_does_not_sync_on_the_clock() {
        // `sync=true` on multisocketsink adds an entire pipeline latency
        // before the byte reaches the socket. The source is already live.
        let cfg = StreamConfig::default();
        let desc = ts_http_pipeline_description(
            &cfg,
            &VideoSource::Test,
            VideoTarget::Exact((cfg.width, cfg.height)),
            None,
        );
        assert!(desc.contains("sync=false"), "{desc}");
        assert!(!desc.contains("sync=true"), "{desc}");
        assert!(desc.contains("blocksize=8192"), "{desc}");
    }

    #[test]
    fn mirror_pipeline_has_no_muxer_and_no_container() {
        // Cast Streaming receives access units; any muxer here would be extra
        // buffering, and the receiver would not know what to do with the
        // container.
        let cfg = StreamConfig::default();
        let desc =
            mirror_pipeline_description(&cfg, &VideoSource::Test, CHROMECAST_MAX_RESOLUTION, false);
        for muxer in ["matroskamux", "mp4mux", "mpegtsmux", "webmmux"] {
            assert!(!desc.contains(muxer), "{muxer} should not be here: {desc}");
        }
        assert!(!desc.contains("multisocketsink"), "{desc}");
        assert!(desc.contains("stream-format=byte-stream"), "{desc}");
        assert!(desc.contains("h264parse config-interval=-1"), "{desc}");
    }

    #[test]
    fn mirror_pipeline_exposes_both_sinks_without_queueing() {
        let cfg = StreamConfig::default();
        let desc =
            mirror_pipeline_description(&cfg, &VideoSource::Test, CHROMECAST_MAX_RESOLUTION, false);
        assert!(
            desc.contains(&format!("name={MIRROR_VIDEO_SINK}")),
            "{desc}"
        );
        assert!(
            desc.contains(&format!("name={MIRROR_AUDIO_SINK}")),
            "{desc}"
        );
        // `sync=false`: the capture sets the pace; waiting on the clock here
        // only adds delay before the frame reaches the network.
        assert!(desc.contains("sync=false"), "{desc}");
        assert!(desc.contains("max-buffers=1"), "{desc}");
    }

    #[test]
    fn the_muxed_path_keeps_its_audio_queue_short() {
        // On the muxed path video is interleaved with audio, so slack given to
        // the audio queue becomes picture delay. Raising this queue to 200 ms
        // took Miracast away from the ~40 ms measured in the field.
        const _: () = assert!(
            AUDIO_QUEUE_MS <= 50,
            "the muxed path's audio queue turns into picture delay"
        );
        // And the mirroring one, which goes through no muxer, can be generous.
        const _: () = assert!(MIRROR_AUDIO_QUEUE_MS >= AUDIO_QUEUE_MS);

        // The generated description has to reflect both choices.
        let cfg = StreamConfig::default();
        assert!(cfg.audio_queue().contains("leaky=downstream"));
        assert!(!cfg.mirror_audio_queue().contains("leaky"));
    }

    #[test]
    fn film_mode_buffers_without_going_under_the_encoder_minimum() {
        // Buffering is added *on top of* what the encoder needs. Setting a
        // latency below the pipeline's own minimum is what made sinks drop
        // late buffers and show a black screen, so the film profile must never
        // reduce it.
        let spiky = StreamConfig {
            encoder: H264Encoder::OpenH264,
            ..Default::default()
        };
        let quick = StreamConfig::default();

        crate::latency::set(crate::latency::Profile::Responsive);
        let quick_responsive = quick.latency_ms();
        let spiky_responsive = spiky.latency_ms();

        crate::latency::set(crate::latency::Profile::Film);
        assert!(
            quick.latency_ms() > quick_responsive,
            "film mode has to add buffering"
        );
        assert!(
            spiky.latency_ms() >= spiky_responsive,
            "and never take away what an encoder already needs"
        );

        // The RTP jitter buffer follows the same profile: it is where uneven
        // arrival is actually absorbed on the Miracast path.
        assert!(rtp_latency_ms() > RTP_LATENCY_MS);

        crate::latency::set(crate::latency::Profile::Responsive);
        assert_eq!(rtp_latency_ms(), RTP_LATENCY_MS);
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
        let wfd = wfd_desc(&cfg);
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

    #[test]
    fn nvenc_gets_its_input_uploaded_by_the_element_that_is_best_at_it() {
        // Measured, 300 frames at 2560x1440: 1.15s through cudaupload against
        // 1.30s letting the encoder upload its own input, repeatably.
        let nv = StreamConfig {
            encoder: H264Encoder::NvH264,
            ..Default::default()
        };
        let desc = mirror_pipeline_description(&nv, &VideoSource::Test, (2560, 1440), false);
        assert!(desc.contains("cudaupload ! nvh264enc"), "{desc}");
        // On the GPU path the frame is already on the card.
        let gpu = mirror_pipeline_description(&nv, &VideoSource::Test, (2560, 1440), true);
        assert!(!gpu.contains("cudaupload"), "{gpu}");
        // And no other encoder grows a CUDA upload it cannot use.
        for encoder in [
            H264Encoder::X264,
            H264Encoder::VaH264,
            H264Encoder::OpenH264,
        ] {
            let cfg = StreamConfig {
                encoder,
                ..Default::default()
            };
            let desc = mirror_pipeline_description(&cfg, &VideoSource::Test, (2560, 1440), false);
            assert!(!desc.contains("cudaupload"), "{desc}");
        }
        // The real string, built and linked: the one check that catches a
        // misspelled element or a caps field the encoder will not take.
        if init().is_ok() {
            let built = mirror_pipeline_description(&nv, &VideoSource::Test, (2560, 1440), false);
            gst::parse::launch(&built).expect("the CUDA description builds");
        }
    }

    #[test]
    fn the_gpu_path_asks_for_a_dmabuf_and_builds() {
        let cfg = StreamConfig {
            width: 2560,
            height: 1440,
            fps: 60,
            encoder: H264Encoder::NvH264,
            ..Default::default()
        };
        let desc = mirror_pipeline_description(&cfg, &VideoSource::Test, (2560, 1440), true);
        // Without asking for the DMA-BUF, `glupload` would accept system memory
        // and upload it — the same copy, behind a longer pipeline.
        assert!(desc.contains("video/x-raw(memory:DMABuf)"), "{desc}");
        assert!(desc.contains("glupload"), "{desc}");
        // `glcolorscale` will not link to this encoder: measured, and the whole
        // reason the caller only picks this path for a screen that fits.
        assert!(!desc.contains("glcolorscale"), "{desc}");
        assert!(!desc.contains("videoconvert"), "{desc}");

        // The requirement is enforced when the pipeline is built, not silently
        // dropped later: a source that cannot produce a DMA-BUF fails to link.
        // That is what makes this path safe to gate behind a switch — it either
        // runs on the graphics card or it refuses to start, and never quietly
        // becomes the copy it was meant to remove.
        if init().is_ok() {
            let error = gst::parse::launch(&desc).expect_err("a test source has no DMA-BUF");
            assert!(
                error.to_string().contains("memory:DMABuf"),
                "the refusal must name what was missing: {error}"
            );
        }
    }

    #[test]
    fn mirroring_asks_for_a_ceiling_rather_than_the_portal_s_guess() {
        // Measured: a 2560x1440 panel at 110% scale is announced by the portal
        // as 2328x1310 and delivered at 2560x1440. Pinning the announced size
        // resampled every frame down and handed the receiver an off-standard
        // mode to resample back up. A range lets the capture through at its
        // own size and only bites above the person's ceiling.
        let cfg = StreamConfig {
            width: 2326,
            height: 1308,
            fps: 60,
            encoder: H264Encoder::X264,
            ..Default::default()
        };
        let desc = mirror_pipeline_description(&cfg, &VideoSource::Test, (2560, 1440), false);
        assert!(
            desc.contains("capsfilter name=scale-caps")
                && desc.contains("width=[2,2560,2],height=[2,1440,2]"),
            "{desc}"
        );
        // The portal's numbers must not reach the scaler at all.
        assert!(!desc.contains("width=2326"), "{desc}");
        assert!(!desc.contains("height=1308"), "{desc}");
    }

    #[test]
    fn the_capture_source_never_has_caps_forced_on_it() {
        // A privacy regression seen in the field, and the reason this is a
        // test rather than a comment.
        //
        // `pipewiresrc` documents `autoconnect` as "Attempt to find a peer to
        // connect to" and defaults it to true. Forcing a size on the producer
        // made the authorised node unable to negotiate, the element went
        // looking for another video peer, and the laptop's **webcam** was
        // streamed to the projector.
        //
        // The portal path is protected by its `fd`: that PipeWire remote holds
        // only the node the user authorised, cameras included in nothing. The
        // Mutter path has no `fd` and lives in the session's own daemon, where
        // the camera does exist — so nothing may narrow the negotiation there.
        for size in [None, Some((1920, 1080))] {
            let desc = VideoSource::PipeWire {
                fd: None,
                node_id: 42,
                serial: None,
                size,
                min_buffers: None,
            }
            .description();
            assert!(
                !desc.contains("video/x-raw,width"),
                "no caps may be forced on the producer: {desc}"
            );
            assert!(
                desc.contains("path=42"),
                "and the node must be named: {desc}"
            );
        }
    }

    #[test]
    fn mirroring_lets_the_encoder_take_the_capture_s_own_format() {
        // The portal delivers BGRA and NVENC accepts BGRA. Asking for NV12
        // first spends a full-frame CPU pass to reach a format the GPU would
        // have produced while encoding: measured over 300 frames at 2560x1440,
        // 2.76s of CPU against 1.21s.
        let nv = StreamConfig {
            encoder: H264Encoder::NvH264,
            ..Default::default()
        };
        let desc = mirror_pipeline_description(&nv, &VideoSource::Test, (2560, 1440), false);
        assert!(
            desc.contains("format={ BGRA, BGRx, RGBA, RGBx, NV12 }"),
            "{desc}"
        );

        // Every other encoder keeps the one format it was verified with: this
        // machine has no other hardware encoder to measure, and a format list
        // that turns out to be wrong shows up as a receiver getting nothing.
        for encoder in [H264Encoder::X264, H264Encoder::VaH264] {
            let cfg = StreamConfig {
                encoder,
                ..Default::default()
            };
            assert_eq!(
                cfg.encoder.accepted_formats(),
                cfg.encoder.preferred_format()
            );
        }
    }

    #[test]
    fn only_the_video_queue_may_drop_data() {
        let cfg = StreamConfig::default();
        let desc = mirror_pipeline_description(
            &cfg,
            &VideoSource::Diagnostic,
            CHROMECAST_MAX_RESOLUTION,
            false,
        );

        // Video may leak: an old frame is of no interest, the next comes whole.
        assert!(
            desc.contains("leaky=downstream"),
            "the video queue must leak: {desc}"
        );
        // Audio, no: dropping a sample opens a hole in the sound — which is
        // what made the mirroring audio stutter.
        let audio = desc
            .split("audioconvert")
            .nth(1)
            .expect("the mirroring pipeline has an audio branch");
        assert!(
            !audio.contains("leaky"),
            "the audio queue must not leak: {audio}"
        );
    }

    #[test]
    fn mirror_audio_is_opus() {
        // The mirroring app expects Opus, not AAC.
        let cfg = StreamConfig::default();
        let desc =
            mirror_pipeline_description(&cfg, &VideoSource::Test, CHROMECAST_MAX_RESOLUTION, false);
        assert!(desc.contains("opusenc"), "{desc}");
        assert!(!desc.contains("avenc_aac"), "{desc}");
    }

    #[test]
    fn mirror_description_parses_in_gstreamer() {
        if init().is_err() {
            return;
        }
        for enc in probe_encoders() {
            let cfg = StreamConfig {
                encoder: enc,
                ..Default::default()
            };
            let desc = mirror_pipeline_description(
                &cfg,
                &VideoSource::Test,
                CHROMECAST_MAX_RESOLUTION,
                false,
            );
            if let Err(err) = gst::parse::launch(&desc) {
                let msg = err.to_string();
                assert!(
                    msg.contains("no element") || msg.contains("elemento"),
                    "{enc:?}: {msg}\n{desc}"
                );
            }
        }
    }

    #[test]
    fn chromecast_sink_is_findable_by_name() {
        // The HTTP server locates the element by this name in order to hand it
        // the receiver's socket.
        let cfg = StreamConfig::default();
        let desc = ts_http_pipeline_description(
            &cfg,
            &VideoSource::Test,
            VideoTarget::Exact((cfg.width, cfg.height)),
            None,
        );
        assert!(
            desc.contains(&format!("name={TS_HTTP_SINK_NAME}")),
            "{desc}"
        );
    }

    #[test]
    fn chromecast_uses_supported_transport_stream_framing() {
        let desc = ts_http_pipeline_description(
            &StreamConfig::default(),
            &VideoSource::Test,
            VideoTarget::Exact((1920, 1080)),
            None,
        );
        assert!(desc.contains("mpegtsmux name=mux alignment=7"), "{desc}");
        assert!(
            desc.contains("stream-format=byte-stream,alignment=au"),
            "{desc}"
        );
        assert!(!desc.contains("matroskamux"), "{desc}");
    }

    #[test]
    fn va_path_converts_on_the_gpu() {
        let cfg = StreamConfig {
            encoder: H264Encoder::VaH264,
            ..Default::default()
        };
        let desc = wfd_desc(&cfg);
        assert!(desc.contains("vapostproc"), "{desc}");
        assert!(!desc.contains("videoscale"), "{desc}");
        assert!(desc.contains("format=NV12"), "{desc}");
    }

    #[test]
    fn software_path_converts_on_the_cpu() {
        let desc = wfd_desc(&StreamConfig::default());
        assert!(desc.contains("videoconvert"), "{desc}");
        assert!(desc.contains("format=I420"), "{desc}");
    }

    #[test]
    fn fit_within_preserves_aspect_and_never_upscales() {
        // A real case: the laptop's 16:10 screen onto a 1080p panel.
        assert_eq!(
            StreamConfig::fit_within((1920, 1200), (1920, 1080)),
            (1728, 1080)
        );
        // 16:9 already fits: it passes through untouched.
        assert_eq!(
            StreamConfig::fit_within((1920, 1080), (1920, 1080)),
            (1920, 1080)
        );
        // Smaller than the cap: it does not enlarge (that would spend bits on
        // upscaling the receiver would undo).
        assert_eq!(
            StreamConfig::fit_within((1280, 800), (1920, 1080)),
            (1280, 800)
        );
        // 4K 16:9 reduzido ao teto.
        assert_eq!(
            StreamConfig::fit_within((3840, 2160), (1920, 1080)),
            (1920, 1080)
        );
        // A very tall screen: the height becomes the limit.
        let (w, h) = StreamConfig::fit_within((1080, 1920), (1920, 1080));
        assert_eq!(h, 1080);
        assert!(w < 1080);
    }

    #[test]
    fn fit_within_always_returns_even_dimensions() {
        // H.264 requires even dimensions.
        for source in [(1365, 767), (999, 555), (1921, 1201)] {
            let (w, h) = StreamConfig::fit_within(source, (1920, 1080));
            assert_eq!(w % 2, 0, "{source:?} -> {w}x{h}");
            assert_eq!(h % 2, 0, "{source:?} -> {w}x{h}");
        }
    }

    #[test]
    fn scaling_preserves_aspect_with_borders() {
        // Without `add-borders`, a 16:10 screen comes out stretched on a 16:9
        // panel. `vapostproc`'s default is `false`, so it has to be explicit.
        let desc = wfd_desc(&StreamConfig::default());
        assert!(desc.contains("videoscale add-borders=true"), "{desc}");

        let cfg = StreamConfig {
            encoder: H264Encoder::VaH264,
            ..Default::default()
        };
        let desc = wfd_desc(&cfg);
        assert!(desc.contains("vapostproc add-borders=true"), "{desc}");
    }

    #[test]
    fn gop_is_one_second() {
        assert_eq!(cfg_at(1920, 1080, 30).gop(), 30);
        assert_eq!(cfg_at(1920, 1080, 60).gop(), 60);
    }

    #[test]
    fn low_latency_encoders_use_the_automatic_minimum() {
        // A field regression: forcing 20 ms sat **below** the pipeline's real
        // minimum (measured at 41 ms on a 1080p60 Miracast link) and the sink
        // received megabits while showing a black screen. "Automatic" is the
        // correct default.
        for enc in [
            H264Encoder::X264,
            H264Encoder::VaH264,
            H264Encoder::VaapiH264,
            H264Encoder::NvH264,
        ] {
            assert_eq!(
                enc.pipeline_latency_ms(),
                PIPELINE_LATENCY_AUTO,
                "{enc:?} should use automatic latency"
            );
        }
        // Only openh264 has spikes that justify fixed headroom.
        assert_eq!(
            H264Encoder::OpenH264.pipeline_latency_ms(),
            OPENH264_PIPELINE_LATENCY_MS
        );
        const { assert!(OPENH264_PIPELINE_LATENCY_MS > 0) };
    }

    #[test]
    fn wfd_uses_automatic_latency() {
        assert_eq!(WFD_PIPELINE_LATENCY_MS, PIPELINE_LATENCY_AUTO);
    }

    #[test]
    fn wfd_pipeline_pins_the_spec_pids() {
        // Without fixed PIDs mpegtsmux chooses on its own and the sink cannot
        // find the video: it receives data and shows a black screen. Verified
        // in the field.
        let desc = wfd_desc(&StreamConfig::default());
        assert!(desc.contains("mux.sink_4113"), "video on 0x1011: {desc}");
        assert!(desc.contains("mux.sink_4352"), "audio on 0x1100: {desc}");
        assert_eq!(WFD_VIDEO_PID, 0x1011);
        assert_eq!(WFD_AUDIO_PID, 0x1100);
    }

    #[test]
    fn wfd_payloader_uses_the_send_clock() {
        // `perfect-rtptime=true` (the default) derives the RTP timestamp from
        // the buffer count; the sink uses that timestamp to schedule display.
        let desc = wfd_desc(&StreamConfig::default());
        assert!(desc.contains("perfect-rtptime=false"), "{desc}");
        assert!(desc.contains("ssrc=1"), "{desc}");
    }

    #[test]
    fn a_buffer_floor_is_asked_for_only_where_it_was_given() {
        let source = |min_buffers| {
            VideoSource::PipeWire {
                fd: None,
                node_id: 7,
                serial: None,
                size: None,
                min_buffers,
            }
            .description()
        };
        // Silence by default: a producer whose advertised range we have not
        // read is left to the element's own minimum, because asking for more
        // than it offers fails the allocation instead of being clamped.
        assert!(!source(None).contains("min-buffers"), "{}", source(None));
        assert!(
            source(Some(4)).contains("min-buffers=4"),
            "{}",
            source(Some(4))
        );
    }

    #[test]
    fn every_encoder_keeps_the_element_that_fixes_the_cadence() {
        // Removed twice, for 35 ms, and put back twice: without it the Cast
        // sender cannot map an encoded frame through its segment, and the
        // receiver shows black. The cost is deliberate.
        for encoder in [
            H264Encoder::NvH264,
            H264Encoder::X264,
            H264Encoder::OpenH264,
            H264Encoder::VaH264,
        ] {
            let stage = StreamConfig {
                encoder,
                fps: 60,
                ..Default::default()
            }
            .convert_scale(VideoTarget::Exact((1280, 720)));
            assert!(stage.contains("videorate"), "{encoder:?}: {stage}");
            assert!(stage.contains("framerate=60/1"), "{encoder:?}: {stage}");
        }
    }

    #[test]
    fn the_software_encoder_is_not_cut_into_a_slice_per_core() {
        // `threads` is also the slice count, and `0` means one per core. A
        // sixteen-core machine was sending eleven slices a frame and paying
        // 14% more bits for the privilege, with no throughput to show for it.
        let cfg = StreamConfig {
            encoder: H264Encoder::X264,
            ..Default::default()
        };
        let desc = cfg.encoder.encoder_description(&cfg);
        let threads: u32 = desc
            .split("threads=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("no thread count in {desc}"));
        assert!(
            (1..=4).contains(&threads),
            "{threads} threads means {threads} slices a frame"
        );

        // The tempting fix for the slice count, and the reason it is not here:
        // it returns x264 to frame threading, measured declaring 700 ms of
        // latency. An assertion rather than a comment because the next person
        // to read "one slice per frame" will reach for exactly this.
        assert!(
            !desc.contains("sliced-threads"),
            "sliced-threads=false costs 700 ms of latency: {desc}"
        );
    }

    #[test]
    fn hardware_encoders_are_told_to_emit_one_slice() {
        // These take an explicit slice count, so here the interoperability
        // requirement is something we can actually ask for.
        for enc in [H264Encoder::VaH264, H264Encoder::VaapiH264] {
            let cfg = StreamConfig {
                encoder: enc,
                ..Default::default()
            };
            assert!(
                cfg.encoder
                    .encoder_description(&cfg)
                    .contains("num-slices=1"),
                "{enc:?}"
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
    #[test]
    fn degenerate_dimensions_never_make_zero_sized_h264_caps() {
        for source in [(0, 0), (1, 1), (1, 1080), (1920, 1)] {
            for cap in [(0, 0), (1, 1), (1919, 1079)] {
                let (w, h) = StreamConfig::fit_within(source, cap);
                assert!(w >= 2 && h >= 2 && w % 2 == 0 && h % 2 == 0);
            }
        }
    }

    #[test]
    fn cast_high_selects_v4l2_high_without_changing_default_wfd_control() {
        let cfg = StreamConfig {
            profile: H264Profile::High,
            ..Default::default()
        };
        assert!(H264Encoder::V4l2H264
            .encoder_description(&cfg)
            .contains("h264_profile=4,"));
        assert!(H264Encoder::V4l2H264
            .encoder_description(&StreamConfig::default())
            .contains("h264_profile=0,"));
    }
}
