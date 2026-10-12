//! Cast Streaming: the OFFER/ANSWER negotiation of the Chromecast mirroring
//! path. [`crate::mirror_session`] streams what is negotiated here.
//!
//! Unlike the Default Media Receiver (`CC1AD845` + HTTP), which is a file
//! player and pre-buffers for seconds, here we talk to the mirroring app built
//! into the device:
//!
//! ```text
//!   LAUNCH 0F5096E8 ("Chrome Mirroring")
//!        ↓
//!   OFFER  (urn:x-cast:com.google.cast.webrtc): codecs, SSRCs, AES keys
//!        ↓
//!   ANSWER: the receiver's UDP port and its SSRCs
//!        ↓
//!   RTP/UDP with an AES-CTR-128 encrypted payload
//! ```
//!
//! There is no container, no HTTP server and no media player on the other end,
//! and therefore none of the pre-buffering that costs seconds.
//!
//! The protocol is not proprietary: it is implemented in the
//! [Open Screen Library](https://chromium.googlesource.com/openscreen/+/HEAD/cast/streaming/),
//! Google's own open source, which is the reference used here.

use std::net::IpAddr;
use std::time::Duration;

use serde_json::{Value, json};

use nd_core::{NdError, Result};

use crate::cast::{CastChannel, LaunchedApp};

mod answer;

pub use answer::{Answer, AudioLimits, VideoLimits, parse_answer};

/// The mirroring app built into Cast devices ("Chrome Mirroring").
pub const MIRRORING_APP_ID: &str = "0F5096E8";
/// Namespace of the OFFER/ANSWER negotiation.
pub const NS_WEBRTC: &str = "urn:x-cast:com.google.cast.webrtc";

/// The RTP payload type of H.264 on Cast (`RtpPayloadType::kVideoH264`).
pub const RTP_PAYLOAD_H264: u8 = 101;
/// The RTP payload type of Opus (`RtpPayloadType::kAudioOpus`).
pub const RTP_PAYLOAD_OPUS: u8 = 96;

/// The video time base: 90 kHz, like all video RTP.
pub const VIDEO_TIME_BASE: u32 = 90_000;
/// The Opus audio time base, which is also its sample rate.
pub const AUDIO_TIME_BASE: u32 = 48_000;
/// The Opus mode the pipeline produces, offered as is.
const OPUS_BITRATE: u32 = 128_000;
const OPUS_CHANNELS: u32 = 2;

/// How long to wait for the receiver's ANSWER.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(15);

/// The playout delay requested from the receiver, in milliseconds.
///
/// This is the jitter buffer on the other end: the floor of end-to-end
/// latency on this path. Chrome uses ~150 ms for screen mirroring; below that
/// the receiver starts stuttering on any network variation. Adjustable because
/// the sweet spot depends on the network.
pub const DEFAULT_TARGET_DELAY_MS: u32 = 150;

/// The playout delay to request, given the latency profile.
///
/// This is the Cast equivalent of the Miracast jitter buffer: the receiver
/// holds frames for this long before showing them, which is what smooths out
/// uneven arrival and delays the pointer by the same amount.
pub fn target_delay_ms() -> u32 {
    let default = nd_core::latency::cast_playout_delay_ms(DEFAULT_TARGET_DELAY_MS);
    resolve_target_delay_ms(
        default,
        std::env::var("BIGNETSCREEN_CAST_TARGET_DELAY_MS")
            .ok()
            .as_deref(),
    )
}

fn resolve_target_delay_ms(default: u32, override_value: Option<&str>) -> u32 {
    match override_value {
        Some(value) => match value.parse::<u32>() {
            // No floor: a knob that exists to measure what the receiver will
            // take cannot refuse the values worth measuring, zero included.
            Ok(delay) => delay.min(1_000),
            Err(_) => {
                tracing::warn!(
                    value,
                    "invalid BIGNETSCREEN_CAST_TARGET_DELAY_MS; using profile default"
                );
                default
            }
        },
        None => default,
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
        // Key material stays out of logs.
        f.write_str("StreamKeys(<hidden>)")
    }
}

pub(crate) fn random_bytes() -> Result<[u8; 16]> {
    let mut bytes = [0u8; 16];
    nd_core::secret::random_bytes(&mut bytes)?;
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

/// The bitrate to ask the encoder for.
///
/// What the mode needs, held to what the sender can actually push. The two are
/// decided independently (the table by resolution and frame rate, the pacer by
/// what a Wi-Fi queue tolerates), and above 1440p60 the table asks for more
/// than the pacer delivers. See [`crate::mirror_session::MAX_VIDEO_BITRATE_KBPS`].
pub(crate) fn cast_bitrate_kbps(cfg: &nd_core::pipeline::StreamConfig) -> u32 {
    let ceiling = crate::mirror_session::MAX_VIDEO_BITRATE_KBPS;
    // `BIGNETSCREEN_CAST_BITRATE_KBPS` answers one question the table cannot:
    // whether a receiver that struggles with a mode is struggling with its
    // bitrate or with its pixel rate. Send the same mode with fewer bits and
    // see. Still held to the pacer, which is a physical limit and not a guess.
    if let Some(kbps) = std::env::var("BIGNETSCREEN_CAST_BITRATE_KBPS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
    {
        return kbps.clamp(500, ceiling);
    }
    cfg.scaled_bitrate_kbps().min(ceiling)
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
    // Match Open Screen's priority ranges without reusing constants between
    // sessions. SSRCs identify streams; unlike AES keys they need no secrecy.
    let [v0, v1, v2, v3, a0, a1, a2, a3, ..] = random_bytes()?;
    let video_ssrc = 50_001 + u32::from_be_bytes([v0, v1, v2, v3]) % 50_000;
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
        let audio_ssrc = 1 + u32::from_be_bytes([a0, a1, a2, a3]) % 50_000;
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
            "bitRate": OPUS_BITRATE,
            "channels": OPUS_CHANNELS,
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

/// Runs the full negotiation against an already connected receiver.
pub async fn negotiate(
    channel: &CastChannel,
    app: &LaunchedApp,
    receiver_ip: IpAddr,
    cfg: &MirrorConfig,
) -> Result<Negotiated> {
    let seq_num = 1;
    let (payload, offer) = build_offer(cfg, seq_num)?;
    tracing::debug!("sending the mirroring OFFER");

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

        if event.closes(app) {
            return Err(NdError::Protocol(
                "receiver closed mirroring during negotiation".into(),
            ));
        }
        if event.namespace != NS_WEBRTC || event.source_id != app.transport_id {
            continue;
        }
        let kind = event.payload.get("type").and_then(Value::as_str);
        match kind {
            Some("ANSWER") => {
                let answer = parse_answer(&event.payload, seq_num)?;
                answer.check_against(&offer)?;
                tracing::info!(
                    port = answer.udp_port,
                    accepted = ?answer.send_indexes,
                    // What was asked of the receiver and what it declared it
                    // can decode: without both, a mode it imposed cannot be
                    // told from our own ceiling. `None` limits mean the
                    // receiver claimed none.
                    target_delay_ms = cfg.target_delay_ms,
                    profile = ?nd_core::latency::current(),
                    max_bitrate = cfg.max_bitrate,
                    offered = format_args!("{}x{}@{}", cfg.width, cfg.height, cfg.fps),
                    video_limits = ?answer.video_limits,
                    audio_limits = ?answer.audio_limits,
                    "mirroring negotiated"
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
                tracing::debug!(?other, "message from the webrtc namespace");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_advertises_implemented_extensions_only() {
        let (payload, offer) = build_offer(&MirrorConfig::default(), 1).unwrap();
        let audio = offer.streams.iter().find(|s| !s.is_video).unwrap();
        let video = offer.streams.iter().find(|s| s.is_video).unwrap();
        assert!((1..=50_000).contains(&audio.ssrc));
        assert!((50_001..=100_000).contains(&video.ssrc));
        for stream in payload["offer"]["supportedStreams"].as_array().unwrap() {
            assert!(stream.get("rtpExtensions").is_none());
        }
    }

    #[test]
    fn invalid_delay_override_keeps_default() {
        for default in [
            DEFAULT_TARGET_DELAY_MS,
            nd_core::latency::FILM_PLAYOUT_DELAY_MS,
        ] {
            for value in [
                None,
                Some(""),
                Some("invalid"),
                Some("-1"),
                Some("4294967296"),
            ] {
                assert_eq!(resolve_target_delay_ms(default, value), default);
            }
        }
    }

    #[test]
    fn delay_override_is_bounded() {
        for default in [
            DEFAULT_TARGET_DELAY_MS,
            nd_core::latency::FILM_PLAYOUT_DELAY_MS,
        ] {
            for (value, expected) in [
                // Zero reaches the receiver: a floor would make a test of a
                // shorter buffer report success without testing it.
                ("0", 0),
                ("50", 50),
                ("100", 100),
                ("1000", 1000),
                ("4294967295", 1000),
            ] {
                assert_eq!(resolve_target_delay_ms(default, Some(value)), expected);
            }
        }
    }

    #[test]
    fn offer_shares_audio_video_delay() {
        for value in [None, Some("75"), Some("0"), Some("2000"), Some("invalid")] {
            let cfg = MirrorConfig {
                target_delay_ms: resolve_target_delay_ms(DEFAULT_TARGET_DELAY_MS, value),
                ..Default::default()
            };
            let (payload, _) = build_offer(&cfg, 1).unwrap();
            let streams = payload["offer"]["supportedStreams"].as_array().unwrap();
            assert_eq!(streams.len(), 2);
            for stream in streams {
                assert_eq!(stream["targetDelay"], cfg.target_delay_ms);
            }
        }
    }

    #[test]
    fn offer_has_required_fields() {
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
    fn aes_keys_are_unique_hex() {
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
    fn debug_output_hides_keys() {
        let keys = StreamKeys::random().unwrap();
        let printed = format!("{keys:?}");
        assert!(
            !printed.contains(&hex(&keys.key)),
            "the key leaked: {printed}"
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
                video_limits: None,
                audio_limits: None,
            },
            offer,
        };
        assert!(negotiated.video().is_some());
        assert!(negotiated.audio().is_none(), "audio was not accepted");
        assert_eq!(negotiated.target().1, 51810);
    }
}
