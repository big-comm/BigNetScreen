//! The H.264 encoder element and the properties each one needs.
//!
//! Every encoder is told the same things (no lookahead, no B-frames, CBR, an
//! access-unit delimiter) in its own vocabulary and units. `x264enc`,
//! `vah264enc`, `vaapih264enc` and `nvh264enc` take `bitrate` in kbit/s and
//! `openh264enc` in bit/s, which is why each builds its own description in
//! [`H264Encoder::encoder_description`] rather than sharing one format string.
//! Which encoder a machine runs is `encoder_selection`'s question.

use gstreamer as gst;

use super::StreamConfig;

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

/// Automatic latency: the pipeline uses the minimum it reports itself.
///
/// Measured in the field on a real 1080p60 Miracast link: **41 ms**. The 20 ms
/// this module once forced sat *below* that minimum — and the effect was not
/// "faster", it was the sink receiving data and displaying nothing.
pub const PIPELINE_LATENCY_AUTO: u64 = 0;

/// `openh264enc` with `usage-type=screen` has latency spikes after scene
/// changes; it alone justifies fixed headroom (the reference C project's
/// value).
pub const OPENH264_PIPELINE_LATENCY_MS: u64 = 500;

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
        if self.is_hardware() { "NV12" } else { "I420" }
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
            _ => PIPELINE_LATENCY_AUTO,
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

/// The name given to the encoder element in every description.
pub const ENCODER_NAME: &str = "enc";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::CAST_VBV_FRAMES;

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
            bitrate_kbps: 10_000,
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

    #[test]
    fn cast_high_selects_v4l2_high_without_changing_default_wfd_control() {
        let cfg = StreamConfig {
            profile: H264Profile::High,
            ..Default::default()
        };
        assert!(
            H264Encoder::V4l2H264
                .encoder_description(&cfg)
                .contains("h264_profile=4,")
        );
        assert!(
            H264Encoder::V4l2H264
                .encoder_description(&StreamConfig::default())
                .contains("h264_profile=0,")
        );
    }
}
