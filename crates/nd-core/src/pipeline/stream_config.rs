//! One transmission's parameters, and the encoder, queue and audio stages
//! derived from them.
//!
//! The protocol crates fill a [`StreamConfig`] from what the receiver
//! negotiated and the person's preferences (`crate::settings`); the path
//! builders in `wfd`, `mirror` and `ts_http` read every stage from it.

use super::{AudioSource, H264Encoder, H264Profile, VideoSource};

/// One session's encoding parameters, as negotiated with the receiver.
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
    /// in frames because that bounds the size of a single one; each encoder
    /// is told in its own unit. It matters on a packet network: NVENC's
    /// default holds a whole second, so one key frame can take most of a
    /// second's budget and the stream overruns its CBR ceiling. One frame is
    /// worse than no limit, because an IDR cannot fit and the rate control
    /// overshoots; two or three keep key frames small.
    pub vbv_frames: u32,
    /// Seconds between forced key frames, or `0` for none at all.
    ///
    /// One by default: on a direct link a periodic IDR bounds recovery after
    /// loss, because the receiver has no way to ask for one. Cast mirroring
    /// asks, with NACK and picture loss indications, so it sets `0`: an IDR
    /// squeezed into a budget a few frames wide lands softer than its
    /// neighbours, and one a second makes the picture pulse.
    pub gop_seconds: u32,
    /// The H.264 feature set the receiver on this path is known to decode.
    ///
    /// Constrained baseline by default, which the Wi-Fi Display profile
    /// requires and any receiver decodes. Raising it is a per-path statement
    /// about the device on the other end.
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

/// The video queue before the encoder: about one frame.
pub const VIDEO_QUEUE_BUFFERS: u32 = 3;

/// The same, in milliseconds.
pub const VIDEO_QUEUE_MS: u64 = 30;

/// The audio queue on the muxed paths, in buffers.
///
/// `mpegtsmux` interleaves the tracks by timestamp, so video waits for audio
/// and every millisecond of slack here becomes picture delay. Leaking a
/// capture's audio is the lesser evil.
pub const AUDIO_QUEUE_BUFFERS: u32 = 4;

/// The same, in milliseconds.
pub const AUDIO_QUEUE_MS: u64 = 40;

/// The Cast mirroring audio queue, in frames.
///
/// No muxer waits on this track (each leaves through its own `appsink` and the
/// receiver aligns them by timestamp), so slack costs no picture delay. The
/// queue is generous and never leaks: dropped samples are a hole in the sound,
/// not a catch-up.
pub const MIRROR_AUDIO_QUEUE_BUFFERS: u32 = 64;

/// The same, in milliseconds.
pub const MIRROR_AUDIO_QUEUE_MS: u64 = 200;

impl StreamConfig {
    /// The CBR bitrate: [`Self::bitrate_kbps`] when set, otherwise a table by
    /// resolution for 30 Hz, raised linearly up to half again at 60 Hz so
    /// motion does not blur.
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
            let factor = 1.0 + 0.5 * ((self.fps.min(60) - 30) as f32 / 30.0);
            (base as f32 * factor) as u32
        } else {
            base
        }
    }

    /// Shrinks a resolution to fit a cap, keeping its aspect ratio.
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
    /// A lower quality preference sends less than the receiver can take; no
    /// preference sends more than it offered.
    pub fn capped_by_preference(receiver_max: (u32, u32)) -> (u32, u32) {
        let (pw, ph) = crate::settings::current().resolution_limit();
        (receiver_max.0.min(pw), receiver_max.1.min(ph))
    }

    /// The size to send when the protocol never states a maximum.
    ///
    /// Cast is the case: the sender declares a resolution in its OFFER and the
    /// receiver takes it or refuses it. `default_max` is a safe guess, used
    /// while the quality preference is untouched; any other preference wins,
    /// above the guess too. Miracast, where the sink lists its modes, uses
    /// [`Self::capped_by_preference`] instead.
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
    /// A preference can lower it but never raise a link that agreed on less.
    pub fn capped_fps(negotiated: u32) -> u32 {
        negotiated.min(crate::settings::current().fps).max(1)
    }

    /// [`Self::vbv_frames`] in milliseconds, for encoders that ask in time.
    ///
    /// `0` stays `0`, "leave the encoder's default alone"; any other count is
    /// at least one millisecond, so it never rounds into that meaning.
    pub fn vbv_ms(&self) -> u32 {
        match self.vbv_frames {
            0 => 0,
            frames => (frames * 1000 / self.fps.max(1)).max(1),
        }
    }

    /// The maximum distance between key frames, in frames: [`Self::gop_seconds`]
    /// at this frame rate, `0` for none. `BIGNETSCREEN_GOP` overrides it.
    pub fn gop(&self) -> u32 {
        if let Ok(value) = std::env::var("BIGNETSCREEN_GOP")
            && let Ok(gop) = value.parse::<u32>()
        {
            return gop.max(1);
        }
        if self.gop_seconds == 0 {
            return 0;
        }
        self.fps.max(1) * self.gop_seconds
    }

    /// The pipeline latency suited to the chosen encoder.
    ///
    /// The film profile adds buffering on top. `BIGNETSCREEN_PIPELINE_LATENCY_MS`
    /// overrides it, for bisecting interoperability problems.
    pub fn latency_ms(&self) -> u64 {
        if let Ok(value) = std::env::var("BIGNETSCREEN_PIPELINE_LATENCY_MS")
            && let Ok(ms) = value.parse::<u64>()
        {
            return ms;
        }
        let encoder_latency = self.encoder.pipeline_latency_ms();
        if crate::latency::is_film() {
            // Never below what the encoder needs: under the pipeline's own
            // minimum, sinks drop late buffers instead of playing them.
            return encoder_latency.max(crate::latency::FILM_PIPELINE_LATENCY_MS);
        }
        encoder_latency
    }

    /// The encoder, preceded by `cudaupload` for NVENC fed from system memory:
    /// it uploads faster than the encoder does by itself. On the GPU path the
    /// frame is already on the card.
    pub(super) fn encoder_stage(&self, gpu: bool) -> String {
        let encoder = self.encoder.encoder_description(self);
        if gpu || self.encoder != H264Encoder::NvH264 {
            encoder
        } else {
            format!("cudaupload ! {encoder}")
        }
    }

    pub(super) fn video_queue(&self, source: &VideoSource) -> String {
        format!(
            "queue max-size-buffers={b} max-size-time={t}000000 max-size-bytes=0 \
             leaky={leaky}",
            b = VIDEO_QUEUE_BUFFERS,
            t = VIDEO_QUEUE_MS,
            leaky = leaky(source),
        )
    }

    /// The audio branch's queue on the muxed path.
    pub(super) fn audio_queue(&self, source: &VideoSource) -> String {
        format!(
            "queue max-size-buffers={b} max-size-time={t}000000 max-size-bytes=0 \
             leaky={leaky}",
            b = AUDIO_QUEUE_BUFFERS,
            t = AUDIO_QUEUE_MS,
            leaky = leaky(source),
        )
    }

    /// The Cast mirroring audio branch's queue — **without** `leaky`.
    ///
    /// The video queue leaks on purpose: an old frame is of no interest, the
    /// next one arrives whole. Audio is the opposite — dropping samples does
    /// not "advance" the sound, it opens a hole in the middle of it. Not
    /// leaking is possible here because nothing waits on this track; on the
    /// muxed path, video would.
    pub(super) fn mirror_audio_queue(&self) -> String {
        format!(
            "queue max-size-buffers={b} max-size-time={t}000000 max-size-bytes=0",
            b = MIRROR_AUDIO_QUEUE_BUFFERS,
            t = MIRROR_AUDIO_QUEUE_MS,
        )
    }

    /// The AAC audio branch attached to the given pad of the muxer named
    /// `mux`.
    ///
    /// `mux_pad` is `"mux."` for Chromecast (automatic PID) and
    /// `"mux.sink_4352"` for WFD, which requires PID 0x1100.
    pub(super) fn audio_branch(&self, source: &VideoSource, mux_pad: &str) -> String {
        format!(
            "{src} ! audioconvert ! audioresample ! \
             audio/x-raw,rate=48000,channels=2 ! {queue} ! \
             avenc_aac bitrate=128000 ! aacparse ! {mux_pad}",
            src = self.audio.description(),
            queue = self.audio_queue(source),
        )
    }
}

/// What a queue in front of the encoder or the muxer does when it is full.
///
/// A capture drops its oldest buffer: a stale frame is worth less than the
/// next one. A file is decoded faster than it plays, so its queues hold the
/// decoder back instead of throwing the film away.
fn leaky(source: &VideoSource) -> &'static str {
    if source.is_media() {
        "no"
    } else {
        "downstream"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::{CHROMECAST_MAX_RESOLUTION, RTP_LATENCY_MS, rtp_latency_ms};

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
        // Cast: 1080p is our own safe guess, so a person who chooses 1440p
        // gets 1440p. Miracast: the sink listed the modes it accepts, and
        // exceeding them is not ours to decide. The preference is set here so
        // the result does not depend on the settings of the machine running
        // the tests.
        let _lock = crate::latency::exclusive();
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
    fn the_muxed_path_keeps_its_audio_queue_short() {
        // On the muxed path video is interleaved with audio, so slack given to
        // the audio queue becomes picture delay.
        const _: () = assert!(
            AUDIO_QUEUE_MS <= 50,
            "the muxed path's audio queue turns into picture delay"
        );
        // And the mirroring one, which goes through no muxer, can be generous.
        const _: () = assert!(MIRROR_AUDIO_QUEUE_MS >= AUDIO_QUEUE_MS);

        // The generated description has to reflect both choices.
        let cfg = StreamConfig::default();
        assert!(
            cfg.audio_queue(&VideoSource::Test)
                .contains("leaky=downstream")
        );
        assert!(!cfg.mirror_audio_queue().contains("leaky"));
    }

    #[test]
    fn film_mode_buffers_without_going_under_the_encoder_minimum() {
        // Buffering is added on top of what the encoder needs: below the
        // pipeline's own minimum, sinks drop late buffers and show black.
        let spiky = StreamConfig {
            encoder: H264Encoder::OpenH264,
            ..Default::default()
        };
        let quick = StreamConfig::default();

        let _lock = crate::latency::exclusive();
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
        // 4K 16:9 is reduced to the cap.
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
    fn gop_is_one_second() {
        assert_eq!(cfg_at(1920, 1080, 30).gop(), 30);
        assert_eq!(cfg_at(1920, 1080, 60).gop(), 60);
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
}
