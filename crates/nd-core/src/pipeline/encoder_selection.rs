//! Which H.264 encoder a session runs on this machine.
//!
//! The registry says what is installed, the kernel driver rules out stacks
//! that cannot work (NVENC off NVIDIA, VA-API under NVIDIA's own driver), and
//! the preferences may forbid the graphics card. What remains is tried in
//! order by [`working_encoder`], which keeps the first candidate that really
//! produces frames: a hardware encoder can exist, accept its configuration and
//! encode nothing.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gst::prelude::*;
use gstreamer as gst;

use super::{
    ENCODER_NAME, H264Encoder, PipelineEvents, PipelineGuard, StreamConfig, VideoTarget,
    build_pipeline, count_buffers, init,
};
use crate::{NdError, Result};

/// The kernel driver behind the graphics card, which decides which hardware
/// encoders can run at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuDriver {
    I915,
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

    /// The primary GPU's driver: the target of the
    /// `/sys/class/drm/card*/device/driver` symlink, whose file name is the
    /// kernel module. The first card with a known driver wins. Sysfs is
    /// readable inside Flatpak too.
    pub fn detect() -> Self {
        Self::detect_in(std::path::Path::new("/sys/class/drm"))
    }

    fn detect_in(drm_root: &std::path::Path) -> Self {
        let Ok(entries) = std::fs::read_dir(drm_root) else {
            tracing::debug!("DRM sysfs unavailable; assuming an unknown GPU");
            return GpuDriver::Unknown;
        };
        let mut cards: Vec<_> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            // `card0`, `card1`; not connectors (`card0-HDMI-A-1`) or `renderD128`.
            .filter(|name| {
                name.starts_with("card") && name["card".len()..].chars().all(|c| c.is_ascii_digit())
            })
            .collect();
        // `card0` is usually the primary GPU.
        cards.sort();
        for card in cards {
            let link = drm_root.join(&card).join("device").join("driver");
            let Ok(target) = std::fs::read_link(&link) else {
                continue;
            };
            let Some(module) = target.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let driver = GpuDriver::from_kernel_module(module);
            tracing::info!(%card, %module, ?driver, "KMS driver detected");
            if driver != GpuDriver::Unknown {
                return driver;
            }
        }
        GpuDriver::Unknown
    }

    /// Whether a hardware encoder is worth trying on this driver.
    ///
    /// Only `nouveau` is ruled out: it exposes no usable NVENC. Elsewhere the
    /// hardware is tried and [`working_encoder`] moves on when it produces no
    /// frame; `BIGNETSCREEN_ENCODER=x264enc` forces software on a machine whose
    /// hardware produces a wrong picture.
    pub fn hardware_encode_is_reliable(self) -> bool {
        self != GpuDriver::Nouveau
    }
}

/// The encoders installed in GStreamer's registry, in [`H264Encoder::ALL`]
/// order; empty when GStreamer cannot start.
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
pub(crate) fn select_encoder(available: &[H264Encoder], driver: GpuDriver) -> Option<H264Encoder> {
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
/// Passed in rather than read from the preferences here, so the selection rules
/// give the same answer whatever the settings file of the machine running the
/// tests says.
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

/// The encoder `BIGNETSCREEN_ENCODER` names, when it is installed.
///
/// It overrides the driver rules and the acceleration preference alike: it
/// exists to measure one encoder on one machine, and a preference that
/// silently overruled it would make the measurement about something else.
fn forced_encoder(installed: &[H264Encoder]) -> Option<H264Encoder> {
    let name = std::env::var("BIGNETSCREEN_ENCODER").ok()?;
    let forced = installed.iter().copied().find(|e| e.element() == name);
    match forced {
        Some(forced) => tracing::warn!(?forced, "encoder forced by BIGNETSCREEN_ENCODER"),
        None => {
            tracing::warn!(%name, ?installed, "BIGNETSCREEN_ENCODER not available; ignoring it")
        }
    }
    forced
}

/// The order in which encoders are tried: hardware first, software in reserve.
///
/// Only what cannot work is filtered out here (NVENC without an NVIDIA card,
/// VA-API under NVIDIA's own driver). Whether a hardware encoder really works
/// is decided by running it, in [`working_encoder`].
pub fn encoder_candidates(driver: GpuDriver, acceleration: Acceleration) -> Vec<H264Encoder> {
    let available = probe_encoders();
    if let Some(forced) = forced_encoder(&available) {
        return vec![forced];
    }
    let mut candidates: Vec<H264Encoder> = available
        .into_iter()
        .filter(|enc| acceleration.permits(*enc) && encoder_fits_driver(*enc, driver))
        .collect();
    candidates.sort_by_key(|enc| std::cmp::Reverse(enc.priority()));
    tracing::info!(?candidates, ?driver, ?acceleration, "encoder attempt order");
    candidates
}

/// A pipeline that counts the frames leaving its encoder.
///
/// The count is what tells a working hardware encoder from one that exists,
/// accepts its configuration and encodes nothing.
pub struct MonitoredPipeline {
    pub pipeline: gst::Pipeline,
    pub events: PipelineEvents,
    pub encoder: H264Encoder,
    frames: Arc<AtomicU64>,
}

impl MonitoredPipeline {
    /// How many frames have left the encoder so far.
    pub fn frames_encoded(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
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
    let frames = count_buffers(&pipeline, ENCODER_NAME).unwrap_or_else(|| {
        tracing::warn!("element `{ENCODER_NAME}` not found; no encoder output verification");
        Arc::new(AtomicU64::new(0))
    });

    Ok(MonitoredPipeline {
        pipeline,
        events,
        encoder,
        frames,
    })
}

/// Select an encoder that produces frames at the requested mode.
///
/// `session` is the configuration the caller is about to stream with; its
/// `encoder` field is replaced with each candidate in turn. The probe builds
/// the same encoder description the session will, so a property the hardware
/// rejects fails here rather than on the receiver.
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

/// The highest-priority installed encoder the driver and the preference allow,
/// without running it.
pub fn best_encoder(driver: GpuDriver, acceleration: Acceleration) -> Result<H264Encoder> {
    let installed = probe_encoders();
    if let Some(forced) = forced_encoder(&installed) {
        return Ok(forced);
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
    fn missing_sysfs_means_unknown_driver() {
        assert_eq!(
            GpuDriver::detect_in(std::path::Path::new("/definitely/does/not/exist")),
            GpuDriver::Unknown
        );
    }

    #[test]
    fn first_known_driver_wins() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("nd-core-drm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (entry, module) in [
            ("card0-HDMI-A-1", "i915"),
            ("renderD128", "i915"),
            ("card0", "simpledrm"),
            ("card1", "amdgpu"),
        ] {
            let device = root.join(entry).join("device");
            std::fs::create_dir_all(&device).unwrap();
            symlink(
                format!("../../bus/pci/drivers/{module}"),
                device.join("driver"),
            )
            .unwrap();
        }
        let driver = GpuDriver::detect_in(&root);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(driver, GpuDriver::Amdgpu);
    }

    #[test]
    fn intel_drivers_prefer_hardware() {
        // A driver that hangs or stalls is caught at runtime by the absence of
        // frames, so no Intel driver is ruled out in advance.
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
    fn driver_encoder_allow_list() {
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

        // Intel `xe` is offered VA-API like any other driver. On a machine
        // whose encoder stalls, what saves the session is `working_encoder`
        // moving on after the probe's timeout.
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
        // list to take over.
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
    fn silent_encoder_reports_zero() {
        // The case that triggers the fall back to software: the encoder
        // exists, accepts the configuration and encodes nothing.
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
    fn proprietary_nvidia_skips_va() {
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
