//! The Cast mirroring pipeline: encoded H.264 and Opus access units handed to
//! `nd-chromecast` through two `appsink`s, with no container.
//!
//! Cast Streaming packetizes, encrypts and paces the frames itself, so this
//! description ends at the encoder's output.

use super::{StreamConfig, VideoSource, VideoTarget};

/// The name of the `appsink` that hands encoded frames to Cast Streaming.
pub const MIRROR_VIDEO_SINK: &str = "mirror-video";

/// The same, for the audio track.
pub const MIRROR_AUDIO_SINK: &str = "mirror-audio";

/// The rate-control budget the Cast mirroring encoder is held to, in frames
/// (see [`StreamConfig::vbv_frames`]).
///
/// Mirroring puts each frame on the wire as its own burst of UDP datagrams,
/// so the size of one frame is something the network sees directly. Without a
/// budget, switching a browser tab makes one enormous frame, and the receiver
/// shows none of the change until its last packet arrives. The encoder cannot
/// know that a frame is a burst on a paced link, so this transport tells it.
pub const CAST_VBV_FRAMES: u32 = 3;

/// The Cast mirroring pipeline, ending in one `appsink` per track.
///
/// No muxer and no receiver-side player, which is what spares this path the
/// Default Media Receiver's seconds of prebuffering:
/// - H.264 byte-stream access units with SPS and PPS at every IDR, so the
///   receiver can join;
/// - Opus audio, which the mirroring receiver expects;
/// - a video `appsink` holding one frame, so it goes straight to the network;
/// - an audio `appsink` holding eight 10 ms frames, enough to absorb thread
///   scheduling without audible delay.
///
/// `ceiling` is the largest picture the person is willing to send; below it
/// the capture reaches the encoder at its own size. `gpu` keeps the frame on
/// the graphics card (see [`super::gpu_path_requested`]); it cannot scale, so
/// the caller sets it only for a screen inside the ceiling.
pub fn mirror_pipeline_description(
    cfg: &StreamConfig,
    source: &VideoSource,
    ceiling: (u32, u32),
    gpu: bool,
) -> String {
    let audio = format!(
        " {src} ! audioconvert ! audioresample ! \
         audio/x-raw,rate=48000,channels=2 ! {queue} ! \
         opusenc bitrate=128000 frame-size=10 ! \
         appsink name={audio_sink} emit-signals=false sync=false \
         max-buffers=8 drop=false",
        src = cfg.audio.description(),
        queue = cfg.mirror_audio_queue(),
        audio_sink = MIRROR_AUDIO_SINK,
    );

    format!(
        // The profile comes from the configuration, unlike WFD where the
        // specification pins it: every Cast receiver decodes H.264 High.
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
        vqueue = cfg.video_queue(source),
        enc = cfg.encoder_stage(gpu),
        video_sink = MIRROR_VIDEO_SINK,
        audio = audio,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::{CHROMECAST_MAX_RESOLUTION, H264Encoder, init, probe_encoders};
    use gstreamer as gst;

    #[test]
    fn mirror_pipeline_has_no_muxer() {
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
    fn mirror_exposes_both_sinks_unqueued() {
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
    fn only_video_queue_drops() {
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
        // Audio may not: dropping a sample opens a hole in the sound.
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
    fn nvenc_input_goes_through_cudaupload() {
        // `cudaupload` uploads faster than NVENC does for itself.
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
    }

    /// The real string, built and linked: the one check that catches a
    /// misspelled element or a caps field the encoder will not take.
    #[test]
    #[ignore = "needs GStreamer's NVIDIA plugins (cudaupload, nvh264enc)"]
    fn the_cuda_description_builds() {
        init().unwrap();
        let nv = StreamConfig {
            encoder: H264Encoder::NvH264,
            ..Default::default()
        };
        let built = mirror_pipeline_description(&nv, &VideoSource::Test, (2560, 1440), false);
        gst::parse::launch(&built).expect("the CUDA description builds");
    }
}
