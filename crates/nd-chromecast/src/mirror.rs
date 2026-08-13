//! Cast Streaming — the Chromecast **mirroring** path.
//!
//! Unlike the Default Media Receiver (`CC1AD845` + HTTP), which is a file
//! player and pre-buffers for seconds, here we talk to the mirroring app built
//! into the device:
//!
//! ```text
//!   LAUNCH 0F5096E8 ("Chrome Mirroring")
//!        ↓
//!   OFFER  (urn:x-cast:com.google.cast.webrtc) — codecs, SSRCs, AES keys
//!        ↓
//!   ANSWER — the receiver's UDP port and its SSRCs
//!        ↓
//!   RTP/UDP with an AES-CTR-128 encrypted payload
//! ```
//!
//! There is no container, no HTTP server and no media player on the other end
//! — and therefore none of the pre-buffering that costs seconds.
//!
//! The protocol is not proprietary: it is implemented in the
//! [Open Screen Library](https://chromium.googlesource.com/openscreen/+/HEAD/cast/streaming/),
//! Google's own open source, which is the reference used here.

use std::io::Read;
use std::net::IpAddr;
use std::time::Duration;

use serde_json::{json, Value};

use nd_core::{NdError, Result};

use crate::cast::{CastChannel, LaunchedApp};

/// App de espelhamento embutido nos aparelhos Cast ("Chrome Mirroring").
pub const MIRRORING_APP_ID: &str = "0F5096E8";
/// Namespace of the OFFER/ANSWER negotiation.
pub const NS_WEBRTC: &str = "urn:x-cast:com.google.cast.webrtc";

/// Tipo de payload RTP do H.264 no Cast (`RtpPayloadType::kVideoH264`).
pub const RTP_PAYLOAD_H264: u8 = 101;
/// Tipo de payload RTP do Opus (`RtpPayloadType::kAudioOpus`).
pub const RTP_PAYLOAD_OPUS: u8 = 96;

/// The video time base: 90 kHz, like all video RTP.
pub const VIDEO_TIME_BASE: u32 = 90_000;
/// The Opus audio time base.
pub const AUDIO_TIME_BASE: u32 = 48_000;

/// How long to wait for the receiver's ANSWER.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(15);

/// The playout delay requested from the receiver, in milliseconds.
///
/// This is the jitter buffer on the other end — the floor of end-to-end
/// latency on this path. Chrome uses ~150 ms for screen mirroring; below that
/// the receiver starts stuttering on any network variation. Adjustable because
/// the sweet spot depends on the network.
pub const DEFAULT_TARGET_DELAY_MS: u32 = 150;

/// The playout delay to request, given the latency profile.
///
/// This is the Cast equivalent of the Miracast jitter buffer: the receiver
/// holds frames for this long before showing them, which is what smooths out
/// uneven arrival — and what delays the pointer by the same amount.
pub fn target_delay_ms() -> u32 {
    if nd_core::latency::is_film() {
        nd_core::latency::FILM_PLAYOUT_DELAY_MS
    } else {
        DEFAULT_TARGET_DELAY_MS
    }
}

/// A stream's keys (the receiver decrypts with what we sent in the OFFER).
#[derive(Clone)]
pub struct StreamKeys {
    pub key: [u8; 16],
    pub iv_mask: [u8; 16],
}

impl StreamKeys {
    /// Generates a random key and mask.
    pub fn random() -> Result<Self> {
        Ok(Self {
            key: random_bytes()?,
            iv_mask: random_bytes()?,
        })
    }

    fn key_hex(&self) -> String {
        hex(&self.key)
    }

    fn iv_hex(&self) -> String {
        hex(&self.iv_mask)
    }
}

impl std::fmt::Debug for StreamKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Do not leak key material into logs.
        f.write_str("StreamKeys(<oculto>)")
    }
}

fn random_bytes() -> Result<[u8; 16]> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| NdError::Network(format!("could not generate a key: {e}")))?;
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

/// The parameters of what we are going to offer the receiver.
#[derive(Clone, Debug)]
pub struct MirrorConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub max_bitrate: u32,
    /// The requested playout delay, in ms.
    pub target_delay_ms: u32,
    /// Include the audio track in the offer.
    pub with_audio: bool,
}

impl Default for MirrorConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 30,
            max_bitrate: 10_000_000,
            target_delay_ms: target_delay_ms(),
            with_audio: true,
        }
    }
}

/// One offered stream.
#[derive(Clone, Debug)]
pub struct OfferedStream {
    pub index: u32,
    pub ssrc: u32,
    pub payload_type: u8,
    pub keys: StreamKeys,
    pub is_video: bool,
}

/// The offer we sent, kept so it can be matched against the ANSWER.
#[derive(Clone, Debug)]
pub struct Offer {
    pub seq_num: i64,
    pub streams: Vec<OfferedStream>,
}

/// What the receiver answered.
#[derive(Clone, Debug)]
pub struct Answer {
    /// The UDP port where it expects the RTP.
    pub udp_port: u16,
    /// Indices of the streams it accepted.
    pub send_indexes: Vec<u32>,
    /// **Its** SSRCs (one per accepted stream), used by RTCP.
    pub ssrcs: Vec<u32>,
}

/// A negotiated session, ready to receive packets.
#[derive(Clone, Debug)]
pub struct Negotiated {
    pub receiver_ip: IpAddr,
    pub answer: Answer,
    pub offer: Offer,
}

impl Negotiated {
    /// The accepted video stream, if any.
    pub fn video(&self) -> Option<&OfferedStream> {
        self.accepted().find(|s| s.is_video)
    }

    /// The accepted audio stream, if any.
    pub fn audio(&self) -> Option<&OfferedStream> {
        self.accepted().find(|s| !s.is_video)
    }

    fn accepted(&self) -> impl Iterator<Item = &OfferedStream> {
        self.offer
            .streams
            .iter()
            .filter(|s| self.answer.send_indexes.contains(&s.index))
    }

    /// Where to send the RTP.
    pub fn target(&self) -> (IpAddr, u16) {
        (self.receiver_ip, self.answer.udp_port)
    }
}

/// Builds the offer's JSON.
///
/// The field names follow what Open Screen serialises; the receiver is picky
/// and silently ignores a malformed offer.
pub fn build_offer(cfg: &MirrorConfig, seq_num: i64) -> Result<(Value, Offer)> {
    let mut streams = Vec::new();
    let mut json_streams = Vec::new();

    let video_keys = StreamKeys::random()?;
    let video_ssrc = 100_001u32;
    json_streams.push(json!({
        "index": 0,
        "type": "video_source",
        "codecName": "h264",
        "rtpProfile": "cast",
        "rtpPayloadType": RTP_PAYLOAD_H264,
        "ssrc": video_ssrc,
        "targetDelay": cfg.target_delay_ms,
        "aesKey": video_keys.key_hex(),
        "aesIvMask": video_keys.iv_hex(),
        "timeBase": format!("1/{VIDEO_TIME_BASE}"),
        "maxFrameRate": format!("{}/1", cfg.fps),
        "maxBitRate": cfg.max_bitrate,
        "receiverRtcpEventLog": true,
        "rtpExtensions": ["adaptive_playout_delay"],
        "resolutions": [{ "width": cfg.width, "height": cfg.height }],
    }));
    streams.push(OfferedStream {
        index: 0,
        ssrc: video_ssrc,
        payload_type: RTP_PAYLOAD_H264,
        keys: video_keys,
        is_video: true,
    });

    if cfg.with_audio {
        let audio_keys = StreamKeys::random()?;
        let audio_ssrc = 100_003u32;
        json_streams.push(json!({
            "index": 1,
            "type": "audio_source",
            "codecName": "opus",
            "rtpProfile": "cast",
            "rtpPayloadType": RTP_PAYLOAD_OPUS,
            "ssrc": audio_ssrc,
            "targetDelay": cfg.target_delay_ms,
            "aesKey": audio_keys.key_hex(),
            "aesIvMask": audio_keys.iv_hex(),
            "timeBase": format!("1/{AUDIO_TIME_BASE}"),
            "receiverRtcpEventLog": true,
            "rtpExtensions": ["adaptive_playout_delay"],
            "bitRate": 128_000,
            "channels": 2,
            "sampleRate": AUDIO_TIME_BASE,
        }));
        streams.push(OfferedStream {
            index: 1,
            ssrc: audio_ssrc,
            payload_type: RTP_PAYLOAD_OPUS,
            keys: audio_keys,
            is_video: false,
        });
    }

    let payload = json!({
        "type": "OFFER",
        "seqNum": seq_num,
        "offer": {
            "castMode": "mirroring",
            "receiverGetStatus": true,
            "supportedStreams": json_streams,
        },
    });

    Ok((payload, Offer { seq_num, streams }))
}

/// Parses the receiver's ANSWER.
pub fn parse_answer(payload: &Value, seq_num: i64) -> Result<Answer> {
    let kind = payload.get("type").and_then(Value::as_str).unwrap_or("");
    if kind != "ANSWER" {
        return Err(NdError::Protocol(format!(
            "esperava ANSWER, veio {kind}: {payload}"
        )));
    }
    if let Some(seq) = payload.get("seqNum").and_then(Value::as_i64) {
        if seq != seq_num {
            return Err(NdError::Protocol(format!(
                "ANSWER from another negotiation (seqNum {seq}, expected {seq_num})"
            )));
        }
    }
    match payload.get("result").and_then(Value::as_str) {
        Some("ok") => {}
        other => {
            // `Unsupported` rather than `Protocol`: this is how the caller
            // tells "this device has no mirroring" from "the session broke".
            return Err(NdError::Unsupported(format!(
                "the receiver refused the offer ({}): {payload}",
                other.unwrap_or("no result")
            )));
        }
    }

    let answer = payload
        .get("answer")
        .ok_or_else(|| NdError::Protocol("ANSWER without the `answer` field".into()))?;

    let udp_port = answer
        .get("udpPort")
        .and_then(Value::as_u64)
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p != 0)
        .ok_or_else(|| NdError::Protocol(format!("ANSWER with no valid UDP port: {answer}")))?;

    let send_indexes = answer
        .get("sendIndexes")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_u64)
                .map(|v| v as u32)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if send_indexes.is_empty() {
        return Err(NdError::Protocol(
            "the receiver accepted none of the offered streams".into(),
        ));
    }

    let ssrcs = answer
        .get("ssrcs")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_u64)
                .map(|v| v as u32)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(Answer {
        udp_port,
        send_indexes,
        ssrcs,
    })
}

/// Runs the full negotiation against an already connected receiver.
pub async fn negotiate(
    channel: &CastChannel,
    app: &LaunchedApp,
    receiver_ip: IpAddr,
    cfg: &MirrorConfig,
) -> Result<Negotiated> {
    let seq_num = 1;
    let (payload, offer) = build_offer(cfg, seq_num)?;
    tracing::debug!(%payload, "sending the mirroring OFFER");

    channel
        .send_json(NS_WEBRTC, &app.transport_id, &payload)
        .await?;

    // The ANSWER carries no `requestId`, so it arrives as a spontaneous event
    // rather than through the normal request correlation.
    let deadline = tokio::time::Instant::now() + ANSWER_TIMEOUT;
    loop {
        let event = tokio::time::timeout_at(deadline, channel.next_event())
            .await
            .map_err(|_| {
                NdError::Unsupported(format!(
                    "the receiver did not answer the offer within {}s",
                    ANSWER_TIMEOUT.as_secs()
                ))
            })?
            .ok_or_else(|| NdError::Protocol("the channel closed during the negotiation".into()))?;

        if event.namespace != NS_WEBRTC {
            continue;
        }
        let kind = event.payload.get("type").and_then(Value::as_str);
        match kind {
            Some("ANSWER") => {
                let answer = parse_answer(&event.payload, seq_num)?;
                tracing::info!(
                    porta = answer.udp_port,
                    aceitas = ?answer.send_indexes,
                    "espelhamento negociado"
                );
                return Ok(Negotiated {
                    receiver_ip,
                    answer,
                    offer,
                });
            }
            // The receiver reports problems here instead of closing the connection.
            Some("ANSWER_ERROR") | Some("ERROR") | Some("INVALID") => {
                return Err(NdError::Unsupported(format!(
                    "the receiver refused the mirroring offer: {}",
                    event.payload
                )));
            }
            other => {
                tracing::debug!(?other, payload = %event.payload, "mensagem do namespace webrtc");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_has_the_fields_the_receiver_requires() {
        let (payload, offer) = build_offer(&MirrorConfig::default(), 7).unwrap();
        assert_eq!(payload["type"], "OFFER");
        assert_eq!(payload["seqNum"], 7);
        assert_eq!(payload["offer"]["castMode"], "mirroring");

        let streams = payload["offer"]["supportedStreams"].as_array().unwrap();
        assert_eq!(streams.len(), 2, "video + audio");

        let video = &streams[0];
        assert_eq!(video["type"], "video_source");
        assert_eq!(video["codecName"], "h264");
        assert_eq!(video["rtpProfile"], "cast");
        assert_eq!(video["rtpPayloadType"], RTP_PAYLOAD_H264);
        assert_eq!(video["timeBase"], "1/90000");

        let audio = &streams[1];
        assert_eq!(audio["type"], "audio_source");
        assert_eq!(audio["codecName"], "opus");
        assert_eq!(audio["timeBase"], "1/48000");

        assert_eq!(offer.streams.len(), 2);
        assert!(offer.streams[0].is_video);
        assert!(!offer.streams[1].is_video);
    }

    #[test]
    fn offer_can_be_video_only() {
        let cfg = MirrorConfig {
            with_audio: false,
            ..Default::default()
        };
        let (payload, offer) = build_offer(&cfg, 1).unwrap();
        assert_eq!(
            payload["offer"]["supportedStreams"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(offer.streams.len(), 1);
    }

    #[test]
    fn aes_material_is_hex_and_unique_per_stream() {
        let (payload, _) = build_offer(&MirrorConfig::default(), 1).unwrap();
        let streams = payload["offer"]["supportedStreams"].as_array().unwrap();
        for stream in streams {
            for field in ["aesKey", "aesIvMask"] {
                let value = stream[field].as_str().unwrap();
                assert_eq!(value.len(), 32, "{field} must be 16 bytes in hex");
                assert!(value.chars().all(|c| c.is_ascii_hexdigit()), "{value}");
            }
        }
        // Video and audio must not share a key.
        assert_ne!(streams[0]["aesKey"], streams[1]["aesKey"]);
    }

    #[test]
    fn keys_are_not_leaked_in_debug_output() {
        let keys = StreamKeys::random().unwrap();
        let printed = format!("{keys:?}");
        assert!(
            !printed.contains(&hex(&keys.key)),
            "vazou a chave: {printed}"
        );
    }

    /// A real ANSWER, in the format the receiver returns.
    fn answer_json(seq: i64) -> Value {
        json!({
            "type": "ANSWER",
            "seqNum": seq,
            "result": "ok",
            "answer": {
                "castMode": "mirroring",
                "udpPort": 51810,
                "sendIndexes": [0, 1],
                "ssrcs": [100002, 100004],
                "receiverGetStatus": true,
            }
        })
    }

    #[test]
    fn parses_a_well_formed_answer() {
        let answer = parse_answer(&answer_json(1), 1).unwrap();
        assert_eq!(answer.udp_port, 51810);
        assert_eq!(answer.send_indexes, vec![0, 1]);
        assert_eq!(answer.ssrcs, vec![100_002, 100_004]);
    }

    #[test]
    fn rejects_an_answer_from_another_negotiation() {
        let err = parse_answer(&answer_json(9), 1).unwrap_err();
        assert!(err.to_string().contains("another negotiation"), "{err}");
    }

    #[test]
    fn rejects_a_refused_offer() {
        let payload = json!({"type": "ANSWER", "seqNum": 1, "result": "error"});
        assert!(parse_answer(&payload, 1).is_err());
    }

    #[test]
    fn rejects_an_answer_without_a_usable_port() {
        let payload = json!({
            "type": "ANSWER", "seqNum": 1, "result": "ok",
            "answer": {"udpPort": 0, "sendIndexes": [0]}
        });
        let err = parse_answer(&payload, 1).unwrap_err();
        assert!(err.to_string().contains("no valid UDP port"), "{err}");
    }

    #[test]
    fn rejects_an_answer_that_accepts_nothing() {
        let payload = json!({
            "type": "ANSWER", "seqNum": 1, "result": "ok",
            "answer": {"udpPort": 51810, "sendIndexes": []}
        });
        let err = parse_answer(&payload, 1).unwrap_err();
        assert!(
            err.to_string().contains("none of the offered streams"),
            "{err}"
        );
    }

    #[test]
    fn negotiated_exposes_only_accepted_streams() {
        let (_, offer) = build_offer(&MirrorConfig::default(), 1).unwrap();
        let negotiated = Negotiated {
            receiver_ip: "192.168.0.2".parse().unwrap(),
            // The receiver accepted video only.
            answer: Answer {
                udp_port: 51810,
                send_indexes: vec![0],
                ssrcs: vec![100_002],
            },
            offer,
        };
        assert!(negotiated.video().is_some());
        assert!(negotiated.audio().is_none(), "audio was not accepted");
        assert_eq!(negotiated.target().1, 51810);
    }
}
