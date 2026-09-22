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

/// The mirroring app built into Cast devices ("Chrome Mirroring").
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
            // No floor. A knob that exists to measure what the receiver will
            // take cannot refuse the values worth measuring: a session run at
            // `0` was silently answered with 50, and reported back as "zero
            // works" when zero had never been sent.
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
    /// Decoder limits, not display size. Missing limits use a conservative mode.
    pub video_limits: Option<VideoLimits>,
    pub audio_limits: Option<AudioLimits>,
}

#[derive(Clone, Debug)]
pub struct VideoLimits {
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: f64,
    pub max_pixels_per_second: Option<f64>,
    pub min_bitrate: u32,
    pub max_bitrate: u32,
    pub min_size: Option<(u32, u32)>,
    pub max_delay_ms: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct AudioLimits {
    pub max_sample_rate: u32,
    pub max_channels: u32,
    pub min_bitrate: u32,
    pub max_bitrate: u32,
    pub max_delay_ms: Option<u32>,
}

fn positive_u32(value: &Value, field: &str) -> Result<u32> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| NdError::Protocol(format!("invalid positive ANSWER field {field}")))
}

fn optional_positive_u32(value: &Value, field: &str) -> Result<Option<u32>> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => positive_u32(value, field).map(Some),
    }
}

fn parse_frame_rate(value: &Value) -> Result<f64> {
    let rate = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => {
            if let Some((n, d)) = text.split_once('/') {
                n.parse::<u32>()
                    .ok()
                    .zip(d.parse::<u32>().ok())
                    .filter(|(_, d)| *d > 0)
                    .map(|(n, d)| f64::from(n) / f64::from(d))
            } else {
                text.parse::<f64>().ok()
            }
        }
        _ => None,
    };
    rate.filter(|n| n.is_finite() && *n > 0.0)
        .ok_or_else(|| NdError::Protocol("invalid ANSWER frameRate".into()))
}

impl VideoLimits {
    fn parse(value: &Value) -> Result<Self> {
        let dims = &value["maxDimensions"];
        let min_bitrate = optional_positive_u32(value, "minBitRate")?.unwrap_or(0);
        let max_bitrate = positive_u32(value, "maxBitRate")?;
        if min_bitrate > max_bitrate {
            return Err(NdError::Protocol(
                "ANSWER bitrate limits are reversed".into(),
            ));
        }
        let max_pixels_per_second = match value.get("maxPixelsPerSecond") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_f64()
                    .filter(|n| n.is_finite() && *n > 0.0)
                    .ok_or_else(|| NdError::Protocol("invalid ANSWER maxPixelsPerSecond".into()))?,
            ),
        };
        let min_size = match value.get("minResolution") {
            None | Some(Value::Null) => None,
            Some(v) => Some((positive_u32(v, "width")?, positive_u32(v, "height")?)),
        };
        Ok(Self {
            max_width: positive_u32(dims, "width")?,
            max_height: positive_u32(dims, "height")?,
            max_fps: parse_frame_rate(&dims["frameRate"])?,
            max_pixels_per_second,
            min_bitrate,
            max_bitrate,
            min_size,
            max_delay_ms: optional_positive_u32(value, "maxDelay")?,
        })
    }
}

impl AudioLimits {
    fn parse(value: &Value) -> Result<Self> {
        let limits = Self {
            max_sample_rate: positive_u32(value, "maxSampleRate")?,
            max_channels: positive_u32(value, "maxChannels")?,
            min_bitrate: optional_positive_u32(value, "minBitRate")?.unwrap_or(0),
            max_bitrate: positive_u32(value, "maxBitRate")?,
            max_delay_ms: optional_positive_u32(value, "maxDelay")?,
        };
        if limits.min_bitrate > limits.max_bitrate {
            return Err(NdError::Protocol(
                "ANSWER audio bitrate limits are reversed".into(),
            ));
        }
        Ok(limits)
    }
}

/// The bitrate to ask the encoder for.
///
/// What the mode needs, held to what the sender can actually push. The two are
/// decided independently — the table by resolution and frame rate, the pacer by
/// what a Wi-Fi queue tolerates — and above 1440p60 the table wins an argument
/// it cannot deliver. See [`crate::mirror_session::MAX_VIDEO_BITRATE_KBPS`].
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

impl Answer {
    /// Apply independent decoder dimension, pixel-rate and bandwidth limits.
    /// Preserve aspect ratio and never raise a user's requested resolution/FPS.
    pub fn constrain(
        &self,
        cfg: &mut nd_core::pipeline::StreamConfig,
        delay_ms: u32,
        with_audio: bool,
    ) -> Result<()> {
        use nd_core::pipeline::StreamConfig;
        if let Some(limits) = &self.video_limits {
            if limits.max_width < 2 || limits.max_height < 2 {
                return Err(NdError::Unsupported(
                    "receiver cannot accept even H.264 dimensions".into(),
                ));
            }
            (cfg.width, cfg.height) = StreamConfig::fit_within(
                (cfg.width, cfg.height),
                (limits.max_width, limits.max_height),
            );
            cfg.fps = cfg.fps.min(limits.max_fps.floor() as u32);
            if let Some(rate) = limits.max_pixels_per_second {
                let pixels = f64::from(cfg.width) * f64::from(cfg.height);
                cfg.fps = cfg.fps.min((rate / pixels).floor() as u32);
            }
            // The receiver's own declaration is the only external ceiling: it
            // is the one number the device actually measured about itself.
            let ceiling = limits.max_bitrate / 1000;
            let floor = limits.min_bitrate.div_ceil(1000);
            if ceiling == 0
                || floor > ceiling
                || cfg.fps == 0
                || limits
                    .min_size
                    .is_some_and(|(w, h)| cfg.width < w || cfg.height < h)
                || limits.max_delay_ms.is_some_and(|limit| delay_ms > limit)
            {
                return Err(NdError::Unsupported(
                    "receiver limits cannot accommodate this mirroring mode".into(),
                ));
            }
            cfg.bitrate_kbps = cast_bitrate_kbps(cfg).min(ceiling).max(floor);
        } else {
            // Silence is not a claim of 1080p30. The receiver was sent this
            // exact mode in the OFFER and accepted the video stream — sending
            // something else now is a different session from the negotiated
            // one, not a safer one.
            //
            // Clamping here could never protect a cautious person either: on
            // the default settings the OFFER is already 1080p30, because
            // `preferred_or` and `capped_fps` built it from those settings. A
            // ceiling applied after the fact can therefore only overrule
            // somebody who deliberately asked for more — which is the exact
            // field report `StreamConfig::preferred_or` exists to answer.
            //
            // Nothing to cap against, so the mode's own bitrate stands.
            cfg.bitrate_kbps = cast_bitrate_kbps(cfg);
        }
        if with_audio {
            if let Some(limits) = &self.audio_limits {
                // The current Opus pipeline is fixed at 48 kHz / stereo / 128 kbit/s.
                if limits.max_sample_rate < AUDIO_TIME_BASE
                    || limits.max_channels < 2
                    || limits.max_bitrate < 128_000
                    || limits.min_bitrate > 128_000
                    || limits.max_delay_ms.is_some_and(|limit| delay_ms > limit)
                {
                    return Err(NdError::Unsupported(
                        "receiver cannot accept the offered Opus audio mode".into(),
                    ));
                }
            }
        }
        Ok(())
    }
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
    let random = random_bytes()?;
    let video_ssrc = 50_001 + u32::from_be_bytes(random[..4].try_into().unwrap()) % 50_000;
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
        let audio_ssrc = 1 + u32::from_be_bytes(random[4..8].try_into().unwrap()) % 50_000;
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
    if payload.get("seqNum").and_then(Value::as_i64) != Some(seq_num) {
        return Err(NdError::Protocol(format!(
            "ANSWER from another negotiation or missing seqNum (expected {seq_num})"
        )));
    }
    match payload.get("result").and_then(Value::as_str) {
        Some("ok") => {}
        Some("error") => {
            // `Unsupported` rather than `Protocol`: this is how the caller
            // tells "this device has no mirroring" from "the session broke".
            return Err(NdError::Unsupported(format!(
                "the receiver refused the offer: {payload}"
            )));
        }
        _ => return Err(NdError::Protocol("ANSWER has no valid result".into())),
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

    let send_indexes = parse_u32_array(answer, "sendIndexes")?;
    if send_indexes.is_empty() {
        return Err(NdError::Protocol(
            "the receiver accepted none of the offered streams".into(),
        ));
    }
    let ssrcs = parse_u32_array(answer, "ssrcs")?;
    if send_indexes.len() != ssrcs.len()
        || send_indexes.len() > 2
        || send_indexes
            .iter()
            .enumerate()
            .any(|(i, id)| send_indexes[..i].contains(id))
        || ssrcs
            .iter()
            .enumerate()
            .any(|(i, id)| *id == 0 || ssrcs[..i].contains(id))
    {
        return Err(NdError::Protocol(
            "ANSWER has invalid stream/SSRC correspondence".into(),
        ));
    }
    let (video_limits, audio_limits) = match answer.get("constraints") {
        None | Some(Value::Null) => (None, None),
        Some(c) => (
            Some(VideoLimits::parse(&c["video"])?),
            Some(AudioLimits::parse(&c["audio"])?),
        ),
    };
    Ok(Answer {
        udp_port,
        send_indexes,
        ssrcs,
        video_limits,
        audio_limits,
    })
}

fn parse_u32_array(value: &Value, field: &str) -> Result<Vec<u32>> {
    let list = value
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| NdError::Protocol(format!("ANSWER has no {field} array")))?;
    list.iter()
        .map(|n| {
            n.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| NdError::Protocol(format!("invalid uint32 in ANSWER {field}")))
        })
        .collect()
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
                if answer
                    .send_indexes
                    .iter()
                    .any(|id| !offer.streams.iter().any(|stream| stream.index == *id))
                {
                    return Err(NdError::Protocol(
                        "ANSWER selected a stream that was not offered".into(),
                    ));
                }
                if answer
                    .ssrcs
                    .iter()
                    .any(|ssrc| offer.streams.iter().any(|stream| stream.ssrc == *ssrc))
                {
                    return Err(NdError::Protocol(
                        "ANSWER reused a sender SSRC for the receiver".into(),
                    ));
                }
                if !offer
                    .streams
                    .iter()
                    .any(|stream| stream.is_video && answer.send_indexes.contains(&stream.index))
                {
                    return Err(NdError::Unsupported(
                        "receiver accepted no video stream".into(),
                    ));
                }
                tracing::info!(
                    port = answer.udp_port,
                    accepted = ?answer.send_indexes,
                    // The delay asked of the receiver, and the profile that
                    // chose it. "Film mode changed nothing" is a claim worth
                    // being able to check rather than argue about: the number
                    // that left this machine is now in the log.
                    target_delay_ms = cfg.target_delay_ms,
                    profile = ?nd_core::latency::current(),
                    // What we committed to putting on the wire. "The picture
                    // is choppy" and "the link cannot carry this" look the
                    // same from the sofa, and only this number tells them
                    // apart without reproducing the session.
                    max_bitrate = cfg.max_bitrate,
                    // The mode that was offered, and what the receiver said it
                    // can decode. Without both, "it arrived at 1080p30" cannot
                    // be told apart: the device may have imposed that, or it
                    // may have declared nothing and been given our own ceiling.
                    // `None` here means the receiver claimed no limit at all.
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
    fn offer_has_audio_priority_and_only_implemented_extensions() {
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
    fn target_delay_keeps_profile_defaults_without_a_valid_override() {
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
    fn target_delay_override_is_bounded_for_both_profiles() {
        for default in [
            DEFAULT_TARGET_DELAY_MS,
            nd_core::latency::FILM_PLAYOUT_DELAY_MS,
        ] {
            for (value, expected) in [
                // Zero reaches the receiver. It used to be rounded up to 50,
                // which made every attempt to test a shorter buffer report
                // success without having tested one.
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
    fn offer_requests_the_same_delay_for_audio_and_video() {
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
                video_limits: None,
                audio_limits: None,
            },
            offer,
        };
        assert!(negotiated.video().is_some());
        assert!(negotiated.audio().is_none(), "audio was not accepted");
        assert_eq!(negotiated.target().1, 51810);
    }
    #[test]
    fn answer_rejects_truncation_overflow_duplicates_and_missing_sequence() {
        for (field, bad) in [
            ("sendIndexes", json!([0, "1"])),
            ("sendIndexes", json!([0, 4294967296u64])),
            ("sendIndexes", json!([0, 0])),
            ("ssrcs", json!([1])),
            ("ssrcs", json!([1, 1])),
            ("ssrcs", json!([0, 2])),
        ] {
            let mut value = answer_json(1);
            value["answer"][field] = bad;
            assert!(parse_answer(&value, 1).is_err());
        }
        let mut value = answer_json(1);
        value.as_object_mut().unwrap().remove("seqNum");
        assert!(parse_answer(&value, 1).is_err());
    }

    #[test]
    fn frame_rate_handles_numeric_and_fractional_limits() {
        assert_eq!(parse_frame_rate(&json!(30)).unwrap(), 30.0);
        assert!((parse_frame_rate(&json!("60000/1001")).unwrap() - 59.94005994).abs() < 0.00001);
        for bad in [
            json!("1/0"),
            json!("NaN"),
            json!("inf"),
            json!(-1),
            json!(null),
        ] {
            assert!(parse_frame_rate(&bad).is_err());
        }
    }

    #[test]
    fn decoder_limits_bound_pixels_bitrate_and_fps_independently() {
        let mut answer = parse_answer(&answer_json(1), 1).unwrap();
        answer.video_limits = Some(VideoLimits {
            max_width: 1920,
            max_height: 1080,
            max_fps: 60.0,
            max_pixels_per_second: Some(1920.0 * 1080.0 * 30.0),
            min_bitrate: 300_000,
            max_bitrate: 6_000_000,
            min_size: None,
            max_delay_ms: Some(300),
        });
        let mut cfg = nd_core::pipeline::StreamConfig {
            width: 3840,
            height: 2160,
            fps: 60,
            bitrate_kbps: 10_000,
            ..Default::default()
        };
        answer.constrain(&mut cfg, 150, false).unwrap();
        assert_eq!(
            (cfg.width, cfg.height, cfg.fps, cfg.bitrate_kbps),
            (1920, 1080, 30, 6000)
        );
        cfg.width = 1280;
        cfg.height = 720;
        cfg.fps = 60;
        answer.constrain(&mut cfg, 150, false).unwrap();
        assert_eq!(cfg.fps, 60);
    }

    #[test]
    fn a_silent_answer_keeps_the_mode_the_receiver_accepted() {
        let answer = parse_answer(&answer_json(1), 1).unwrap();
        let mut cfg = nd_core::pipeline::StreamConfig {
            width: 2560,
            height: 1440,
            fps: 60,
            ..Default::default()
        };
        answer.constrain(&mut cfg, 150, false).unwrap();
        assert_eq!((cfg.width, cfg.height, cfg.fps), (2560, 1440, 60));
        // And the bitrate the mode needs, not 1080p30's.
        assert_eq!(cfg.bitrate_kbps, 21_000);
    }

    #[test]
    fn a_declared_bitrate_limit_still_caps_a_larger_mode() {
        let mut answer = parse_answer(&answer_json(1), 1).unwrap();
        answer.video_limits = Some(VideoLimits {
            max_width: 3840,
            max_height: 2160,
            max_fps: 60.0,
            max_pixels_per_second: None,
            min_bitrate: 300_000,
            max_bitrate: 12_000_000,
            min_size: None,
            max_delay_ms: None,
        });
        let mut cfg = nd_core::pipeline::StreamConfig {
            width: 2560,
            height: 1440,
            fps: 60,
            ..Default::default()
        };
        answer.constrain(&mut cfg, 150, false).unwrap();
        assert_eq!((cfg.width, cfg.height, cfg.fps), (2560, 1440, 60));
        assert_eq!(cfg.bitrate_kbps, 12_000);
    }
}
