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
use std::sync::OnceLock;

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
pub type PipelineEvents = futures::channel::mpsc::UnboundedReceiver<PipelineEvent>;

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

/// Builds and prepares a pipeline from a description.
///
/// Unlike a bare `parse::launch`, here:
/// 1. the **pipeline latency is applied** (`set_latency`) — without it the
///    tuning constants in this module would have no effect at all;
/// 2. the bus is watched and errors are delivered to the caller over a
///    channel, instead of merely being logged and dropped;
/// 3. the pipeline comes back in the **`Ready`** state, not `Null`.
///
/// Item 3 is not cosmetic: elements such as `multisocketsink` refuse to `add`
/// a client while the pipeline is in `Null` (*"must be set to READY, PAUSED or
/// PLAYING state before clients can be added"*), and the refusal is only a
/// `WARNING` on the bus — the cast fails silently, delivering zero bytes.
/// Returning in `Ready` closes that trap for every caller.
/// Measures how long a frame spends **inside our own pipeline**.
///
/// It exists to answer a question a photo of the two screens cannot separate:
/// of the milliseconds of delay visible on the projector, how many are ours
/// and how many are the device's? The probe sits on every element's pad and
/// compares the frame's running time against the pipeline clock — that is, the
/// age of the frame at the moment it goes out to the network.
///
/// What it does **not** measure: the network, and the receiver's decoding and
/// image processing. The gap between this number and the photo's is exactly
/// that.
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
            let Some(idade) = now.checked_sub(pts) else {
                return gst::PadProbeReturn::Ok;
            };

            let ms = idade.mseconds();
            if let Ok(mut guard) = state.lock() {
                let (ref mut last, ref mut n, ref mut soma, ref mut pior) = *guard;
                *n += 1;
                *soma += ms;
                *pior = (*pior).max(ms);
                if last.elapsed() >= std::time::Duration::from_secs(5) {
                    tracing::info!(
                        sink = %name,
                        media_ms = *soma / (*n).max(1),
                        pior_ms = *pior,
                        quadros = *n,
                        "latency inside the pipeline (no network, no receiver)"
                    );
                    *last = std::time::Instant::now();
                    *n = 0;
                    *soma = 0;
                    *pior = 0;
                }
            }
            gst::PadProbeReturn::Ok
        });
        found += 1;
    }
    tracing::debug!(sinks = found, "latency probes installed");
}

pub fn build_pipeline(
    description: &str,
    latency_ms: u64,
) -> Result<(gst::Pipeline, PipelineEvents)> {
    init()?;
    tracing::debug!(%description, latency_ms, "construindo pipeline");

    let element = gst::parse::launch(description).map_err(|e| NdError::Gst(e.to_string()))?;
    let pipeline = element
        .downcast::<gst::Pipeline>()
        .map_err(|_| NdError::Gst("the description did not produce a Pipeline".into()))?;

    // `0` = automatic: let GStreamer use the minimum latency the pipeline
    // itself reports. That is the right default — forcing a value **below**
    // that minimum makes sinks drop late buffers (the sink receives megabits
    // and shows a black screen), and forcing one far **above** it only adds
    // lag. Setting it explicitly is for giving headroom to spiky encoders.
    if latency_ms > 0 {
        pipeline.set_latency(gst::ClockTime::from_mseconds(latency_ms));
    }

    // Optional diagnostic: how long a frame spends in here. Kept behind an
    // environment variable because it installs a probe on every buffer.
    if std::env::var("BIGNETSCREEN_LATENCY").is_ok() {
        instrument_latency(&pipeline);
    }

    let (tx, rx) = futures::channel::mpsc::unbounded();
    if let Some(bus) = pipeline.bus() {
        bus.set_sync_handler(move |_, msg| {
            match msg.view() {
                gst::MessageView::Error(err) => {
                    let message = err.error().to_string();
                    let details = err.debug().map(|d| d.to_string()).unwrap_or_default();
                    tracing::error!(%message, %details, "pipeline error");
                    let _ = tx.unbounded_send(PipelineEvent::Error {
                        message,
                        debug: details,
                    });
                }
                gst::MessageView::Warning(w) => {
                    let message = w.error().to_string();
                    tracing::warn!(%message, "aviso do pipeline");
                    let _ = tx.unbounded_send(PipelineEvent::Warning { message });
                }
                gst::MessageView::Eos(_) => {
                    let _ = tx.unbounded_send(PipelineEvent::Eos);
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
    pipeline
        .set_state(gst::State::Ready)
        .map_err(|e| NdError::Gst(format!("could not prepare the pipeline: {e}")))?;

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
/// The RTP jitter buffer's latency (`rtpbin`).
pub const RTP_LATENCY_MS: u64 = 20;
/// The video queue before the encoder: few buffers, ~1 frame.
pub const VIDEO_QUEUE_BUFFERS: u32 = 3;
/// Idem, em milissegundos.
pub const VIDEO_QUEUE_MS: u64 = 30;
/// The audio queue on the muxed path (the reference C used 100000 — a bug).
///
/// Small on purpose, and here leaking is the lesser evil: `mpegtsmux`
/// interleaves the tracks by timestamp, so **video waits for audio**. Every bit
/// of slack given to this queue becomes picture delay — at 200 ms Miracast left
/// the measured ~40 ms behind and the lag became visible when moving the
/// mouse.
pub const AUDIO_QUEUE_BUFFERS: u32 = 4;
/// Idem, em milissegundos.
pub const AUDIO_QUEUE_MS: u64 = 40;
/// The Cast mirroring audio queue, in frames.
///
/// There is no muxer here: each track leaves through its own `appsink`, and the
/// receiver aligns the two by their timestamps. With nobody waiting on the
/// audio, the slack costs no picture latency — so this queue can be generous
/// and, above all, **does not leak**: dropping samples does not bring the sound
/// forward, it opens a hole in it.
pub const MIRROR_AUDIO_QUEUE_BUFFERS: u32 = 64;
/// Idem, em milissegundos.
pub const MIRROR_AUDIO_QUEUE_MS: u64 = 200;
/// Porta RTP local usada como origem do stream WFD.
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

/// Candidate H.264 encoders, in order of preference.
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

    /// Encode por hardware?
    pub fn is_hardware(self) -> bool {
        !matches!(self, H264Encoder::X264 | H264Encoder::OpenH264)
    }

    /// Usa a stack VA-API (permite converter/escalar na GPU com `vapostproc`).
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
        let gop = cfg.gop();
        match self {
            H264Encoder::X264 => {
                let intra = if cfg.intra_refresh { "true" } else { "false" };
                format!(
                    // No `sliced-threads`: it splits each frame into several
                    // slices, and the hardware decoders in TVs and projectors
                    // expect **one slice per frame** (the reference C forces
                    // `num-slices=1` on every encoder). It costs a little
                    // parallelism, but it is an interoperability requirement.
                    "x264enc name=enc tune=zerolatency speed-preset=ultrafast \
                     rc-lookahead=0 sync-lookahead=0 bframes=0 b-adapt=false \
                     threads=0 aud=true cabac=false ref=1 \
                     pass=cbr vbv-buf-capacity=50 intra-refresh={intra} \
                     key-int-max={gop} bitrate={kbps}"
                )
            }
            // `bitrate` in kbps. No B-frames and CBR: the VA-API defaults
            // (VBR + B-frames) reorder frames and leave the "fast" path with
            // more latency than x264.
            H264Encoder::VaH264 => format!(
                "vah264enc name=enc rate-control=cbr bitrate={kbps} key-int-max={gop} \
                 b-frames=0 ref-frames=1 num-slices=1 target-usage=6 \
                 aud=true cabac=false"
            ),
            H264Encoder::VaapiH264 => format!(
                "vaapih264enc name=enc rate-control=cbr bitrate={kbps} keyframe-period={gop} \
                 max-bframes=0 refs=1 num-slices=1 quality-level=7 cabac=false aud=true"
            ),
            H264Encoder::NvH264 => format!(
                "nvh264enc name=enc preset=low-latency-hq rc-mode=cbr bitrate={kbps} \
                 gop-size={gop} bframes=0 zerolatency=true aud=true"
            ),
            // V4L2 stateful: as propriedades ficam em `extra-controls`.
            H264Encoder::V4l2H264 => format!(
                "v4l2h264enc name=enc extra-controls=\"controls,h264_profile=0,\
                 h264_i_frame_period={gop},video_bitrate={bps},\
                 repeat_sequence_header=1\"",
                bps = kbps as u64 * 1000
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
    let hw_ok = driver.hardware_encode_is_reliable();
    available
        .iter()
        .copied()
        .filter(|enc| {
            if !enc.is_hardware() {
                return true;
            }
            if !hw_ok {
                return false;
            }
            match (*enc, driver) {
                (H264Encoder::NvH264, GpuDriver::Nvidia) => true,
                (H264Encoder::NvH264, _) => false,
                (e, GpuDriver::Nvidia) if e.is_va() => false,
                _ => true,
            }
        })
        .max_by_key(|enc| enc.priority())
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
pub fn encoder_candidates(driver: GpuDriver) -> Vec<H264Encoder> {
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
        .filter(|enc| {
            if !enc.is_hardware() {
                return true;
            }
            if !driver.hardware_encode_is_reliable() {
                return false;
            }
            match (*enc, driver) {
                (H264Encoder::NvH264, GpuDriver::Nvidia) => true,
                (H264Encoder::NvH264, _) => false,
                (e, GpuDriver::Nvidia) if e.is_va() => false,
                _ => true,
            }
        })
        .collect();
    candidates.sort_by_key(|enc| std::cmp::Reverse(enc.priority()));
    tracing::info!(?candidates, ?driver, "encoder attempt order");
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
}

impl PipelineGuard {
    pub fn new(pipeline: gst::Pipeline) -> Self {
        Self { pipeline }
    }

    pub fn pipeline(&self) -> &gst::Pipeline {
        &self.pipeline
    }
}

impl Drop for PipelineGuard {
    fn drop(&mut self) {
        if let Err(err) = self.pipeline.set_state(gst::State::Null) {
            tracing::debug!(%err, "falha ao levar o pipeline a NULL");
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

/// Convenience: scans the registry and picks the best encoder for the driver.
pub fn best_encoder(driver: GpuDriver) -> Result<H264Encoder> {
    let available = probe_encoders();

    // `BIGNETSCREEN_ENCODER=vah264enc` forces a specific encoder. It is there
    // to measure in the field whether a quirk (such as Intel `xe`'s) still
    // holds on current hardware, rather than carrying it forever out of
    // inertia.
    if let Ok(name) = std::env::var("BIGNETSCREEN_ENCODER") {
        if let Some(forced) = available.iter().copied().find(|e| e.element() == name) {
            tracing::warn!(?forced, "encoder forced by BIGNETSCREEN_ENCODER");
            return Ok(forced);
        }
        tracing::warn!(%name, ?available, "BIGNETSCREEN_ENCODER not available; ignoring it");
    }
    let chosen = select_encoder(&available, driver);
    tracing::info!(?available, ?driver, ?chosen, "H.264 encoder selection");
    chosen.ok_or_else(|| {
        NdError::Unsupported(
            "nenhum encoder H.264 encontrado — instale gst-plugins-ugly (x264) \
             ou gst-plugins-bad (openh264/va)"
                .into(),
        )
    })
}

// ------------------------------------------------------------------------
// Fontes
// ------------------------------------------------------------------------

/// Where the video frames come from.
#[derive(Clone, Copy, Debug)]
pub enum VideoSource {
    /// The PipeWire stream handed over by the portal/Mutter.
    ///
    /// `node_id` may **never** be missing: a `pipewiresrc` without `path`
    /// picks an arbitrary node from the daemon instead of the one the capture
    /// session authorised. `fd` is the portal's remote descriptor (mandatory
    /// under Flatpak); with Mutter directly the node lives in the session's own
    /// daemon and `fd` is `None`.
    PipeWire { fd: Option<RawFd>, node_id: u32 },
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
    fn description(&self) -> String {
        match self {
            // `keepalive-time`/`resend-last` make the source re-emit the last
            // frame when the screen is still. Without them the encoder starves
            // and the receiver drops the session for lack of data.
            VideoSource::PipeWire { fd, node_id } => {
                let fd_prop = match fd {
                    Some(fd) => format!("fd={fd} "),
                    None => String::new(),
                };
                format!(
                    "pipewiresrc {fd_prop}path={node_id} do-timestamp=true \
                     keepalive-time=1000 resend-last=true"
                )
            }
            VideoSource::Test => "videotestsrc is-live=true".to_string(),
            VideoSource::Diagnostic => "videotestsrc is-live=true pattern=ball ! \
                 timeoverlay halignment=center valignment=center font-desc=\"Sans 48\" \
                 time-mode=running-time"
                .to_string(),
        }
    }
}

/// Where the streamed programme's audio comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioSource {
    /// Synthesised silence.
    ///
    /// **Not optional on WFD:** the sink declares an A/V session and discards
    /// the programme if it arrives with video alone.
    Silence,
    /// System audio: the default output's *monitor*, that is, whatever is
    /// playing on the computer.
    System,
}

impl AudioSource {
    /// Picks the best source available on this machine.
    ///
    /// Falls back to silence if there is no way to capture system audio — the
    /// audio branch is **not optional** on WFD (the sink discards a video-only
    /// programme), so sending silence beats sending nothing.
    pub fn detect() -> Self {
        if init().is_err() {
            return AudioSource::Silence;
        }
        if gst::ElementFactory::find("pulsesrc").is_some() {
            AudioSource::System
        } else {
            tracing::info!("no `pulsesrc`; the cast will go without system audio");
            AudioSource::Silence
        }
    }
}

impl AudioSource {
    fn description(&self) -> String {
        match self {
            // `samplesperbuffer=480` = 10 ms at 48 kHz (the default, 1024, is
            // 21 ms). The audio branch is what sets the pipeline's latency
            // floor: measured in the field, 41 ms with the default.
            AudioSource::Silence => {
                "audiotestsrc is-live=true wave=silence samplesperbuffer=480".to_string()
            }
            // `@DEFAULT_MONITOR@` is resolved by the server (PulseAudio or
            // PipeWire's compatibility mode) to the default output's monitor —
            // it follows whatever device the user switches to mid-session,
            // with nothing for us to query.
            //
            // `provide-clock=false`: the screen capture sets the pace; a second
            // clock in the pipeline fights with it.
            AudioSource::System => {
                "pulsesrc device=@DEFAULT_MONITOR@ provide-clock=false do-timestamp=true"
                    .to_string()
            }
        }
    }
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
        let (w, h) = source;
        let (max_w, max_h) = max;
        if w == 0 || h == 0 {
            return max;
        }
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

    /// The maximum distance between keyframes.
    ///
    /// One second (not two): over an unstable Wi-Fi Direct link the GOP sets
    /// the recovery time after packet loss, and more frequent IDRs keep the
    /// bitrate curve flatter (less VBV jitter).
    pub fn gop(&self) -> u32 {
        if let Ok(value) = std::env::var("BIGNETSCREEN_GOP") {
            if let Ok(gop) = value.parse::<u32>() {
                return gop.max(1);
            }
        }
        self.fps.max(1)
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
        self.encoder.pipeline_latency_ms()
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
    fn convert_scale(&self) -> String {
        let fmt = self.encoder.preferred_format();
        let (w, h, fps) = (self.width, self.height, self.fps);
        if self.encoder.is_va() {
            // `add-borders=true` is **not** vapostproc's default (unlike
            // videoscale's): without it, a 16:10 screen sent to a 16:9 panel
            // comes out stretched vertically.
            format!(
                "videorate ! vapostproc add-borders=true ! \
                 video/x-raw,format={fmt},width={w},height={h},framerate={fps}/1"
            )
        } else {
            format!(
                "videorate ! videoscale add-borders=true ! videoconvert n-threads=0 ! \
                 video/x-raw,format={fmt},width={w},height={h},framerate={fps}/1"
            )
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
    /// Porta RTP local de origem.
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
        rtp_latency = RTP_LATENCY_MS,
        src = source.description(),
        convert = cfg.convert_scale(),
        vqueue = cfg.video_queue(),
        enc = cfg.encoder.encoder_description(cfg),
        video_pid = WFD_VIDEO_PID,
        sync = if rtp_sink_syncs_to_clock() {
            "true"
        } else {
            "false"
        },
        ip = transport.sink_ip,
        port = transport.rtp_port,
        local = transport.local_rtp_port,
        audio = cfg.audio_branch(&format!("mux.sink_{}", WFD_AUDIO_PID)),
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
pub fn mirror_pipeline_description(cfg: &StreamConfig, source: &VideoSource) -> String {
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
        "{src} ! {convert} ! {vqueue} ! \
         {enc} ! video/x-h264,profile=constrained-baseline,stream-format=byte-stream,\
alignment=au ! \
         h264parse config-interval=-1 ! \
         appsink name={video_sink} emit-signals=false sync=false \
         max-buffers=1 drop=false{audio}",
        src = source.description(),
        convert = cfg.convert_scale(),
        vqueue = cfg.video_queue(),
        enc = cfg.encoder.encoder_description(cfg),
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

/// The `multisocketsink`'s name in the Chromecast pipeline description.
///
/// The HTTP server looks the element up by this name in order to hand it the
/// receiver's socket with the headers already written.
pub const CHROMECAST_SINK_NAME: &str = "cc-sink";

/// Builds the Chromecast pipeline description (H.264 + AAC in Matroska).
///
/// Decisions carried over from the reference C project
/// (`src/cc/cc-media-factory.c`) that matter for time-to-first-picture:
/// - `matroskamux` with 50–100 ms clusters (the 500 ms default is far too late
///   for live content);
/// - a short post-muxer queue (50 ms), so the segment leaves as soon as it
///   closes;
/// - **`multisocketsink sync=false`**: the pace already comes from the live
///   source; synchronising on the clock here adds an entire pipeline latency
///   before the byte reaches the socket;
/// - `blocksize=8192`: 8 KiB is enough for TCP packetisation — larger blocks
///   only accumulate buffer before the segment reaches the receiver;
/// - `sync-method=latest-keyframe` + `recover-policy=keyframe`: a client that
///   arrives mid-stream (or falls behind) joins from the most recent keyframe,
///   rather than receiving garbage or being disconnected.
///
/// The audio track is not decorative: the Default Media Receiver rejects
/// containers without audio.
pub fn chromecast_pipeline_description(cfg: &StreamConfig, source: &VideoSource) -> String {
    format!(
        "{src} ! {convert} ! {vqueue} ! \
         {enc} ! h264parse config-interval=-1 ! \
         matroskamux name=mux streamable=true min-cluster-duration=20000000 \
         max-cluster-duration=40000000 ! \
         queue max-size-buffers=0 max-size-bytes=0 max-size-time=50000000 silent=true ! \
         multisocketsink name={sink} sync=false async=false blocksize=8192 \
         burst-format=buffers sync-method={sync_method} recover-policy=keyframe \
         {audio}",
        src = source.description(),
        convert = cfg.convert_scale(),
        vqueue = cfg.video_queue(),
        enc = cfg.encoder.encoder_description(cfg),
        sink = CHROMECAST_SINK_NAME,
        sync_method = chromecast_sync_method(),
        audio = cfg.audio_branch("mux."),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_pipeline_guard_leaves_the_pipeline_in_null() {
        // Descartar um pipeline em PLAYING derrubava o app ao apertar Parar.
        init().expect("gstreamer");
        let pipeline = gst::parse::launch("fakesrc num-buffers=1 ! fakesink")
            .expect("pipeline de teste")
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
    fn candidates_keep_a_software_backup() {
        // If the GPU produces no frames, the software path has to be in the
        // list to take over — without it the fallback would have nowhere to
        // go.
        let candidates = encoder_candidates(GpuDriver::Xe);
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
        };
        let desc = chromecast_pipeline_description(&cfg, &src);
        assert!(desc.contains("pipewiresrc path=42"), "{desc}");
        assert!(!desc.contains("fd="), "{desc}");
    }

    #[test]
    fn pipewire_source_carries_fd_and_node() {
        // A regression: without `fd=`/`path=` pipewiresrc does not capture the
        // stream the portal opened — it captured an arbitrary node from the
        // daemon.
        let desc = wfd_desc(&StreamConfig::default());
        assert!(desc.contains("pipewiresrc fd=7 path=42"), "{desc}");
    }

    #[test]
    fn videorate_precedes_scaling() {
        let desc = wfd_desc(&StreamConfig::default());
        let rate = desc.find("videorate").expect("videorate is mandatory");
        let scale = desc.find("videoscale").expect("videoscale");
        assert!(rate < scale, "videorate must precede videoscale: {desc}");
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
        let desc = chromecast_pipeline_description(&cfg, &VideoSource::Test);
        assert!(desc.contains("avenc_aac"), "{desc}");
        assert!(desc.contains("matroskamux"), "{desc}");
    }

    #[test]
    fn chromecast_sink_does_not_sync_on_the_clock() {
        // `sync=true` on multisocketsink adds an entire pipeline latency
        // before the byte reaches the socket. The source is already live.
        let cfg = StreamConfig::default();
        let desc = chromecast_pipeline_description(&cfg, &VideoSource::Test);
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
        let desc = mirror_pipeline_description(&cfg, &VideoSource::Test);
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
        let desc = mirror_pipeline_description(&cfg, &VideoSource::Test);
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
    fn only_the_video_queue_may_drop_data() {
        let cfg = StreamConfig::default();
        let desc = mirror_pipeline_description(&cfg, &VideoSource::Diagnostic);

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
        let desc = mirror_pipeline_description(&cfg, &VideoSource::Test);
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
            let desc = mirror_pipeline_description(&cfg, &VideoSource::Test);
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
        let desc = chromecast_pipeline_description(&cfg, &VideoSource::Test);
        assert!(
            desc.contains(&format!("name={CHROMECAST_SINK_NAME}")),
            "{desc}"
        );
    }

    #[test]
    fn chromecast_clusters_are_short_enough_for_live() {
        // matroskamux's default (500 ms) delays the first picture far too much.
        let cfg = StreamConfig::default();
        let desc = chromecast_pipeline_description(&cfg, &VideoSource::Test);
        // 20–40 ms: a cluster has to **close** before going out to the
        // network, and that time enters the latency directly. matroskamux's
        // default is 500 ms — far too late for live content.
        assert!(desc.contains("min-cluster-duration=20000000"), "{desc}");
        assert!(desc.contains("max-cluster-duration=40000000"), "{desc}");
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
    fn encoders_emit_one_slice_per_frame() {
        // The hardware decoders in TVs expect one slice per frame.
        let cfg = StreamConfig::default();
        assert!(
            !cfg.encoder
                .encoder_description(&cfg)
                .contains("sliced-threads"),
            "x264 must not slice the frame"
        );
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
                chromecast_pipeline_description(&cfg, &VideoSource::Test),
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
