//! The Miracast (Wi-Fi Display) pipeline: H.264 and AAC in MPEG-TS over RTP.
//!
//! `nd-wfd` negotiates the mode and the sink's RTP port over RTSP and builds
//! this description. Everything WFD-specific stays here: the fixed PES PIDs,
//! constrained baseline, `rtpbin`'s sender reports and the RTP jitter buffer.

use std::net::IpAddr;

use super::{H264Profile, StreamConfig, VideoSource, VideoTarget};

/// The RTP jitter buffer's latency (`rtpbin`), when responding quickly.
pub const RTP_LATENCY_MS: u64 = 20;

/// The RTP jitter buffer to use, given the latency profile.
///
/// This is the buffer that actually absorbs network variance on the Miracast
/// path: frames arriving unevenly are held here and released on schedule. It
/// costs exactly its own size in delay, pointer included.
pub fn rtp_latency_ms() -> u64 {
    if crate::latency::is_film() {
        crate::latency::FILM_RTP_LATENCY_MS
    } else {
        RTP_LATENCY_MS
    }
}

/// Local RTP port used as the source of the WFD stream.
pub const LOCAL_RTP_PORT: u16 = 16384;

/// The video PES PID the Wi-Fi Display specification requires (0x1011).
///
/// `mpegtsmux` assigns PIDs automatically when the generic `mux.` pad is used;
/// WFD sinks look for video on **this** PID. It is the difference between the
/// projector showing the picture and sitting black while receiving data.
pub const WFD_VIDEO_PID: u16 = 0x1011;

/// The audio PES PID the specification requires (0x1100).
pub const WFD_AUDIO_PID: u16 = 0x1100;

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
    /// Local source RTP port.
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
    // The caps below pin the profile, so the encoder must not be told
    // otherwise: CABAC under constrained baseline is a stream the sink refuses.
    let cfg = &StreamConfig {
        profile: H264Profile::ConstrainedBaseline,
        ..*cfg
    };
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
        rtp_latency = rtp_latency_ms(),
        src = source.description(),
        // Wi-Fi Display agreed a mode with the sink in M3/M4; sending any
        // other size is a mode it never accepted.
        convert = cfg.convert_scale(VideoTarget::Exact((cfg.width, cfg.height))),
        vqueue = cfg.video_queue(source),
        enc = cfg.encoder_stage(false),
        video_pid = WFD_VIDEO_PID,
        // A screen produces frames at the pace of the screen, so waiting on
        // the clock before sending is pure delay. **A file does not**: with
        // `sync=false` a decoder reads a film as fast as the disc allows and
        // the receiver is sent an hour of video in a few seconds. So the file
        // is the one case that has to be paced.
        sync = if rtp_sink_syncs_to_clock() || source.is_media() {
            "true"
        } else {
            "false"
        },
        ip = transport.sink_ip,
        port = transport.rtp_port,
        local = transport.local_rtp_port,
        audio = cfg.audio_branch(source, &format!("mux.sink_{}", WFD_AUDIO_PID)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn wfd_desc(cfg: &StreamConfig) -> String {
        let src = VideoSource::PipeWire {
            fd: Some(7),
            node_id: 42,
            serial: None,
            size: None,
            min_buffers: None,
        };
        let transport = WfdTransport::new(IpAddr::V4(Ipv4Addr::new(192, 168, 49, 1)), 19000);
        wfd_pipeline_description(cfg, &src, &transport)
    }

    #[test]
    fn pipewire_source_carries_fd_and_node() {
        // A regression: without `fd=`/`path=` pipewiresrc does not capture the
        // stream the portal opened — it captured an arbitrary node from the
        // daemon.
        let desc = wfd_desc(&StreamConfig::default());
        assert!(
            desc.contains("pipewiresrc name=capture fd=7 path=42"),
            "{desc}"
        );
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
}
