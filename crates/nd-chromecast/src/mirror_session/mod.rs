//! A Cast mirroring session: negotiate, encode and stream.
//!
//! Capture feeds a pipeline ending in an appsink with no container; each
//! H.264 or Opus frame is encrypted with AES-CTR-128, packetised into Cast RTP
//! and sent over UDP to the receiver.
//!
//! No muxer, no HTTP and no media player on the other end: the delay is the
//! negotiated `targetDelay` (the receiver's jitter buffer) rather than the
//! Default Media Receiver's seconds of pre-buffering ([`crate::session`]).
//!
//! Retransmission is not optional. The transport is UDP with no error
//! correction; the receiver declares what it lost and waits for the resend,
//! and without it one lost datagram freezes the picture while the receiver
//! keeps sending healthy-looking feedback.
//!
//! `sender` puts one track on the wire, `worker` drives it on its own
//! thread, `feedback` routes what the receiver sends back, and `pacing`
//! bounds the bursts every send goes through.

mod feedback;
mod pacing;
mod sender;
mod worker;

pub(crate) use pacing::MAX_VIDEO_BITRATE_KBPS;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gst::prelude::*;
use gstreamer as gst;

use nd_core::pipeline::{self, MIRROR_AUDIO_SINK, MIRROR_VIDEO_SINK, StreamConfig};
use nd_core::sink::{SinkState, SinkStatus};
use nd_core::{NdError, Result};

use crate::cast::{CastChannel, LaunchedApp};
use crate::mirror::{self, MIRRORING_APP_ID, MirrorConfig, Negotiated};
use feedback::FeedbackRoutes;
use pacing::BurstPacer;
use sender::StreamSender;
use worker::{SessionSignals, VideoControl, Worker};

/// Does the receiver lack mirroring support, so that the HTTP path is worth
/// a try?
///
/// Only that case justifies falling back to the Default Media Receiver:
/// restarting over HTTP after the stream broke midway trades one failure for
/// another at many times the delay. The error type carries the distinction;
/// message text does not.
pub fn is_unsupported(err: &NdError) -> bool {
    matches!(err, NdError::Unsupported(_))
}

/// Runs a mirroring session until cancellation or end of stream.
#[expect(
    clippy::too_many_arguments,
    reason = "the session's own inputs, passed through once"
)]
pub async fn run(
    receiver_ip: IpAddr,
    // The control port the receiver announced over mDNS.
    receiver_port: u16,
    // The discovery identity its device key is remembered under.
    receiver: &str,
    video: pipeline::VideoSource,
    // A file played in place of the screen: its controls and where it starts.
    media: Option<&nd_core::capture::MediaPlayback>,
    size: (u32, u32),
    status: &SinkStatus,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    status.set(SinkState::Connecting);

    // Silence Wi-Fi Direct scanning for as long as this lasts: it uses the
    // same antenna and interrupts the link to the receiver (see
    // `nd_core::radio`).
    let _radio = nd_core::radio::quiet();

    if *cancel.borrow() {
        return Ok(());
    }
    let channel = CastChannel::connect_to(receiver_ip, receiver_port, receiver).await?;
    if *cancel.borrow() {
        channel.close().await;
        return Ok(());
    }
    let app = match channel.launch(MIRRORING_APP_ID).await {
        Ok(app) => app,
        Err(err) => {
            channel.close().await;
            return Err(err);
        }
    };
    let result = if *cancel.borrow() {
        Ok(())
    } else {
        status.set(SinkState::WaitSocket);
        mirror(
            &channel,
            &app,
            receiver_ip,
            receiver_port,
            &video,
            media,
            size,
            status,
            &mut cancel,
        )
        .await
    };
    channel.finish_app(&app, result).await
}

/// Starts the capture, negotiates the mode with the receiver and streams it.
#[expect(
    clippy::too_many_arguments,
    reason = "the session's own inputs, passed through once"
)]
async fn mirror(
    channel: &CastChannel,
    app: &LaunchedApp,
    receiver_ip: IpAddr,
    receiver_port: u16,
    video: &pipeline::VideoSource,
    media: Option<&nd_core::capture::MediaPlayback>,
    size: (u32, u32),
    status: &SinkStatus,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let mut cfg = mirror_config(video, size).await?;
    if *cancel.borrow() {
        return Ok(());
    }

    let (gst_pipeline, events, _capture) = start_capture(&cfg, video, size)?;
    // A played file answers position, pause and seek through its control,
    // and starts where the player left it, as on a DLNA television.
    if let Some(control) = media.and_then(|media| media.control.as_ref()) {
        control.attach(&gst_pipeline);
        if let Some(start) = media.and_then(|media| media.start) {
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
    adopt_capture_size(&mut cfg, &gst_pipeline).await;

    let Some(session) =
        negotiate_mode(channel, app, receiver_ip, &mut cfg, &gst_pipeline, cancel).await?
    else {
        return Ok(());
    };
    // What goes on the wall is what the receiver agreed to.
    status.set_link(nd_core::sink::StreamLink {
        width: cfg.width,
        height: cfg.height,
        fps: cfg.fps,
        endpoint: Some(SocketAddr::new(receiver_ip, receiver_port)),
        receivers: None,
    });

    let (stop, mut stopped) = tokio::sync::watch::channel(*cancel.borrow());
    let streaming = stream(
        &cfg,
        gst_pipeline.clone(),
        events,
        &session,
        status,
        &mut stopped,
    );
    tokio::pin!(streaming);
    loop {
        tokio::select! {
            result = &mut streaming => break result,
            _ = cancel.changed() => {
                let _ = stop.send(true);
                break streaming.await;
            }
            event = channel.next_event() => {
                let outcome = match event {
                    None => Some(Err(NdError::Protocol("Cast control channel closed".into()))),
                    Some(event) if event.closes(app) => Some(Ok(())),
                    _ => None,
                };
                if let Some(outcome) = outcome {
                    let _ = stop.send(true);
                    let stopped = streaming.await;
                    break outcome.and(stopped);
                }
            }
        }
    }
}

/// Offers the captured mode at the bitrate it needs and applies the
/// receiver's answer to `cfg`, rescaling the capture if it asked for less.
/// `None` when cancelled.
async fn negotiate_mode(
    channel: &CastChannel,
    app: &LaunchedApp,
    receiver_ip: IpAddr,
    cfg: &mut StreamConfig,
    gst_pipeline: &gst::Pipeline,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<Option<Negotiated>> {
    // Offer the bitrate this mode needs: a bigger, faster picture sent at
    // 1080p30's bitrate arrives smeared. The receiver's own `maxBitRate`
    // still caps it in `constrain`.
    cfg.bitrate_kbps = 0;
    let wanted = cfg.scaled_bitrate_kbps();
    cfg.bitrate_kbps = mirror::cast_bitrate_kbps(cfg);
    if cfg.bitrate_kbps < wanted {
        tracing::info!(
            wanted_kbps = wanted,
            sending_kbps = cfg.bitrate_kbps,
            "this mode wants more bitrate than the sender can push; a lower \
             frame rate would spend the same bits on fewer, sharper frames"
        );
    }
    let mirror_cfg = MirrorConfig {
        width: cfg.width,
        height: cfg.height,
        fps: cfg.fps,
        max_bitrate: cfg.bitrate_kbps * 1000,
        with_audio: true,
        ..Default::default()
    };
    let session = tokio::select! {
        result = mirror::negotiate(channel, app, receiver_ip, &mirror_cfg) => result?,
        _ = cancel.changed() => return Ok(None),
    };

    let offered = (cfg.width, cfg.height);
    session
        .answer
        .constrain(cfg, mirror_cfg.target_delay_ms, session.audio().is_some())?;
    if *cancel.borrow() {
        return Ok(None);
    }
    // Only a receiver that asked for something smaller makes the scaler work.
    if (cfg.width, cfg.height) != offered {
        pipeline::pin_video_size(gst_pipeline, cfg)?;
    }
    tracing::info!(
        width = cfg.width,
        height = cfg.height,
        fps = cfg.fps,
        bitrate_kbps = cfg.bitrate_kbps,
        rescaled = (cfg.width, cfg.height) != offered,
        "Cast transmission mode after receiver constraints"
    );
    Ok(Some(session))
}

/// The mode to offer before the receiver has spoken, with an encoder proven
/// to start at it.
async fn mirror_config(video: &pipeline::VideoSource, size: (u32, u32)) -> Result<StreamConfig> {
    let (width, height) = StreamConfig::fit_within(
        size,
        StreamConfig::preferred_or(pipeline::CHROMECAST_MAX_RESOLUTION),
    );
    let mut cfg = StreamConfig {
        width,
        height,
        // No frame rate to negotiate against: the preference is the whole
        // answer, capped at what H.264 mirroring receivers accept.
        fps: StreamConfig::fps_for(video, 60).await,
        // A whole-screen change, such as switching a browser tab, otherwise
        // becomes one enormous frame: hundreds of datagrams the pacer lets out
        // a burst at a time, and the update shows only when the last arrives.
        // The encoder cannot know a frame is a burst on a paced link.
        vbv_frames: pipeline::CAST_VBV_FRAMES,
        // The receiver asks for a key frame when it needs one, and NACK
        // repairs loss before that; a scheduled one only makes the picture
        // pulse.
        gop_seconds: 0,
        // Google documents H.264 High for Cast playback; mirroring
        // interoperability across receivers still needs validation.
        profile: pipeline::H264Profile::High,
        // A file's sound comes from its own decoder; the computer's would be
        // silent, or another application.
        audio: if video.is_media() {
            pipeline::AudioSource::MediaFile
        } else {
            pipeline::AudioSource::detect()
        },
        ..Default::default()
    };
    // Chosen before the receiver has spoken, because the pipeline has to exist
    // before there is a size to offer. A GPU that encodes the portal's guess
    // encodes the real size: they differ by the compositor's scale factor.
    cfg.encoder = pipeline::working_encoder(
        pipeline::GpuDriver::detect(),
        pipeline::Acceleration::preferred(),
        cfg,
    )
    .await?;
    // The thread count is also the slice count for software encoding.
    tracing::info!(
        encoder = ?cfg.encoder,
        threads = (!cfg.encoder.is_hardware()).then(pipeline::software_encode_threads),
        "encoder in use"
    );
    Ok(cfg)
}

/// Builds and starts the capture.
fn start_capture(
    cfg: &StreamConfig,
    video: &pipeline::VideoSource,
    size: (u32, u32),
) -> Result<(gst::Pipeline, pipeline::PipelineEvents, StreamWorkers)> {
    let ceiling = StreamConfig::preferred_or(pipeline::CHROMECAST_MAX_RESOLUTION);
    // The GPU path cannot scale, so it is only eligible while the screen fits
    // inside the ceiling, which the portal's size is accurate enough to decide.
    let gpu = pipeline::gpu_path_requested()
        && cfg.encoder == pipeline::H264Encoder::NvH264
        && size.0 <= ceiling.0
        && size.1 <= ceiling.1;
    tracing::info!(gpu, "mirroring capture path");
    let desc = pipeline::mirror_pipeline_description(cfg, video, ceiling, gpu);
    let (gst_pipeline, events) = pipeline::build_pipeline(&desc, cfg.latency_ms())?;
    let capture = StreamWorkers::new(&gst_pipeline);
    gst_pipeline
        .set_state(gst::State::Playing)
        .map_err(|err| NdError::Gst(err.to_string()))?;
    // Frames produced before the senders exist are dropped by the leaky
    // video queue; nothing reaches the network until the ANSWER arrives.
    Ok((gst_pipeline, events, capture))
}

/// Waits for the capture to negotiate and takes its size into `cfg`. Until
/// then every size in play is the portal's compositor-space guess (`size_or`
/// documents why that is not pixels), and the OFFER must not carry a guess.
async fn adopt_capture_size(cfg: &mut StreamConfig, gst_pipeline: &gst::Pipeline) {
    let portal = (cfg.width, cfg.height);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(negotiated) = pipeline::negotiated_video_size(gst_pipeline) {
            (cfg.width, cfg.height) = negotiated;
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::info!("the capture did not negotiate a size in time");
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    log_capture(gst_pipeline, cfg, portal);
}

/// One line on what the capture delivered against what is encoded, so the
/// log says whether the scaler is doing anything. `portal` is the size the
/// portal claimed.
fn log_capture(gst_pipeline: &gst::Pipeline, cfg: &StreamConfig, portal: (u32, u32)) {
    let delivered = pipeline::delivered_capture_size(gst_pipeline);
    let (caps, dmabuf) = pipeline::capture_memory(gst_pipeline)
        .unwrap_or_else(|| ("unknown".into(), "unknown".into()));
    tracing::info!(
        delivered = ?delivered.map(|(w, h)| format!("{w}x{h}")),
        encoding = format!("{}x{}", cfg.width, cfg.height),
        portal = format!("{}x{}", portal.0, portal.1),
        rescaled = delivered.is_some_and(|size| size != (cfg.width, cfg.height)),
        // Whether the frame is still on the GPU when it arrives, and whether
        // it could have been. See `pipeline::capture_memory`.
        %caps,
        %dmabuf,
        "capture negotiated"
    );
}

/// Stops the pipeline before joining the session's threads, including on a
/// failed startup.
struct StreamWorkers {
    pipeline: gst::Pipeline,
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl StreamWorkers {
    fn new(pipeline: &gst::Pipeline) -> Self {
        Self {
            pipeline: pipeline.clone(),
            stop: Arc::default(),
            threads: Vec::new(),
        }
    }
}

impl Drop for StreamWorkers {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.pipeline.set_state(gst::State::Null);
        for thread in self.threads.drain(..) {
            if thread.join().is_err() {
                tracing::error!("a Cast worker panicked during shutdown");
            }
        }
    }
}

/// The UDP socket every track of the session sends from.
///
/// The receiver ties the session to the address of the first packet it gets;
/// a socket per track would make audio arrive from another port, and the
/// receiver would take it for another session.
fn session_socket(target: SocketAddr) -> Result<UdpSocket> {
    let net = |e: std::io::Error| NdError::Network(e.to_string());
    let bind = if target.is_ipv4() {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
    };
    let socket = UdpSocket::bind(bind).map_err(net)?;
    socket.connect(target).map_err(net)?;
    socket
        .set_write_timeout(Some(Duration::from_millis(50)))
        .map_err(net)?;
    // Bounds how long the feedback reader takes to notice the stop.
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(net)?;
    Ok(socket)
}

/// A sender for each accepted track. Video and its repairs share one pacer;
/// audio gets its own, since a video frame fills a burst window by itself and
/// an Opus packet queued behind it would drift behind the picture. Audio is
/// 128 kbit/s and cannot swamp anything.
fn track_senders(
    cfg: &StreamConfig,
    gst_pipeline: &gst::Pipeline,
    session: &Negotiated,
    socket: &Arc<UdpSocket>,
    stop: &Arc<AtomicBool>,
) -> Result<Vec<StreamSender>> {
    let mut senders = Vec::new();
    if let Some(stream) = session.video() {
        let pacer = BurstPacer::new(stop.clone());
        senders.push(StreamSender::new(
            gst_pipeline,
            stream,
            socket.clone(),
            cfg.fps,
            pacer,
        )?);
    }
    if let Some(stream) = session.audio() {
        let pacer = BurstPacer::new(stop.clone());
        match StreamSender::new(gst_pipeline, stream, socket.clone(), cfg.fps, pacer) {
            Ok(sender) => senders.push(sender),
            // Video is what matters here, and the receiver accepted the
            // streams individually.
            Err(err) => tracing::warn!(%err, "no audio track in this session"),
        }
    }
    // An appsink nobody drains must not stall the pipeline.
    for (name, accepted) in [
        (MIRROR_VIDEO_SINK, session.video().is_some()),
        (MIRROR_AUDIO_SINK, session.audio().is_some()),
    ] {
        if !accepted && let Some(element) = gst_pipeline.by_name(name) {
            element.set_property("drop", true);
            element.set_property("max-buffers", 1u32);
            element.set_property("wait-on-eos", false);
        }
    }
    Ok(senders)
}

/// Streams the negotiated tracks until cancellation, the end of the stream,
/// or a failure in any thread.
///
/// The pipeline arrives built and running: its negotiated size is what the
/// OFFER was written from.
async fn stream(
    cfg: &StreamConfig,
    gst_pipeline: gst::Pipeline,
    events: pipeline::PipelineEvents,
    session: &Negotiated,
    status: &SinkStatus,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    if *cancel.borrow() {
        return Ok(());
    }
    status.set(SinkState::WaitStreaming);

    let mut workers = StreamWorkers::new(&gst_pipeline);
    let (ip, port) = session.target();
    let target = SocketAddr::new(ip, port);
    let socket = Arc::new(session_socket(target)?);
    tracing::debug!(source = ?socket.local_addr().ok(), %target, "mirroring session socket");

    let senders = track_senders(cfg, &gst_pipeline, session, &socket, &workers.stop)?;
    if senders.is_empty() {
        return Err(NdError::Unsupported(
            "the receiver accepted no usable stream".into(),
        ));
    }
    let (signals, finished) = SessionSignals::new(workers.stop.clone(), senders.len());
    let mut routes = FeedbackRoutes::new(session);
    let inboxes: Vec<_> = senders
        .iter()
        .map(|sender| routes.add_track(sender.ssrc(), sender.checkpoint.clone()))
        .collect();
    workers
        .threads
        .push(feedback::spawn_reader(socket, routes, signals.clone())?);

    tracing::info!(%target, streams = senders.len(), "starting Cast mirroring");
    gst_pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| NdError::Gst(e.to_string()))?;
    status.set(SinkState::Streaming);
    // Sender reports pair NTP with RTP for the same instant: NTP is the wall
    // clock at running time zero plus the running time.
    let now = std::time::SystemTime::now();
    let running = gst_pipeline
        .current_running_time()
        .unwrap_or(gst::ClockTime::ZERO);
    let clock_origin = now
        .checked_sub(Duration::from_nanos(running.nseconds()))
        .unwrap_or(now);

    for (mut sender, nacks) in senders.into_iter().zip(inboxes) {
        sender.clock_origin = clock_origin;
        sender.base_origin = gst_pipeline.base_time().unwrap_or(gst::ClockTime::ZERO);
        let video = sender
            .is_video()
            .then(|| VideoControl::new(&gst_pipeline, cfg.encoder, cfg.bitrate_kbps));
        let name = format!("cast-{}", sender.label());
        let worker = Worker::new(sender, nacks, signals.clone(), video);
        let thread = std::thread::Builder::new()
            .name(name)
            .spawn(move || worker.run())
            .map_err(|e| NdError::Gst(e.to_string()))?;
        workers.threads.push(thread);
    }

    let outcome = outcome(finished, events, cancel).await;

    // Joining the threads blocks; keep it off the async runtime.
    tokio::task::spawn_blocking(move || drop(workers))
        .await
        .map_err(|e| NdError::Gst(format!("Cast worker shutdown failed: {e}")))?;
    outcome
}

/// Waits for the first of: a thread's outcome, a pipeline error or end of
/// stream, or cancellation.
async fn outcome(
    mut finished: tokio::sync::oneshot::Receiver<Result<()>>,
    mut events: pipeline::PipelineEvents,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    loop {
        tokio::select! {
            result = &mut finished => break result.unwrap_or_else(|_| {
                Err(NdError::Gst("Cast workers exited without a completion status".into()))
            }),
            event = futures::StreamExt::next(&mut events) => match event {
                Some(pipeline::PipelineEvent::Error { message, debug: details }) => {
                    tracing::error!(%message, %details, "the mirroring pipeline failed");
                    break Err(NdError::Gst(message));
                }
                Some(pipeline::PipelineEvent::Warning { message }) => {
                    tracing::warn!(%message, "mirroring pipeline warning");
                }
                Some(pipeline::PipelineEvent::Eos) => break Ok(()),
                None => break Err(NdError::Gst("mirroring pipeline event channel closed".into())),
            },
            _ = cancel.changed() => break Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_fallback_on_negotiation_failure_only() {
        // The HTTP path costs seconds of delay: it is only worth taking when
        // the device genuinely has no mirroring support.
        assert!(is_unsupported(&NdError::Unsupported(
            "the receiver refused to start app 0F5096E8: NOT_FOUND".into()
        )));
        assert!(is_unsupported(&NdError::Unsupported(
            "the receiver refused the mirroring offer: {}".into()
        )));
        assert!(is_unsupported(&NdError::Unsupported(
            "the receiver accepted no usable stream".into()
        )));

        // A network or pipeline failure mid-session must not restart over
        // HTTP: that would trade one problem for another, at twice the delay.
        assert!(!is_unsupported(&NdError::Network(
            "connection dropped".into()
        )));
        assert!(!is_unsupported(&NdError::Gst("the encoder failed".into())));
        assert!(!is_unsupported(&NdError::Protocol(
            "the receiver stopped responding".into()
        )));
        assert!(!is_unsupported(&NdError::Cancelled));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod lifecycle_tests {
    use super::*;
    fn rx_threads() -> usize {
        std::fs::read_dir("/proc/self/task")
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| {
                std::fs::read_to_string(e.path().join("comm"))
                    .unwrap_or_default()
                    .trim()
                    == "cast-rtcp-rx"
            })
            .count()
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn rtcp_thread_is_joined_on_stop() {
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        let cfg = StreamConfig {
            width: 320,
            height: 240,
            fps: 30,
            encoder: pipeline::H264Encoder::X264,
            audio: pipeline::AudioSource::Silence,
            ..Default::default()
        };
        let (_, offer) = mirror::build_offer(
            &MirrorConfig {
                width: 320,
                height: 240,
                ..Default::default()
            },
            1,
        )
        .unwrap();
        let negotiated = Negotiated {
            receiver_ip: peer.local_addr().unwrap().ip(),
            answer: mirror::Answer {
                udp_port: peer.local_addr().unwrap().port(),
                send_indexes: vec![0, 1],
                ssrcs: vec![100002, 100004],
                video_limits: None,
                audio_limits: None,
            },
            offer,
        };
        let before = rx_threads();
        // The session builds and starts the pipeline before negotiating, so
        // the test hands `stream` a running pipeline too.
        let desc = pipeline::mirror_pipeline_description(
            &cfg,
            &pipeline::VideoSource::Test,
            pipeline::CHROMECAST_MAX_RESOLUTION,
            false,
        );
        let (gst_pipeline, events) = pipeline::build_pipeline(&desc, cfg.latency_ms()).unwrap();
        // The session's own guard: a panic or early return must not dispose a
        // running pipeline, whose streaming threads would then use freed state.
        let _guard = pipeline::PipelineGuard::new(gst_pipeline.clone());
        gst_pipeline.set_state(gst::State::Playing).unwrap();
        // Armed only now: a cold plugin registry can make the build above slower
        // than the deadline, and `stream` would then return before starting.
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            tx.send(true).unwrap();
        });
        stream(
            &cfg,
            gst_pipeline,
            events,
            &negotiated,
            &SinkStatus::new(),
            &mut rx,
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let after = rx_threads();
        assert_eq!(after, before);
    }
}
