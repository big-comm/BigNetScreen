//! Orchestration of a Chromecast cast session.
//!
//! It joins the three pieces that already existed separately:
//!
//! ```text
//!   capture ──► pipeline (nd-core) ──► multisocketsink
//!                                            ▲
//!                                            │ socket handed over by the
//!                                            │ HTTP server (http.rs)
//!   Cast channel (cast.rs) ── LAUNCH ── LOAD(url) ──► receiver opens the GET
//! ```
//!
//! ## The order of operations (it matters, and it cost a bug)
//!
//! 1. **the HTTP server listening** — the `LOAD` URL has to answer already;
//! 2. **the pipeline built and in `Ready`** (which is what `build_pipeline`
//!    returns): `multisocketsink` refuses `add` while the pipeline is in
//!    `Null`, and the refusal is only a `WARNING` — the receiver gets zero
//!    bytes and the cast fails silently;
//! 3. `LAUNCH` of the Default Media Receiver and `LOAD` with the URL;
//! 4. the receiver opens the `GET`; the socket goes to the sink and **only
//!    then** does the pipeline move to `Playing`.
//!
//! Going to `Playing` before there is a client makes the encoder run into the
//! void and burns the first keyframe, delaying the picture.

use std::net::IpAddr;
use std::time::Duration;

use gio::prelude::*;
use gst::prelude::*;
use gstreamer as gst;

use serde_json::{json, Value};

use nd_core::capture::CaptureSource;
use nd_core::pipeline::{self, StreamConfig, CHROMECAST_SINK_NAME};
use nd_core::sink::{SinkState, SinkStatus};
use nd_core::{NdError, Result};

use crate::cast::{CastChannel, DEFAULT_MEDIA_RECEIVER, NS_MEDIA};
use crate::http::{StreamServer, CONTENT_TYPE};

/// How long to wait for the receiver to open the `GET` after the `LOAD`.
const FIRST_CLIENT_TIMEOUT: Duration = Duration::from_secs(20);
/// How often we ask the receiver where it is playing.
const LAG_PROBE_INTERVAL: Duration = Duration::from_secs(2);

/// The initial delay target, in seconds.
///
/// It starts from a **safe** value and tightens from there. The opposite
/// approach (start aggressive and loosen on stalls) was measured in the field
/// and is worse: every stall throws the delay past 3 s and stutters the
/// picture — the cure hurts more than the disease.
///
/// Measured on this projector: **2.61 s** with no draining at all.
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
/// What to prioritise when the two cannot both be had.
///
/// The choice genuinely belongs to the user and depends on what they are
/// doing: watching a video, the delay is irrelevant and any stutter grates;
/// presenting or using the computer on the big screen, the opposite holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LatencyPreference {
    /// No draining: uses the receiver's natural buffer (~2.6 s), smooth.
    Smooth,
    /// Drains the buffer for a quick response, accepting some stutter.
    #[default]
    Responsive,
}

static PREFERENCE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Sets the preference (process-wide). The interface calls this.
pub fn set_latency_preference(pref: LatencyPreference) {
    PREFERENCE.store(
        match pref {
            LatencyPreference::Responsive => 0,
            LatencyPreference::Smooth => 1,
        },
        std::sync::atomic::Ordering::Relaxed,
    );
    tracing::info!(?pref, "latency preference");
}

/// The current preference.
pub fn latency_preference() -> LatencyPreference {
    // The film profile decides this too. There is one choice to make — quick
    // response or smooth playback — and it should not have to be made once per
    // protocol; on this path "smooth" simply means leaving the receiver's own
    // buffer alone instead of draining it.
    if nd_core::latency::is_film() {
        return LatencyPreference::Smooth;
    }
    match PREFERENCE.load(std::sync::atomic::Ordering::Relaxed) {
        1 => LatencyPreference::Smooth,
        _ => LatencyPreference::Responsive,
    }
}

/// How many stalls are tolerated before the target is **frozen**.
///
/// After that, this device's floor — with this content and this network — is
/// known well enough; pushing further only costs smoothness. Stutter grates
/// far more than delay.
const MAX_STALL_PROBES: u32 = 2;
/// Hysteresis: the margin above the target before speeding up again.
const LAG_TARGET_MARGIN: f64 = 0.15;
/// The speed used to drain. Above it the effect becomes visible.
const DRAIN_RATE: f64 = 1.2;

/// Keeps the receiver as close to real time as it can go without stalling.
///
/// **Why it exists.** The Default Media Receiver is a file player: it
/// pre-buffers a few seconds before starting and, since it receives at the
/// same rate it plays, keeps that slack forever. Playing back slightly faster
/// consumes the slack and brings the picture closer to real time.
///
/// **Why it self-calibrates.** Draining too hard empties the buffer, the
/// receiver stalls to refill and the whole delay comes back — worse than the
/// original problem. Exactly where that happens depends on the device and the
/// network, so the target starts low and **rises on its own** with each stall,
/// rather than hardcoding a number that would only suit one receiver model.
struct DrainController {
    target: f64,
    rate: f64,
    enabled: bool,
    /// Consecutive stall-free samples.
    stable: u32,
    /// The lowest target that ever caused a stall — never tried below again.
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
                .unwrap_or(d)
        };
        Self {
            target: get("BIGNETSCREEN_CC_LAG_TARGET", LAG_TARGET_START),
            rate: get("BIGNETSCREEN_CC_DRAIN_RATE", DRAIN_RATE),
            enabled: std::env::var("BIGNETSCREEN_CC_NO_DRAIN").is_err()
                && latency_preference() == LatencyPreference::Responsive,
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
                alvo_final_s = format!("{:.2}", self.target),
                travadas = self.stalls,
                "delay target frozen: prioritising smoothness"
            );
        } else {
            tracing::info!(
                novo_alvo_s = format!("{:.2}", self.target),
                "the receiver stalled while draining; loosening the target"
            );
        }
    }

    /// A stall-free sample: after enough stability, it tightens.
    ///
    /// This is how the target converges **downwards** without stuttering the
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
                novo_alvo_s = format!("{:.2}", self.target),
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

/// Ensures the pipeline returns to `Null` even if the session leaves via `?`.
///
/// `build_pipeline` returns the pipeline in `Ready`; leaving without taking it
/// down makes GStreamer complain loudly ("Trying to dispose element, but it is
/// in READY instead of the NULL state") and leaks the encoder's resources.
struct PipelineGuard(Option<gst::Pipeline>);

impl PipelineGuard {
    fn new(pipeline: gst::Pipeline) -> Self {
        Self(Some(pipeline))
    }

    fn get(&self) -> &gst::Pipeline {
        self.0.as_ref().expect("the pipeline is still present")
    }
}

impl Drop for PipelineGuard {
    fn drop(&mut self) {
        if let Some(pipeline) = self.0.take() {
            let _ = pipeline.set_state(gst::State::Null);
        }
    }
}

/// A handle for ending a running session.
#[derive(Clone, Debug)]
pub struct SessionHandle {
    cancel: tokio::sync::watch::Sender<bool>,
}

impl SessionHandle {
    /// Requests that the session end.
    pub fn stop(&self) {
        let _ = self.cancel.send(true);
    }
}

/// Runs a cast session from a screen capture.
///
/// `status` receives the transitions for the UI to follow. The `CaptureSource`
/// is kept alive to the end: the PipeWire descriptor is used by the pipeline
/// for the whole session.
pub async fn run(
    receiver_ip: IpAddr,
    source: CaptureSource,
    status: &SinkStatus,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let video = source.video_source();
    let size = source.size_or((1920, 1080));
    let result = run_with_video(receiver_ip, video, size, status, cancel).await;
    drop(source);
    result
}

/// Runs a session from an arbitrary video source.
///
/// Kept apart from [`run`] so a test pattern can be streamed during field
/// testing without opening the screen capture dialog.
pub async fn run_with_video(
    receiver_ip: IpAddr,
    video: pipeline::VideoSource,
    size: (u32, u32),
    status: &SinkStatus,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    status.set(SinkState::Connecting);

    // Silence Wi-Fi Direct scanning for as long as this lasts (see `nd_core::radio`).
    let _radio = nd_core::radio::quiet();

    // 1. The stream server, on the IP that reaches this receiver.
    let server = StreamServer::bind(receiver_ip).await?;
    let url = server.url();
    tracing::debug!(%url, "URL do stream pronta");

    // 2. The pipeline assembled (in `Ready`) with the encoder this machine
    //    supports.
    //
    // Unlike WFD, here the pipeline only produces frames after the receiver
    // opens the connection, so the encoder cannot be proven beforehand. We use
    // the first candidate (hardware before software) and let the bus error
    // report the failure; the evidence-based fallback lives on the WFD path,
    // where the source runs from the start.
    let driver = detect_gpu_driver();
    let encoder = *pipeline::encoder_candidates(driver)
        .first()
        .ok_or_else(|| {
            NdError::Unsupported(
                "no H.264 encoder available — install gst-plugins-ugly (x264) \
             ou gst-plugins-bad (openh264/va)"
                    .into(),
            )
        })?;
    // The user's screen rarely has the receiver's aspect ratio. Shrinking to
    // fit while preserving that ratio avoids sending 1920x1200 to a 1080p
    // panel (which would rescale) and avoids stretching the picture.
    let (width, height) = StreamConfig::fit_within(
        size,
        StreamConfig::capped_by_preference(pipeline::CHROMECAST_MAX_RESOLUTION),
    );
    if (width, height) != size {
        tracing::info!(
            origem = format!("{}x{}", size.0, size.1),
            enviado = format!("{width}x{height}"),
            "resolution adjusted for the receiver"
        );
    }
    let cfg = StreamConfig {
        width,
        height,
        // The Cast paths have no frame rate to negotiate against, so the
        // preference is the whole answer, capped at what H.264 mirroring
        // receivers accept.
        fps: StreamConfig::capped_fps(60),
        encoder,
        audio: pipeline::AudioSource::detect(),
        ..Default::default()
    };
    // What the session settled on, for the interface to show. The receiver's
    // control port is the one it is reached on, so the same value doubles as
    // the address to measure the link against.
    status.set_link(nd_core::sink::StreamLink {
        width: cfg.width,
        height: cfg.height,
        fps: cfg.fps,
        endpoint: Some(std::net::SocketAddr::new(receiver_ip, crate::cast::PORT)),
    });

    let desc = pipeline::chromecast_pipeline_description(&cfg, &video);
    let (built, mut events) = pipeline::build_pipeline(&desc, cfg.latency_ms())?;
    // From here on, any `?` still takes the pipeline down.
    let guard = PipelineGuard::new(built);
    let gst_pipeline = guard.get().clone();

    let sink = gst_pipeline
        .by_name(CHROMECAST_SINK_NAME)
        .ok_or_else(|| NdError::Gst(format!("element {CHROMECAST_SINK_NAME} not found")))?;

    // When the receiver disconnects, GStreamer hands the socket back: closing
    // it is our responsibility (the C code did the same in
    // `client_socket_removed`).
    sink.connect("client-socket-removed", false, |values| {
        if let Ok(socket) = values[1].get::<gio::Socket>() {
            let _ = socket.close();
        }
        tracing::info!("the receiver disconnected from the stream");
        None
    });

    // 3. Control channel: start the receiver app and tell it to fetch the URL.
    status.set(SinkState::WaitSocket);
    let channel = CastChannel::connect(receiver_ip).await?;
    let app = channel.launch(DEFAULT_MEDIA_RECEIVER).await?;
    channel.load_media(&app, &url, CONTENT_TYPE).await?;
    tracing::info!(%url, "LOAD sent; waiting for the receiver to fetch the stream");

    // 4. Serve the receiver, and only then hit play.
    status.set(SinkState::WaitStreaming);
    let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Records *when* the stream started, so the receiver's delay can be measured.
    let started_at: std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));

    let serve = {
        let pipeline_for_play = gst_pipeline.clone();
        let started = started.clone();
        let started_at = started_at.clone();
        let cancel = cancel.clone();
        async move {
            server
                .serve(
                    sink,
                    move || {
                        started.store(true, std::sync::atomic::Ordering::SeqCst);
                        *started_at
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(std::time::Instant::now());
                        pipeline_for_play
                            .set_state(gst::State::Playing)
                            .map(|_| ())
                            .map_err(|e| NdError::Gst(e.to_string()))
                    },
                    cancel,
                )
                .await
        }
    };
    tokio::pin!(serve);

    // A deadline for the *first* client only: after that the session lasts as
    // long as it lasts. Without it, a receiver that ignores the LOAD would
    // leave everything hanging.
    let first_client = tokio::time::sleep(FIRST_CLIENT_TIMEOUT);
    tokio::pin!(first_client);

    let mut cancel = cancel;
    // The delay stopwatch resets when the receiver opens the connection (that
    // is where the stream starts, from its point of view).
    let mut lag = LagMeter::new();

    // The receiver only sends `MEDIA_STATUS` on a state change; following the
    // delay continuously means asking.
    let mut probe = tokio::time::interval(LAG_PROBE_INTERVAL);
    probe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    probe.tick().await;

    // The receiver advertises `PLAYBACK_RATE` among its supported commands;
    // if it refuses in practice, draining is switched off and we carry on
    // without it.
    let mut drain = DrainController::new();
    let mut playback_rate = 1.0_f64;

    let outcome = loop {
        tokio::select! {
            result = &mut serve => break result,

            // The condition is checked **inside** the branch, not as a
            // `select!` guard: a guard is only re-evaluated when `select!` is
            // re-entered, and `serve` stays pending for the whole session. The
            // receiver would connect, the deadline would fire anyway, and the
            // session died over an error that did not exist.
            _ = &mut first_client => {
                if !started.load(std::sync::atomic::Ordering::SeqCst) {
                    break Err(NdError::Protocol(format!(
                        "the receiver did not fetch the stream within {}s — the receiver app \
                         ter sido fechado na TV",
                        FIRST_CLIENT_TIMEOUT.as_secs()
                    )));
                }
                // It has started: disarm the deadline by pushing it far out.
                first_client
                    .as_mut()
                    .reset(tokio::time::Instant::now() + Duration::from_secs(86_400));
            }

            event = futures::StreamExt::next(&mut events) => {
                match event {
                    Some(pipeline::PipelineEvent::Error { message, debug: details }) => {
                        tracing::error!(%message, %details, "the Chromecast pipeline failed");
                        break Err(NdError::Gst(message));
                    }
                    Some(pipeline::PipelineEvent::Eos) => break Ok(()),
                    // Warnings do not kill the session.
                    Some(_) => continue,
                    None => continue,
                }
            }

            // Spontaneous messages from the receiver. Without reading these, a
            // `LOAD_FAILED` (media refused) went unnoticed and the session sat
            // there "streaming" to a device parked on its home screen.
            event = channel.next_event() => {
                match event {
                    Some(event) => {
                        if event.payload.get("type").and_then(Value::as_str) == Some("MEDIA_STATUS")
                        {
                            lag.observe(&event.payload);
                        }
                        if let Some(err) = media_error(&event) {
                            break Err(NdError::Protocol(err));
                        }
                    }
                    None => break Err(NdError::Protocol(
                        "the receiver closed the control channel".into(),
                    )),
                }
            }

            _ = probe.tick(), if started.load(std::sync::atomic::Ordering::SeqCst) => {
                if let Some(at) = *started_at
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                {
                    lag.start(at);
                }
                let status = match channel
                    .request(NS_MEDIA, &app.transport_id, json!({"type": "GET_STATUS"}))
                    .await
                {
                    Ok(status) => status,
                    Err(err) => {
                        tracing::debug!(%err, "media status query failed");
                        continue;
                    }
                };
                lag.observe(&status);

                // The receiver pre-buffers a few seconds before starting and
                // keeps that slack forever, because it receives at the same
                // rate it plays. Playing back slightly faster consumes the
                // slack and brings the picture closer to real time; once close,
                // it returns to normal speed so the buffer is not emptied
                // (which would make the receiver stall and start over).
                if lag.stalled {
                    drain.on_stall();
                } else {
                    drain.on_stable();
                }

                if let (Some(current), Some(session_id)) =
                    (lag.last(), media_session_id(&status))
                {
                    {
                        let wanted = drain.wanted_rate(current);
                        if let Some(rate) = wanted {
                            if (rate - playback_rate).abs() > f64::EPSILON {
                                match channel
                                    .request(
                                        NS_MEDIA,
                                        &app.transport_id,
                                        json!({
                                            "type": "SET_PLAYBACK_RATE",
                                            "mediaSessionId": session_id,
                                            "playbackRate": rate,
                                        }),
                                    )
                                    .await
                                {
                                    Ok(_) => {
                                        playback_rate = rate;
                                        tracing::info!(
                                            rate,
                                            atraso_s = format!("{current:.2}"),
                                            "playback rate adjusted"
                                        );
                                    }
                                    Err(err) => {
                                        tracing::warn!(%err, "the receiver refused to change the rate");
                                        drain.enabled = false;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            _ = cancel.changed() => break Ok(()),
        }
    };

    if started.load(std::sync::atomic::Ordering::SeqCst) {
        status.set(SinkState::Streaming);
    }

    if let Some(median) = lag.median() {
        tracing::info!(
            atraso_mediano_s = format!("{median:.2}"),
            amostras = lag.samples.len(),
            "receiver delay in this session"
        );
    }

    // Teardown: stop the app on the TV before taking the pipeline down, so the
    // receiver is not left showing a media error.
    let _ = channel.stop_app(&app).await;
    drop(guard);

    outcome
}

/// Measures the receiver's real delay from its `MEDIA_STATUS` messages.
///
/// The receiver reports `currentTime`: the position it is playing at within
/// the stream. Since the stream starts the moment it opens the connection,
/// `elapsed − currentTime` is exactly **how far behind the receiver is** — the
/// depth of its buffer.
///
/// Having that number makes it possible to optimise on evidence rather than
/// asking someone to look at the TV and guess.
struct LagMeter {
    /// The instant the receiver opened the connection — the stream's zero from
    /// its point of view. It only exists once that has happened.
    started: Option<std::time::Instant>,
    samples: Vec<f64>,
    /// The last sample (instant, position), for detecting stalls.
    previous: Option<(std::time::Instant, f64)>,
    /// Did the last observation indicate the receiver stopped advancing?
    stalled: bool,
}

impl LagMeter {
    fn new() -> Self {
        Self {
            started: None,
            samples: Vec::new(),
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
        self.samples.push(lag);
        tracing::info!(
            delay_s = format!("{lag:.2}"),
            position_s = format!("{current:.2}"),
            "receiver delay"
        );
    }

    /// The most recent measured sample.
    fn last(&self) -> Option<f64> {
        self.samples.last().copied()
    }

    /// The samples' median — resistant to the spikes at the start.
    fn median(&self) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
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

/// Turns a receiver message into an error when it signals the media was
/// refused.
///
/// The Default Media Receiver does not close the connection when it cannot
/// play something: it answers `LOAD_FAILED`, or an idle `MEDIA_STATUS` with
/// `idleReason` `ERROR`. Without interpreting that, the app would say
/// "streaming" to a TV sitting on its home screen.
fn media_error(event: &crate::cast::CastEvent) -> Option<String> {
    let kind = event.payload.get("type").and_then(Value::as_str)?;
    match kind {
        "LOAD_FAILED" | "LOAD_CANCELLED" | "INVALID_REQUEST" | "ERROR" => {
            let detail = event
                .payload
                .get("reason")
                .or_else(|| event.payload.get("detailedErrorCode"))
                .map(|v| v.to_string())
                .unwrap_or_else(|| "no detail".into());
            tracing::error!(%kind, %detail, payload = %event.payload, "the receiver refused the media");
            Some(format!(
                "the receiver refused the media ({kind}: {detail}) — the container or \
                 the codec is not accepted by this device"
            ))
        }
        "MEDIA_STATUS" => {
            let idle_error = event
                .payload
                .get("status")
                .and_then(Value::as_array)
                .and_then(|list| list.first())
                .and_then(|st| st.get("idleReason"))
                .and_then(Value::as_str)
                == Some("ERROR");
            if idle_error {
                tracing::error!(payload = %event.payload, "the receiver stopped with a media error");
                Some("the receiver stopped playback with a media error".into())
            } else {
                tracing::debug!(payload = %event.payload, "media status");
                None
            }
        }
        other => {
            tracing::debug!(kind = %other, payload = %event.payload, "receiver event");
            None
        }
    }
}

/// Creates a session's cancellation pair.
pub fn cancellation() -> (SessionHandle, tokio::sync::watch::Receiver<bool>) {
    let (tx, rx) = tokio::sync::watch::channel(false);
    (SessionHandle { cancel: tx }, rx)
}

/// Detects the KMS driver without depending on `nd-net`.
///
/// `nd-chromecast` works under Flatpak, where `nd-net` (NetworkManager, the
/// system bus) cannot operate — hence reading sysfs directly, which the
/// sandbox does allow.
pub(crate) fn detect_gpu_driver() -> pipeline::GpuDriver {
    use pipeline::GpuDriver;

    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return GpuDriver::Unknown;
    };
    let mut cards: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.starts_with("card") && name["card".len()..].chars().all(|c| c.is_ascii_digit())
        })
        .collect();
    cards.sort();

    for card in cards {
        let link = format!("/sys/class/drm/{card}/device/driver");
        if let Ok(target) = std::fs::read_link(&link) {
            if let Some(module) = target.file_name().and_then(|n| n.to_str()) {
                let driver = GpuDriver::from_kernel_module(module);
                if driver != GpuDriver::Unknown {
                    return driver;
                }
            }
        }
    }
    GpuDriver::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_accelerates_only_above_the_target() {
        let drain = DrainController {
            target: 0.9,
            rate: 1.2,
            enabled: true,
            stable: 0,
            floor: LAG_TARGET_MIN,
            stalls: 0,
            frozen: false,
        };
        // Well behind: speed up.
        assert_eq!(drain.wanted_rate(2.6), Some(1.2));
        // Close enough: back to normal.
        assert_eq!(drain.wanted_rate(0.5), Some(1.0));
        // Within the hysteresis: do not flip back and forth for nothing.
        assert_eq!(drain.wanted_rate(1.0), None);
    }

    #[test]
    fn drain_backs_off_when_the_receiver_stalls() {
        // Each stall loosens the target: that is how the logic adapts to a
        // device that cannot hold a small buffer, instead of insisting on a
        // fixed number and leaving the picture stuttering.
        let mut drain = DrainController {
            target: 0.9,
            rate: 1.2,
            enabled: true,
            stable: 0,
            floor: LAG_TARGET_MIN,
            stalls: 0,
            frozen: false,
        };
        drain.on_stall();
        assert!((drain.target - 1.15).abs() < 1e-9, "{}", drain.target);
        // The stall also becomes a floor: nothing below it is tried again.
        assert!(drain.floor > LAG_TARGET_MIN);

        // Successive stalls lead to freezing and then to giving up — at no
        // point does the target run away unbounded.
        for _ in 0..20 {
            drain.on_stall();
        }
        assert!(drain.target <= LAG_TARGET_MAX, "{}", drain.target);
        assert!(!drain.enabled, "it should have given up on draining");
    }

    #[test]
    fn drain_converges_downwards_while_stable() {
        // With no stalls the target tightens on its own — that is how it
        // approaches the device's floor without stuttering along the way.
        let mut drain = DrainController {
            target: 0.9,
            rate: 1.2,
            enabled: true,
            stable: 0,
            floor: LAG_TARGET_MIN,
            stalls: 0,
            frozen: false,
        };
        for _ in 0..LAG_TIGHTEN_AFTER {
            drain.on_stable();
        }
        assert!(drain.target < 0.9, "deveria ter apertado: {}", drain.target);

        // E nunca abaixo do piso conhecido.
        for _ in 0..500 {
            drain.on_stable();
        }
        assert!(drain.target >= LAG_TARGET_MIN, "{}", drain.target);
    }

    #[test]
    fn a_stall_stops_further_tightening() {
        let mut drain = DrainController {
            target: 0.7,
            rate: 1.2,
            enabled: true,
            stable: 0,
            floor: LAG_TARGET_MIN,
            stalls: 0,
            frozen: false,
        };
        let stalled_at = drain.target;
        drain.on_stall();
        assert!(drain.target > stalled_at, "deveria ter afrouxado");

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
        // A field regression: the controller probed the limit forever and
        // re-tightened right after each retreat, hitting the same point — 10
        // stalls in one session, each a visible stutter while watching video.
        let mut drain = DrainController {
            target: 0.9,
            rate: 1.2,
            enabled: true,
            stable: 0,
            floor: LAG_TARGET_MIN,
            stalls: 0,
            frozen: false,
        };

        for _ in 0..MAX_STALL_PROBES {
            drain.on_stall();
        }
        assert!(drain.frozen, "deveria ter congelado");

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
        // A narrow margin made the target scrape the point that had already failed.
        let mut drain = DrainController {
            target: 1.0,
            rate: 1.2,
            enabled: true,
            stable: 0,
            floor: LAG_TARGET_MIN,
            stalls: 0,
            frozen: false,
        };
        drain.on_stall();
        assert!(
            drain.floor >= 1.0 + LAG_STALL_MARGIN - 1e-9,
            "floor with no margin: {}",
            drain.floor
        );
    }

    #[test]
    fn drain_gives_up_when_even_the_frozen_target_stalls() {
        // If not even the final target holds, pushing on would only cause more stutter.
        let mut drain = DrainController {
            target: 0.9,
            rate: 1.2,
            enabled: true,
            stable: 0,
            floor: LAG_TARGET_MIN,
            stalls: 0,
            frozen: false,
        };
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
    fn smooth_preference_disables_draining() {
        set_latency_preference(LatencyPreference::Smooth);
        let drain = DrainController::new();
        assert!(
            !drain.enabled,
            "with smoothness prioritised there is no draining"
        );

        set_latency_preference(LatencyPreference::Responsive);
        let drain = DrainController::new();
        assert!(drain.enabled);
    }

    #[test]
    fn drain_can_be_disabled() {
        let drain = DrainController {
            target: 0.9,
            rate: 1.2,
            enabled: false,
            stable: 0,
            floor: LAG_TARGET_MIN,
            stalls: 0,
            frozen: false,
        };
        assert_eq!(drain.wanted_rate(5.0), None);
    }

    #[test]
    fn lag_meter_ignores_samples_before_the_stream_starts() {
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
        // o buffer esvaziou.
        std::thread::sleep(Duration::from_millis(600));
        meter.observe(&json!({"status": [{"currentTime": 8.05}]}));
        assert!(meter.stalled, "a stalled position should register a stall");
    }

    #[test]
    fn media_session_id_is_extracted() {
        let payload = json!({"status": [{"mediaSessionId": 7, "currentTime": 1.0}]});
        assert_eq!(media_session_id(&payload), Some(7));
        assert_eq!(media_session_id(&json!({"status": []})), None);
    }

    #[test]
    fn load_failure_is_reported_as_an_error() {
        let event = crate::cast::CastEvent {
            namespace: NS_MEDIA.to_string(),
            payload: json!({"type": "LOAD_FAILED", "reason": "MEDIA_UNSUPPORTED"}),
        };
        let err = media_error(&event).expect("it should become an error");
        assert!(err.contains("MEDIA_UNSUPPORTED"), "{err}");
    }

    #[test]
    fn ordinary_media_status_is_not_an_error() {
        let event = crate::cast::CastEvent {
            namespace: NS_MEDIA.to_string(),
            payload: json!({"type": "MEDIA_STATUS", "status": [{"playerState": "PLAYING"}]}),
        };
        assert!(media_error(&event).is_none());
    }

    #[test]
    fn cancellation_signals_the_session() {
        let (handle, mut rx) = cancellation();
        assert!(!*rx.borrow());
        handle.stop();
        assert!(*rx.borrow_and_update());
    }

    #[test]
    fn gpu_detection_never_panics() {
        let _ = detect_gpu_driver();
    }

    #[test]
    fn content_type_matches_the_muxer() {
        // The pipeline uses matroskamux; the LOAD has to announce the same container,
        // or the Default Media Receiver refuses the media.
        let cfg = StreamConfig::default();
        let desc = pipeline::chromecast_pipeline_description(&cfg, &pipeline::VideoSource::Test);
        assert!(desc.contains("matroskamux"), "{desc}");
        assert_eq!(CONTENT_TYPE, "video/x-matroska");
    }
}
