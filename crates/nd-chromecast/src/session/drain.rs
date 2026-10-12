//! Keeping the Default Media Receiver close to real time.
//!
//! The receiver reports where it is playing ([`LagMeter`]); playing slightly
//! faster than real time drains the buffer it built up front
//! ([`DrainController`]), and [`BufferDrain`] ties both to the Cast channel
//! for [`super::serve_receiver`].

use std::time::Duration;

use serde_json::{Value, json};

use crate::cast::{CastChannel, LaunchedApp, NS_MEDIA};

/// How often we ask the receiver where it is playing.
pub(super) const LAG_PROBE_INTERVAL: Duration = Duration::from_secs(2);
/// Delay samples kept for the session's median: the receiver also reports on
/// its own, so the count follows its messages and needs a bound.
const MAX_LAG_SAMPLES: usize = 512;

/// The initial delay target, in seconds.
///
/// It starts from a safe value and tightens from there. Starting aggressive
/// and loosening on stalls is worse: every stall throws the delay back past
/// the receiver's own buffer and stutters the picture.
const LAG_TARGET_START: f64 = 0.9;
/// The target's ceiling: above it there is nothing left worth doing.
const LAG_TARGET_MAX: f64 = 2.5;
/// The floor: below it the buffer cannot absorb even normal network variation.
const LAG_TARGET_MIN: f64 = 0.35;
/// How much the target loosens on each detected stall.
const LAG_TARGET_STEP: f64 = 0.25;
/// How much the target tightens after a stable stretch.
const LAG_TIGHTEN_STEP: f64 = 0.1;
/// Consecutive stall-free samples before tightening further.
///
/// At ~2 s per sample, that is ~20 s of stability before each tightening:
/// enough time for a network variation to show up.
const LAG_TIGHTEN_AFTER: u32 = 10;
/// The safety margin above a target that demonstrably stalled.
///
/// Generous on purpose: scraping the same point again only produces more
/// stalls, and every stall is a visible stutter.
const LAG_STALL_MARGIN: f64 = 0.4;
/// How many stalls are tolerated before the target is frozen.
///
/// After that, this device's floor with this content and this network is
/// known well enough; pushing further only costs smoothness, and stutter
/// grates more than delay.
const MAX_STALL_PROBES: u32 = 2;
/// Hysteresis: how far above the target the delay must be before draining
/// starts.
const LAG_TARGET_MARGIN: f64 = 0.15;
/// The playback rate while draining; faster than this, the speed-up shows.
const DRAIN_RATE: f64 = 1.2;

/// Keeps the receiver as close to real time as it can go without stalling.
///
/// The Default Media Receiver is a file player: it
/// pre-buffers a few seconds before starting and, since it receives at the
/// same rate it plays, keeps that slack forever. Playing back slightly faster
/// consumes the slack and brings the picture closer to real time.
///
/// Draining too hard empties the buffer, the receiver stalls to refill and the
/// whole delay comes back. Where that happens depends on the device and the
/// network, so the target starts low and rises with each stall.
struct DrainController {
    target: f64,
    rate: f64,
    enabled: bool,
    /// Consecutive stall-free samples.
    stable: u32,
    /// The lowest target that ever caused a stall; nothing below it is tried again.
    floor: f64,
    /// How many stalls have happened in this session.
    stalls: u32,
    /// Probing has stopped: the target tightens no further.
    frozen: bool,
}

impl DrainController {
    fn new() -> Self {
        let get = |k: &str, d: f64| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v > 0.0)
                .unwrap_or(d)
        };
        Self {
            target: get("BIGNETSCREEN_CC_LAG_TARGET", LAG_TARGET_START),
            rate: get("BIGNETSCREEN_CC_DRAIN_RATE", DRAIN_RATE),
            // The film profile answers "quick response or smooth playback"
            // for every protocol; on this path smooth means leaving the
            // receiver's own buffer alone.
            enabled: std::env::var("BIGNETSCREEN_CC_NO_DRAIN").is_err()
                && !nd_core::latency::is_film(),
            stable: 0,
            floor: LAG_TARGET_MIN,
            stalls: 0,
            frozen: false,
        }
    }

    /// The receiver stalled: the target was too optimistic for this device.
    fn on_stall(&mut self) {
        if !self.enabled {
            return;
        }
        self.stable = 0;
        self.stalls += 1;
        // This target demonstrably does not hold. The floor gets a wide
        // margin: scraping the same point again would only cause more
        // stutter.
        self.floor = (self.target + LAG_STALL_MARGIN).min(LAG_TARGET_MAX);
        self.target = (self.target + LAG_TARGET_STEP).min(LAG_TARGET_MAX);

        // Stalling even after freezing means not even the final target holds
        // on this device with this content. Pushing on would only produce more
        // stutter: better to give up draining and leave the receiver with its
        // natural buffer, which is smooth.
        if self.frozen {
            self.enabled = false;
            tracing::info!(
                "the receiver cannot hold the reduced buffer; giving up on \
                 draining to keep the picture smooth"
            );
            return;
        }

        if self.stalls >= MAX_STALL_PROBES {
            // Enough is known about this device/content/network. Probing on
            // would trade smoothness for tenths of a second, and stutter
            // grates far more than delay.
            self.frozen = true;
            tracing::info!(
                final_target_s = format!("{:.2}", self.target),
                stalls = self.stalls,
                "delay target frozen: prioritising smoothness"
            );
        } else {
            tracing::info!(
                new_target_s = format!("{:.2}", self.target),
                "the receiver stalled while draining; loosening the target"
            );
        }
    }

    /// A stall-free sample: after enough stability, it tightens.
    ///
    /// This is how the target converges downwards without stuttering the
    /// picture: instead of finding the floor by falling into it, it comes
    /// close from above and stops.
    fn on_stable(&mut self) {
        if !self.enabled {
            return;
        }
        self.stable += 1;
        if self.frozen || self.stable < LAG_TIGHTEN_AFTER || self.target <= self.floor {
            return;
        }
        self.stable = 0;
        let tightened = (self.target - LAG_TIGHTEN_STEP).max(self.floor);
        if (tightened - self.target).abs() > f64::EPSILON {
            self.target = tightened;
            tracing::info!(
                new_target_s = format!("{:.2}", self.target),
                "stable; tightening the delay target"
            );
        }
    }

    /// The rate wanted now, or `None` to keep the current one.
    fn wanted_rate(&self, lag: f64) -> Option<f64> {
        if !self.enabled {
            return None;
        }
        if lag > self.target + LAG_TARGET_MARGIN {
            Some(self.rate)
        } else if lag < self.target {
            Some(1.0)
        } else {
            None
        }
    }
}

/// Follows the receiver's delay and changes its playback rate toward the
/// target. The receiver advertises `PLAYBACK_RATE` among its commands; one
/// that refuses it in practice switches draining off for the session.
pub(super) struct BufferDrain {
    lag: LagMeter,
    controller: DrainController,
    /// The playback rate the receiver last confirmed.
    rate: f64,
}

impl BufferDrain {
    pub(super) fn new() -> Self {
        Self {
            lag: LagMeter::new(),
            controller: DrainController::new(),
            rate: 1.0,
        }
    }

    /// A message the receiver sent on its own.
    pub(super) fn observe(&mut self, payload: &Value) {
        if payload.get("type").and_then(Value::as_str) == Some("MEDIA_STATUS") {
            self.lag.observe(payload);
        }
    }

    /// Asks where the receiver is playing, moves the target on a stall or a
    /// stable stretch, and changes the rate when the delay left the target.
    /// `started` is when the receiver opened the stream.
    pub(super) async fn probe(
        &mut self,
        channel: &CastChannel,
        app: &LaunchedApp,
        started: std::time::Instant,
    ) {
        self.lag.start(started);
        let status = match channel
            .request(NS_MEDIA, &app.transport_id, json!({"type": "GET_STATUS"}))
            .await
        {
            Ok(status) => status,
            Err(err) => {
                tracing::debug!(%err, "media status query failed");
                return;
            }
        };
        self.lag.observe(&status);
        if self.lag.stalled {
            self.controller.on_stall();
        } else {
            self.controller.on_stable();
        }
        let (Some(current), Some(session_id)) = (self.lag.last(), media_session_id(&status)) else {
            return;
        };
        let Some(rate) = self
            .controller
            .wanted_rate(current)
            .filter(|rate| (rate - self.rate).abs() > f64::EPSILON)
        else {
            return;
        };
        let reply = channel
            .request(
                NS_MEDIA,
                &app.transport_id,
                json!({
                    "type": "SET_PLAYBACK_RATE",
                    "mediaSessionId": session_id,
                    "playbackRate": rate,
                }),
            )
            .await;
        if reply.as_ref().is_ok_and(rate_change_confirmed) {
            self.rate = rate;
            tracing::info!(
                rate,
                delay_s = format!("{current:.2}"),
                "playback rate adjusted"
            );
        } else {
            // Only the reply's type: a media status carries the stream URL.
            let refusal = match &reply {
                Ok(reply) => reply
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                Err(err) => err.to_string(),
            };
            tracing::warn!(%refusal, "the receiver refused to change the rate");
            self.controller.enabled = false;
        }
    }

    pub(super) fn log_summary(&self) {
        if let Some(median) = self.lag.median() {
            tracing::info!(
                median_delay_s = format!("{median:.2}"),
                samples = self.lag.samples.len(),
                "receiver delay in this session"
            );
        }
    }
}

/// Measures the receiver's real delay from its `MEDIA_STATUS` messages.
///
/// The receiver reports `currentTime`: the position it is playing at within
/// the stream. Since the stream starts the moment it opens the connection,
/// `elapsed − currentTime` is how far behind the receiver is: the depth of its
/// buffer.
struct LagMeter {
    /// The instant the receiver opened the connection: the stream's zero from
    /// its point of view. It only exists once that has happened.
    started: Option<std::time::Instant>,
    /// The most recent [`MAX_LAG_SAMPLES`] delays, oldest first.
    samples: std::collections::VecDeque<f64>,
    /// The last sample (instant, position), for detecting stalls.
    previous: Option<(std::time::Instant, f64)>,
    /// Did the last observation indicate the receiver stopped advancing?
    stalled: bool,
}

impl LagMeter {
    fn new() -> Self {
        Self {
            started: None,
            samples: std::collections::VecDeque::new(),
            previous: None,
            stalled: false,
        }
    }

    /// Resets the stopwatch at the instant the stream started flowing.
    fn start(&mut self, at: std::time::Instant) {
        if self.started.is_none() {
            self.started = Some(at);
        }
    }

    /// Records a sample from a `MEDIA_STATUS`, if it carries a playback
    /// position.
    fn observe(&mut self, payload: &Value) {
        let Some(current) = payload
            .get("status")
            .and_then(Value::as_array)
            .and_then(|list| list.first())
            .and_then(|st| st.get("currentTime"))
            .and_then(Value::as_f64)
        else {
            return;
        };
        // While it is buffering, `currentTime` stands still and the arithmetic
        // means nothing yet.
        if current <= 0.0 {
            return;
        }
        let Some(started) = self.started else {
            return;
        };
        let elapsed = started.elapsed().as_secs_f64();
        let lag = elapsed - current;

        // A stall = the position advanced far less than the clock. That is the
        // sign the buffer emptied and the receiver paused to refill.
        let now = std::time::Instant::now();
        self.stalled = match self.previous {
            Some((then, before)) => {
                let wall = now.duration_since(then).as_secs_f64();
                let played = current - before;
                wall > 0.5 && played < wall * 0.6
            }
            None => false,
        };
        self.previous = Some((now, current));
        if self.samples.len() == MAX_LAG_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(lag);
        tracing::info!(
            delay_s = format!("{lag:.2}"),
            position_s = format!("{current:.2}"),
            "receiver delay"
        );
    }

    /// The most recent measured sample.
    fn last(&self) -> Option<f64> {
        self.samples.back().copied()
    }

    /// The samples' median, which resists the spikes at the start.
    fn median(&self) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted = Vec::from(self.samples.clone());
        sorted.sort_by(f64::total_cmp);
        Some(sorted[sorted.len() / 2])
    }
}

/// Extracts the `mediaSessionId` from a `MEDIA_STATUS` (required to command media).
fn media_session_id(payload: &Value) -> Option<i64> {
    payload
        .get("status")
        .and_then(Value::as_array)
        .and_then(|list| list.first())
        .and_then(|st| st.get("mediaSessionId"))
        .and_then(Value::as_i64)
}

/// A media command is applied only when the receiver answers with its media
/// status; a refusal (`INVALID_REQUEST`, `INVALID_PLAYER_STATE`) is a reply too.
fn rate_change_confirmed(reply: &Value) -> bool {
    reply.get("type").and_then(Value::as_str) == Some("MEDIA_STATUS")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller(target: f64) -> DrainController {
        DrainController {
            target,
            rate: 1.2,
            enabled: true,
            stable: 0,
            floor: LAG_TARGET_MIN,
            stalls: 0,
            frozen: false,
        }
    }

    #[test]
    fn drain_accelerates_only_above_the_target() {
        let drain = controller(0.9);
        // Well behind: speed up.
        assert_eq!(drain.wanted_rate(2.6), Some(1.2));
        // Close enough: back to normal.
        assert_eq!(drain.wanted_rate(0.5), Some(1.0));
        // Within the hysteresis: do not flip back and forth for nothing.
        assert_eq!(drain.wanted_rate(1.0), None);
    }

    #[test]
    fn drain_backs_off_on_stall() {
        // Each stall loosens the target: that is how the logic adapts to a
        // device that cannot hold a small buffer, instead of insisting on a
        // fixed number and leaving the picture stuttering.
        let mut drain = controller(0.9);
        drain.on_stall();
        assert!((drain.target - 1.15).abs() < 1e-9, "{}", drain.target);
        // The stall also becomes a floor: nothing below it is tried again.
        assert!(drain.floor > LAG_TARGET_MIN);

        // Successive stalls freeze the target and then give up; the target
        // stays bounded throughout.
        for _ in 0..20 {
            drain.on_stall();
        }
        assert!(drain.target <= LAG_TARGET_MAX, "{}", drain.target);
        assert!(!drain.enabled, "it should have given up on draining");
    }

    #[test]
    fn drain_converges_downwards_while_stable() {
        // With no stalls the target tightens on its own, approaching the
        // device's floor without stuttering along the way.
        let mut drain = controller(0.9);
        for _ in 0..LAG_TIGHTEN_AFTER {
            drain.on_stable();
        }
        assert!(
            drain.target < 0.9,
            "the target did not tighten: {}",
            drain.target
        );

        // And never below the known floor.
        for _ in 0..500 {
            drain.on_stable();
        }
        assert!(drain.target >= LAG_TARGET_MIN, "{}", drain.target);
    }

    #[test]
    fn a_stall_stops_further_tightening() {
        let mut drain = controller(0.7);
        let stalled_at = drain.target;
        drain.on_stall();
        assert!(drain.target > stalled_at, "the target did not loosen");

        // Even if stable forever, the target never returns to the value that stalled.
        for _ in 0..500 {
            drain.on_stable();
        }
        assert!(
            drain.target > stalled_at,
            "went back to a target that already stalled ({stalled_at}): {}",
            drain.target
        );
    }

    #[test]
    fn drain_freezes_after_repeated_stalls() {
        // Probing forever re-tightens right after each retreat and hits the
        // same point again; every stall is a visible stutter.
        let mut drain = controller(0.9);

        for _ in 0..MAX_STALL_PROBES {
            drain.on_stall();
        }
        assert!(drain.frozen, "the target did not freeze");

        let settled = drain.target;
        for _ in 0..1000 {
            drain.on_stable();
        }
        assert_eq!(
            drain.target, settled,
            "once frozen it must not tighten again"
        );
    }

    #[test]
    fn stall_floor_keeps_a_real_margin() {
        // A narrow margin makes the target scrape the point that already failed.
        let mut drain = controller(1.0);
        drain.on_stall();
        assert!(
            drain.floor >= 1.0 + LAG_STALL_MARGIN - 1e-9,
            "floor with no margin: {}",
            drain.floor
        );
    }

    #[test]
    fn drain_gives_up_on_frozen_stall() {
        // If not even the final target holds, pushing on would only cause more stutter.
        let mut drain = controller(0.9);
        for _ in 0..MAX_STALL_PROBES {
            drain.on_stall();
        }
        assert!(drain.frozen && drain.enabled);

        drain.on_stall();
        assert!(!drain.enabled, "it should have given up on draining");
        assert_eq!(
            drain.wanted_rate(5.0),
            None,
            "having given up, it must not touch the rate again"
        );
    }

    #[test]
    fn the_film_profile_disables_draining() {
        let restore = nd_core::latency::current();
        nd_core::latency::set(nd_core::latency::Profile::Film);
        assert!(!DrainController::new().enabled);
        nd_core::latency::set(nd_core::latency::Profile::Responsive);
        assert!(DrainController::new().enabled);
        nd_core::latency::set(restore);
    }

    #[test]
    fn drain_can_be_disabled() {
        let drain = DrainController {
            enabled: false,
            ..controller(0.9)
        };
        assert_eq!(drain.wanted_rate(5.0), None);
    }

    #[test]
    fn lag_meter_ignores_prestart_samples() {
        // Before the receiver connects there is no stream, and the arithmetic
        // makes no sense.
        let mut meter = LagMeter::new();
        meter.observe(&json!({"status": [{"currentTime": 3.0}]}));
        assert!(meter.last().is_none());
    }

    #[test]
    fn lag_meter_detects_a_stall() {
        let mut meter = LagMeter::new();
        meter.start(std::time::Instant::now() - Duration::from_secs(10));
        // First sample: with no earlier reference, there is no stall.
        meter.observe(&json!({"status": [{"currentTime": 8.0}]}));
        assert!(!meter.stalled);

        // A second sample right after, with the position essentially still:
        // the buffer ran dry.
        std::thread::sleep(Duration::from_millis(600));
        meter.observe(&json!({"status": [{"currentTime": 8.05}]}));
        assert!(meter.stalled, "a stalled position should register a stall");
    }

    #[test]
    fn lag_samples_stay_bounded() {
        let mut meter = LagMeter::new();
        meter.start(std::time::Instant::now() - Duration::from_secs(10));
        for _ in 0..MAX_LAG_SAMPLES * 3 {
            meter.observe(&json!({"status": [{"currentTime": 8.0}]}));
        }
        assert_eq!(meter.samples.len(), MAX_LAG_SAMPLES);
    }

    #[test]
    fn refused_rate_change_is_unapplied() {
        let refused = json!({"type": "INVALID_REQUEST", "reason": "INVALID_COMMAND"});
        assert!(!rate_change_confirmed(&refused));
        let applied = json!({"type": "MEDIA_STATUS", "status": [{"playbackRate": 1.2}]});
        assert!(rate_change_confirmed(&applied));
    }

    #[test]
    fn media_session_id_is_extracted() {
        let payload = json!({"status": [{"mediaSessionId": 7, "currentTime": 1.0}]});
        assert_eq!(media_session_id(&payload), Some(7));
        assert_eq!(media_session_id(&json!({"status": []})), None);
    }
}
