//! Running one transmission to a DLNA renderer.
//!
//! ## The order matters, and getting it wrong cost a field run
//!
//! 1. **the stream server listening**;
//! 2. **the pipeline built and in `Ready`**: `multisocketsink` refuses `add`
//!    while the pipeline is in `Null`, and refuses it with nothing louder than
//!    a warning — the renderer would get zero bytes and the session would fail
//!    with everything apparently fine;
//! 3. **the server accepting**, and only then `Stop`, `SetAVTransportURI`,
//!    `Play` — *concurrently*, because a Panasonic fetches the URL from inside
//!    `SetAVTransportURI` rather than after `Play`. Handing it over first and
//!    serving afterwards deadlocks the handshake against our own accept loop;
//! 4. the renderer opens the `GET`; the socket goes to the sink and **only
//!    then** does the pipeline move to `Playing`.
//!
//! Going to `Playing` before there is a client burns the first keyframe into
//! nothing and delays the picture by a whole GOP.

use std::net::IpAddr;
use std::time::Duration;

use gst::prelude::*;
use gstreamer as gst;

use nd_core::capture::CaptureSource;
use nd_core::pipeline::{self, PipelineGuard, StreamConfig, TS_HTTP_SINK_NAME};
use nd_core::sink::{SinkState, SinkStatus, StreamLink};
use nd_core::stream_server::{StreamServer, DLNA_MEDIA};
use nd_core::{NdError, Result};

use crate::avtransport::{self, TransportState};
use crate::upnp::Endpoint;

/// How long the renderer gets to open the `GET` after `Play`.
///
/// Generous: a television that was showing something else has to switch input
/// and start its media player first.
const FIRST_CLIENT_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the renderer is asked whether it is still playing.
const STATE_POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Consecutive idle answers before the session is called over.
///
/// Two, because a renderer briefly reports `STOPPED` between loading and
/// playing on some firmware, and ending the session there would make it
/// impossible to ever start one.
const IDLE_POLLS_BEFORE_ENDING: u32 = 2;
/// How long after starting to ask the pipeline what size it settled on.
const SIZE_REPORT_DELAY: Duration = Duration::from_secs(2);

/// What the renderer shows as the item's title.
///
/// Not translated, and deliberately so: the service runs with whatever
/// environment D-Bus activation gave it, and this string is read off a
/// television, not out of a window.
const ITEM_TITLE: &str = "BigNetScreen";

pub async fn run(
    receiver: IpAddr,
    control: &Endpoint,
    source: CaptureSource,
    status: &SinkStatus,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let video = source.video_source();
    let size = source.size_or(pipeline::DLNA_MAX_RESOLUTION);
    let result = stream(receiver, control, video, size, status, cancel).await;
    drop(source);
    result
}

async fn stream(
    receiver: IpAddr,
    control: &Endpoint,
    video: pipeline::VideoSource,
    size: (u32, u32),
    status: &SinkStatus,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    status.set(SinkState::Connecting);
    let _radio = nd_core::radio::quiet();

    let server = StreamServer::bind(receiver, DLNA_MEDIA).await?;
    let url = server.url();

    // The screen rarely has the television's aspect ratio; shrinking to fit
    // keeps the picture from being stretched or rescaled on the panel.
    let (width, height) = StreamConfig::fit_within(
        size,
        StreamConfig::preferred_or(pipeline::DLNA_MAX_RESOLUTION),
    );
    if (width, height) != size {
        tracing::info!(
            source = format!("{}x{}", size.0, size.1),
            sent = format!("{width}x{height}"),
            "resolution adjusted for the renderer"
        );
    }
    let mut cfg = StreamConfig {
        width,
        height,
        // A renderer negotiates nothing about frame rate, so this is a
        // ceiling rather than an agreement — and the ceiling is the person's
        // own setting. 60 because the television says so: its `GetProtocolInfo`
        // lists `AVC_TS_HD_60_AC3`, and the `_24`/`_50`/`_60` in those profile
        // names *are* the frame rate. Capping at 30 here overrode a setting of
        // 60 with a number nothing asked for.
        fps: StreamConfig::capped_fps(60),
        audio: pipeline::AudioSource::detect(),
        ..Default::default()
    };
    let driver = nd_net::detect_gpu_driver();
    cfg.encoder =
        pipeline::working_encoder(driver, pipeline::Acceleration::preferred(), cfg).await?;

    status.set_link(StreamLink {
        width: cfg.width,
        height: cfg.height,
        fps: cfg.fps,
        endpoint: Some(control.addr),
        receivers: None,
    });

    // The padding is what keeps the renderer's byte-counted prebuffer short;
    // see `ts_http_pipeline_description` for the measurements behind it.
    let mux_bitrate = cfg.scaled_bitrate_kbps().saturating_mul(1_000);
    let desc = pipeline::ts_http_pipeline_description(
        &cfg,
        &video,
        pipeline::VideoTarget::Exact((cfg.width, cfg.height)),
        Some(mux_bitrate),
    );
    let (built, mut events) = pipeline::build_pipeline(&desc, cfg.latency_ms())?;
    let guard = PipelineGuard::new(built);
    let gst_pipeline = guard.pipeline().clone();

    let sink = gst_pipeline
        .by_name(TS_HTTP_SINK_NAME)
        .ok_or_else(|| NdError::Gst(format!("element {TS_HTTP_SINK_NAME} not found")))?;

    status.set(SinkState::WaitSocket);
    if *cancel.borrow() {
        return Ok(());
    }

    tracing::info!(%receiver, "handing the renderer the stream URL");
    let asked = (cfg.width, cfg.height);
    let outcome = serve_until_over(
        &server,
        sink,
        &gst_pipeline,
        &mut events,
        &mut cancel,
        control,
        status,
        &url,
        asked,
    )
    .await;

    // Always told, even on the way out of an error: a renderer left holding a
    // URL that stopped answering shows a frozen frame or an error card until
    // somebody picks up the remote.
    if let Err(err) = avtransport::stop(control).await {
        tracing::warn!(%err, "the renderer did not confirm the transmission ended");
    }
    drop(guard);
    outcome
}

/// Hands the renderer the URL and serves it until the session is over.
///
/// **The handshake runs while the server is already accepting**, and that
/// ordering is the whole reason this is one function. A Panasonic fetches the
/// URL from inside `SetAVTransportURI` — proven by pointing it at an
/// unreachable address, which came back `500` with UPnP `errorCode 716,
/// Resource not found`, a verdict it could only have reached by trying. Doing
/// the handshake first and serving afterwards means nobody accepts that
/// connection: the television waits, the SOAP call times out, and the session
/// dies with a network error that names the wrong culprit.
#[allow(clippy::too_many_arguments)]
async fn serve_until_over(
    server: &StreamServer,
    sink: gst::Element,
    gst_pipeline: &gst::Pipeline,
    events: &mut pipeline::PipelineEvents,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
    control: &Endpoint,
    status: &SinkStatus,
    url: &str,
    asked: (u32, u32),
) -> Result<()> {
    let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let serve = {
        let pipeline_for_play = gst_pipeline.clone();
        let started = started.clone();
        let cancel = cancel.clone();
        async move {
            server
                .serve(
                    sink,
                    move || {
                        started.store(true, std::sync::atomic::Ordering::SeqCst);
                        // The renderer is reading: that is the moment the
                        // screen is actually on the television, and the moment
                        // the interface should stop saying "preparing".
                        status.set(SinkState::Streaming);
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

    let deadline = tokio::time::sleep(FIRST_CLIENT_TIMEOUT);
    tokio::pin!(deadline);

    // Caps do not exist until frames flow, so asking before the renderer
    // connects only ever answers `None` — which reads like a fault and is
    // just a question asked too early. This fires once, shortly after the
    // stream starts, and is the only place that can say whether the capture
    // was rescaled on the way to the encoder.
    let measure = tokio::time::sleep(SIZE_REPORT_DELAY);
    tokio::pin!(measure);
    let mut measured = false;

    let renderer_done = until_renderer_stops(control);
    tokio::pin!(renderer_done);

    let handshake = async {
        // A renderer already showing something may refuse a new URI, and one
        // request settles it. Failing here is not fatal: most often there was
        // simply nothing to stop.
        if let Err(err) = avtransport::stop(control).await {
            tracing::debug!(%err, "the renderer had nothing to stop, or would not");
        }
        avtransport::set_uri(control, url, ITEM_TITLE, asked).await?;
        avtransport::play(control).await
    };
    tokio::pin!(handshake);
    let mut handed_over = false;

    loop {
        tokio::select! {
            result = &mut serve => return result,

            // Polling a finished future panics, and a `select!` guard *is*
            // re-evaluated on every loop iteration, which is what makes this
            // the right place for the check.
            result = &mut handshake, if !handed_over => {
                result?;
                handed_over = true;
                status.set(SinkState::WaitStreaming);
                tracing::info!("Play accepted; waiting for the renderer to fetch the stream");
            }

            // The viewer pressed stop on the remote, or switched the set off.
            // A normal end, not an error. Not watched before the handshake
            // finishes: until then the renderer has nothing of ours to play.
            () = &mut renderer_done, if handed_over => return Ok(()),

            // The check lives inside the branch, not in a `select!` guard: a
            // guard is only re-evaluated when `select!` is re-entered, and
            // `serve` stays pending for the whole session.
            _ = &mut deadline => {
                if !started.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err(NdError::Protocol(format!(
                        "the renderer never fetched the stream within {}s",
                        FIRST_CLIENT_TIMEOUT.as_secs()
                    )));
                }
                // Disarm: from here the session lasts as long as it lasts.
                deadline
                    .as_mut()
                    .reset(tokio::time::Instant::now() + Duration::from_secs(86_400));
            }

            () = &mut measure, if !measured => {
                measured = true;
                let delivered = pipeline::delivered_capture_size(gst_pipeline);
                tracing::info!(
                    delivered = ?delivered.map(|(w, h)| format!("{w}x{h}")),
                    encoding = format!("{}x{}", asked.0, asked.1),
                    rescaled = delivered.is_some_and(|d| d != asked),
                    "capture negotiated"
                );
            }

            event = futures::StreamExt::next(events) => match event {
                Some(pipeline::PipelineEvent::Error { message, debug: details }) => {
                    tracing::error!(%message, %details, "the DLNA pipeline failed");
                    return Err(NdError::Gst(message));
                }
                Some(pipeline::PipelineEvent::Eos) => return Ok(()),
                Some(_) => continue,
                None => return Err(NdError::Gst("the DLNA pipeline event stream closed".into())),
            },
        }
    }
}

/// Resolves when the renderer's own view of the session says it is over.
///
/// Without this, pressing stop on the television's remote leaves us capturing,
/// encoding and sending to nobody: the TCP connection can stay open long after
/// the renderer stopped reading it, so silence on the socket proves nothing.
async fn until_renderer_stops(control: &Endpoint) {
    let mut ticker = tokio::time::interval(STATE_POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;

    let mut ever_played = false;
    let mut idle_polls = 0;
    loop {
        ticker.tick().await;
        match avtransport::transport_state(control).await {
            Ok(TransportState::Playing) => {
                ever_played = true;
                idle_polls = 0;
                continue;
            }
            Ok(TransportState::Transitioning) => {
                idle_polls = 0;
                continue;
            }
            // Idle before it ever played just means the television has not
            // caught up yet; only a renderer that started and then stopped is
            // a renderer that finished.
            Ok(TransportState::Idle) if !ever_played => continue,
            Ok(TransportState::Idle) => {
                idle_polls += 1;
                if idle_polls >= IDLE_POLLS_BEFORE_ENDING {
                    tracing::info!("the renderer stopped playing; ending the transmission");
                    return;
                }
            }
            // A renderer that stopped answering is a television that was
            // switched off. Same conclusion, reached from the other side.
            Err(err) => {
                idle_polls += 1;
                if idle_polls >= IDLE_POLLS_BEFORE_ENDING {
                    tracing::info!(%err, "the renderer stopped answering; ending the transmission");
                    return;
                }
            }
        }
    }
}
