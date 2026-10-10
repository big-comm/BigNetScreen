//! A hand-written RTSP/WFD (Miracast) source.
//!
//! Once the Wi-Fi Direct group forms, the **sink** (TV/projector) connects to
//! our RTSP server (port 7236). We, in the **source** role, drive the WFD
//! negotiation over a single TCP connection where both sides exchange
//! requests and responses:
//!
//! - **M1** us→sink: `OPTIONS` (Require: org.wfa.wfd1.0)
//! - **M2** sink→us: `OPTIONS` → we answer with the methods we support
//! - **M3** us→sink: `GET_PARAMETER` asking for the capabilities
//! - **M4** us→sink: `SET_PARAMETER` with the format **chosen from the sink's
//!   capabilities** and the presentation URL
//! - **M5** us→sink: `SET_PARAMETER wfd_trigger_method: SETUP`
//! - **M6/M7** sink→us: `SETUP`/`PLAY` → streaming begins
//! - **M16** us→sink: an empty periodic `GET_PARAMETER` (keepalive)
//!
//! Capabilities and the mode chosen from them live in [`capabilities`]; the
//! byte-level framing in `message`.

mod capabilities;
mod message;

pub use capabilities::{
    MAX_SOURCE_SIZE, ResolutionTable, SinkCaps, WfdMode, m4_video_formats, mode_from_native,
    parse_caps, parse_video_formats,
};
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

use gst::prelude::*;
use gstreamer as gst;
use tokio::io::BufReader;
use tokio::net::TcpStream;

use nd_core::pipeline::{self, StreamConfig, VideoSource, WfdTransport};
use nd_core::sink::{SinkStatus, StreamLink};
use nd_core::{NdError, Result};

use message::{read_message, send};

/// The WFD RTSP server's port, the one the firewall lease opens.
pub use nd_net::firewall::RTSP_PORT;

const WFD_URL: &str = "rtsp://localhost/wfd1.0";
const WFD_METHODS: &str =
    "org.wfa.wfd1.0, OPTIONS, GET_PARAMETER, SET_PARAMETER, SETUP, PLAY, TEARDOWN";

/// How long to go without hearing from the sink before calling the session dead.
const IDLE_TIMEOUT: Duration = Duration::from_secs(70);
/// The interval between the M16 keepalives we send (below the 60 s `timeout=`
/// we announce in SETUP).
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(25);
/// How long the whole negotiation gets to reach PLAY.
const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(30);
/// The window in which an encoder must prove it produces frames.
///
/// Short enough not to delay the picture noticeably, long enough for a
/// hardware encoder to initialise its driver.
const ENCODER_PROBE: Duration = Duration::from_millis(1200);

/// Performs the WFD M1–M3 handshake and returns the sink's advertised
/// capabilities.
///
/// Used by the diagnostic example; the full session is [`cast_to_sink`].
pub async fn negotiate_caps(stream: TcpStream) -> Result<SinkCaps> {
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut cseq = 0u32;

    cseq += 1;
    send(
        &mut writer,
        &format!("OPTIONS * RTSP/1.0\r\nCSeq: {cseq}\r\nRequire: org.wfa.wfd1.0\r\n\r\n"),
    )
    .await?;
    tracing::debug!("WFD M1 OPTIONS sent");

    let mut m3_cseq = None;
    let deadline = tokio::time::Instant::now() + NEGOTIATION_TIMEOUT;

    loop {
        let msg = tokio::time::timeout_at(deadline, read_message(&mut reader))
            .await
            .map_err(|_| {
                NdError::Protocol("the sink did not finish the negotiation in time".into())
            })??;

        if msg.is_response() {
            if Some(msg.cseq()) == m3_cseq {
                if !msg.is_success() {
                    return Err(NdError::Protocol(format!(
                        "the sink refused M3 ({})",
                        msg.start_line
                    )));
                }
                return Ok(parse_caps(&msg.body));
            }
            continue;
        }

        match msg.method() {
            Some("OPTIONS") => {
                send(
                    &mut writer,
                    &format!(
                        "RTSP/1.0 200 OK\r\nCSeq: {}\r\nPublic: {WFD_METHODS}\r\n\r\n",
                        msg.cseq_str()
                    ),
                )
                .await?;
                if m3_cseq.is_none() {
                    cseq += 1;
                    send(&mut writer, &m3_request(cseq)).await?;
                    m3_cseq = Some(cseq);
                    tracing::debug!("WFD M3 GET_PARAMETER sent");
                }
            }
            other => {
                tracing::debug!(method = ?other, "unexpected request; answering 200");
                send(
                    &mut writer,
                    &format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\n\r\n", msg.cseq_str()),
                )
                .await?;
            }
        }
    }
}

fn m3_request(cseq: u32) -> String {
    let query = "wfd_video_formats\r\nwfd_audio_codecs\r\nwfd_client_rtp_ports\r\n";
    format!(
        "GET_PARAMETER {WFD_URL} RTSP/1.0\r\nCSeq: {cseq}\r\n\
         Content-Type: text/parameters\r\nContent-Length: {}\r\n\r\n{query}",
        query.len()
    )
}

/// What we are waiting for on each `CSeq` we sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Awaiting {
    Options,
    GetParameter,
    SetFormat,
    TriggerSetup,
    Keepalive,
}

/// Parameters of the WFD cast session.
pub struct WfdCastConfig {
    pub media_control: Option<nd_core::media::FilePlaybackControl>,
    /// Our IP on the P2P link (goes into `wfd_presentation_URL`).
    pub our_ip: IpAddr,
    /// The sink's IP (RTP destination).
    pub sink_ip: IpAddr,
    /// The video source (a real capture, a media file, or a test pattern).
    pub video: VideoSource,
    /// Where the sound comes from.
    ///
    /// Carried with the configuration rather than decided when the pipeline is
    /// built: a file brings its own sound, and mixing the computer's system
    /// audio over a film is not what anybody meant.
    pub audio: nd_core::pipeline::AudioSource,
    /// The encoder chosen from the registry/driver.
    pub encoder: nd_core::pipeline::H264Encoder,
    /// The captured screen's resolution (it sets the target aspect ratio).
    pub source_size: (u32, u32),
    /// A resolution cap (bounds the chosen mode; useful on weak machines).
    pub max_resolution: (u32, u32),
}

impl WfdCastConfig {
    pub fn new(
        our_ip: IpAddr,
        sink_ip: IpAddr,
        video: VideoSource,
        encoder: nd_core::pipeline::H264Encoder,
    ) -> Self {
        Self {
            media_control: None,
            our_ip,
            sink_ip,
            video,
            audio: nd_core::pipeline::AudioSource::detect(),
            encoder,
            source_size: (1920, 1080),
            max_resolution: (1920, 1080),
        }
    }

    /// Takes the sound from somewhere other than this computer's output.
    pub fn with_audio(mut self, audio: nd_core::pipeline::AudioSource) -> Self {
        self.audio = audio;
        self
    }

    /// Supplies the capture's real resolution, so the mode choice respects the
    /// user's screen aspect.
    pub fn with_source_size(mut self, size: (u32, u32)) -> Self {
        self.source_size = size;
        self
    }
}

/// Drives the entire WFD session (M1–M7) and, on PLAY, starts the MPEG-TS/RTP
/// streaming to the sink. Returns when the session ends.
///
/// `status` is here for one reason: the resolution and frame rate are settled
/// *inside* this negotiation, and the interface has nothing truthful to show
/// about a session until they are. Reporting them from the caller would mean
/// reporting what was asked for.
pub async fn cast_to_sink(
    stream: TcpStream,
    cfg: WfdCastConfig,
    status: &SinkStatus,
) -> Result<()> {
    let _ = stream.set_nodelay(true);
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let mut cseq = 0u32;
    let mut pending: HashMap<u32, Awaiting> = HashMap::new();
    let mut caps = SinkCaps::default();
    let mut chosen: Option<WfdMode> = None;
    let mut sink_rtp_port = 0u16;
    let mut playing: Option<pipeline::PipelineGuard> = None;
    let mut pipeline_events: Option<nd_core::pipeline::PipelineEvents> = None;

    // M1
    cseq += 1;
    send(
        &mut writer,
        &format!("OPTIONS * RTSP/1.0\r\nCSeq: {cseq}\r\nRequire: org.wfa.wfd1.0\r\n\r\n"),
    )
    .await?;
    pending.insert(cseq, Awaiting::Options);

    let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive.tick().await; // the first tick completes at once

    let negotiation_deadline = tokio::time::Instant::now() + NEGOTIATION_TIMEOUT;
    let result = 'session: loop {
        // Keep the same read future and deadline across keepalive ticks.
        let msg = {
            let incoming = tokio::time::timeout(IDLE_TIMEOUT, read_message(&mut reader));
            tokio::pin!(incoming);
            loop {
                tokio::select! {
                    incoming = &mut incoming => match incoming {
                        Ok(Ok(msg)) => break msg,
                        Ok(Err(err)) => break 'session Err(err),
                        Err(_) => break 'session Err(NdError::Protocol("RTSP receiver timed out".into())),
                    },
                    event = async {
                        match pipeline_events.as_mut() {
                            Some(events) => futures::StreamExt::next(events).await,
                            None => std::future::pending().await,
                        }
                    } => match event {
                        Some(pipeline::PipelineEvent::Eos) => break 'session Ok(()),
                        Some(pipeline::PipelineEvent::Error { message, .. }) => break 'session Err(NdError::Gst(message)),
                        None => pipeline_events = None,
                        _ => {},
                    },
                    _ = tokio::time::sleep_until(negotiation_deadline), if playing.is_none() => {
                        break 'session Err(NdError::Protocol("RTSP negotiation timed out".into()));
                    }
                    _ = keepalive.tick(), if playing.is_some() => {
                        if pending.values().any(|request| matches!(request, Awaiting::Keepalive)) {
                            break 'session Err(NdError::Protocol("RTSP keepalive was not acknowledged".into()));
                        }
                        cseq += 1;
                        let request = format!("GET_PARAMETER {WFD_URL} RTSP/1.0\r\nCSeq: {cseq}\r\nSession: 1\r\n\r\n");
                        if let Err(err) = send(&mut writer, &request).await { break 'session Err(err); }
                        pending.insert(cseq, Awaiting::Keepalive);
                    }
                }
            }
        };

        if msg.is_response() {
            let Some(awaiting) = pending.remove(&msg.cseq()) else {
                tracing::debug!(cseq = msg.cseq(), "response with no matching request");
                continue;
            };

            // A refusal must not advance the negotiation: the session would
            // die later with no diagnosis.
            if !msg.is_success() {
                if awaiting == Awaiting::Keepalive {
                    tracing::warn!(status = ?msg.status(), "keepalive refused");
                    continue;
                }
                break Err(NdError::Protocol(format!(
                    "the sink refused {awaiting:?}: {}",
                    msg.start_line
                )));
            }

            match awaiting {
                Awaiting::Options | Awaiting::Keepalive => {}
                Awaiting::GetParameter => {
                    caps = parse_caps(&msg.body);
                    if caps.rtp_port != 0 {
                        sink_rtp_port = caps.rtp_port;
                    }

                    // Choose the mode from what the sink actually accepts.
                    let preferences = nd_core::settings::current();
                    let limit = StreamConfig::capped_by_preference(cfg.max_resolution);
                    let aac = caps
                        .audio_codecs
                        .as_deref()
                        .unwrap_or("")
                        .split(',')
                        .any(|codec| {
                            let mut fields = codec.split_whitespace();
                            fields.next() == Some("AAC")
                                && fields
                                    .next()
                                    .and_then(|v| u32::from_str_radix(v, 16).ok())
                                    .is_some_and(|bits| bits & 1 != 0)
                        });
                    if !aac {
                        return Err(NdError::Unsupported(
                            "receiver does not support AAC stereo at 48 kHz".into(),
                        ));
                    }
                    let mode = caps
                        .preferred_mode(cfg.source_size, limit, preferences.fps)
                        .ok_or_else(|| {
                            NdError::Protocol(format!(
                                "receiver has no compatible video mode within {}x{} at {} FPS; choose a larger resolution or frame rate limit",
                                limit.0, limit.1, preferences.fps
                            ))
                        })?;
                    tracing::info!(
                        w = mode.width, h = mode.height, fps = mode.fps,
                        table = ?mode.table, "video mode negotiated"
                    );
                    chosen = Some(mode);

                    let url = format!("rtsp://{}:{RTSP_PORT}/wfd1.0/streamid=0", cfg.our_ip);
                    let body = format!(
                        "wfd_video_formats: {}\r\n\
                         wfd_audio_codecs: AAC 00000001 00\r\n\
                         wfd_presentation_URL: {url} none\r\n\
                         wfd_client_rtp_ports: RTP/AVP/UDP;unicast {} 0 mode=play\r\n\
                         wfd_content_protection: none\r\n",
                        m4_video_formats(&mode, caps.profile, caps.level),
                        sink_rtp_port,
                    );
                    cseq += 1;
                    send(&mut writer, &set_parameter(cseq, &body)).await?;
                    pending.insert(cseq, Awaiting::SetFormat);
                }
                Awaiting::SetFormat => {
                    cseq += 1;
                    send(
                        &mut writer,
                        &set_parameter(cseq, "wfd_trigger_method: SETUP\r\n"),
                    )
                    .await?;
                    pending.insert(cseq, Awaiting::TriggerSetup);
                }
                Awaiting::TriggerSetup => {
                    tracing::debug!("waiting for the sink's SETUP");
                }
            }
            continue;
        }

        // A request coming from the sink.
        match msg.method() {
            Some("OPTIONS") => {
                send(
                    &mut writer,
                    &format!(
                        "RTSP/1.0 200 OK\r\nCSeq: {}\r\nPublic: {WFD_METHODS}\r\n\r\n",
                        msg.cseq_str()
                    ),
                )
                .await?;
                // The sink's M2 → we send M3, exactly once.
                if !pending.values().any(|a| *a == Awaiting::GetParameter) && caps.modes.is_empty()
                {
                    cseq += 1;
                    send(&mut writer, &m3_request(cseq)).await?;
                    pending.insert(cseq, Awaiting::GetParameter);
                }
            }
            Some("SETUP") => {
                if let Some(port) = msg
                    .headers
                    .get("transport")
                    .and_then(|t| parse_client_port(t))
                {
                    sink_rtp_port = port;
                }
                if sink_rtp_port == 0 || sink_rtp_port == u16::MAX {
                    break Err(NdError::Protocol(
                        "the sink reported no RTP port, neither in M3 nor in SETUP".into(),
                    ));
                }
                let local = pipeline::LOCAL_RTP_PORT;
                let transport = format!(
                    "RTP/AVP/UDP;unicast;client_port={sink_rtp_port}-{};server_port={local}-{};mode=play",
                    sink_rtp_port + 1,
                    local + 1
                );
                send(
                    &mut writer,
                    &format!(
                        "RTSP/1.0 200 OK\r\nCSeq: {}\r\nSession: 1;timeout=60\r\nTransport: {transport}\r\n\r\n",
                        msg.cseq_str()
                    ),
                )
                .await?;
            }
            Some("PLAY") => {
                send(
                    &mut writer,
                    &format!(
                        "RTSP/1.0 200 OK\r\nCSeq: {}\r\nSession: 1\r\n\r\n",
                        msg.cseq_str()
                    ),
                )
                .await?;

                // Validate before bringing the pipeline up: `udpsink port=0`
                // was accepted silently and the cast simply never appeared.
                if sink_rtp_port == 0 || sink_rtp_port == u16::MAX {
                    break Err(NdError::Protocol(
                        "PLAY received with no negotiated RTP port".into(),
                    ));
                }
                let mode = chosen.ok_or_else(|| {
                    NdError::Protocol("PLAY received before the format was negotiated".into())
                })?;

                tracing::info!(sink = %cfg.sink_ip, port = sink_rtp_port, "PLAY — starting the stream");
                let monitored = start_pipeline(&cfg, &mode, sink_rtp_port, status).await?;
                // A guard rather than a bare value: ending through an error,
                // a TEARDOWN or the Stop button all have to pass through
                // `NULL` just the same.
                if let Some(control) = &cfg.media_control {
                    control.attach(&monitored.pipeline);
                }
                playing = Some(pipeline::PipelineGuard::new(monitored.pipeline));
                pipeline_events = Some(monitored.events);
            }
            Some("TEARDOWN") => {
                send(
                    &mut writer,
                    &format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\n\r\n", msg.cseq_str()),
                )
                .await?;
                break Ok(());
            }
            // The sink's keepalive and any other method: 200 OK.
            _ => {
                send(
                    &mut writer,
                    &format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\n\r\n", msg.cseq_str()),
                )
                .await?;
            }
        }
    };

    drop(playing);
    // If the pipeline reported an error, it is more informative than the protocol's.
    if let Some(mut events) = pipeline_events
        && let Ok(nd_core::pipeline::PipelineEvent::Error { message, .. }) = events.try_recv()
    {
        return Err(NdError::Gst(message));
    }

    result
}

fn set_parameter(cseq: u32, body: &str) -> String {
    format!(
        "SET_PARAMETER {WFD_URL} RTSP/1.0\r\nCSeq: {cseq}\r\n\
         Content-Type: text/parameters\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// Extracts the RTP port from SETUP's `Transport` header.
pub fn parse_client_port(transport: &str) -> Option<u16> {
    transport
        .split(';')
        .find_map(|p| p.trim().strip_prefix("client_port="))
        .and_then(|range| range.split('-').next())
        .and_then(|p| p.parse::<u16>().ok())
        .filter(|port| *port != 0 && *port != u16::MAX)
}

/// Brings the streaming pipeline up, choosing the encoder from evidence.
///
/// Tries the candidates in order, hardware before software, and accepts the
/// first one that actually produces frames within [`ENCODER_PROBE`]. A static
/// driver blocklist would age: a driver that hangs while encoding in one
/// release works in the next.
async fn start_pipeline(
    cfg: &WfdCastConfig,
    mode: &WfdMode,
    sink_rtp_port: u16,
    status: &SinkStatus,
) -> Result<pipeline::MonitoredPipeline> {
    let transport = WfdTransport::new(cfg.sink_ip, sink_rtp_port);
    let driver = nd_net::detect_gpu_driver();

    let candidates = pipeline::encoder_candidates(driver, pipeline::Acceleration::preferred());
    if candidates.is_empty() {
        return Err(NdError::Unsupported(
            "no H.264 encoder available — install gst-plugins-ugly (x264) \
             or gst-plugins-bad (openh264/va)"
                .into(),
        ));
    }
    let mut failure = None;

    for encoder in candidates {
        // Encoding must match the mode already advertised in M4.
        let (width, height) = (mode.width, mode.height);
        let stream_cfg = StreamConfig {
            width,
            height,
            fps: mode.fps,
            encoder,
            audio: cfg.audio,
            ..Default::default()
        };
        status.set_link(StreamLink {
            width: stream_cfg.width,
            height: stream_cfg.height,
            fps: stream_cfg.fps,
            // No endpoint: in Miracast we are the RTSP server and the sink
            // listens on nothing we may rely on, so there is no address to
            // measure a round trip against. The sink's IP on our own port
            // would read as "the receiver is not answering" all through a
            // healthy session.
            endpoint: None,
            receivers: None,
        });

        let desc = pipeline::wfd_pipeline_description(&stream_cfg, &cfg.video, &transport);

        // `BIGNETSCREEN_PIPELINE_LATENCY_MS` overrides it, for field measurement.
        let latency = match std::env::var("BIGNETSCREEN_PIPELINE_LATENCY_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            Some(ms) => ms,
            None => stream_cfg.latency_ms(),
        };

        let monitored = match pipeline::build_monitored(&desc, latency, encoder) {
            Ok(monitored) => monitored,
            Err(err) => {
                tracing::warn!(?encoder, %err, "could not assemble the pipeline");
                failure = Some(err);
                continue;
            }
        };

        let startup_guard = pipeline::PipelineGuard::new(monitored.pipeline.clone());
        if let Err(err) = monitored.pipeline.set_state(gst::State::Playing) {
            tracing::warn!(?encoder, %err, "the encoder refused to start running");
            failure = Some(NdError::Gst(err.to_string()));
            monitored.shutdown();
            continue;
        }

        tokio::time::sleep(ENCODER_PROBE).await;
        let frames = monitored.frames_encoded();
        if frames > 0 {
            tracing::info!(?encoder, frames, "encoder in use");
            let _ = pipeline::query_min_latency_ms(&monitored.pipeline);
            startup_guard.disarm();
            return Ok(monitored);
        }

        tracing::warn!(
            ?encoder,
            "the encoder produced no frames within {:?}; trying the next one",
            ENCODER_PROBE
        );
        failure = Some(NdError::Gst(format!(
            "the {encoder:?} encoder produced no frames"
        )));
        monitored.shutdown();
    }

    Err(failure.unwrap_or_else(|| NdError::Gst("no usable encoder".into())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_client_port_and_rejects_unusable_ones() {
        assert_eq!(
            parse_client_port("RTP/AVP/UDP;unicast;client_port=19000-19001"),
            Some(19000)
        );
        assert_eq!(
            parse_client_port("RTP/AVP/UDP;unicast;client_port=0-1"),
            None
        );
        assert_eq!(parse_client_port("RTP/AVP;client_port=65535"), None);
        assert_eq!(parse_client_port("RTP/AVP/UDP;unicast"), None);
    }
}
