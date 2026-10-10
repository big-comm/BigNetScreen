//! The Miracast (Wi-Fi Display) pipeline: H.264 and AAC in MPEG-TS over RTP.
//!
//! `nd-wfd` negotiates the mode and the sink's RTP port over RTSP and builds
//! this description. Everything WFD-specific stays here: the fixed PES PIDs,
//! constrained baseline, `rtpbin`'s sender reports and the pacing of the RTP
//! sink.

use std::net::IpAddr;

use super::{H264Profile, StreamConfig, VideoSource, VideoTarget};

/// Local RTP port used as the source of the WFD stream.
pub const LOCAL_RTP_PORT: u16 = 16384;

/// The video PES PID the Wi-Fi Display specification requires (0x1011).
///
/// Through the generic `mux.` pad `mpegtsmux` picks PIDs itself, and a WFD
/// sink that finds no video on this one receives data and shows black.
pub const WFD_VIDEO_PID: u16 = 0x1011;

/// The audio PES PID the specification requires (0x1100).
pub const WFD_AUDIO_PID: u16 = 0x1100;

/// Whether the RTP `udpsink` holds each packet until its running time plus
/// the pipeline latency.
///
/// A screen capture is already paced by the screen, so for it the wait is
/// pure delay unless the film profile asked for exactly that buffering: a
/// send-only `rtpbin` has no jitter buffer, so this sink is the only place it
/// can happen. A file is decoded as fast as it can be read and is always
/// paced. `BIGNETSCREEN_RTP_SYNC=1` forces the wait, for comparison.
fn rtp_sink_syncs_to_clock(source: &VideoSource) -> bool {
    source.is_media()
        || crate::latency::is_film()
        || std::env::var("BIGNETSCREEN_RTP_SYNC")
            .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// A WFD session's RTP destination.
#[derive(Clone, Copy, Debug)]
pub struct WfdTransport {
    /// The sink's address on the Wi-Fi Direct or LAN link.
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
/// `source ! convert_scale ! encoder ! h264parse ! mpegtsmux ! rtpmp2tpay !
/// rtpbin ! udpsink`, with an AAC branch into the same multiplex. What a WFD
/// sink requires of it:
/// - audio and video in one programme: the sink declared an audio and video
///   session and discards a programme missing either;
/// - video on PID 0x1011 and audio on 0x1100;
/// - `perfect-rtptime=false`, so RTP timestamps follow the send clock rather
///   than the buffer count; the sink schedules display by them;
/// - constrained baseline, required by the WFD profile;
/// - SPS and PPS at every IDR (`config-interval=-1`), so the sink can join at
///   any moment;
/// - RTCP sender reports from `rtpbin`, which the sink synchronises by;
/// - a short, non-leaky queue before the payloader: dropping part of a TS
///   packet corrupts the stream.
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
        "rtpbin name=rtpbin rtp-profile=avp \
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
        src = source.description(),
        // The sink agreed to one mode in M3/M4 and accepts no other size.
        convert = cfg.convert_scale(VideoTarget::Exact((cfg.width, cfg.height))),
        vqueue = cfg.video_queue(source),
        enc = cfg.encoder_stage(false),
        video_pid = WFD_VIDEO_PID,
        sync = rtp_sink_syncs_to_clock(source),
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

    #[test]
    fn film_mode_holds_the_stream_for_the_pipeline_latency() {
        // A send-only `rtpbin` has no jitter buffer, so the only place the
        // film profile's buffering can happen on this side is a sink that
        // waits for the pipeline latency before sending.
        let _lock = crate::latency::exclusive();
        crate::latency::set(crate::latency::Profile::Film);
        let film = wfd_desc(&StreamConfig::default());
        crate::latency::set(crate::latency::Profile::Responsive);
        let responsive = wfd_desc(&StreamConfig::default());
        let rtp_sink = format!("port=19000 bind-port={LOCAL_RTP_PORT} sync=");
        assert!(film.contains(&format!("{rtp_sink}true")), "{film}");
        assert!(
            responsive.contains(&format!("{rtp_sink}false")),
            "{responsive}"
        );
    }
}
