//! Sizing the picture for the receiver, and reading back the size the
//! pipeline really settled on.
//!
//! Every H.264 path runs the same `videorate ! scale ! capsfilter` stage,
//! built by [`StreamConfig::convert_scale`]. The portal announces sizes in the
//! compositor's coordinate space, so where nothing was promised to the
//! receiver the capsfilter takes a range and the true size is only known once
//! caps are negotiated ([`negotiated_video_size`]).

use gst::prelude::*;
use gstreamer as gst;

use super::{CAPTURE_SOURCE_NAME, StreamConfig};
use crate::{NdError, Result};

/// The video capsfilter's name, so its size can be read back or narrowed after
/// the pipeline has negotiated.
pub const SCALE_CAPS: &str = "scale-caps";

/// What size to encode at: a size the receiver agreed to, or a ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoTarget {
    /// Encode at exactly this, for a link that agreed on a mode and refuses
    /// anything else: Miracast's negotiated mode, or a Cast receiver whose
    /// ANSWER declares a decoder smaller than the screen.
    Exact((u32, u32)),
    /// Encode at whatever the capture is, up to this ceiling.
    ///
    /// For every link that promised nothing. The portal announces a size in
    /// the compositor's coordinate space, which under fractional scaling is
    /// not the pixel size delivered; pinning it would resample every frame,
    /// where a range lets the scaler stand aside.
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

/// Whether to keep mirroring frames on the graphics card end to end, when
/// `BIGNETSCREEN_GPU_PATH=1`.
///
/// Off by default because it is slower where it was measured: the capture's
/// DMA-BUF is imported through GL, and NVENC encoding from a GL texture costs
/// several times what encoding from system memory does. GStreamer 1.28 has no
/// NVIDIA element that takes `memory:DMABuf` directly. The switch stays so
/// another card, compositor or modifier can be measured in one run; making it
/// the default would need a fallback, because a machine without the DMA-BUF
/// or the GL interop does not start at all.
pub fn gpu_path_requested() -> bool {
    std::env::var("BIGNETSCREEN_GPU_PATH").as_deref() == Ok("1")
}

/// The longest input gap `videorate` will fill with repeats, in nanoseconds.
///
/// Well above the capture keepalive, so ordinary jitter is still smoothed.
/// A longer gap is a stalled capture, and every repeat of it would pay a
/// full-size scale and colour conversion to show nothing new.
const MAX_DUPLICATION_NS: u64 = 500_000_000;

/// The size the encoder will really be fed, once the pipeline has negotiated.
///
/// The Cast OFFER carries this size, and nothing earlier knows it: the portal
/// announces a compositor-space size, and a second PipeWire stream cannot
/// look up the portal's node. `None` until the caps are negotiated.
pub fn negotiated_video_size(pipeline: &gst::Pipeline) -> Option<(u32, u32)> {
    pipeline
        .by_name(SCALE_CAPS)?
        .static_pad("src")?
        .current_caps()
        .as_deref()
        .and_then(caps_size)
}

/// Narrows the mirroring capsfilter to one exact size.
///
/// Only for a receiver whose ANSWER declares a decoder smaller than the screen.
/// Left alone otherwise, so the capture reaches the encoder untouched.
pub fn pin_video_size(pipeline: &gst::Pipeline, cfg: &StreamConfig) -> Result<()> {
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
/// When it equals [`negotiated_video_size`], the scaler is a passthrough.
pub fn delivered_capture_size(pipeline: &gst::Pipeline) -> Option<(u32, u32)> {
    let caps = pipeline
        .by_name(CAPTURE_SOURCE_NAME)?
        .static_pad("src")?
        .current_caps()?;
    caps_size(&caps)
}

/// The positive `width` and `height` of fixed video caps.
fn caps_size(caps: &gst::CapsRef) -> Option<(u32, u32)> {
    let structure = caps.structure(0)?;
    let width = u32::try_from(structure.get::<i32>("width").ok()?).ok()?;
    let height = u32::try_from(structure.get::<i32>("height").ok()?).ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

impl StreamConfig {
    /// The `videorate ! scale ! capsfilter` stage every H.264 path runs.
    ///
    /// `videorate` comes first so frames it drops pay for no scaling. On the
    /// VA-API path `vapostproc` converts and scales on the card instead of
    /// copying every frame to memory and back.
    pub(super) fn convert_scale(&self, target: VideoTarget) -> String {
        let fmt = self.encoder.accepted_formats();
        let fps = self.fps;
        // `videorate` turns the compositor's variable cadence into a fixed
        // one, which is what lets the Cast sender map each encoded frame
        // through its segment; without it (`drop-only=true`, or skipping it
        // for `nvh264enc`) frames arrive with no valid running time and the
        // receiver shows black. It is also the costliest element in the path,
        // holding each frame until the next. Removing that cost needs the
        // mapping fixed first.
        let rate = format!("videorate max-duplication-time={MAX_DUPLICATION_NS}");
        // `add-borders` is off by default on `vapostproc`, unlike `videoscale`;
        // without it a 16:10 screen is stretched on a 16:9 panel.
        let scale = if self.encoder.is_va() {
            "vapostproc add-borders=true".to_string()
        } else {
            // Converted first: in the 10-bit formats an HEVC film decodes to,
            // `videoscale` fills its borders pink.
            "videoconvert n-threads=0 ! videoscale add-borders=true".to_string()
        };
        // Square pixels, or the borders never appear: with the ratio left
        // open the scaler fills the frame and records the stretch as a pixel
        // shape, which receivers ignore.
        let caps = format!(
            "video/x-raw,format={fmt},{dims},pixel-aspect-ratio=1/1,framerate={fps}/1",
            dims = target.caps_dimensions()
        );
        format!("{rate} ! {scale} ! capsfilter name={SCALE_CAPS} caps=\"{caps}\"")
    }

    /// The same stage, keeping the frame on the GPU from end to end.
    ///
    /// The capture is imported as a GL texture and handed to NVENC through GL
    /// interop. It cannot scale (`glcolorscale` does not link to the encoder),
    /// so the caller uses it only for a screen already inside the ceiling.
    /// `memory:DMABuf` is demanded explicitly: otherwise `glupload` would
    /// accept system memory and upload it, which saves nothing.
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
    /// run feeds the minimum as a fixed value, to show the scaler still
    /// misbehaves on it without the pin.
    #[test]
    fn any_pixel_aspect_keeps_size() {
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
    fn ten_bit_film_gets_bars() {
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
    fn every_encoder_keeps_cadence_element() {
        // Without `videorate` the Cast sender cannot map an encoded frame
        // through its segment and the receiver shows black. Its holding delay
        // is the price of that.
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
    fn gpu_path_requests_dmabuf() {
        let cfg = StreamConfig {
            width: 2560,
            height: 1440,
            fps: 60,
            encoder: H264Encoder::NvH264,
            ..Default::default()
        };
        let desc = mirror_pipeline_description(&cfg, &VideoSource::Test, (2560, 1440), true);
        // Without asking for the DMA-BUF, `glupload` would accept system memory
        // and upload it: the same copy, behind a longer pipeline.
        assert!(desc.contains("video/x-raw(memory:DMABuf)"), "{desc}");
        assert!(desc.contains("glupload"), "{desc}");
        // `glcolorscale` does not link to this encoder, which is why the caller
        // picks this path only for a screen that fits.
        assert!(!desc.contains("glcolorscale"), "{desc}");
        assert!(!desc.contains("videoconvert"), "{desc}");
    }

    /// The requirement is enforced when the pipeline is built, not silently
    /// dropped later: a source that cannot produce a DMA-BUF fails to link.
    /// So the path either runs on the graphics card or refuses to start, and
    /// never quietly becomes the copy it was meant to remove.
    #[test]
    #[ignore = "needs GStreamer's NVIDIA plugins (nvh264enc)"]
    fn gpu_path_needs_dmabuf_source() {
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
    fn mirroring_requests_a_ceiling() {
        // A 2560x1440 panel at 110% scale is announced by the portal as
        // 2328x1310 and delivered at 2560x1440. A range lets the capture
        // through at its own size and only bites above the person's ceiling.
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
    fn mirroring_keeps_capture_format() {
        // The portal delivers BGRA and NVENC accepts it; asking for NV12 first
        // spends a CPU pass over every frame on a conversion the card does
        // while encoding.
        let nv = StreamConfig {
            encoder: H264Encoder::NvH264,
            ..Default::default()
        };
        let desc = mirror_pipeline_description(&nv, &VideoSource::Test, (2560, 1440), false);
        assert!(
            desc.contains("format={ BGRA, BGRx, RGBA, RGBx, NV12 }"),
            "{desc}"
        );

        // Every other encoder keeps the one format it was verified with: a
        // wrong format list shows up as a receiver getting nothing.
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
