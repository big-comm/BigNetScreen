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
//! ## Fixes over the earlier version
//!
//! - the video format was **hardcoded to 1080p30**, ignoring
//!   `wfd_video_formats`: any sink that did not support that mode (720p
//!   projectors, cheap dongles) simply failed. The mode is now chosen from the
//!   intersection;
//! - responses were matched by **position**, and the status code was ignored:
//!   a `400 Bad Request` advanced the state machine as though it had
//!   succeeded. There is now `CSeq` correlation and a `2xx` check;
//! - there was no timeout at all (a mute sink hung the session forever) and no
//!   keepalive **sent** by us — the specification requires the *source* to
//!   send M16, and without it several sinks drop the session after ~60 s.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

use gst::prelude::*;
use gstreamer as gst;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;

use nd_core::pipeline::{self, StreamConfig, VideoSource, WfdTransport};
use nd_core::{NdError, Result};

/// The WFD RTSP server's port.
pub const RTSP_PORT: u16 = 7236;

const WFD_URL: &str = "rtsp://localhost/wfd1.0";
const WFD_METHODS: &str =
    "org.wfa.wfd1.0, OPTIONS, GET_PARAMETER, SET_PARAMETER, SETUP, PLAY, TEARDOWN";

/// Cap on an RTSP message's body.
///
/// `Content-Length` comes off the network; with no cap, `vec![0u8; len]` let a
/// peer exhaust the process's memory from a single header.
const MAX_BODY_BYTES: usize = 64 * 1024;

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

fn proto_err<E: std::fmt::Display>(e: E) -> NdError {
    NdError::Protocol(e.to_string())
}

// ---------------------------------------------------------------------------
// WFD video formats
// ---------------------------------------------------------------------------

/// A WFD resolution table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolutionTable {
    Cea = 0,
    Vesa = 1,
    Handheld = 2,
}

/// A video mode the sink supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WfdMode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub interlaced: bool,
    pub table: ResolutionTable,
    /// The bit's index within the table.
    pub index: u8,
}

impl WfdMode {
    fn pixels(&self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

/// The CEA table (bit → mode). Source: Wi-Fi Display 1.1 spec, table 5-9.
const CEA_MODES: &[(u32, u32, u32, bool)] = &[
    (640, 480, 60, false),
    (720, 480, 60, false),
    (720, 480, 60, true),
    (720, 576, 50, false),
    (720, 576, 50, true),
    (1280, 720, 30, false),
    (1280, 720, 60, false),
    (1920, 1080, 30, false),
    (1920, 1080, 60, false),
    (1920, 1080, 60, true),
    (1280, 720, 25, false),
    (1280, 720, 50, false),
    (1920, 1080, 25, false),
    (1920, 1080, 50, false),
    (1920, 1080, 50, true),
    (1280, 720, 24, false),
    (1920, 1080, 24, false),
];

/// The VESA table (bit → mode).
const VESA_MODES: &[(u32, u32, u32, bool)] = &[
    (800, 600, 30, false),
    (800, 600, 60, false),
    (1024, 768, 30, false),
    (1024, 768, 60, false),
    (1152, 864, 30, false),
    (1152, 864, 60, false),
    (1280, 768, 30, false),
    (1280, 768, 60, false),
    (1280, 800, 30, false),
    (1280, 800, 60, false),
    (1360, 768, 30, false),
    (1360, 768, 60, false),
    (1366, 768, 30, false),
    (1366, 768, 60, false),
    (1280, 1024, 30, false),
    (1280, 1024, 60, false),
    (1400, 1050, 30, false),
    (1400, 1050, 60, false),
    (1440, 900, 30, false),
    (1440, 900, 60, false),
    (1600, 900, 30, false),
    (1600, 900, 60, false),
    (1600, 1200, 30, false),
    (1600, 1200, 60, false),
    (1680, 1024, 30, false),
    (1680, 1024, 60, false),
    (1680, 1050, 30, false),
    (1680, 1050, 60, false),
    (1920, 1200, 30, false),
    (1920, 1200, 60, false),
];

/// The Handheld table (bit → mode).
const HH_MODES: &[(u32, u32, u32, bool)] = &[
    (800, 480, 30, false),
    (800, 480, 60, false),
    (854, 480, 30, false),
    (854, 480, 60, false),
    (864, 480, 30, false),
    (864, 480, 60, false),
    (640, 360, 30, false),
    (640, 360, 60, false),
    (960, 540, 30, false),
    (960, 540, 60, false),
    (848, 480, 30, false),
    (848, 480, 60, false),
];

fn decode_table(
    bitmap: u32,
    table: ResolutionTable,
    modes: &[(u32, u32, u32, bool)],
) -> Vec<WfdMode> {
    modes
        .iter()
        .enumerate()
        .filter(|(index, _)| bitmap & (1u32 << index) != 0)
        .map(|(index, (width, height, fps, interlaced))| WfdMode {
            width: *width,
            height: *height,
            fps: *fps,
            interlaced: *interlaced,
            table,
            index: index as u8,
        })
        .collect()
}

/// The capabilities the sink advertises (M3's response).
#[derive(Debug, Default, Clone)]
pub struct SinkCaps {
    /// Valor cru de `wfd_video_formats`.
    pub video_formats: Option<String>,
    /// Video modes decoded from the CEA/VESA/HH tables.
    pub modes: Vec<WfdMode>,
    /// Perfil H.264 aceito (bitmask; 0x01 = Constrained Baseline).
    pub profile: u8,
    /// The accepted H.264 level (a bitmask).
    pub level: u8,
    /// Valor cru de `wfd_audio_codecs`.
    pub audio_codecs: Option<String>,
    /// The RTP port where the sink wants to receive the stream.
    pub rtp_port: u16,
    /// The **native** mode the sink declares (its actual panel).
    ///
    /// The first field of `wfd_video_formats` exists precisely for this: the
    /// index in the high 5 bits, the table in the low 3. Ignoring it led to
    /// picking 1680x1050 (VESA) for a Full HD projector merely because the
    /// source screen is 16:10 — and the projector then rescaled everything.
    pub native: Option<WfdMode>,
}

impl SinkCaps {
    /// Picks the best common mode: largest area, then highest frame rate.
    ///
    /// Interlaced modes are discarded — the pipeline produces progressive, and
    /// sending progressive while declaring interlaced confuses the sink.
    pub fn best_mode(&self) -> Option<WfdMode> {
        self.modes
            .iter()
            .copied()
            .filter(|m| !m.interlaced)
            .max_by_key(|m| (m.pixels(), m.fps))
    }

    /// The best mode that does not exceed a resolution cap.
    pub fn best_mode_within(&self, max_width: u32, max_height: u32) -> Option<WfdMode> {
        self.modes
            .iter()
            .copied()
            .filter(|m| !m.interlaced && m.width <= max_width && m.height <= max_height)
            .max_by_key(|m| (m.pixels(), m.fps))
            .or_else(|| self.best_mode())
    }

    /// The best mode for a specific source screen.
    ///
    /// The criteria, in order:
    /// 1. **the aspect ratio closest to the captured screen** — picking a mode
    ///    with another aspect forces letterboxing or distorts the picture,
    ///    which is worse than losing a few pixels;
    /// 2. the largest area, without exceeding the requested cap;
    /// 3. the highest frame rate.
    ///
    /// Without criterion (1), a sink that advertises the whole VESA table
    /// leads to 1920x1200p30 on a 16:9 screen — more pixels, but with bars and
    /// half the smoothness of 1920x1080p60.
    pub fn best_mode_for(&self, source: (u32, u32), max: (u32, u32)) -> Option<WfdMode> {
        // The sink's native mode beats everything when it fits the cap: it is
        // the panel's real resolution, and sending anything else only makes
        // the device rescale (losing sharpness) or refuse.
        if let Some(native) = self.native {
            if !native.interlaced
                && native.width <= max.0
                && native.height <= max.1
                && self.modes.contains(&native)
            {
                return Some(native);
            }
        }

        let (src_w, src_h) = source;
        if src_w == 0 || src_h == 0 {
            return self.best_mode_within(max.0, max.1);
        }

        let candidates: Vec<WfdMode> = self
            .modes
            .iter()
            .copied()
            .filter(|m| !m.interlaced && m.width <= max.0 && m.height <= max.1)
            .collect();
        let pool = if candidates.is_empty() {
            self.modes
                .iter()
                .copied()
                .filter(|m| !m.interlaced)
                .collect()
        } else {
            candidates
        };

        pool.into_iter().min_by_key(|m| {
            // The aspect difference in thousandths, using integer arithmetic.
            let cross = (m.width as i64 * src_h as i64) - (src_w as i64 * m.height as i64);
            let aspect_diff = cross.abs() * 1000 / (src_w as i64 * src_h as i64);
            (
                aspect_diff,
                std::cmp::Reverse(m.pixels()),
                std::cmp::Reverse(m.fps),
            )
        })
    }
}

/// Parses the `wfd_video_formats` value the sink advertises.
///
/// Formato: `<native> <pref-display-mode> <profile> <level> <CEA> <VESA> <HH>
/// <latency> <min-slice> <slice-enc> <frame-rate-control> [<h-res> <v-res>]`
pub fn parse_video_formats(value: &str) -> SinkCaps {
    let mut caps = SinkCaps {
        video_formats: Some(value.to_string()),
        ..Default::default()
    };

    let fields: Vec<&str> = value.split_whitespace().collect();
    if fields.len() < 7 {
        tracing::warn!(%value, "wfd_video_formats with too few fields");
        return caps;
    }

    caps.profile = u8::from_str_radix(fields[2], 16).unwrap_or(0x01);
    caps.level = u8::from_str_radix(fields[3], 16).unwrap_or(0x02);

    if let Ok(native) = u8::from_str_radix(fields[0], 16) {
        caps.native = mode_from_native(native);
    }

    let cea = u32::from_str_radix(fields[4], 16).unwrap_or(0);
    let vesa = u32::from_str_radix(fields[5], 16).unwrap_or(0);
    let hh = u32::from_str_radix(fields[6], 16).unwrap_or(0);

    caps.modes
        .extend(decode_table(cea, ResolutionTable::Cea, CEA_MODES));
    caps.modes
        .extend(decode_table(vesa, ResolutionTable::Vesa, VESA_MODES));
    caps.modes
        .extend(decode_table(hh, ResolutionTable::Handheld, HH_MODES));

    caps
}

/// Decodes the `native` field: index in the high 5 bits, table in the low 3.
pub fn mode_from_native(native: u8) -> Option<WfdMode> {
    let index = (native >> 3) as usize;
    let (table, modes) = match native & 0b111 {
        0 => (ResolutionTable::Cea, CEA_MODES),
        1 => (ResolutionTable::Vesa, VESA_MODES),
        2 => (ResolutionTable::Handheld, HH_MODES),
        _ => return None,
    };
    let (width, height, fps, interlaced) = *modes.get(index)?;
    Some(WfdMode {
        width,
        height,
        fps,
        interlaced,
        table,
        index: index as u8,
    })
}

/// Builds M4's `wfd_video_formats` with **a single selected mode**.
pub fn m4_video_formats(mode: &WfdMode, profile: u8, level: u8) -> String {
    let bit = 1u32 << mode.index;
    let (cea, vesa, hh) = match mode.table {
        ResolutionTable::Cea => (bit, 0, 0),
        ResolutionTable::Vesa => (0, bit, 0),
        ResolutionTable::Handheld => (0, 0, bit),
    };
    // `native` = the mode's index (5 bits) + the table (3 bits).
    let native = (mode.index << 3) | (mode.table as u8);
    // We only advertise Constrained Baseline, which is what the pipeline produces.
    let profile = if profile & 0x01 != 0 { 0x01 } else { profile };
    format!(
        "{native:02x} 00 {profile:02x} {level:02x} \
         {cea:08X} {vesa:08X} {hh:08X} 00 0000 0000 00 none none"
    )
}

/// An RTSP message (request or response).
#[derive(Debug)]
struct RtspMessage {
    start_line: String,
    headers: HashMap<String, String>,
    body: String,
}

impl RtspMessage {
    fn is_response(&self) -> bool {
        self.start_line.starts_with("RTSP/")
    }

    fn method(&self) -> Option<&str> {
        self.start_line.split_whitespace().next()
    }

    /// A response's status code (`RTSP/1.0 200 OK` → 200).
    fn status(&self) -> Option<u16> {
        self.start_line.split_whitespace().nth(1)?.parse().ok()
    }

    fn is_success(&self) -> bool {
        matches!(self.status(), Some(code) if (200..300).contains(&code))
    }

    fn cseq(&self) -> u32 {
        self.headers
            .get("cseq")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    }

    fn cseq_str(&self) -> &str {
        self.headers.get("cseq").map(String::as_str).unwrap_or("0")
    }
}

async fn read_message<R>(reader: &mut R) -> Result<RtspMessage>
where
    R: AsyncBufReadExt + Unpin,
{
    let mut start_line = String::new();
    if reader.read_line(&mut start_line).await.map_err(proto_err)? == 0 {
        return Err(NdError::Protocol(
            "RTSP connection closed by the sink".into(),
        ));
    }
    let start_line = start_line.trim_end().to_string();

    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.map_err(proto_err)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let mut body = String::new();
    if let Some(len) = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
    {
        if len > MAX_BODY_BYTES {
            return Err(NdError::Protocol(format!(
                "corpo RTSP de {len} bytes excede o limite de {MAX_BODY_BYTES}"
            )));
        }
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).await.map_err(proto_err)?;
        body = String::from_utf8_lossy(&buf).to_string();
    }

    let msg = RtspMessage {
        start_line,
        headers,
        body,
    };
    tracing::trace!(
        "\n<<< RECEIVED <<<\n{}\n{}\n{}",
        msg.start_line,
        msg.headers
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n"),
        msg.body.trim_end(),
    );
    Ok(msg)
}

async fn send(writer: &mut OwnedWriteHalf, msg: &str) -> Result<()> {
    // The raw RTSP dialogue is the only way to debug interoperability with a
    // real sink: enable it with `RUST_LOG=nd_wfd::rtsp=trace`.
    tracing::trace!("\n>>> ENVIADO >>>\n{}", msg.trim_end());
    writer.write_all(msg.as_bytes()).await.map_err(proto_err)?;
    writer.flush().await.map_err(proto_err)?;
    Ok(())
}

/// Parseia o corpo `text/parameters` da resposta M3.
pub fn parse_caps(body: &str) -> SinkCaps {
    let mut caps = SinkCaps::default();
    for line in body.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "wfd_video_formats" => {
                let parsed = parse_video_formats(value);
                caps.video_formats = parsed.video_formats;
                caps.modes = parsed.modes;
                caps.profile = parsed.profile;
                caps.level = parsed.level;
                caps.native = parsed.native;
            }
            "wfd_audio_codecs" => caps.audio_codecs = Some(value.to_string()),
            "wfd_client_rtp_ports" => {
                // ex.: "RTP/AVP/UDP;unicast 19000 0 mode=play"
                if let Some(port) = value
                    .split_whitespace()
                    .nth(1)
                    .and_then(|p| p.parse::<u16>().ok())
                {
                    caps.rtp_port = port;
                }
            }
            _ => {}
        }
    }
    caps
}

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
    tracing::debug!("WFD M1 OPTIONS enviado");

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
                        "o sink recusou a M3 ({})",
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
                    tracing::debug!("WFD M3 GET_PARAMETER enviado");
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

// ---------------------------------------------------------------------------
// The full M1–M7 session plus streaming
// ---------------------------------------------------------------------------

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
    /// Nosso IP no link P2P (vai na `wfd_presentation_URL`).
    pub our_ip: IpAddr,
    /// IP do sink (destino do RTP).
    pub sink_ip: IpAddr,
    /// The video source (a real capture or a test pattern).
    pub video: VideoSource,
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
            our_ip,
            sink_ip,
            video,
            encoder,
            source_size: (1920, 1080),
            max_resolution: (1920, 1080),
        }
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
pub async fn cast_to_sink(stream: TcpStream, cfg: WfdCastConfig) -> Result<()> {
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
    keepalive.tick().await; // consome o tick imediato

    let result = loop {
        let msg = tokio::select! {
            // The M16 keepalive: it is the *source* that has to send it.
            // Without it the sink drops the session after the timeout we
            // announced in SETUP.
            _ = keepalive.tick(), if playing.is_some() => {
                cseq += 1;
                let request = format!(
                    "GET_PARAMETER {WFD_URL} RTSP/1.0\r\nCSeq: {cseq}\r\nSession: 1\r\n\r\n"
                );
                if let Err(err) = send(&mut writer, &request).await {
                    break Err(err);
                }
                pending.insert(cseq, Awaiting::Keepalive);
                tracing::debug!(cseq, "keepalive M16 enviado");
                continue;
            }
            incoming = tokio::time::timeout(IDLE_TIMEOUT, read_message(&mut reader)) => {
                match incoming {
                    Ok(Ok(msg)) => msg,
                    Ok(Err(err)) => {
                        tracing::info!(%err, "RTSP connection closed");
                        break Ok(());
                    }
                    Err(_) => {
                        break Err(NdError::Protocol(format!(
                            "o sink parou de responder por {}s",
                            IDLE_TIMEOUT.as_secs()
                        )));
                    }
                }
            }
        };

        if msg.is_response() {
            let Some(awaiting) = pending.remove(&msg.cseq()) else {
                tracing::debug!(cseq = msg.cseq(), "response with no matching request");
                continue;
            };

            // Check the status: previously a 4xx advanced the state machine as
            // though it had worked, and the session died later with no
            // diagnosis.
            if !msg.is_success() {
                if awaiting == Awaiting::Keepalive {
                    tracing::warn!(status = ?msg.status(), "keepalive recusado");
                    continue;
                }
                break Err(NdError::Protocol(format!(
                    "o sink recusou {awaiting:?}: {}",
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
                    let mode = caps
                        .best_mode_for(cfg.source_size, cfg.max_resolution)
                        .ok_or_else(|| {
                            NdError::Protocol(format!(
                                "the sink advertised no usable video mode \
                                 (wfd_video_formats: {:?})",
                                caps.video_formats
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
                if sink_rtp_port == 0 {
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
                if sink_rtp_port == 0 {
                    break Err(NdError::Protocol(
                        "PLAY received with no negotiated RTP port".into(),
                    ));
                }
                let mode = chosen.ok_or_else(|| {
                    NdError::Protocol("PLAY received before the format was negotiated".into())
                })?;

                tracing::info!(sink = %cfg.sink_ip, port = sink_rtp_port, "PLAY — starting the stream");
                let monitored = start_pipeline(&cfg, &mode, sink_rtp_port).await?;
                // A guard rather than a bare value: ending through an error,
                // a TEARDOWN or the Stop button all have to pass through
                // `NULL` just the same.
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
    if let Some(mut events) = pipeline_events {
        if let Ok(nd_core::pipeline::PipelineEvent::Error { message, .. }) = events.try_recv() {
            return Err(NdError::Gst(message));
        }
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
        .filter(|port| *port != 0)
}

/// Brings the streaming pipeline up, choosing the encoder from evidence.
///
/// This function used to assemble a pipeline of its own, with a hardcoded
/// `videotestsrc` and bitrate, ignoring `nd_core::pipeline` entirely — so none
/// of the core's latency decisions ever reached a real cast.
///
/// It now tries the candidates in order (hardware before software) and only
/// accepts one that **actually produces frames**. That replaces the driver
/// blacklist: Intel's `xe` hung during encoding in 2022 and works fine today —
/// a static list would be wrong in both directions.
async fn start_pipeline(
    cfg: &WfdCastConfig,
    mode: &WfdMode,
    sink_rtp_port: u16,
) -> Result<pipeline::MonitoredPipeline> {
    let transport = WfdTransport::new(cfg.sink_ip, sink_rtp_port);
    let driver = nd_net::detect_gpu_driver();

    let candidates = pipeline::encoder_candidates(driver);
    if candidates.is_empty() {
        return Err(NdError::Unsupported(
            "no H.264 encoder available — install gst-plugins-ugly (x264) \
             ou gst-plugins-bad (openh264/va)"
                .into(),
        ));
    }
    let last = candidates.len() - 1;
    let mut failure = None;

    // Try hardware first and fall back to software **on evidence**: an encoder
    // that produces no frames is discarded, with no reliance on a driver
    // blacklist (which ages and never covers every machine in the world).
    for (index, encoder) in candidates.into_iter().enumerate() {
        let stream_cfg = StreamConfig {
            width: mode.width,
            height: mode.height,
            fps: mode.fps,
            encoder,
            audio: nd_core::pipeline::AudioSource::detect(),
            ..Default::default()
        };
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

        if let Err(err) = monitored.pipeline.set_state(gst::State::Playing) {
            tracing::warn!(?encoder, %err, "the encoder refused to start running");
            failure = Some(NdError::Gst(err.to_string()));
            monitored.shutdown();
            continue;
        }

        // The last candidate is accepted without proof: there is nowhere left
        // to fall back to, and spending the verification time would only delay
        // the picture.
        if index == last {
            tracing::info!(?encoder, "encoder in use");
            let _ = pipeline::query_min_latency_ms(&monitored.pipeline);
            return Ok(monitored);
        }

        tokio::time::sleep(ENCODER_PROBE).await;
        let frames = monitored.frames_encoded();
        if frames > 0 {
            tracing::info!(?encoder, frames, "encoder in use");
            let _ = pipeline::query_min_latency_ms(&monitored.pipeline);
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

    /// A **real** M3 response, captured from a Samsung Projector LSP3 during
    /// field testing (2026-08-07). Worth more than any invented fixture: it was
    /// against this device that the interoperability bugs showed up.
    ///
    /// `native = 0x40` → index 8 of the CEA table = 1920x1080p60 (its panel).
    const M3_BODY: &str = "wfd_audio_codecs: LPCM 00000003 00, AAC 00000001 00\r\n\
         wfd_video_formats: 40 00 01 10 000001e3 0f3fffff 00000fff 00 0000 00c8 01 none none\r\n\
         wfd_client_rtp_ports: RTP/AVP/UDP;unicast 19002 0 mode=play\r\n";

    #[test]
    fn parses_a_real_m3_response() {
        let caps = parse_caps(M3_BODY);
        assert_eq!(caps.rtp_port, 19002);
        assert!(caps.audio_codecs.unwrap().contains("AAC"));
        assert_eq!(caps.profile, 0x01, "Constrained Baseline");
        assert_eq!(caps.level, 0x10, "level 4.2");
        assert!(!caps.modes.is_empty());
    }

    #[test]
    fn native_mode_survives_parse_caps() {
        // A regression: `parse_caps` copied modes/profile/level from the parse
        // but forgot `native`, so the "respect the sink's panel" rule never
        // fired and a Full HD projector got 1680x1050.
        let caps = parse_caps(M3_BODY);
        let native = caps.native.expect("the sink declared a native mode");
        assert_eq!((native.width, native.height, native.fps), (1920, 1080, 60));
        assert_eq!(native.table, ResolutionTable::Cea);
    }

    #[test]
    fn native_mode_wins_over_source_aspect() {
        // On a 16:10 source screen, the projector's panel mode (16:9) must
        // win: sending 1680x1050 makes the device rescale or refuse.
        let caps = parse_caps(M3_BODY);
        let mode = caps
            .best_mode_for((1920, 1200), (1920, 1080))
            .expect("some mode");
        assert_eq!((mode.width, mode.height, mode.fps), (1920, 1080, 60));
    }

    #[test]
    fn native_is_ignored_when_it_exceeds_the_ceiling() {
        let caps = parse_caps(M3_BODY);
        let mode = caps
            .best_mode_for((1920, 1080), (1280, 720))
            .expect("some mode");
        assert!(mode.width <= 1280 && mode.height <= 720, "{mode:?}");
    }

    #[test]
    fn decodes_the_native_field() {
        // 0x40 = index 8, table 0 (CEA) = 1920x1080p60.
        let mode = mode_from_native(0x40).expect("a valid mode");
        assert_eq!((mode.width, mode.height, mode.fps), (1920, 1080, 60));
        // 0x49 = index 9, table 1 (VESA) = 1280x800p60.
        let mode = mode_from_native(0x49).expect("a valid mode");
        assert_eq!((mode.width, mode.height), (1280, 800));
        assert_eq!(mode.table, ResolutionTable::Vesa);
        // A table that does not exist.
        assert!(mode_from_native(0x07).is_none());
    }

    #[test]
    fn picks_the_widest_mode_when_aspect_is_ignored() {
        let caps = parse_caps(M3_BODY);
        let mode = caps.best_mode().expect("some mode");
        // On area alone, this sink reaches 1920x1080p60.
        assert_eq!((mode.width, mode.height, mode.fps), (1920, 1080, 60));
        assert!(!mode.interlaced);
    }

    #[test]
    fn matching_the_source_aspect_beats_raw_pixel_count() {
        // Without `native`, between 1600x900p60 and 1680x1050p60 on a 16:9
        // source, the 16:9 one wins even with fewer pixels.
        let mut caps = parse_caps(M3_BODY);
        caps.native = None;
        let mode = caps
            .best_mode_for((1920, 1080), (1700, 1080))
            .expect("some mode");
        assert_eq!(
            (mode.width, mode.height),
            (1600, 900),
            "deveria preferir 16:9 a 1680x1050"
        );
    }

    #[test]
    fn aspect_rule_applies_when_the_sink_declares_no_native() {
        // Without `native`, the source-aspect rule applies. A 16:10 source
        // should get the closest 16:10 mode (1680x1050 from the VESA table).
        let mut caps = parse_caps(M3_BODY);
        caps.native = None;
        let mode = caps
            .best_mode_for((1920, 1200), (1920, 1200))
            .expect("some mode");
        assert_eq!((mode.width, mode.height), (1680, 1050));
    }

    #[test]
    fn source_size_zero_falls_back_gracefully() {
        let caps = parse_caps(M3_BODY);
        assert!(caps.best_mode_for((0, 0), (1920, 1080)).is_some());
    }

    #[test]
    fn respects_the_resolution_ceiling() {
        let caps = parse_caps(M3_BODY);
        let mode = caps.best_mode_within(1280, 720).expect("some mode");
        assert_eq!((mode.width, mode.height, mode.fps), (1280, 720, 60));
    }

    #[test]
    fn a_720p_only_sink_is_not_offered_1080p() {
        // A regression: the old code sent a hardcoded 1080p30 and 720p sinks
        // refused the whole session.
        // CEA bit 5 = 1280x720p30, bit 6 = 1280x720p60.
        let body = "wfd_video_formats: 00 00 01 01 00000060 00000000 00000000 00 0000 0000 00 none none\r\n\
                    wfd_client_rtp_ports: RTP/AVP/UDP;unicast 19000 0 mode=play\r\n";
        let caps = parse_caps(body);
        let mode = caps.best_mode().expect("some mode");
        assert_eq!((mode.width, mode.height, mode.fps), (1280, 720, 60));
    }

    #[test]
    fn handheld_only_sink_is_supported() {
        // A cheap dongle advertising only the HH table: bit 9 = 960x540p60.
        let body =
            "wfd_video_formats: 00 00 01 01 00000000 00000000 00000200 00 0000 0000 00 none none";
        let caps = parse_caps(body);
        let mode = caps.best_mode().expect("some mode");
        assert_eq!((mode.width, mode.height, mode.fps), (960, 540, 60));
        assert_eq!(mode.table, ResolutionTable::Handheld);
    }

    #[test]
    fn interlaced_modes_are_never_selected() {
        // CEA bit 2 = 720x480i60 and bit 9 = 1920x1080i60, both interlaced.
        let body =
            "wfd_video_formats: 00 00 01 01 00000204 00000000 00000000 00 0000 0000 00 none none";
        let caps = parse_caps(body);
        assert!(caps.best_mode().is_none(), "there is no progressive mode");
    }

    #[test]
    fn m4_encodes_exactly_one_mode() {
        let mode = WfdMode {
            width: 1280,
            height: 720,
            fps: 60,
            interlaced: false,
            table: ResolutionTable::Cea,
            index: 6,
        };
        let value = m4_video_formats(&mode, 0x01, 0x02);
        let fields: Vec<&str> = value.split_whitespace().collect();
        let cea = u32::from_str_radix(fields[4], 16).unwrap();
        assert_eq!(cea, 1 << 6, "only the chosen mode's bit");
        assert_eq!(u32::from_str_radix(fields[5], 16).unwrap(), 0, "VESA vazio");
        assert_eq!(u32::from_str_radix(fields[6], 16).unwrap(), 0, "HH vazio");
        // `native` = index<<3 | table.
        assert_eq!(u8::from_str_radix(fields[0], 16).unwrap(), 6 << 3);
    }

    #[test]
    fn m4_marks_the_right_table_for_vesa() {
        let mode = WfdMode {
            width: 1280,
            height: 800,
            fps: 60,
            interlaced: false,
            table: ResolutionTable::Vesa,
            index: 9,
        };
        let value = m4_video_formats(&mode, 0x01, 0x02);
        let fields: Vec<&str> = value.split_whitespace().collect();
        assert_eq!(u32::from_str_radix(fields[4], 16).unwrap(), 0);
        assert_eq!(u32::from_str_radix(fields[5], 16).unwrap(), 1 << 9);
        assert_eq!(u8::from_str_radix(fields[0], 16).unwrap(), (9 << 3) | 1);
    }

    #[test]
    fn parses_client_port_and_rejects_zero() {
        assert_eq!(
            parse_client_port("RTP/AVP/UDP;unicast;client_port=19000-19001"),
            Some(19000)
        );
        assert_eq!(
            parse_client_port("RTP/AVP/UDP;unicast;client_port=0-1"),
            None
        );
        assert_eq!(parse_client_port("RTP/AVP/UDP;unicast"), None);
    }

    #[test]
    fn malformed_video_formats_do_not_panic() {
        for value in ["", "lixo", "00 00", "zz zz zz zz zz zz zz"] {
            let caps = parse_video_formats(value);
            assert!(caps.modes.is_empty() || !caps.modes.is_empty());
        }
    }

    // --- parsing de mensagens RTSP ------------------------------------------

    async fn parse(raw: &str) -> Result<RtspMessage> {
        let mut reader = BufReader::new(raw.as_bytes());
        read_message(&mut reader).await
    }

    #[tokio::test]
    async fn reads_a_request_with_body() {
        let msg = parse(
            "SET_PARAMETER rtsp://x/wfd1.0 RTSP/1.0\r\nCSeq: 4\r\nContent-Length: 11\r\n\r\nhello world",
        )
        .await
        .unwrap();
        assert!(!msg.is_response());
        assert_eq!(msg.method(), Some("SET_PARAMETER"));
        assert_eq!(msg.cseq(), 4);
        assert_eq!(msg.body, "hello world");
    }

    #[tokio::test]
    async fn recognises_error_responses() {
        // A regression: the status was ignored and a 400 advanced the machine.
        let msg = parse("RTSP/1.0 400 Bad Request\r\nCSeq: 3\r\n\r\n")
            .await
            .unwrap();
        assert!(msg.is_response());
        assert_eq!(msg.status(), Some(400));
        assert!(!msg.is_success());

        let ok = parse("RTSP/1.0 200 OK\r\nCSeq: 3\r\n\r\n").await.unwrap();
        assert!(ok.is_success());
    }

    #[tokio::test]
    async fn oversized_body_is_rejected() {
        // A security regression: Content-Length came off the network uncapped.
        let raw = "PLAY x RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 999999999\r\n\r\n";
        let err = parse(raw).await.unwrap_err();
        assert!(err.to_string().contains("excede o limite"), "{err}");
    }

    #[tokio::test]
    async fn headers_are_case_insensitive() {
        let msg = parse("PLAY x RTSP/1.0\r\ncSeQ: 7\r\nTRANSPORT: a;client_port=5000-5001\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(msg.cseq(), 7);
        assert_eq!(
            msg.headers.get("transport").map(String::as_str),
            Some("a;client_port=5000-5001")
        );
    }

    #[tokio::test]
    async fn closed_connection_is_an_error_not_a_hang() {
        assert!(parse("").await.is_err());
    }
}
