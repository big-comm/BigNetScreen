//! Publishing the screen to any web browser on the network.
//!
//! No app on the receiver: a TV, a phone or a laptop opens a short address,
//! types the four-digit PIN shown on this computer, and the picture appears.
//! OBS gets it the same way through a *Browser Source*.
//!
//! Under the hood this is WebRTC with WHEP signalling. The bundled
//! `whepserversink` (from gst-plugins-rs) encodes with the best encoder it
//! finds, adapts the bitrate to the network and retransmits what gets lost;
//! [`server`] is the front door in front of it, serving the page and holding
//! the PIN. The element itself listens on the loopback interface only.

mod net;
mod page;
mod server;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use gstreamer::{self as gst, prelude::*};
use nd_core::capture::CaptureSource;
use nd_core::pipeline::{self, GpuDriver, H264Encoder, PipelineEvent, PipelineGuard, StreamConfig};
use nd_core::sink::{Sink, SinkAccess, SinkInfo, SinkKind, SinkState, SinkStatus, StreamLink};
use nd_core::{NdError, Result};

pub use page::PageText;

/// The publisher's stable identifier in the receiver list.
pub const ID: &str = "local:webrtc-publisher";

/// The name the element carries in every description.
const SINK_NAME: &str = "web-output";

/// How often the receiver count is read off the element.
const RECEIVERS_POLL: Duration = Duration::from_secs(1);

static PLUGIN: OnceLock<std::result::Result<(), String>> = OnceLock::new();

/// Registers the bundled plugin once; `true` when publishing is possible.
///
/// Besides the bundled element this needs `webrtcbin`, DTLS-SRTP and the
/// H.264/VP8 payloaders from the system's GStreamer, so the answer can be
/// `false` on a minimal installation.
pub fn available() -> bool {
    pipeline::init().is_ok()
        && PLUGIN
            .get_or_init(|| gstrswebrtc::plugin_register_static().map_err(|err| err.to_string()))
            .is_ok()
        && [
            "whepserversink",
            "webrtcbin",
            "dtlssrtpenc",
            "nicesink",
            "rtph264pay",
            "opusenc",
        ]
        .iter()
        .all(|name| gst::ElementFactory::find(name).is_some())
}

/// Bends `webrtcsink`'s encoder choice to this project's policy, once.
///
/// The element takes the highest-ranked encoder that produces the codec. Our
/// own choice (see `nd_core::pipeline::encoder_candidates`) already knows
/// which hardware encoders cannot work on this driver and in what order the
/// rest deserve a try, so those ranks are written into the registry before
/// the element reads them. The list is read on the first element, which is
/// why this must run before the first pipeline and only ever changes ranks
/// in this process.
fn apply_encoder_policy(driver: GpuDriver) {
    static APPLIED: OnceLock<()> = OnceLock::new();
    APPLIED.get_or_init(|| {
        let candidates = pipeline::encoder_candidates(driver);
        let known = [
            H264Encoder::VaH264,
            H264Encoder::VaapiH264,
            H264Encoder::NvH264,
            H264Encoder::V4l2H264,
            H264Encoder::X264,
            H264Encoder::OpenH264,
        ];
        for encoder in known {
            let Some(factory) = gst::ElementFactory::find(encoder.element()) else {
                continue;
            };
            let rank = match candidates.iter().position(|c| *c == encoder) {
                // Above PRIMARY (256) so nothing the distribution ships beats
                // the policy; spaced so the order survives.
                Some(index) => gst::Rank::from(300 + 8 * (candidates.len() - index) as i32),
                None => gst::Rank::NONE,
            };
            tracing::debug!(
                element = encoder.element(),
                rank = i32::from(rank),
                "encoder rank for WebRTC"
            );
            factory.set_rank(rank);
        }
    });
}

/// Bitrate targets for the encoder, in bits per second.
///
/// A screen is mostly still with sharp edges: about 0.08 bit per pixel per
/// frame keeps text readable (1080p60 lands near 10 Mbit/s), with room above
/// for the adaptive controller to climb when the network allows and a floor
/// below which the picture would stop being a screen.
fn bitrates(size: (u32, u32), fps: u32) -> (u32, u32, u32) {
    let pixels_per_second = size.0 as u64 * size.1 as u64 * fps.max(1) as u64;
    let start = (pixels_per_second * 8 / 100).clamp(1_000_000, 20_000_000) as u32;
    let max = (start * 2).min(30_000_000);
    let min = 800_000;
    (min, start, max)
}

/// The pipeline: the capture scaled and paced, sound if any, both into the
/// element. Pixel format is left to the element, which converts to whatever
/// its encoder needs.
fn description(
    video: &pipeline::VideoSource,
    audio: Option<pipeline::AudioSource>,
    size: (u32, u32),
    fps: u32,
) -> String {
    let mut description = format!(
        "whepserversink name={SINK_NAME} \
         {} ! videorate ! videoscale add-borders=true ! \
         video/x-raw,width={},height={},framerate={}/1 ! \
         queue max-size-buffers=2 max-size-bytes=0 max-size-time=0 leaky=downstream ! {SINK_NAME}.",
        video.description(),
        size.0,
        size.1,
        fps
    );
    if let Some(audio) = audio {
        description.push_str(&format!(
            " {} ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! \
             queue max-size-buffers=0 max-size-bytes=0 max-size-time=200000000 ! {SINK_NAME}.",
            audio.description()
        ));
    }
    description
}

/// Publishes the screen to browsers on the local network.
pub struct WebRtcPublisher {
    name: String,
    driver: GpuDriver,
    text: PageText,
    status: SinkStatus,
    access: Mutex<Option<SinkAccess>>,
    cancel: Mutex<Option<tokio::sync::watch::Sender<bool>>>,
}

impl WebRtcPublisher {
    /// `name` is how this computer introduces itself on the page; `text` is
    /// what the page says, in the application's language.
    pub fn new(name: String, driver: GpuDriver, text: PageText) -> Self {
        Self {
            name,
            driver,
            text,
            status: SinkStatus::new(),
            access: Mutex::new(None),
            cancel: Mutex::new(None),
        }
    }

    async fn publish(
        &self,
        source: CaptureSource,
        mut cancelled: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let _radio = nd_core::radio::quiet();
        let preferences = nd_core::settings::current();
        let size =
            StreamConfig::fit_within(source.size_or((1920, 1080)), preferences.resolution_limit());
        let audio = match source.audio_source() {
            pipeline::AudioSource::Silence => None,
            audio => Some(audio),
        };
        let mut session = Session::start(
            &source.video_source(),
            audio,
            size,
            preferences.fps,
            self.driver,
            &self.text,
        )
        .await?;

        tracing::info!(url = %session.access.url, "publishing to web browsers");
        *self
            .access
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(session.access.clone());
        self.status.set_link(StreamLink {
            width: size.0,
            height: size.1,
            fps: preferences.fps,
            endpoint: None,
            receivers: Some(0),
        });
        self.status.set(SinkState::Streaming);

        let mut poll = tokio::time::interval(RECEIVERS_POLL);
        let outcome = loop {
            tokio::select! {
                _ = cancelled.changed() => break Ok(()),
                _ = poll.tick() => self.status.set_receivers(session.receivers()),
                event = session.events.next() => match event {
                    Some(PipelineEvent::Error { message, .. }) => break Err(NdError::Gst(message)),
                    Some(PipelineEvent::Eos) | None => break Ok(()),
                    _ => {},
                }
            }
        };
        self.access
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        session.close().await;
        drop(source);
        outcome
    }
}

/// A running publication: the pipeline, the front door and the secrets.
///
/// [`WebRtcPublisher`] drives one of these; it is public so an example can
/// publish a test pattern for trying the page on real receivers.
/// Dropping it takes the pipeline to `Null` and closes the door.
pub struct Session {
    /// Kept for its `Drop`; the pipeline is reached through `events`.
    _guard: PipelineGuard,
    events: pipeline::PipelineEvents,
    _door: server::FrontDoor,
    access: SinkAccess,
    connected: Arc<AtomicU32>,
}

impl Session {
    /// How many receivers are connected right now.
    pub fn receivers(&self) -> u32 {
        self.connected.load(Ordering::Relaxed)
    }

    /// Where receivers come to, and the PIN that lets them in.
    pub fn access(&self) -> &SinkAccess {
        &self.access
    }

    /// The pipeline's errors and end of stream.
    pub fn events(&mut self) -> &mut pipeline::PipelineEvents {
        &mut self.events
    }

    /// Takes everything down.
    ///
    /// `webrtcsink` refuses to be set to `Null` from inside the async runtime
    /// (it would block it while its own tasks wind down), so the drop runs on
    /// a blocking thread.
    pub async fn close(self) {
        let _ = tokio::task::spawn_blocking(move || drop(self)).await;
    }

    /// Starts publishing `video` (and `audio`, if any) at `size` and `fps`.
    pub async fn start(
        video: &pipeline::VideoSource,
        audio: Option<pipeline::AudioSource>,
        size: (u32, u32),
        fps: u32,
        driver: GpuDriver,
        text: &PageText,
    ) -> Result<Self> {
        if !available() {
            return Err(NdError::Unsupported(
                "WebRTC publishing needs GStreamer's webrtcbin, DTLS-SRTP and the RTP payloaders"
                    .into(),
            ));
        }
        apply_encoder_policy(driver);

        // Where the receivers come to, and what lets them in.
        let ip = net::lan_ip()?;
        let whep_port = net::free_loopback_port()?;
        let token = net::random_token()?;
        let pin = net::random_pin()?;
        let listener = net::bind_front_door(ip).await?;
        let door = server::serve(
            listener,
            server::FrontDoorConfig {
                pin: pin.clone(),
                token,
                whep_port,
                page: page::render(text),
            },
        )?;
        let url = format!("http://{}", door.local_addr());

        let (min, start, max) = bitrates(size, fps);
        let description = description(video, audio, size, fps);
        let (pipeline, events) =
            pipeline::build_configured_pipeline(&description, 0, |pipeline| {
                let sink = pipeline
                    .by_name(SINK_NAME)
                    .ok_or_else(|| NdError::Gst("WebRTC sink missing".into()))?;
                // Local network only: no STUN round trip, no outside address.
                sink.set_property("stun-server", None::<String>);
                sink.set_property("do-retransmission", true);
                sink.set_property("min-bitrate", min);
                sink.set_property("start-bitrate", start);
                sink.set_property("max-bitrate", max);
                // H.264 first: hardware encoders and every TV decode it. VP8
                // stays as the fallback for a browser that offers nothing else.
                sink.set_property(
                    "video-caps",
                    gst::Caps::builder_full()
                        .structure(gst::Structure::new_empty("video/x-h264"))
                        .structure(gst::Structure::new_empty("video/x-vp8"))
                        .build(),
                );
                let signaller = sink.property::<gst::glib::Object>("signaller");
                signaller.set_property("host-addr", format!("http://127.0.0.1:{whep_port}"));
                Ok(())
            })?;
        let guard = PipelineGuard::new(pipeline.clone());
        let sink = pipeline
            .by_name(SINK_NAME)
            .ok_or_else(|| NdError::Gst("WebRTC sink missing".into()))?;
        let connected = Arc::new(AtomicU32::new(0));
        for (signal, joining) in [("consumer-added", true), ("consumer-removed", false)] {
            let connected = connected.clone();
            sink.connect(signal, false, move |_| {
                if joining {
                    connected.fetch_add(1, Ordering::Relaxed);
                } else {
                    let _ = connected.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                        Some(n.saturating_sub(1))
                    });
                }
                None
            });
        }

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| NdError::Gst(format!("WebRTC startup failed: {e}")))?;

        Ok(Self {
            _guard: guard,
            events,
            _door: door,
            access: SinkAccess { url, pin },
            connected,
        })
    }
}

#[async_trait]
impl Sink for WebRtcPublisher {
    fn info(&self) -> SinkInfo {
        SinkInfo {
            id: ID.into(),
            display_name: self.name.clone(),
            kind: SinkKind::WebRtc,
            address: None,
        }
    }
    fn state(&self) -> SinkState {
        self.status.state()
    }
    fn error_message(&self) -> Option<String> {
        self.status.message()
    }
    fn link(&self) -> Option<StreamLink> {
        self.status.link()
    }
    fn access(&self) -> Option<SinkAccess> {
        self.access
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    async fn start_stream(&self, source: CaptureSource) -> Result<()> {
        self.status.reset();
        self.status.set(SinkState::WaitStreaming);
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        *self
            .cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cancel);
        let result = self.publish(source, cancelled).await;
        self.cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        match &result {
            Ok(()) => self.status.reset(),
            Err(err) => self.status.fail(err.to_string()),
        }
        result
    }
    async fn stop_stream(&self) -> Result<()> {
        if let Some(cancel) = self
            .cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            cancel.send_replace(true);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_plugin_registers() {
        assert!(available());
    }

    #[test]
    fn bitrates_follow_the_picture() {
        let (min, start, max) = bitrates((1920, 1080), 60);
        assert!((9_000_000..=11_000_000).contains(&start), "{start}");
        assert_eq!(max, start * 2);
        assert_eq!(min, 800_000);
        assert_eq!(bitrates((320, 240), 10).1, 1_000_000, "the floor holds");
        assert_eq!(
            bitrates((7680, 4320), 60).1,
            20_000_000,
            "the ceiling holds"
        );
    }

    fn text() -> PageText {
        PageText {
            title: "Test".into(),
            prompt: "PIN".into(),
            join: "Join".into(),
            wrong_pin: "Wrong".into(),
            locked: "Locked".into(),
            connecting: "Connecting".into(),
            failed: "Failed".into(),
            ended: "Ended".into(),
            fullscreen_hint: "Full screen".into(),
        }
    }

    /// A real receiver (`whepclientsrc`, bundled with the same plugin) joins
    /// through the front door with the PIN and gets decoded video.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_receiver_joins_with_the_pin_and_gets_video() {
        use std::sync::atomic::AtomicUsize;
        use std::time::Instant;

        assert!(available());
        if net::lan_ip().is_err() {
            eprintln!("no network; skipping the end-to-end check");
            return;
        }
        let session = Session::start(
            &pipeline::VideoSource::Test,
            None,
            (640, 360),
            30,
            GpuDriver::Unknown,
            &text(),
        )
        .await
        .expect("the publication starts");
        let url = session.access.url.clone();

        // The PIN buys the token, like the page does.
        let (mut sender, connection) = {
            let addr: std::net::SocketAddr = url.trim_start_matches("http://").parse().unwrap();
            let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                .await
                .unwrap()
        };
        tokio::spawn(connection);
        let request = http::Request::builder()
            .method("POST")
            .uri("/pin")
            .header("host", "x")
            .header("content-type", "text/plain")
            .body(http_body_util::Full::new(bytes::Bytes::from(
                session.access.pin.clone(),
            )))
            .unwrap();
        let response = sender.send_request(request).await.unwrap();
        assert_eq!(response.status(), 200);
        let token = String::from_utf8(
            http_body_util::BodyExt::collect(response.into_body())
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();

        let receiver = gst::parse::launch(
            "whepclientsrc name=client ! queue ! fakesink name=out sync=false signal-handoffs=true",
        )
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let client = receiver.by_name("client").unwrap();
        client.set_property("stun-server", None::<String>);
        client
            .property::<gst::glib::Object>("signaller")
            .set_property("whep-endpoint", format!("{url}/whep?token={token}"));
        let _receiver_guard = PipelineGuard::new(receiver.clone());
        let frames = Arc::new(AtomicUsize::new(0));
        let counted = frames.clone();
        receiver
            .by_name("out")
            .unwrap()
            .connect("handoff", false, move |_| {
                counted.fetch_add(1, Ordering::Relaxed);
                None
            });
        receiver.set_state(gst::State::Playing).unwrap();

        let deadline = Instant::now() + Duration::from_secs(20);
        while frames.load(Ordering::Relaxed) < 10 && Instant::now() < deadline {
            if let Some(error) = receiver
                .bus()
                .unwrap()
                .pop_filtered(&[gst::MessageType::Error])
            {
                panic!("receiver failed: {error:?}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let received = frames.load(Ordering::Relaxed);
        let receivers = session.receivers();
        eprintln!("WebRTC loopback: {received} frames, {receivers} receiver(s)");
        let _ = tokio::task::spawn_blocking(move || drop(_receiver_guard)).await;
        session.close().await;
        assert!(received >= 10, "decoded frames received: {received}");
        assert_eq!(receivers, 1, "the publisher counts its receiver");
    }

    #[test]
    fn the_description_parses_and_audio_is_optional() {
        assert!(available());
        for audio in [None, Some(pipeline::AudioSource::Silence)] {
            let desc = description(&pipeline::VideoSource::Test, audio, (1280, 720), 30);
            assert_eq!(desc.contains("audioconvert"), audio.is_some(), "{desc}");
            let element = gst::parse::launch(&desc).expect(&desc);
            let _ = element.set_state(gst::State::Null);
        }
    }
}
