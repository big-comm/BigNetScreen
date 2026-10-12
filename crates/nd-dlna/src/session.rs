//! Running one transmission to a DLNA renderer.
//!
//! ## The order matters
//!
//! 1. the stream server listening;
//! 2. the pipeline built and in `Ready`: `multisocketsink` refuses `add`
//!    while the pipeline is in `Null` with only a warning, and the renderer
//!    then gets zero bytes;
//! 3. the server accepting, and only then `Stop`, `SetAVTransportURI` and
//!    `Play`, concurrently with serving, because a Panasonic fetches the URL
//!    from inside `SetAVTransportURI` rather than after `Play`. Handing it over
//!    first and serving afterwards deadlocks against our own accept loop;
//! 4. the renderer opens the `GET`; the socket goes to the sink and only then
//!    does the pipeline move to `Playing`.
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
use nd_core::stream_server::{DLNA_FILE_MEDIA, DLNA_MEDIA, MediaType, StreamServer};
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
/// A connection held this long means the renderer got past its start.
///
/// A Panasonic starts in steps: two short probes, a connection of about four
/// seconds, then the one it plays from, for as long as the picture is on. It
/// can give up two seconds into either of the last two and not come back, so
/// only a connection that outlasts the four-second step shows the start held.
const SETTLED_CONNECTION: Duration = Duration::from_secs(6);
/// How long the stream must have gone unread, with nothing ever settled,
/// before the renderer is taken to have given up on its start.
///
/// Its transport state is no help: the Panasonic keeps reporting the stream
/// it abandoned for half a minute or more.
const ABANDONED_START: Duration = Duration::from_secs(2);
/// How long after a hand-over the renderer is left to its probing before
/// that is judged.
const START_GRACE: Duration = Duration::from_secs(5);
/// How many times a renderer that gives up during its start is handed the
/// stream, the first included.
const START_ATTEMPTS: u32 = 3;
const SIZE_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How long a screen may take to deliver its first frame before the
/// renderer is handed the stream anyway.
const PICTURE_START_TIMEOUT: Duration = Duration::from_secs(3);
/// How often a file transmission's pause is checked to pass it on.
const PAUSE_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// What the renderer shows as the item's title.
///
/// Not translated: the service runs with whatever environment D-Bus
/// activation gave it, and this string is read off a television.
const ITEM_TITLE: &str = "BigNetScreen";

pub async fn run(
    receiver: IpAddr,
    control: &Endpoint,
    source: CaptureSource,
    status: &SinkStatus,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let result = stream(receiver, control, &source, status, cancel).await;
    drop(source);
    result
}

async fn stream(
    receiver: IpAddr,
    control: &Endpoint,
    source: &CaptureSource,
    status: &SinkStatus,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    status.set(SinkState::Connecting);
    let _radio = nd_core::radio::quiet();

    // A file may be paused by the renderer; a screen may not (see
    // `DLNA_FILE_MEDIA`).
    let media = if source.media.is_some() {
        DLNA_FILE_MEDIA
    } else {
        DLNA_MEDIA
    };
    let server = StreamServer::bind(receiver, media).await?;

    // Portal dimensions use compositor coordinates, not necessarily pixels.
    // Let the scaler negotiate from the actual capture up to the preference.
    // A file comes with the frame it is to be sent in (see
    // `nd_core::media::tv_frame`); a screen takes the person's preference.
    let (width, height) = match (&source.media, source.size) {
        (Some(_), Some(frame)) => frame,
        _ => StreamConfig::preferred_or(pipeline::DLNA_MAX_RESOLUTION),
    };
    let mut cfg = StreamConfig {
        width,
        height,
        // An encoding target, not a frame rate negotiated with the TV.
        fps: StreamConfig::fps_for(&source.video_source(), 60).await,
        audio: source.audio_source(),
        ..Default::default()
    };
    let driver = nd_core::pipeline::GpuDriver::detect();
    cfg.encoder =
        pipeline::working_encoder(driver, pipeline::Acceleration::preferred(), cfg).await?;

    // The padding is what keeps the renderer's byte-counted prebuffer short;
    // see `ts_http_pipeline_description` for the measurements behind it.
    let mux_bitrate = cfg.scaled_bitrate_kbps().saturating_mul(1_000);
    let desc = pipeline::ts_http_pipeline_description(
        &cfg,
        &source.video_source(),
        // A television stretches whatever frame it gets over its whole
        // screen, so a film wider than 16:9 goes inside a 16:9 frame, black
        // bars included.
        if source.media.is_some() {
            pipeline::VideoTarget::Exact((cfg.width, cfg.height))
        } else {
            pipeline::VideoTarget::UpTo((cfg.width, cfg.height))
        },
        Some(mux_bitrate),
    );
    let (built, mut events) = pipeline::build_pipeline(&desc, cfg.latency_ms())?;
    let guard = PipelineGuard::new(built);
    let gst_pipeline = guard.pipeline().clone();
    if let Some(control) = source
        .media
        .as_ref()
        .and_then(|media| media.control.as_ref())
    {
        control.attach(&gst_pipeline);
        if let Some(start) = source.media.as_ref().and_then(|media| media.start) {
            let control = control.clone();
            let mut prepare = tokio::task::spawn_blocking(move || control.prepare(start));
            tokio::select! {
                result = &mut prepare => result.map_err(|err| NdError::Gst(err.to_string()))??,
                _ = cancel.changed() => {
                    let _ = gst_pipeline.set_state(gst::State::Null);
                    let _ = prepare.await;
                    return Ok(());
                }
            }
        }
    }

    status.set(SinkState::WaitSocket);
    if *cancel.borrow() {
        return Ok(());
    }

    tracing::info!(%receiver, "handing the renderer the stream URL");
    let outcome = serve_until_over(
        &server,
        &gst_pipeline,
        &mut events,
        &mut cancel,
        control,
        status,
        cfg,
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
/// The handshake runs while the server is already accepting, which is why this
/// is one function. A Panasonic fetches the URL from inside
/// `SetAVTransportURI` (an unreachable URL answers `500` with UPnP
/// `errorCode 716, Resource not found`). Handshaking first and serving
/// afterwards leaves that connection unaccepted: the SOAP call times out and
/// the session fails with a network error.
async fn serve_until_over(
    server: &StreamServer,
    gst_pipeline: &gst::Pipeline,
    events: &mut pipeline::PipelineEvents,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
    control: &Endpoint,
    status: &SinkStatus,
    mut cfg: StreamConfig,
) -> Result<()> {
    let sink = gst_pipeline
        .by_name(TS_HTTP_SINK_NAME)
        .ok_or_else(|| NdError::Gst(format!("element {TS_HTTP_SINK_NAME} not found")))?;
    let url = &server.url();
    if server.media() == DLNA_MEDIA && !start_picture(gst_pipeline, cancel).await? {
        return Ok(());
    }
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

    // Some TVs fetch only after a slow SetAVTransportURI/Play handshake.
    // Read caps after that fetch, never publish the initial ceiling as fact.
    let mut measure = tokio::time::interval(SIZE_POLL_INTERVAL);
    measure.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut measured = false;

    // A player pauses the file pipeline, which then sends nothing; the
    // renderer has to be told as well, or it gives up on the silent stream.
    // A screen is never paused.
    let pausable = server.media() == DLNA_FILE_MEDIA;
    let paused_here = std::sync::atomic::AtomicBool::new(false);
    let mut pause_watch = tokio::time::interval(PAUSE_POLL_INTERVAL);
    pause_watch.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Run beside the server, like the handshake: a resume may hand the URL
    // over again, and some renderers fetch it from inside that request.
    let telling = futures::FutureExt::fuse(follow_pause(control, url, false, server.media()));
    tokio::pin!(telling);
    telling.set(futures::future::Fuse::terminated());

    let renderer_done = until_renderer_stops(
        control,
        &paused_here,
        gst_pipeline,
        pausable,
        STATE_POLL_INTERVAL,
    );
    tokio::pin!(renderer_done);

    let handshake = hand_over(control, url, server.media());
    tokio::pin!(handshake);
    let mut handed_over = false;
    let mut handed_at = tokio::time::Instant::now();
    let mut attempts = 1;
    let mut settled = false;
    let mut start_watch = tokio::time::interval(Duration::from_millis(500));
    start_watch.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            result = &mut serve => return result,

            // Polling a finished future panics, and a `select!` guard *is*
            // re-evaluated on every loop iteration, which is what makes this
            // the right place for the check.
            result = &mut handshake, if !handed_over => {
                result?;
                handed_over = true;
                handed_at = tokio::time::Instant::now();
                if !started.load(std::sync::atomic::Ordering::SeqCst) {
                    status.set(SinkState::WaitStreaming);
                }
                tracing::info!("Play accepted by the renderer");
            }

            // The viewer pressed stop on the remote, or switched the set off.
            // A normal end, not an error. Not watched before the handshake
            // finishes: until then the renderer has nothing of ours to play.
            () = &mut renderer_done, if handed_over => return Ok(()),

            // The renderer gave up before the picture started, which the same
            // stream, handed over again, usually survives. Once a connection
            // has settled the picture has started, so the watch ends.
            _ = start_watch.tick(), if handed_over && attempts < START_ATTEMPTS && !settled => {
                if server.longest_receiver() >= SETTLED_CONNECTION {
                    settled = true;
                    continue;
                }
                let given_up = handed_at.elapsed() >= START_GRACE
                    && server.abandoned_for().is_some_and(|gap| gap >= ABANDONED_START);
                if !given_up {
                    continue;
                }
                attempts += 1;
                tracing::warn!(
                    attempt = attempts,
                    "the renderer gave up before the picture started; handing the stream over again"
                );
                handed_over = false;
                handshake.set(hand_over(control, url, server.media()));
            }

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

            result = &mut telling => {
                if let Err(err) = result {
                    tracing::warn!(%err, "the renderer did not follow the pause");
                }
            }

            _ = pause_watch.tick(), if handed_over && pausable => {
                if !started.load(std::sync::atomic::Ordering::SeqCst)
                    || !futures::future::FusedFuture::is_terminated(&*telling)
                {
                    continue;
                }
                let paused = heading_for_pause(gst_pipeline);
                if paused == paused_here.load(std::sync::atomic::Ordering::SeqCst) {
                    continue;
                }
                paused_here.store(paused, std::sync::atomic::Ordering::SeqCst);
                telling.set(futures::FutureExt::fuse(follow_pause(control, url, paused, server.media())));
            }

            _ = measure.tick(), if !measured => {
                if !started.load(std::sync::atomic::Ordering::SeqCst) {
                    continue;
                }
                measured = adopt_negotiated_size(gst_pipeline, &mut cfg, control, status);
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

/// Starts a screen's pipeline and waits until its picture flows; `false` if
/// the session was cancelled meanwhile.
///
/// A renderer's first connection lasts a few tens of milliseconds, less than
/// a capture takes to deliver its first frame (about 160 ms for a new virtual
/// screen), and a renderer that finds no key frame there gives up; so the
/// renderer is told where the stream is only after that. A file is not
/// started here: it waits for the first read, or its opening would play to
/// nobody.
async fn start_picture(
    pipeline: &gst::Pipeline,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<bool> {
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| NdError::Gst(e.to_string()))?;
    let deadline = tokio::time::Instant::now() + PICTURE_START_TIMEOUT;
    while pipeline::negotiated_video_size(pipeline).is_none()
        && tokio::time::Instant::now() < deadline
    {
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(10)) => {}
            _ = cancel.changed() => return Ok(false),
        }
    }
    Ok(true)
}

/// Tells the renderer to play `url`.
async fn hand_over(control: &Endpoint, url: &str, media: MediaType) -> Result<()> {
    // A renderer already showing something may refuse a new URI, and one
    // request settles it. Failing here is not fatal: most often there was
    // simply nothing to stop.
    if let Err(err) = avtransport::stop(control).await {
        tracing::debug!(%err, "the renderer had nothing to stop, or would not");
    }
    avtransport::set_uri(control, url, ITEM_TITLE, None, media).await?;
    avtransport::play(control).await
}

/// Whether the pipeline is paused or on its way there: the renderer should
/// hear about a pause as soon as it is asked for, not once it is reached.
fn heading_for_pause(pipeline: &gst::Pipeline) -> bool {
    let (_, current, pending) = pipeline.state(gst::ClockTime::ZERO);
    let target = if pending == gst::State::VoidPending {
        current
    } else {
        pending
    };
    target == gst::State::Paused
}

/// Once caps are negotiated, rates the encoder and multiplex for the real
/// picture size and publishes it. `false` while there is nothing to measure.
fn adopt_negotiated_size(
    gst_pipeline: &gst::Pipeline,
    cfg: &mut StreamConfig,
    control: &Endpoint,
    status: &SinkStatus,
) -> bool {
    let Some((width, height)) = pipeline::negotiated_video_size(gst_pipeline) else {
        return false;
    };
    (cfg.width, cfg.height) = (width, height);
    let kbps = cfg.scaled_bitrate_kbps();
    if pipeline::set_encoder_bitrate(gst_pipeline, cfg.encoder, kbps)
        && let Some(mux) = gst_pipeline.by_name("mux")
    {
        mux.set_property("bitrate", u64::from(kbps) * 1_000);
    }
    status.set_link(StreamLink {
        width,
        height,
        fps: cfg.fps,
        endpoint: Some(control.addr),
        receivers: None,
    });
    let delivered = pipeline::delivered_capture_size(gst_pipeline);
    tracing::info!(
        delivered = ?delivered.map(|(w, h)| format!("{w}x{h}")),
        encoding = format!("{width}x{height}"),
        rescaled = delivered.is_some_and(|d| d != (width, height)),
        "capture negotiated"
    );
    true
}

/// Passes the file pipeline's pause or resume on to the renderer.
async fn follow_pause(control: &Endpoint, url: &str, paused: bool, media: MediaType) -> Result<()> {
    if paused {
        return avtransport::pause(control).await;
    }
    if avtransport::play(control).await.is_ok() {
        return Ok(());
    }
    // A renderer can give up on a stream that stayed silent through a long
    // pause: gmediarender's HTTP source times out after 15 s, retries with a
    // Range this live stream cannot honour, then refuses Play. Handed the URL
    // again, it fetches afresh from the file's current position.
    avtransport::set_uri(control, url, ITEM_TITLE, None, media).await?;
    avtransport::play(control).await
}

/// Resolves when the renderer's own view of the session says it is over.
///
/// Without this, pressing stop on the television's remote leaves us capturing,
/// encoding and sending to nobody: the TCP connection can stay open long after
/// the renderer stopped reading it, so silence on the socket proves nothing.
///
/// A pause is not an end when `paused_here` says the renderer was told to
/// pause because the file pipeline did. A file the renderer can pause
/// (`follows_pause`) also follows a pause from the remote: the pipeline holds
/// until the renderer plays again. Left producing, it would fill a connection
/// the renderer stopped reading until the queue limit ended the session.
async fn until_renderer_stops(
    control: &Endpoint,
    paused_here: &std::sync::atomic::AtomicBool,
    file_pipeline: &gst::Pipeline,
    follows_pause: bool,
    every: Duration,
) {
    use std::sync::atomic::Ordering;
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;

    let mut ever_played = false;
    let mut idle_polls = 0;
    let mut paused_by_renderer = false;
    loop {
        ticker.tick().await;
        match avtransport::transport_state(control).await {
            Ok(TransportState::Playing) => {
                ever_played = true;
                idle_polls = 0;
                // Flag first, with no await between: the pause watcher must
                // not read the resumed pipeline as a pause to pass back.
                if std::mem::take(&mut paused_by_renderer) {
                    paused_here.store(false, Ordering::SeqCst);
                    tracing::info!("the renderer resumed; resuming the file");
                    if let Err(err) = file_pipeline.set_state(gst::State::Playing) {
                        tracing::warn!(%err, "the file did not resume");
                    }
                }
                continue;
            }
            Ok(TransportState::Paused)
                if follows_pause && ever_played && !paused_here.load(Ordering::SeqCst) =>
            {
                paused_by_renderer = true;
                paused_here.store(true, Ordering::SeqCst);
                tracing::info!("the renderer paused; holding the file");
                if let Err(err) = file_pipeline.set_state(gst::State::Paused) {
                    tracing::warn!(%err, "the file did not pause");
                }
                idle_polls = 0;
                continue;
            }
            Ok(TransportState::Transitioning) => {
                idle_polls = 0;
                continue;
            }
            Ok(TransportState::Paused) if paused_here.load(Ordering::SeqCst) => {
                idle_polls = 0;
                continue;
            }
            // Idle before it ever played just means the television has not
            // caught up yet; only a renderer that started and then stopped is
            // a renderer that finished.
            Ok(TransportState::Idle | TransportState::Paused) if !ever_played => continue,
            Ok(TransportState::Idle | TransportState::Paused) => {
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

#[cfg(test)]
mod tests;
