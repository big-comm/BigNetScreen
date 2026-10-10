//! Which H.264 encoder a session runs on this machine.
//!
//! The registry says what is installed, the kernel driver rules out stacks
//! that cannot work (NVENC off NVIDIA, VA-API under NVIDIA's own driver), and
//! the preferences may forbid the graphics card. What remains is tried in
//! order by [`working_encoder`], which keeps the first candidate that really
//! produces frames: a hardware encoder can exist, accept its configuration and
//! encode nothing.

use gst::prelude::*;
use gstreamer as gst;

use super::{
    ENCODER_NAME, H264Encoder, PipelineEvents, PipelineGuard, StreamConfig, VideoTarget,
    build_pipeline, init,
};
use crate::{NdError, Result};

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
             or gst-plugins-bad (openh264/va)"
                .into(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::PIPELINE_LATENCY_AUTO;

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
}
