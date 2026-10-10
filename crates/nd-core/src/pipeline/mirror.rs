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
