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
const SIZE_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How often a file transmission's pause is checked to pass it on.
const PAUSE_POLL_INTERVAL: Duration = Duration::from_millis(250);

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
    let url = server.url();

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
        fps: StreamConfig::capped_fps(60),
        audio: source.audio_source(),
        ..Default::default()
    };
    let driver = nd_net::detect_gpu_driver();
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

    let sink = gst_pipeline
        .by_name(TS_HTTP_SINK_NAME)
        .ok_or_else(|| NdError::Gst(format!("element {TS_HTTP_SINK_NAME} not found")))?;

    status.set(SinkState::WaitSocket);
    if *cancel.borrow() {
        return Ok(());
    }

    tracing::info!(%receiver, "handing the renderer the stream URL");
    let outcome = serve_until_over(
        &server,
        sink,
        &gst_pipeline,
        &mut events,
        &mut cancel,
        control,
        status,
        &url,
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
    mut cfg: StreamConfig,
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

    // Some TVs fetch only after a slow SetAVTransportURI/Play handshake.
    // Read caps after that fetch, never publish the initial ceiling as fact.
    let mut measure = tokio::time::interval(SIZE_POLL_INTERVAL);
    measure.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut measured = false;

    // A player pauses the file pipeline, which then sends nothing; the
    // renderer has to be told as well, or it gives up on the silent stream.
    let paused_here = std::sync::atomic::AtomicBool::new(false);
    let mut pause_watch = tokio::time::interval(PAUSE_POLL_INTERVAL);
    pause_watch.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Run beside the server, like the handshake: a resume may hand the URL
    // over again, and some renderers fetch it from inside that request.
    let telling = futures::FutureExt::fuse(follow_pause(control, url, false, server.media()));
    tokio::pin!(telling);
    telling.set(futures::future::Fuse::terminated());

    let renderer_done = until_renderer_stops(control, &paused_here);
    tokio::pin!(renderer_done);

    let handshake = async {
        // A renderer already showing something may refuse a new URI, and one
        // request settles it. Failing here is not fatal: most often there was
        // simply nothing to stop.
        if let Err(err) = avtransport::stop(control).await {
            tracing::debug!(%err, "the renderer had nothing to stop, or would not");
        }
        avtransport::set_uri(control, url, ITEM_TITLE, None, server.media()).await?;
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
                if !started.load(std::sync::atomic::Ordering::SeqCst) {
                    status.set(SinkState::WaitStreaming);
                }
                tracing::info!("Play accepted by the renderer");
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

            result = &mut telling => {
                if let Err(err) = result {
                    tracing::warn!(%err, "the renderer did not follow the pause");
                }
            }

            _ = pause_watch.tick(), if handed_over => {
                if !started.load(std::sync::atomic::Ordering::SeqCst)
                    || !futures::future::FusedFuture::is_terminated(&*telling)
                {
                    continue;
                }
                // The state being moved to, not the one reached: the renderer
                // should hear about a pause as soon as it is asked for.
                let (_, current, pending) = gst_pipeline.state(gst::ClockTime::ZERO);
                let paused = if pending == gst::State::VoidPending { current } else { pending }
                    == gst::State::Paused;
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
                let Some((width, height)) = pipeline::negotiated_video_size(
                    gst_pipeline, std::time::Instant::now(),
                ) else {
                    continue;
                };
                measured = true;
                (cfg.width, cfg.height) = (width, height);
                let kbps = cfg.scaled_bitrate_kbps();
                if pipeline::set_encoder_bitrate(gst_pipeline, cfg.encoder, kbps)
                    && let Some(mux) = gst_pipeline.by_name("mux") {
                        mux.set_property("bitrate", u64::from(kbps) * 1_000);
                    }
                status.set_link(StreamLink {
                    width, height, fps: cfg.fps,
                    endpoint: Some(control.addr), receivers: None,
                });
                let delivered = pipeline::delivered_capture_size(gst_pipeline);
                tracing::info!(
                    delivered = ?delivered.map(|(w, h)| format!("{w}x{h}")),
                    encoding = format!("{width}x{height}"),
                    rescaled = delivered.is_some_and(|d| d != (width, height)),
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
    // again, it fetches afresh from where the file is now.
    avtransport::set_uri(control, url, ITEM_TITLE, None, media).await?;
    avtransport::play(control).await
}

/// Resolves when the renderer's own view of the session says it is over.
///
/// Without this, pressing stop on the television's remote leaves us capturing,
/// encoding and sending to nobody: the TCP connection can stay open long after
/// the renderer stopped reading it, so silence on the socket proves nothing.
///
/// A pause counts as over only when it was not ours: `paused_here` says the
/// renderer was told to pause because the file pipeline did.
async fn until_renderer_stops(control: &Endpoint, paused_here: &std::sync::atomic::AtomicBool) {
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
            Ok(TransportState::Paused) if paused_here.load(std::sync::atomic::Ordering::SeqCst) => {
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
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    #[tokio::test]
    async fn delayed_renderer_get_reports_actual_pixels_and_stays_streaming() {
        pipeline::init().unwrap();
        let path = std::env::temp_dir().join(format!("bns-dlna-media-{}.webm", std::process::id()));
        let fixture = gst::parse::launch(&format!(
            "webmmux name=mux ! filesink location=\"{}\" videotestsrc num-buffers=360 ! video/x-raw,width=320,height=240,framerate=30/1 ! vp8enc deadline=1 ! mux. audiotestsrc num-buffers=563 ! audio/x-raw,rate=48000 ! vorbisenc ! mux.", path.display()
        )).unwrap().downcast::<gst::Pipeline>().unwrap();
        let guard = PipelineGuard::new(fixture.clone());
        fixture.set_state(gst::State::Playing).unwrap();
        let message = fixture
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .unwrap();
        assert_eq!(message.type_(), gst::MessageType::Eos, "{message:?}");
        drop(guard);
        let playback = nd_core::media::FilePlaybackControl::default();
        let source = CaptureSource::media_file(
            nd_core::capture::MediaPlayback {
                control: Some(playback.clone()),
                start: Some(nd_core::media::PlaybackStart {
                    seconds: 1.0,
                    paused: false,
                    volume: 0.25,
                    muted: true,
                    height: 0,
                }),
                source: nd_core::media::MediaSource::File(path.clone()),
                kind: nd_core::media::MediaKind::Video,
                title: "DLNA test".into(),
            },
            // The 16:9 frame `tv_frame` gives this 4:3 film at its own size.
            (426, 240),
        );
        let previous = nd_core::settings::current();
        nd_core::settings::set_in_memory(&nd_core::settings::Settings {
            port: 0,
            system_audio: false,
            microphone: false,
            hardware_encoding: false,
            ..Default::default()
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control = Endpoint::parse(&format!(
            "http://{}/control",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let status = SinkStatus::new();
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        let renderer = async {
            let mut media = None;
            let mut stops = 0;
            let mut plays = 0;
            let mut uris = 0;
            let mut paused = false;
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut headers = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    assert!(socket.read_line(&mut line).await.unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                    headers.push_str(&line);
                }
                let mut body = vec![0; length];
                socket.read_exact(&mut body).await.unwrap();
                let body = String::from_utf8(body).unwrap();
                if headers.contains("#SetAVTransportURI") {
                    uris += 1;
                    assert!(!body.contains("resolution="));
                    // Fetch inside SetAVTransportURI, after the old two-second
                    // one-shot caps measurement would already have fired.
                    tokio::time::sleep(Duration::from_millis(2100)).await;
                    let url = crate::upnp::tag_text(&body, "CurrentURI").unwrap();
                    let endpoint = Endpoint::parse(url).unwrap();
                    let mut client = TcpStream::connect(endpoint.addr).await.unwrap();
                    client
                        .write_all(
                            format!(
                                "GET {} HTTP/1.1\r\nHost: {}\r\n\r\n",
                                endpoint.path, endpoint.authority
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                    let mut client = BufReader::new(client);
                    loop {
                        let mut line = String::new();
                        assert!(client.read_line(&mut line).await.unwrap() > 0);
                        if line == "\r\n" {
                            break;
                        }
                    }
                    let mut packet = [0; 188];
                    client.read_exact(&mut packet).await.unwrap();
                    assert_eq!(packet[0], 0x47, "receiver must get MPEG-TS");
                    media = Some(client);
                }
                if headers.contains("#Play") {
                    plays += 1;
                }
                // The resume is refused once, as by a renderer that gave up
                // on the silent stream during the pause.
                let refused = plays == 2;
                socket
                    .write_all(if refused {
                        b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n"
                    } else {
                        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"
                    })
                    .await
                    .unwrap();
                if headers.contains("#Pause") {
                    paused = true;
                    playback
                        .command(&nd_core::media::MediaCommand::SetPaused(false))
                        .unwrap();
                }
                if plays == 3 && headers.contains("#Play") {
                    // The refused resume handed the stream over again.
                    assert!(paused, "Play again only after a Pause");
                    assert_eq!(uris, 2);
                    cancel.send_replace(true);
                }
                if headers.contains("#Play") && plays == 1 {
                    let link = tokio::time::timeout(Duration::from_secs(3), async {
                        loop {
                            if let Some(link) = status.link() {
                                break link;
                            }
                            tokio::time::sleep(Duration::from_millis(25)).await;
                        }
                    })
                    .await
                    .expect("caps must be reported after the late GET");
                    assert_eq!(
                        (link.width, link.height),
                        (426, 240),
                        "a file must reach the television in the frame it was given"
                    );
                    assert_eq!(status.state(), SinkState::Streaming);
                    use nd_core::media::MediaCommand;
                    assert!(playback.state().can_pause);
                    assert!((playback.state().volume.unwrap() - 0.25).abs() < 0.000_001);
                    assert_eq!(playback.state().muted, Some(true));
                    assert!(playback.state().seconds >= 0.9);
                    playback.command(&MediaCommand::SetVolume(0.37)).unwrap();
                    playback.command(&MediaCommand::SetMute(false)).unwrap();
                    assert!((playback.state().volume.unwrap() - 0.37).abs() < 0.000_001);
                    assert_eq!(playback.state().muted, Some(false));
                    // A paused file pipeline must reach the renderer as Pause.
                    playback.command(&MediaCommand::SetPaused(true)).unwrap();
                }
                if headers.contains("#Stop") {
                    stops += 1;
                    if stops == 2 {
                        break;
                    }
                }
            }
            drop(media);
        };
        let result = tokio::time::timeout(Duration::from_secs(20), async {
            tokio::join!(
                stream(control.addr.ip(), &control, &source, &status, cancelled),
                renderer
            )
        })
        .await;
        nd_core::settings::set_in_memory(&previous);
        std::fs::remove_file(path).unwrap();
        result.expect("DLNA handshake must not deadlock").0.unwrap();
    }
}
