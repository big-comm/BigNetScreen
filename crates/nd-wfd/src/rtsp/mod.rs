//! A hand-written RTSP/WFD (Miracast) source.
//!
//! Once the Wi-Fi Direct group forms, the sink (TV/projector) connects to
//! our RTSP server (port 7236). We, in the source role, drive the WFD
//! negotiation over a single TCP connection where both sides exchange
//! requests and responses:
//!
//! - M1 us→sink: `OPTIONS` (Require: org.wfa.wfd1.0)
//! - M2 sink→us: `OPTIONS` → we answer with the methods we support
//! - M3 us→sink: `GET_PARAMETER` asking for the capabilities
//! - M4 us→sink: `SET_PARAMETER` with the format chosen from the sink's
//!   capabilities and the presentation URL
//! - M5 us→sink: `SET_PARAMETER wfd_trigger_method: SETUP`
//! - M6/M7 sink→us: `SETUP`/`PLAY` → streaming begins
//! - M16 us→sink: an empty periodic `GET_PARAMETER` (keepalive)
//!
//! Capabilities and the mode chosen from them live in `capabilities`; the
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
use tokio::net::tcp::OwnedWriteHalf;

use nd_core::pipeline::{self, StreamConfig, VideoSource, WfdTransport};
use nd_core::sink::{SinkStatus, StreamLink};
use nd_core::{NdError, Result};

use message::{RtspMessage, read_message, send};

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

/// What we are waiting for on each `CSeq` we sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Awaiting {
    Options,
    GetParameter,
    SetFormat,
    TriggerSetup,
    Keepalive,
}

/// What a request from the sink leads to, once answered.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    Continue,
    /// PLAY: stream `mode` to the sink's RTP port.
    Stream(WfdMode, u16),
    /// TEARDOWN.
    End,
}

/// The source side of one RTSP connection: what we asked and what was agreed.
///
/// Protocol state only. The caller owns the socket, the timers and the
/// pipeline: every message it reads goes through [`Self::on_response`] or
/// [`Self::on_request`], and whatever those queue in `outbox` is sent, in
/// order, before the next read.
#[derive(Default)]
struct Negotiation {
    cseq: u32,
    pending: HashMap<u32, Awaiting>,
    outbox: Vec<String>,
    caps_requested: bool,
    mode: Option<WfdMode>,
    sink_rtp_port: u16,
    streaming: bool,
}

impl Negotiation {
    fn request(&mut self, awaiting: Awaiting, message: impl FnOnce(u32) -> String) {
        self.cseq += 1;
        self.pending.insert(self.cseq, awaiting);
        self.outbox.push(message(self.cseq));
    }

    /// M1, which opens the negotiation.
    fn begin(&mut self) {
        self.request(Awaiting::Options, |cseq| {
            format!("OPTIONS * RTSP/1.0\r\nCSeq: {cseq}\r\nRequire: org.wfa.wfd1.0\r\n\r\n")
        });
    }

    /// M16. Fails while the previous one is unanswered: the sink is gone.
    fn keepalive(&mut self) -> Result<()> {
        if self.pending.values().any(|a| *a == Awaiting::Keepalive) {
            return Err(NdError::Protocol(
                "RTSP keepalive was not acknowledged".into(),
            ));
        }
        self.request(Awaiting::Keepalive, |cseq| {
            format!("GET_PARAMETER {WFD_URL} RTSP/1.0\r\nCSeq: {cseq}\r\nSession: 1\r\n\r\n")
        });
        Ok(())
    }

    /// The request `response` answers, or `None` when it answers nothing we
    /// are waiting for. A refusal ends the session, except for a keepalive.
    fn answered(&mut self, response: &RtspMessage) -> Result<Option<Awaiting>> {
        let Some(awaiting) = self.pending.remove(&response.cseq()) else {
            tracing::debug!(cseq = response.cseq(), "response with no matching request");
            return Ok(None);
        };
        if response.is_success() {
            return Ok(Some(awaiting));
        }
        if awaiting == Awaiting::Keepalive {
            tracing::warn!(status = ?response.status(), "keepalive refused");
            return Ok(None);
        }
        Err(NdError::Protocol(format!(
            "the sink refused {awaiting:?}: {}",
            response.start_line
        )))
    }

    fn on_response(&mut self, response: &RtspMessage, cfg: &WfdCastConfig) -> Result<()> {
        match self.answered(response)? {
            Some(Awaiting::GetParameter) => {
                let format = self.choose_format(&parse_caps(&response.body), cfg)?;
                self.request(Awaiting::SetFormat, |cseq| set_parameter(cseq, &format));
            }
            Some(Awaiting::SetFormat) => self.request(Awaiting::TriggerSetup, |cseq| {
                set_parameter(cseq, "wfd_trigger_method: SETUP\r\n")
            }),
            Some(Awaiting::TriggerSetup) => tracing::debug!("waiting for the sink's SETUP"),
            Some(Awaiting::Options | Awaiting::Keepalive) | None => {}
        }
        Ok(())
    }

    /// Picks the mode from what the sink accepts and returns M4's body.
    fn choose_format(&mut self, caps: &SinkCaps, cfg: &WfdCastConfig) -> Result<String> {
        if caps.rtp_port != 0 {
            self.sink_rtp_port = caps.rtp_port;
        }
        // The pipeline sends AAC stereo at 48 kHz, bit 0 of the AAC modes.
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
        let fps = nd_core::settings::current().fps;
        let limit = StreamConfig::capped_by_preference(cfg.max_resolution);
        let mode = caps
            .preferred_mode(cfg.source_size, limit, fps)
            .ok_or_else(|| {
                NdError::Protocol(format!(
                    "receiver has no compatible video mode within {}x{} at {} FPS; choose a larger resolution or frame rate limit",
                    limit.0, limit.1, fps
                ))
            })?;
        tracing::info!(
            w = mode.width, h = mode.height, fps = mode.fps,
            table = ?mode.table, "video mode negotiated"
        );
        self.mode = Some(mode);

        let url = format!("rtsp://{}:{RTSP_PORT}/wfd1.0/streamid=0", cfg.our_ip);
        Ok(format!(
            "wfd_video_formats: {}\r\n\
             wfd_audio_codecs: AAC 00000001 00\r\n\
             wfd_presentation_URL: {url} none\r\n\
             wfd_client_rtp_ports: RTP/AVP/UDP;unicast {} 0 mode=play\r\n\
             wfd_content_protection: none\r\n",
            m4_video_formats(&mode, caps.profile, caps.level),
            self.sink_rtp_port,
        ))
    }

    fn on_request(&mut self, request: &RtspMessage) -> Result<Next> {
        let cseq = request.cseq_str();
        match request.method() {
            Some("OPTIONS") => {
                self.outbox.push(format!(
                    "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nPublic: {WFD_METHODS}\r\n\r\n"
                ));
                // That was M2; M3 follows it, once.
                if !std::mem::replace(&mut self.caps_requested, true) {
                    self.request(Awaiting::GetParameter, m3_request);
                }
            }
            Some("SETUP") => {
                if let Some(port) = request
                    .headers
                    .get("transport")
                    .and_then(|t| parse_client_port(t))
                {
                    self.sink_rtp_port = port;
                }
                if self.sink_rtp_port == 0 || self.sink_rtp_port == u16::MAX {
                    return Err(NdError::Protocol(
                        "the sink reported no RTP port, neither in M3 nor in SETUP".into(),
                    ));
                }
                let (sink, local) = (self.sink_rtp_port, pipeline::LOCAL_RTP_PORT);
                self.outbox.push(format!(
                    "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: 1;timeout=60\r\n\
                     Transport: RTP/AVP/UDP;unicast;client_port={sink}-{};server_port={local}-{};mode=play\r\n\r\n",
                    sink + 1,
                    local + 1
                ));
            }
            Some("PLAY") => {
                // `udpsink port=0` is accepted silently, and the picture
                // simply never appears.
                if self.sink_rtp_port == 0 || self.sink_rtp_port == u16::MAX {
                    return Err(NdError::Protocol(
                        "PLAY received with no negotiated RTP port".into(),
                    ));
                }
                let mode = self.mode.ok_or_else(|| {
                    NdError::Protocol("PLAY received before the format was negotiated".into())
                })?;
                self.outbox.push(format!(
                    "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: 1\r\n\r\n"
                ));
                // PAUSE leaves the stream running, so a later PLAY has
                // nothing to start.
                if !std::mem::replace(&mut self.streaming, true) {
                    return Ok(Next::Stream(mode, self.sink_rtp_port));
                }
            }
            Some("TEARDOWN") => {
                self.outbox
                    .push(format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n\r\n"));
                return Ok(Next::End);
            }
            // The sink's keepalive and any other method.
            _ => self
                .outbox
                .push(format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n\r\n")),
        }
        Ok(Next::Continue)
    }
}

/// Sends what the negotiation queued, in order.
async fn flush(writer: &mut OwnedWriteHalf, outbox: &mut Vec<String>) -> Result<()> {
    for message in outbox.drain(..) {
        send(writer, &message).await?;
    }
    Ok(())
}

/// Performs the WFD M1–M3 handshake and returns the sink's advertised
/// capabilities.
///
/// Used by the diagnostic example; the full session is [`cast_to_sink`].
pub async fn negotiate_caps(stream: TcpStream) -> Result<SinkCaps> {
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut session = Negotiation::default();
    session.begin();
    let deadline = tokio::time::Instant::now() + NEGOTIATION_TIMEOUT;
    loop {
        flush(&mut writer, &mut session.outbox).await?;
        let message = tokio::time::timeout_at(deadline, read_message(&mut reader))
            .await
            .map_err(|_| {
                NdError::Protocol("the sink did not finish the negotiation in time".into())
            })??;
        if !message.is_response() {
            session.on_request(&message)?;
        } else if session.answered(&message)? == Some(Awaiting::GetParameter) {
            return Ok(parse_caps(&message.body));
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

fn set_parameter(cseq: u32, body: &str) -> String {
    format!(
        "SET_PARAMETER {WFD_URL} RTSP/1.0\r\nCSeq: {cseq}\r\n\
         Content-Type: text/parameters\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
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
    /// The captured screen's resolution (it sets the target aspect ratio).
    pub source_size: (u32, u32),
    /// A resolution cap (bounds the chosen mode; useful on weak machines).
    pub max_resolution: (u32, u32),
}

impl WfdCastConfig {
    /// The encoder is chosen once PLAY arrives, by trying the candidates
    /// (see `start_pipeline`).
    pub fn new(our_ip: IpAddr, sink_ip: IpAddr, video: VideoSource) -> Self {
        Self {
            media_control: None,
            our_ip,
            sink_ip,
            video,
            audio: nd_core::pipeline::AudioSource::detect(),
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
/// `status` is reported from here because the resolution and frame rate are
/// settled inside this negotiation; the caller only knows what was asked for.
pub async fn cast_to_sink(
    stream: TcpStream,
    cfg: WfdCastConfig,
    status: &SinkStatus,
) -> Result<()> {
    let _ = stream.set_nodelay(true);
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let mut session = Negotiation::default();
    session.begin();
    // Ending through an error, a TEARDOWN or the Stop button all have to take
    // the pipeline through `NULL` just the same.
    let mut playing: Option<pipeline::PipelineGuard> = None;
    let mut pipeline_events: Option<pipeline::PipelineEvents> = None;

    let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive.tick().await; // the first tick completes at once

    let negotiation_deadline = tokio::time::Instant::now() + NEGOTIATION_TIMEOUT;
    let result = 'session: loop {
        if let Err(err) = flush(&mut writer, &mut session.outbox).await {
            break Err(err);
        }
        // One read future for the whole wait: dropping it on a keepalive tick
        // would lose whatever part of a message it had already consumed.
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
                        let sent = match session.keepalive() {
                            Ok(()) => flush(&mut writer, &mut session.outbox).await,
                            Err(err) => Err(err),
                        };
                        if let Err(err) = sent {
                            break 'session Err(err);
                        }
                    }
                }
            }
        };

        let next = if msg.is_response() {
            session.on_response(&msg, &cfg).map(|()| Next::Continue)
        } else {
            session.on_request(&msg)
        };
        // The answer goes out first: TEARDOWN's before closing, PLAY's
        // before the encoder probe delays it.
        let next = match next {
            Ok(next) => flush(&mut writer, &mut session.outbox).await.map(|()| next),
            Err(err) => Err(err),
        };
        match next {
            Ok(Next::Continue) => {}
            Ok(Next::End) => break Ok(()),
            Ok(Next::Stream(mode, port)) => {
                tracing::info!(sink = %cfg.sink_ip, port, "PLAY — starting the stream");
                let monitored = match start_pipeline(&cfg, &mode, port, status).await {
                    Ok(monitored) => monitored,
                    Err(err) => break Err(err),
                };
                if let Some(control) = &cfg.media_control {
                    control.attach(&monitored.pipeline);
                }
                playing = Some(pipeline::PipelineGuard::new(monitored.pipeline));
                pipeline_events = Some(monitored.events);
            }
            Err(err) => break Err(err),
        }
    };

    drop(playing);
    // If the pipeline reported an error, it is more informative than the protocol's.
    if let Some(mut events) = pipeline_events
        && let Ok(pipeline::PipelineEvent::Error { message, .. }) = events.try_recv()
    {
        return Err(NdError::Gst(message));
    }

    result
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
    let driver = nd_core::pipeline::GpuDriver::detect();

    let candidates = pipeline::encoder_candidates(driver, pipeline::Acceleration::preferred());
    if candidates.is_empty() {
        return Err(NdError::Unsupported(
            "no H.264 encoder available — install gst-plugins-ugly (x264) \
             or gst-plugins-bad (openh264/va)"
                .into(),
        ));
    }
    status.set_link(StreamLink {
        width: mode.width,
        height: mode.height,
        fps: mode.fps,
        // No endpoint: in Miracast we are the RTSP server and the sink
        // listens on nothing we may rely on, so there is no address to
        // measure a round trip against. The sink's IP on our own port
        // would read as "the receiver is not answering" all through a
        // healthy session.
        endpoint: None,
        receivers: None,
    });

    let mut failure = None;
    for encoder in candidates {
        // Encoding must match the mode already advertised in M4.
        let stream_cfg = StreamConfig {
            width: mode.width,
            height: mode.height,
            fps: mode.fps,
            encoder,
            audio: cfg.audio,
            ..Default::default()
        };
        let desc = pipeline::wfd_pipeline_description(&stream_cfg, &cfg.video, &transport);
        match probe_encoder(&desc, &stream_cfg).await {
            Ok(monitored) => return Ok(monitored),
            Err(err) => {
                tracing::warn!(?encoder, %err, "encoder unusable; trying the next one");
                failure = Some(err);
            }
        }
    }
    Err(failure.unwrap_or_else(|| NdError::Gst("no usable encoder".into())))
}

/// Starts the pipeline described by `desc` and keeps it only if its encoder
/// produces frames within [`ENCODER_PROBE`].
async fn probe_encoder(
    desc: &str,
    stream_cfg: &StreamConfig,
) -> Result<pipeline::MonitoredPipeline> {
    let encoder = stream_cfg.encoder;
    // `BIGNETSCREEN_PIPELINE_LATENCY_MS` overrides it, for field measurement.
    let latency = std::env::var("BIGNETSCREEN_PIPELINE_LATENCY_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or_else(|| stream_cfg.latency_ms());
    let monitored = pipeline::build_monitored(desc, latency, encoder)?;

    let startup_guard = pipeline::PipelineGuard::new(monitored.pipeline.clone());
    if let Err(err) = monitored.pipeline.set_state(gst::State::Playing) {
        monitored.shutdown();
        return Err(NdError::Gst(format!(
            "the {encoder:?} encoder refused to start: {err}"
        )));
    }
    tokio::time::sleep(ENCODER_PROBE).await;
    let frames = monitored.frames_encoded();
    if frames == 0 {
        monitored.shutdown();
        return Err(NdError::Gst(format!(
            "the {encoder:?} encoder produced no frames"
        )));
    }
    tracing::info!(?encoder, frames, "encoder in use");
    let _ = pipeline::query_min_latency_ms(&monitored.pipeline);
    startup_guard.disarm();
    Ok(monitored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_port_rejects_unusable_ports() {
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

    fn message(start_line: &str, cseq: u32) -> RtspMessage {
        RtspMessage {
            start_line: start_line.into(),
            headers: HashMap::from([("cseq".to_string(), cseq.to_string())]),
            body: String::new(),
        }
    }

    const MODE: WfdMode = WfdMode {
        width: 1280,
        height: 720,
        fps: 60,
        interlaced: false,
        table: ResolutionTable::Cea,
        index: 6,
    };

    #[test]
    fn capabilities_are_asked_once() {
        let mut session = Negotiation::default();
        session.begin();
        assert!(
            session
                .outbox
                .remove(0)
                .starts_with("OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n")
        );
        for cseq in [1, 2] {
            let next = session.on_request(&message("OPTIONS * RTSP/1.0", cseq));
            assert_eq!(next.unwrap(), Next::Continue);
        }
        let sent: Vec<_> = session
            .outbox
            .iter()
            .map(|m| m.lines().next().unwrap())
            .collect();
        assert_eq!(
            sent,
            [
                "RTSP/1.0 200 OK",
                "GET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0",
                "RTSP/1.0 200 OK"
            ]
        );
        assert_eq!(session.pending.get(&2), Some(&Awaiting::GetParameter));
    }

    #[test]
    fn repeated_play_keeps_stream() {
        // A sink that pauses (M9) and plays again (M7) must not get a second
        // pipeline: it would find the local RTP port taken and end the session.
        let mut session = Negotiation {
            mode: Some(MODE),
            sink_rtp_port: 19000,
            ..Default::default()
        };
        let play = message("PLAY rtsp://localhost/wfd1.0 RTSP/1.0", 6);
        assert_eq!(
            session.on_request(&play).unwrap(),
            Next::Stream(MODE, 19000)
        );
        let pause = message("PAUSE rtsp://localhost/wfd1.0 RTSP/1.0", 7);
        assert_eq!(session.on_request(&pause).unwrap(), Next::Continue);
        assert_eq!(session.on_request(&play).unwrap(), Next::Continue);
        assert_eq!(session.outbox.len(), 3, "every request is answered");
    }

    #[test]
    fn unanswered_keepalive_ends_session() {
        let mut session = Negotiation::default();
        session.keepalive().unwrap();
        assert!(session.keepalive().is_err());
        let refused = message("RTSP/1.0 404 Not Found", 1);
        assert_eq!(session.answered(&refused).unwrap(), None);
        session.keepalive().unwrap();
        let refused_setup = {
            session.request(Awaiting::SetFormat, |_| String::new());
            message("RTSP/1.0 400 Bad Request", session.cseq)
        };
        assert!(session.answered(&refused_setup).is_err());
    }
}
