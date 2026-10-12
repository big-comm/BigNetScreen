//! The HTTP path to a Chromecast: the Default Media Receiver fetching an
//! MPEG-TS stream. The sink in `lib.rs` takes it when the receiver has no
//! mirroring app ([`crate::mirror_session`] is the low-latency path).
//!
//! Capture feeds the nd-core pipeline into a `multisocketsink`. The Cast
//! channel ([`crate::cast`]) sends LAUNCH and then LOAD with the URL; the
//! receiver opens the GET, and `nd_core::stream_server` hands that socket to
//! the sink.
//!
//! The order is load-bearing:
//!
//! 1. the HTTP server listens, because the `LOAD` URL has to answer;
//! 2. the pipeline is built and in `Ready`, as `build_pipeline` returns it:
//!    `multisocketsink` refuses `add` in `Null` with only a `WARNING`, and the
//!    receiver then gets zero bytes;
//! 3. `LAUNCH` of the Default Media Receiver and `LOAD` of the URL;
//! 4. the receiver opens the `GET`, the socket goes to the sink, and only then
//!    does the pipeline go to `Playing`.
//!
//! The receiver buffers seconds of a stream it treats as a file; `drain`
//! plays it slightly fast to bring the picture back toward real time.

mod drain;

use std::net::IpAddr;
use std::time::Duration;

use gst::prelude::*;
use gstreamer as gst;

use serde_json::Value;

use nd_core::capture::CaptureSource;
use nd_core::pipeline::{self, PipelineGuard, StreamConfig, TS_HTTP_SINK_NAME};
use nd_core::sink::{SinkState, SinkStatus};
use nd_core::stream_server::{CAST_MEDIA, StreamServer};
use nd_core::{NdError, Result};

use crate::cast::{CastChannel, CastEvent, DEFAULT_MEDIA_RECEIVER, LaunchedApp, NS_MEDIA};
use drain::{BufferDrain, LAG_PROBE_INTERVAL};

/// How long to wait for the receiver to open the `GET` after the `LOAD`.
const FIRST_CLIENT_TIMEOUT: Duration = Duration::from_secs(20);

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
    // The control port the receiver announced over mDNS.
    receiver_port: u16,
    // The discovery identity its device key is remembered under.
    receiver: &str,
    source: CaptureSource,
    status: &SinkStatus,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let video = source.video_source();
    let size = source.size_or((1920, 1080));
    let result = run_with_video(
        receiver_ip,
        receiver_port,
        receiver,
        video,
        size,
        status,
        cancel,
    )
    .await;
    drop(source);
    result
}

/// Runs a session from an arbitrary video source.
///
/// Kept apart from [`run`] so a test pattern can be streamed during field
/// testing without opening the screen capture dialog.
pub async fn run_with_video(
    receiver_ip: IpAddr,
    receiver_port: u16,
    receiver: &str,
    video: pipeline::VideoSource,
    size: (u32, u32),
    status: &SinkStatus,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    status.set(SinkState::Connecting);

    // Silence Wi-Fi Direct scanning for as long as this lasts (see `nd_core::radio`).
    let _radio = nd_core::radio::quiet();

    let server = StreamServer::bind(receiver_ip, CAST_MEDIA).await?;
    let cfg = stream_config(size).await?;
    // The receiver's control port is the one it is reached on, so it doubles
    // as the address the interface measures the link against.
    status.set_link(nd_core::sink::StreamLink {
        width: cfg.width,
        height: cfg.height,
        fps: cfg.fps,
        endpoint: Some(std::net::SocketAddr::new(receiver_ip, receiver_port)),
        receivers: None,
    });
    let mut stream = TsPipeline::build(&cfg, &video)?;

    status.set(SinkState::WaitSocket);
    if *cancel.borrow() {
        return Ok(());
    }
    let channel = CastChannel::connect_to(receiver_ip, receiver_port, receiver).await?;
    if *cancel.borrow() {
        channel.close().await;
        return Ok(());
    }
    let app = match channel.launch(DEFAULT_MEDIA_RECEIVER).await {
        Ok(app) => app,
        Err(err) => {
            channel.close().await;
            return Err(err);
        }
    };
    let result = if *cancel.borrow() {
        Ok(())
    } else {
        serve_receiver(&channel, &app, &server, &mut stream, status, cancel).await
    };
    // The app on the TV is stopped before the pipeline goes down, so the
    // receiver is not left showing a media error.
    let result = channel.finish_app(&app, result).await;
    drop(stream);
    result
}

/// The mode to encode: the capture shrunk to fit the preferred ceiling with
/// its aspect ratio kept, and an encoder proven to start at that mode.
async fn stream_config(size: (u32, u32)) -> Result<StreamConfig> {
    // Fitting avoids sending 1920x1200 to a 1080p panel, which would rescale
    // it, and avoids stretching the picture.
    let (width, height) = StreamConfig::fit_within(
        size,
        StreamConfig::preferred_or(pipeline::CHROMECAST_MAX_RESOLUTION),
    );
    if (width, height) != size {
        tracing::info!(
            source = format!("{}x{}", size.0, size.1),
            sent = format!("{width}x{height}"),
            "resolution adjusted for the receiver"
        );
    }
    let mut cfg = StreamConfig {
        width,
        height,
        // The Default Media Receiver has no OFFER/ANSWER to negotiate a rate
        // against, so this path stays at a compatibility ceiling.
        fps: StreamConfig::capped_fps(30),
        audio: pipeline::AudioSource::detect(),
        ..Default::default()
    };
    cfg.encoder = pipeline::working_encoder(
        pipeline::GpuDriver::detect(),
        pipeline::Acceleration::preferred(),
        cfg,
    )
    .await?;
    Ok(cfg)
}

/// The MPEG-TS pipeline, in `Ready` until the receiver connects, and taken to
/// `Null` when dropped.
struct TsPipeline {
    guard: PipelineGuard,
    sink: gst::Element,
    events: pipeline::PipelineEvents,
}

impl TsPipeline {
    fn build(cfg: &StreamConfig, video: &pipeline::VideoSource) -> Result<Self> {
        // Nothing was promised to this receiver, which reads the resolution
        // out of the stream, so encode what was captured rather than the
        // portal's compositor-space guess about it. See `VideoTarget`.
        let ceiling = StreamConfig::preferred_or(pipeline::CHROMECAST_MAX_RESOLUTION);
        let desc = pipeline::ts_http_pipeline_description(
            cfg,
            video,
            pipeline::VideoTarget::UpTo(ceiling),
            None,
        );
        let (built, events) = pipeline::build_pipeline(&desc, cfg.latency_ms())?;
        let guard = PipelineGuard::new(built);
        let sink = guard
            .pipeline()
            .by_name(TS_HTTP_SINK_NAME)
            .ok_or_else(|| NdError::Gst(format!("element {TS_HTTP_SINK_NAME} not found")))?;
        Ok(Self {
            guard,
            sink,
            events,
        })
    }
}

/// LOADs the stream URL and serves the receiver until the session ends.
///
/// The pipeline goes to `Playing` only when the receiver opens the `GET`:
/// started earlier, the encoder runs into the void and burns the first key
/// frame.
async fn serve_receiver(
    channel: &CastChannel,
    app: &LaunchedApp,
    server: &StreamServer,
    stream: &mut TsPipeline,
    status: &SinkStatus,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    channel
        .load_media(app, &server.url(), CAST_MEDIA.content_type)
        .await?;
    tracing::info!("LOAD sent; waiting for the receiver to fetch the stream");
    status.set(SinkState::WaitStreaming);

    // When the receiver opened the connection: the stream's zero from its
    // point of view, and what its delay is measured from.
    let started_at = std::sync::OnceLock::new();
    let playing = stream.guard.pipeline().clone();
    let serve = server.serve(
        stream.sink.clone(),
        || {
            let _ = started_at.set(std::time::Instant::now());
            status.set(SinkState::Streaming);
            playing
                .set_state(gst::State::Playing)
                .map(|_| ())
                .map_err(|e| NdError::Gst(e.to_string()))
        },
        cancel.clone(),
    );
    tokio::pin!(serve);

    // A deadline for the first client only: without it, a receiver that
    // ignores the LOAD leaves the session hanging.
    let first_client = tokio::time::sleep(FIRST_CLIENT_TIMEOUT);
    tokio::pin!(first_client);

    // The receiver only sends `MEDIA_STATUS` on a state change; following the
    // delay continuously means asking.
    let mut probe = tokio::time::interval(LAG_PROBE_INTERVAL);
    probe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    probe.tick().await;
    let mut drain = BufferDrain::new();

    let outcome = loop {
        tokio::select! {
            result = &mut serve => break result,

            // Checked inside the branch, not as a `select!` guard: a guard is
            // only re-evaluated when `select!` is re-entered, and `serve` stays
            // pending for the whole session.
            _ = &mut first_client => {
                if started_at.get().is_none() {
                    break Err(NdError::Protocol(format!(
                        "the receiver did not fetch the stream within {}s — the receiver app \
                         may have been closed on the TV",
                        FIRST_CLIENT_TIMEOUT.as_secs()
                    )));
                }
                first_client
                    .as_mut()
                    .reset(tokio::time::Instant::now() + Duration::from_secs(86_400));
            }

            event = futures::StreamExt::next(&mut stream.events) => match event {
                Some(pipeline::PipelineEvent::Error { message, debug: details }) => {
                    tracing::error!(%message, %details, "the Chromecast pipeline failed");
                    break Err(NdError::Gst(message));
                }
                Some(pipeline::PipelineEvent::Eos) => break Ok(()),
                Some(pipeline::PipelineEvent::Warning { .. }) => {}
                None => {
                    break Err(NdError::Gst("the Chromecast pipeline event stream closed".into()));
                }
            },

            // A receiver that refuses the media says so here and keeps the
            // connection open, parked on its home screen.
            event = channel.next_event() => {
                if let Some(outcome) = receiver_event(event, app, &mut drain) {
                    break outcome;
                }
            }

            _ = probe.tick(), if started_at.get().is_some() => {
                if let Some(started) = started_at.get() {
                    drain.probe(channel, app, *started).await;
                }
            }

            _ = cancel.changed() => break Ok(()),
        }
    };
    drain.log_summary();
    outcome
}

/// What a message from the receiver means for the session: `Some` ends it.
///
/// A receiver that refuses the media says so here and keeps the connection
/// open, parked on its home screen.
fn receiver_event(
    event: Option<CastEvent>,
    app: &LaunchedApp,
    drain: &mut BufferDrain,
) -> Option<Result<()>> {
    let Some(event) = event else {
        return Some(Err(NdError::Protocol(
            "the receiver closed the control channel".into(),
        )));
    };
    if event.closes(app) {
        return Some(Ok(()));
    }
    if event.source_id != app.transport_id || event.namespace != NS_MEDIA {
        return None;
    }
    drain.observe(&event.payload);
    media_error(&event).map(|err| Err(NdError::Protocol(err)))
}

/// Turns a receiver message into an error when it signals the media was
/// refused.
///
/// The Default Media Receiver does not close the connection when it cannot
/// play something: it answers `LOAD_FAILED`, or an idle `MEDIA_STATUS` with
/// `idleReason` `ERROR`. Without interpreting that, the app would say
/// "streaming" to a TV sitting on its home screen.
fn media_error(event: &CastEvent) -> Option<String> {
    let kind = event.payload.get("type").and_then(Value::as_str)?;
    match kind {
        "LOAD_FAILED" | "LOAD_CANCELLED" | "INVALID_REQUEST" | "ERROR" => {
            let detail = event
                .payload
                .get("reason")
                .or_else(|| event.payload.get("detailedErrorCode"))
                .map(|v| v.to_string())
                .unwrap_or_else(|| "no detail".into());
            tracing::error!(%kind, %detail, "the receiver refused the media");
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
                tracing::error!("the receiver stopped with a media error");
                Some("the receiver stopped playback with a media error".into())
            } else {
                tracing::debug!("media status");
                None
            }
        }
        other => {
            tracing::debug!(kind = %other, "receiver event");
            None
        }
    }
}

/// Creates a session's cancellation pair.
pub fn cancellation() -> (SessionHandle, tokio::sync::watch::Receiver<bool>) {
    let (tx, rx) = tokio::sync::watch::channel(false);
    (SessionHandle { cancel: tx }, rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn load_failure_is_an_error() {
        let event = CastEvent {
            source_id: "transport-test".into(),
            namespace: NS_MEDIA.to_string(),
            payload: json!({"type": "LOAD_FAILED", "reason": "MEDIA_UNSUPPORTED"}),
        };
        let err = media_error(&event).expect("it should become an error");
        assert!(err.contains("MEDIA_UNSUPPORTED"), "{err}");
    }

    #[test]
    fn ordinary_media_status_passes() {
        let event = CastEvent {
            source_id: "transport-test".into(),
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
    fn content_type_matches_the_muxer() {
        // The pipeline uses mpegtsmux; LOAD must announce the same container,
        // or the Default Media Receiver refuses the media.
        let cfg = StreamConfig::default();
        let desc = pipeline::ts_http_pipeline_description(
            &cfg,
            &pipeline::VideoSource::Test,
            nd_core::pipeline::VideoTarget::Exact((cfg.width, cfg.height)),
            None,
        );
        assert!(desc.contains("mpegtsmux"), "{desc}");
        assert_eq!(CAST_MEDIA.content_type, "video/mp2t");
    }
}
