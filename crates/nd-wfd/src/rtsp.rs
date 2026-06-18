//! Source RTSP/WFD (Miracast) escrito à mão — porta de `bkp/src/wfd`.
//!
//! Após o grupo Wi-Fi Direct formar, o **sink** (TV/projetor) conecta no nosso
//! servidor RTSP (porta 7236). Nós, no papel de **source**, dirigimos a
//! negociação WFD numa única conexão TCP onde os dois lados trocam
//! requests/responses:
//!
//! - **M1** nós→sink: `OPTIONS` (Require: org.wfa.wfd1.0)
//! - **M2** sink→nós: `OPTIONS` → respondemos com os métodos suportados
//! - **M3** nós→sink: `GET_PARAMETER` pedindo `wfd_video_formats`,
//!   `wfd_audio_codecs`, `wfd_client_rtp_ports` → o sink responde com as
//!   capacidades dele
//! - **M4** nós→sink: `SET_PARAMETER` com o formato/URL escolhidos *(próximo)*
//! - **M5** nós→sink: `SET_PARAMETER wfd_trigger_method: SETUP` *(próximo)*
//! - **M6/M7** sink→nós: `SETUP`/`PLAY` → começa o streaming *(próximo)*
//!
//! Este módulo cobre, por ora, M1–M3 (capacidades). M4–M7 + pipeline vêm a
//! seguir.

use std::collections::HashMap;
use std::net::IpAddr;

use gstreamer as gst;
use gst::prelude::*;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;

use nd_core::{NdError, Result};

const WFD_URL: &str = "rtsp://localhost/wfd1.0";
const WFD_METHODS: &str =
    "org.wfa.wfd1.0, OPTIONS, GET_PARAMETER, SET_PARAMETER, SETUP, PLAY, TEARDOWN";

fn proto_err<E: std::fmt::Display>(e: E) -> NdError {
    NdError::Protocol(e.to_string())
}

/// Capacidades anunciadas pelo sink (resposta da M3).
#[derive(Debug, Default, Clone)]
pub struct SinkCaps {
    /// Valor cru de `wfd_video_formats` (descritor de resolução/codec H.264).
    pub video_formats: Option<String>,
    /// Valor cru de `wfd_audio_codecs`.
    pub audio_codecs: Option<String>,
    /// Porta RTP onde o sink quer receber o stream (de `wfd_client_rtp_ports`).
    pub rtp_port: u16,
}

/// Uma mensagem RTSP (request ou response).
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
    fn cseq(&self) -> &str {
        self.headers.get("cseq").map(String::as_str).unwrap_or("0")
    }
}

async fn read_message<R>(reader: &mut R) -> Result<RtspMessage>
where
    R: AsyncBufReadExt + Unpin,
{
    let mut start_line = String::new();
    if reader.read_line(&mut start_line).await.map_err(proto_err)? == 0 {
        return Err(NdError::Protocol("conexão RTSP fechada pelo sink".into()));
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
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).await.map_err(proto_err)?;
        body = String::from_utf8_lossy(&buf).to_string();
    }

    Ok(RtspMessage {
        start_line,
        headers,
        body,
    })
}

async fn send(writer: &mut OwnedWriteHalf, msg: &str) -> Result<()> {
    writer.write_all(msg.as_bytes()).await.map_err(proto_err)?;
    writer.flush().await.map_err(proto_err)?;
    Ok(())
}

/// Faz o handshake WFD M1–M3 e devolve as capacidades anunciadas pelo sink.
pub async fn negotiate_caps(stream: TcpStream) -> Result<SinkCaps> {
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut cseq = 0u32;

    // M1: nós perguntamos as OPTIONS do sink.
    cseq += 1;
    send(
        &mut writer,
        &format!("OPTIONS * RTSP/1.0\r\nCSeq: {cseq}\r\nRequire: org.wfa.wfd1.0\r\n\r\n"),
    )
    .await?;
    tracing::debug!("WFD M1 OPTIONS enviado");

    let mut sent_m3 = false;
    loop {
        let msg = read_message(&mut reader).await?;

        if msg.is_response() {
            // Resposta a um request nosso (M1 ou M3).
            if sent_m3 && !msg.body.is_empty() {
                tracing::debug!("WFD M3 resposta recebida (capacidades)");
                return Ok(parse_caps(&msg.body));
            }
            // Senão é a resposta da M1; seguimos aguardando a M2 do sink.
            continue;
        }

        // Request vindo do sink.
        match msg.method() {
            Some("OPTIONS") => {
                // M2: respondemos com os métodos suportados…
                send(
                    &mut writer,
                    &format!(
                        "RTSP/1.0 200 OK\r\nCSeq: {}\r\nPublic: {WFD_METHODS}\r\n\r\n",
                        msg.cseq()
                    ),
                )
                .await?;
                tracing::debug!("WFD M2 OPTIONS respondido");

                // …e então mandamos a M3 perguntando as capacidades.
                if !sent_m3 {
                    cseq += 1;
                    let query =
                        "wfd_video_formats\r\nwfd_audio_codecs\r\nwfd_client_rtp_ports\r\n";
                    send(
                        &mut writer,
                        &format!(
                            "GET_PARAMETER {WFD_URL} RTSP/1.0\r\nCSeq: {cseq}\r\n\
                             Content-Type: text/parameters\r\nContent-Length: {}\r\n\r\n{query}",
                            query.len()
                        ),
                    )
                    .await?;
                    sent_m3 = true;
                    tracing::debug!("WFD M3 GET_PARAMETER enviado");
                }
            }
            other => {
                // Outros requests do sink durante o handshake: 200 OK genérico.
                tracing::debug!(method = ?other, "request inesperado do sink; respondendo 200");
                send(
                    &mut writer,
                    &format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\n\r\n", msg.cseq()),
                )
                .await?;
            }
        }
    }
}

/// Parseia o corpo `text/parameters` da resposta M3.
fn parse_caps(body: &str) -> SinkCaps {
    let mut caps = SinkCaps::default();
    for line in body.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "wfd_video_formats" => caps.video_formats = Some(value.to_string()),
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

// ---------------------------------------------------------------------------
// Sessão completa M1–M7 + streaming
// ---------------------------------------------------------------------------

const RTSP_PORT: u16 = 7236;

/// Descritor `wfd_video_formats` para M4 — 1920x1080@30, H.264 CBP, nível 4.2.
/// CEA bit 7 (0x80) = 1920x1080@30. (Ver bkp/src/wfd/wfd-video-codec.c.)
const M4_VIDEO_1080P30: &str =
    "00 00 01 10 00000080 00000000 00000000 00 0000 0000 00 none none";

/// Conduz a sessão WFD inteira (M1–M7) e, no PLAY, inicia o streaming
/// MPEG-TS/RTP para o sink. Bloqueia tratando keepalive até TEARDOWN/desconexão.
///
/// `our_ip` = nosso IP no link P2P (presentation URL); `sink_ip` = IP do sink.
pub async fn cast_to_sink(stream: TcpStream, our_ip: IpAddr, sink_ip: IpAddr) -> Result<()> {
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut cseq = 0u32;

    // M1
    cseq += 1;
    send(
        &mut writer,
        &format!("OPTIONS * RTSP/1.0\r\nCSeq: {cseq}\r\nRequire: org.wfa.wfd1.0\r\n\r\n"),
    )
    .await?;

    let mut stage = Stage::M1;
    let mut sink_rtp_port = 0u16;
    let mut pipeline: Option<gst::Element> = None;

    loop {
        let msg = match read_message(&mut reader).await {
            Ok(m) => m,
            Err(err) => {
                tracing::info!(%err, "conexão RTSP encerrada");
                break;
            }
        };

        if msg.is_response() {
            match stage {
                // Resposta da M3 traz as capacidades → enviamos M4.
                Stage::M3Sent if !msg.body.is_empty() => {
                    let caps = parse_caps(&msg.body);
                    sink_rtp_port = caps.rtp_port;
                    tracing::info!(?caps, "capacidades do sink; enviando M4");

                    let url = format!("rtsp://{our_ip}:{RTSP_PORT}/wfd1.0/streamid=0");
                    let body = format!(
                        "wfd_video_formats: {M4_VIDEO_1080P30}\r\n\
                         wfd_audio_codecs: AAC 00000001 00\r\n\
                         wfd_presentation_URL: {url} none\r\n\
                         wfd_client_rtp_ports: RTP/AVP/UDP;unicast {} 0 mode=play\r\n",
                        caps.rtp_port,
                    );
                    send_set_parameter(&mut writer, cseq + 1, &body).await?;
                    cseq += 1;
                    stage = Stage::M4Sent;
                }
                // Resposta da M4 → enviamos M5 (trigger SETUP).
                Stage::M4Sent => {
                    send_set_parameter(&mut writer, cseq + 1, "wfd_trigger_method: SETUP\r\n")
                        .await?;
                    cseq += 1;
                    stage = Stage::M5Sent;
                }
                Stage::M5Sent => stage = Stage::WaitSetup,
                _ => {}
            }
            continue;
        }

        // Request vindo do sink.
        match msg.method() {
            Some("OPTIONS") => {
                send(
                    &mut writer,
                    &format!(
                        "RTSP/1.0 200 OK\r\nCSeq: {}\r\nPublic: {WFD_METHODS}\r\n\r\n",
                        msg.cseq()
                    ),
                )
                .await?;
                // M2 do sink → mandamos a M3.
                if matches!(stage, Stage::M1) {
                    let query = "wfd_video_formats\r\nwfd_audio_codecs\r\nwfd_client_rtp_ports\r\n";
                    send(
                        &mut writer,
                        &format!(
                            "GET_PARAMETER {WFD_URL} RTSP/1.0\r\nCSeq: {}\r\n\
                             Content-Type: text/parameters\r\nContent-Length: {}\r\n\r\n{query}",
                            cseq + 1,
                            query.len()
                        ),
                    )
                    .await?;
                    cseq += 1;
                    stage = Stage::M3Sent;
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
                let transport = format!(
                    "RTP/AVP/UDP;unicast;client_port={sink_rtp_port}-{};server_port=16384-16385;mode=play",
                    sink_rtp_port + 1
                );
                send(
                    &mut writer,
                    &format!(
                        "RTSP/1.0 200 OK\r\nCSeq: {}\r\nSession: 1;timeout=60\r\nTransport: {transport}\r\n\r\n",
                        msg.cseq()
                    ),
                )
                .await?;
            }
            Some("PLAY") => {
                send(
                    &mut writer,
                    &format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\nSession: 1\r\n\r\n", msg.cseq()),
                )
                .await?;
                tracing::info!(%sink_ip, port = sink_rtp_port, "PLAY — iniciando streaming");
                pipeline = Some(start_pipeline(sink_ip, sink_rtp_port)?);
            }
            Some("TEARDOWN") => {
                send(
                    &mut writer,
                    &format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\n\r\n", msg.cseq()),
                )
                .await?;
                break;
            }
            // Keepalive e demais: 200 OK.
            _ => {
                send(
                    &mut writer,
                    &format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\n\r\n", msg.cseq()),
                )
                .await?;
            }
        }
    }

    if let Some(p) = pipeline {
        let _ = p.set_state(gst::State::Null);
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    M1,
    M3Sent,
    M4Sent,
    M5Sent,
    WaitSetup,
}

async fn send_set_parameter(writer: &mut OwnedWriteHalf, cseq: u32, body: &str) -> Result<()> {
    send(
        writer,
        &format!(
            "SET_PARAMETER {WFD_URL} RTSP/1.0\r\nCSeq: {cseq}\r\n\
             Content-Type: text/parameters\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
    .await
}

fn parse_client_port(transport: &str) -> Option<u16> {
    transport
        .split(';')
        .find_map(|p| p.trim().strip_prefix("client_port="))
        .and_then(|range| range.split('-').next())
        .and_then(|p| p.parse::<u16>().ok())
}

/// Pipeline de teste: padrão de barras 1080p30 → H.264 CBP → MPEG-TS → RTP →
/// udpsink para o sink. (Depois trocaremos `videotestsrc` pela captura real.)
fn start_pipeline(sink_ip: IpAddr, port: u16) -> Result<gst::Element> {
    nd_core::pipeline::init()?;
    // Vídeo (H.264 CBP) + áudio (AAC silencioso) → MPEG-TS → RTP → udpsink.
    // O áudio é obrigatório: o sink WFD declara A/V e descarta a sessão se o
    // programa vier incompleto (foi o que derrubou o cast só-vídeo).
    // RTP via rtpbin (perfil AVP), como o C: gera RTCP Sender Reports com o
    // mapeamento timestamp↔relógio que o sink usa para sincronizar e exibir.
    let server_rtp = 16384u16;
    let server_rtcp = server_rtp + 1;
    let rtcp_port = port + 1;
    let desc = format!(
        "rtpbin name=rtpbin rtp-profile=avp \
         videotestsrc is-live=true ! \
         video/x-raw,width=1920,height=1080,framerate=30/1 ! videoconvert ! \
         x264enc tune=zerolatency speed-preset=ultrafast key-int-max=30 bitrate=8000 ! \
         video/x-h264,profile=constrained-baseline ! h264parse config-interval=-1 ! \
         mpegtsmux name=mux alignment=7 ! rtpmp2tpay ! rtpbin.send_rtp_sink_0 \
         rtpbin.send_rtp_src_0 ! \
         udpsink host={sink_ip} port={port} bind-port={server_rtp} sync=true async=false \
         rtpbin.send_rtcp_src_0 ! \
         udpsink host={sink_ip} port={rtcp_port} bind-port={server_rtcp} sync=false async=false \
         audiotestsrc is-live=true wave=silence ! audioconvert ! audioresample ! \
         audio/x-raw,rate=48000,channels=2 ! avenc_aac ! aacparse ! mux."
    );
    tracing::info!("pipeline: {desc}");
    let pipeline = gst::parse::launch(&desc).map_err(proto_err)?;

    // Loga erros do pipeline (antes não víamos se ele falhava internamente).
    if let Some(bus) = pipeline.bus() {
        bus.set_sync_handler(|_, msg| {
            if let gst::MessageView::Error(err) = msg.view() {
                tracing::error!("pipeline: {} ({:?})", err.error(), err.debug());
            }
            gst::BusSyncReply::Drop
        });
    }

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| NdError::Gst(e.to_string()))?;
    Ok(pipeline)
}
