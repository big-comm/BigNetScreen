//! The MPEG-TS over HTTP pipeline, for the Cast fallback and DLNA renderers.
//!
//! The stream ends in `multisocketsink`; `crate::stream_server` writes the
//! HTTP headers and then hands it the receiver's socket by name.

use super::{StreamConfig, VideoSource, VideoTarget};

/// How `multisocketsink` positions a client that has just connected.
///
/// `latest-keyframe` would hand over up to a whole GOP of data already sent,
/// which becomes permanent delay because the receiver keeps what it buffered.
/// `next-keyframe` costs a few frames before the first picture and nothing
/// after. `BIGNETSCREEN_CC_SYNC_METHOD` overrides it, for comparison.
fn chromecast_sync_method() -> String {
    std::env::var("BIGNETSCREEN_CC_SYNC_METHOD").unwrap_or_else(|_| "next-keyframe".to_string())
}

/// The maximum resolution sent to a Chromecast.
///
/// The Default Media Receiver decodes H.264 up to 1080p; above that it would
/// need VP9 or HEVC, which this pipeline does not produce.
pub const CHROMECAST_MAX_RESOLUTION: (u32, u32) = (1920, 1080);

/// The `multisocketsink`'s name in the transport-stream pipeline description.
///
/// The HTTP server looks it up by this name to hand it the receiver's socket
/// once the headers are written.
pub const TS_HTTP_SINK_NAME: &str = "cc-sink";

/// The DLNA ceiling while the quality preference is untouched; a safe guess,
/// not a capability the renderer declared.
pub const DLNA_MAX_RESOLUTION: (u32, u32) = (1920, 1080);

/// How long the transport-stream multiplex waits for a file's first picture.
const MEDIA_MUX_WAIT_NS: u64 = 1_000_000_000;

/// Builds an H.264 + AAC transport stream for a receiver that fetches it over
/// HTTP — the Cast fallback and every DLNA renderer.
///
/// MPEG-TS is a supported Cast container and the one DLNA profile family a
/// television is certain to have. Short mux and queue batches bound what the
/// sender holds; the receiver may still prebuffer seconds.
///
/// `mux_bitrate_bps` pads the multiplex with null packets to a constant rate;
/// `None`, for Cast, sends only real data. A DLNA renderer needs the padding:
/// some prebuffer a fixed number of bytes, so a stream that compresses well
/// arrives seconds late. Padding to the encoder's target removes most of that
/// delay without spending the network on more null packets.
pub fn ts_http_pipeline_description(
    cfg: &StreamConfig,
    source: &VideoSource,
    target: VideoTarget,
    mux_bitrate_bps: Option<u32>,
) -> String {
    // MPEG-TS buffers carry no H.264 DELTA_UNIT flags, so a keyframe resync
    // would cut TS packets mid-stream. The DLNA backlog is bounded in bytes
    // instead: eight seconds at the target rate, within 1-32 MiB, enough for a
    // renderer that keeps reading for seconds after Pause until the session's
    // next poll notices. A stalled reader is disconnected (`recover-policy=none`
    // ends it with status "slow") rather than queued for without bound.
    let sink_policy = match mux_bitrate_bps {
        Some(bps) => format!(
            "sync-method=latest recover-policy=none unit-format=bytes units-max={}",
            u64::from(bps).clamp(1_048_576, 33_554_432)
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
        // A file's picture leaves the decoder after the silence that backs its
        // sound, and without a wait the first program table lists audio alone;
        // some renderers take that table as the whole program and give up.
        // The wait costs nothing once both tracks flow.
        wait_for_video = if source.is_media() {
            format!(" latency={MEDIA_MUX_WAIT_NS}")
        } else {
            String::new()
        },
        src = source.description(),
        convert = cfg.convert_scale(target),
        vqueue = cfg.video_queue(source),
        enc = cfg.encoder_stage(false),
        sink = TS_HTTP_SINK_NAME,
        audio = cfg.audio_branch(source, "mux."),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::{
        ENCODER_NAME, PipelineGuard, build_pipeline, count_buffers, negotiated_video_size,
    };
    use gstreamer as gst;

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
}
