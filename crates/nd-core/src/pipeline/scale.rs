//! Sizing the picture for the receiver, and reading back the size the
//! pipeline really settled on.
//!
//! Every H.264 path runs the same `videorate ! scale ! capsfilter` stage,
//! built by [`StreamConfig::convert_scale`]. The portal announces sizes in the
//! compositor's coordinate space, so where nothing was promised to the
//! receiver the capsfilter takes a range and the true size is only known once
//! caps are negotiated ([`negotiated_video_size`]).

use gstreamer as gst;

use super::{CAPTURE_SOURCE_NAME, StreamConfig};
use crate::{NdError, Result};

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

impl StreamConfig {
    /// The `rate → scale → convert` fragment, tuned per encoder.
    ///
    /// Two performance decisions:
    /// 1. `videorate` comes **before** scaling: frames that will be dropped pay
    ///    for neither conversion nor scaling (which matters when the source is
    ///    60 Hz and the sink negotiated 30);
    /// 2. on the VA-API path `vapostproc` is used, converting and scaling **on
    ///    the GPU** — at 4K that avoids hundreds of MB/s of GPU→RAM→GPU
    ///    copying.
    pub(super) fn convert_scale(&self, target: VideoTarget) -> String {
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
    pub(super) fn gpu_convert(&self, ceiling: (u32, u32)) -> String {
        let fps = self.fps;
        let dims = VideoTarget::UpTo(ceiling).caps_dimensions();
        format!(
            "video/x-raw(memory:DMABuf) ! \
             videorate max-duplication-time={MAX_DUPLICATION_NS} ! glupload ! \
             capsfilter name={SCALE_CAPS} \
             caps=\"video/x-raw(memory:GLMemory),{dims},framerate={fps}/1\""
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::{
        H264Encoder, PipelineGuard, SQUARE_PIXELS, VideoSource, init, mirror_pipeline_description,
    };

    /// The stage a path that agreed on one mode (Miracast) runs.
    fn exact_stage(cfg: &StreamConfig) -> String {
        cfg.convert_scale(VideoTarget::Exact((cfg.width, cfg.height)))
    }

    /// `pipewiresrc` offers every pixel aspect ratio and fixation took the
    /// minimum, which drove the ranged scaler to 2x2 (`videoscale`) or an
    /// overflow (`vapostproc`). With the ratio pinned the picture keeps its
    /// size. `videotestsrc` fixates a range to 1/1 by itself, so the second
    /// run feeds the minimum as a fixed value — what the capture measured as
    /// negotiated before the pin — to show the scaler still misbehaves on it.
    #[test]
    fn a_capture_offering_any_pixel_aspect_keeps_its_size() {
        use gst::prelude::*;
        init().unwrap();
        let cfg = StreamConfig {
            width: 1920,
            height: 1080,
            ..Default::default()
        };
        let negotiated = |aspect: &str, pin: &str| -> Option<(i32, i32)> {
            let description = format!(
                "videotestsrc num-buffers=3 \
                 ! video/x-raw,width=640,height=360,framerate=30/1,\
                 pixel-aspect-ratio={aspect} {pin} ! {} ! fakesink sync=false",
                cfg.convert_scale(VideoTarget::UpTo((1920, 1080)))
            );
            let pipeline = gst::parse::launch(&description)
                .unwrap()
                .downcast::<gst::Pipeline>()
                .unwrap();
            let guard = PipelineGuard::new(pipeline);
            guard.pipeline().set_state(gst::State::Playing).ok()?;
            let message = guard.pipeline().bus().unwrap().timed_pop_filtered(
                gst::ClockTime::from_seconds(5),
                &[gst::MessageType::Error, gst::MessageType::Eos],
            )?;
            if message.type_() != gst::MessageType::Eos {
                return None;
            }
            let caps = guard
                .pipeline()
                .by_name(SCALE_CAPS)?
                .static_pad("src")?
                .current_caps()?;
            let s = caps.structure(0)?;
            Some((s.get::<i32>("width").ok()?, s.get::<i32>("height").ok()?))
        };
        assert_eq!(
            negotiated("[1/2147483647,2147483647/1]", &format!("! {SQUARE_PIXELS}")),
            Some((640, 360))
        );
        assert_ne!(
            negotiated("1/2147483647", ""),
            Some((640, 360)),
            "the minimum ratio no longer breaks the ranged scaler; the pin may be redundant"
        );
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
    fn videorate_precedes_scaling() {
        let desc = exact_stage(&StreamConfig::default());
        let rate = desc.find("videorate").expect("videorate is mandatory");
        let scale = desc.find("videoscale").expect("videoscale");
        assert!(rate < scale, "videorate must precede videoscale: {desc}");
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
    fn scaling_preserves_aspect_with_borders() {
        // Without `add-borders`, a 16:10 screen comes out stretched on a 16:9
        // panel. `vapostproc`'s default is `false`, so it has to be explicit.
        let desc = exact_stage(&StreamConfig::default());
        assert!(desc.contains("videoscale add-borders=true"), "{desc}");

        let cfg = StreamConfig {
            encoder: H264Encoder::VaH264,
            ..Default::default()
        };
        let desc = exact_stage(&cfg);
        assert!(desc.contains("vapostproc add-borders=true"), "{desc}");
    }

    #[test]
    fn va_path_converts_on_the_gpu() {
        let cfg = StreamConfig {
            encoder: H264Encoder::VaH264,
            ..Default::default()
        };
        let desc = exact_stage(&cfg);
        assert!(desc.contains("vapostproc"), "{desc}");
        assert!(!desc.contains("videoscale"), "{desc}");
        assert!(desc.contains("format=NV12"), "{desc}");
    }

    #[test]
    fn software_path_converts_on_the_cpu() {
        let desc = exact_stage(&StreamConfig::default());
        assert!(desc.contains("videoconvert"), "{desc}");
        assert!(desc.contains("format=I420"), "{desc}");
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
    }

    /// The requirement is enforced when the pipeline is built, not silently
    /// dropped later: a source that cannot produce a DMA-BUF fails to link.
    /// That is what makes this path safe to gate behind a switch — it either
    /// runs on the graphics card or it refuses to start, and never quietly
    /// becomes the copy it was meant to remove.
    #[test]
    #[ignore = "needs GStreamer's NVIDIA plugins (nvh264enc)"]
    fn the_gpu_path_refuses_a_source_without_a_dmabuf() {
        init().unwrap();
        let cfg = StreamConfig {
            width: 2560,
            height: 1440,
            fps: 60,
            encoder: H264Encoder::NvH264,
            ..Default::default()
        };
        let desc = mirror_pipeline_description(&cfg, &VideoSource::Test, (2560, 1440), true);
        let error = gst::parse::launch(&desc).expect_err("a test source has no DMA-BUF");
        assert!(
            error.to_string().contains("memory:DMABuf"),
            "the refusal must name what was missing: {error}"
        );
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
}
