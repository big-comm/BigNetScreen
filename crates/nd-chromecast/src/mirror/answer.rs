//! The receiver's ANSWER to a mirroring OFFER: the UDP port, the streams it
//! accepted with its own SSRCs, and the decoder limits that bound the mode
//! [`super::negotiate`] then streams.

use serde_json::Value;

use nd_core::{NdError, Result};

use super::{AUDIO_TIME_BASE, OPUS_BITRATE, OPUS_CHANNELS, Offer, cast_bitrate_kbps};

/// What the receiver answered.
#[derive(Clone, Debug)]
pub struct Answer {
    /// The UDP port where it expects the RTP.
    pub udp_port: u16,
    /// Indices of the streams it accepted.
    pub send_indexes: Vec<u32>,
    /// The receiver's SSRCs (one per accepted stream), used by RTCP.
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
    /// Whether the receiver takes the one Opus mode the pipeline produces
    /// (48 kHz, stereo, [`OPUS_BITRATE`]) at this playout delay.
    fn accept(&self, delay_ms: u32) -> bool {
        self.max_sample_rate >= AUDIO_TIME_BASE
            && self.max_channels >= OPUS_CHANNELS
            && (self.min_bitrate..=self.max_bitrate).contains(&OPUS_BITRATE)
            && self.max_delay_ms.is_none_or(|limit| delay_ms <= limit)
    }

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

impl Answer {
    /// Holds the ANSWER to the OFFER it answers: only offered streams, SSRCs
    /// of its own, and the video stream among them.
    pub(super) fn check_against(&self, offer: &Offer) -> Result<()> {
        if self
            .send_indexes
            .iter()
            .any(|id| !offer.streams.iter().any(|stream| stream.index == *id))
        {
            return Err(NdError::Protocol(
                "ANSWER selected a stream that was not offered".into(),
            ));
        }
        if self
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
            .any(|stream| stream.is_video && self.send_indexes.contains(&stream.index))
        {
            return Err(NdError::Unsupported(
                "receiver accepted no video stream".into(),
            ));
        }
        Ok(())
    }

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
            // No limits means the receiver accepted the video stream of the
            // exact mode in the OFFER, so the mode and its own bitrate stand.
            // On default settings the OFFER is already 1080p30; a ceiling here
            // could only overrule a user who asked for more.
            cfg.bitrate_kbps = cast_bitrate_kbps(cfg);
        }
        if with_audio
            && self
                .audio_limits
                .as_ref()
                .is_some_and(|limits| !limits.accept(delay_ms))
        {
            return Err(NdError::Unsupported(
                "receiver cannot accept the offered Opus audio mode".into(),
            ));
        }
        Ok(())
    }
}

/// Parses the receiver's ANSWER.
pub(crate) fn parse_answer(payload: &Value, seq_num: i64) -> Result<Answer> {
    let kind = payload.get("type").and_then(Value::as_str).unwrap_or("");
    if kind != "ANSWER" {
        return Err(NdError::Protocol(format!(
            "expected ANSWER, got {kind}: {payload}"
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn rejects_answer_without_usable_port() {
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
    fn rejects_malformed_answers() {
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
    fn parses_fractional_frame_rate_limits() {
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
    fn decoder_limits_apply_independently() {
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
    fn silent_answer_keeps_accepted_mode() {
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
    fn bitrate_limit_caps_larger_mode() {
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

    #[test]
    fn audio_limits_apply_only_with_audio() {
        let mut answer = parse_answer(&answer_json(1), 1).unwrap();
        let fits = AudioLimits {
            max_sample_rate: 48_000,
            max_channels: 2,
            min_bitrate: 32_000,
            max_bitrate: 256_000,
            max_delay_ms: Some(400),
        };
        let mut cfg = nd_core::pipeline::StreamConfig::default();
        answer.audio_limits = Some(fits.clone());
        answer.constrain(&mut cfg, 150, true).unwrap();
        for refused in [
            AudioLimits {
                max_channels: 1,
                ..fits.clone()
            },
            AudioLimits {
                max_bitrate: 96_000,
                ..fits.clone()
            },
            AudioLimits {
                min_bitrate: 192_000,
                ..fits.clone()
            },
            AudioLimits {
                max_delay_ms: Some(100),
                ..fits.clone()
            },
        ] {
            answer.audio_limits = Some(refused);
            assert!(answer.constrain(&mut cfg, 150, true).is_err());
            answer.constrain(&mut cfg, 150, false).unwrap();
        }
    }
}
