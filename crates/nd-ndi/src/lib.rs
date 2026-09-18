//! Open-source NDI sender integration. The vendor runtime is an optional system dependency.

use std::sync::{Mutex, OnceLock};

use async_trait::async_trait;
use futures::StreamExt;
use gstreamer::{self as gst, prelude::*};
use nd_core::capture::CaptureSource;
use nd_core::pipeline::{self, PipelineEvent, PipelineGuard, StreamConfig};
use nd_core::sink::{Sink, SinkInfo, SinkKind, SinkState, SinkStatus, StreamLink};
use nd_core::{NdError, Result};

pub const ID: &str = "local:ndi-publisher";

static NDI_PLUGIN: OnceLock<std::result::Result<(), String>> = OnceLock::new();

/// Register the bundled plugin without loading the vendor runtime or starting capture.
pub fn available() -> bool {
    pipeline::init().is_ok()
        && NDI_PLUGIN
            .get_or_init(|| gstndi::plugin_register_static().map_err(|err| err.to_string()))
            .is_ok()
        && ["ndisink", "ndisinkcombiner"]
            .iter()
            .all(|name| gst::ElementFactory::find(name).is_some())
}

/// Check the optional runtime without creating a sender or starting capture.
/// A source in READY only loads symbols. A sink would already create a sender.
pub fn runtime_available() -> bool {
    if !available() {
        return false;
    }
    let Ok(probe) = gst::ElementFactory::make("ndisrc").build() else {
        return false;
    };
    let ready = probe.set_state(gst::State::Ready).is_ok();
    let reset = probe.set_state(gst::State::Null).is_ok();
    ready && reset
}

pub struct NdiPublisher {
    name: String,
    status: SinkStatus,
    cancel: Mutex<Option<tokio::sync::watch::Sender<bool>>>,
}

impl NdiPublisher {
    pub fn new(name: String) -> Self {
        Self {
            name,
            status: SinkStatus::new(),
            cancel: Mutex::new(None),
        }
    }

    async fn publish(
        &self,
        source: CaptureSource,
        mut cancelled: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        if !available() {
            return Err(NdError::Unsupported(
                "The built-in NDI plugin could not be initialized".into(),
            ));
        }
        let _radio = nd_core::radio::quiet();
        let preferences = nd_core::settings::current();
        let size =
            StreamConfig::fit_within(source.size_or((1920, 1080)), preferences.resolution_limit());
        let fps = preferences.fps;
        let description = description(&source.video_source(), source.audio_source(), size, fps);
        let (pipeline, mut events) =
            pipeline::build_configured_pipeline(&description, 0, |pipeline| {
                let sink = pipeline
                    .by_name("ndi-output")
                    .ok_or_else(|| NdError::Gst("NDI sink missing".into()))?;
                // Configure before READY starts the NDI sender; avoid launch-string interpolation.
                sink.set_property("ndi-name", format!("BigNetScreen — {}", self.name));
                Ok(())
            })
            .map_err(|err| {
                NdError::Gst(format!(
                    "NDI initialization failed; check the NDI runtime installation: {err}"
                ))
            })?;
        let guard = PipelineGuard::new(pipeline.clone());
        pipeline.set_state(gst::State::Playing).map_err(|e| {
            NdError::Gst(format!(
                "NDI startup failed; check the runtime installation: {e}"
            ))
        })?;
        self.status.set_link(StreamLink {
            width: size.0,
            height: size.1,
            fps,
            endpoint: None,
        });
        self.status.set(SinkState::Streaming);
        let outcome = loop {
            tokio::select! {
                _ = cancelled.changed() => break Ok(()),
                event = events.next() => match event {
                    Some(PipelineEvent::Error { message, .. }) => break Err(NdError::Gst(message)),
                    Some(PipelineEvent::Eos) | None => break Ok(()),
                    _ => {},
                }
            }
        };
        drop(guard);
        drop(source);
        outcome
    }
}

fn description(
    video: &pipeline::VideoSource,
    audio: pipeline::AudioSource,
    size: (u32, u32),
    fps: u32,
) -> String {
    format!(
        "ndisinkcombiner name=ndi-combine ! ndisink name=ndi-output sync=true \
         {} ! videorate ! videoscale add-borders=true ! videoconvert ! \
         video/x-raw,format=UYVY,width={},height={},framerate={}/1 ! \
         queue max-size-buffers=2 max-size-bytes=0 max-size-time=0 leaky=downstream ! ndi-combine.video \
         {} ! audioconvert ! audioresample ! audio/x-raw,format=F32LE,layout=interleaved,rate=48000,channels=2 ! \
         queue max-size-buffers=0 max-size-bytes=0 max-size-time=200000000 ! ndi-combine.audio",
        video.description(), size.0, size.1, fps, audio.description()
    )
}

#[async_trait]
impl Sink for NdiPublisher {
    fn info(&self) -> SinkInfo {
        SinkInfo {
            id: ID.into(),
            display_name: format!("BigNetScreen — {}", self.name),
            kind: SinkKind::Ndi,
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
    #[ignore = "Requires an absent or incompatible NDI runtime"]
    fn missing_runtime_is_reported_before_capture() {
        assert!(available());
        assert!(!runtime_available());
    }

    #[test]
    #[ignore = "Requires a compatible NDI runtime"]
    fn installed_runtime_passes_preflight() {
        assert!(runtime_available());
    }

    #[test]
    fn bundled_plugin_registers_without_a_system_plugin_or_runtime() {
        assert!(available());
        assert!(available());
        let plugin = gst::Registry::get().find_plugin("ndi").unwrap();
        assert!(
            plugin.filename().is_none(),
            "NDI must use the bundled plugin"
        );
        for name in ["ndisink", "ndisinkcombiner"] {
            gst::ElementFactory::make(name).build().unwrap();
        }
    }

    #[test]
    #[ignore = "Requires the NDI runtime, Avahi and network access"]
    fn ndi_loopback_receives_video_and_audio() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use std::time::{Duration, Instant};

        assert!(available());
        let name = format!("BigNetScreen test {}", std::process::id());
        let sender = gst::parse::launch(&description(
            &pipeline::VideoSource::Test,
            pipeline::AudioSource::Silence,
            (1280, 720),
            30,
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let _sender_guard = PipelineGuard::new(sender.clone());
        sender
            .by_name("ndi-output")
            .unwrap()
            .set_property("ndi-name", &name);
        sender.set_state(gst::State::Playing).unwrap();

        let monitor = gst::DeviceMonitor::new();
        monitor.add_filter(
            Some("Source/Network"),
            Some(&gst::Caps::builder("application/x-ndi").build()),
        );
        monitor.start().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let device = loop {
            if let Some(device) = monitor
                .devices()
                .into_iter()
                .find(|device| device.display_name().contains(&name))
            {
                break Some(device);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        monitor.stop();
        let device = device.expect("NDI publication must be discovered");

        let receiver = gst::parse::launch(
            "ndisrc name=source ! ndisrcdemux name=demux \
             demux.video ! queue ! fakesink name=video sync=false signal-handoffs=true \
             demux.audio ! queue ! fakesink name=audio sync=false signal-handoffs=true",
        )
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let _receiver_guard = PipelineGuard::new(receiver.clone());
        let properties = device.properties().unwrap();
        let source = receiver.by_name("source").unwrap();
        source.set_property("ndi-name", properties.get::<String>("ndi-name").unwrap());
        source.set_property(
            "url-address",
            properties.get::<String>("url-address").unwrap(),
        );
        let counts: Vec<_> = ["video", "audio"]
            .into_iter()
            .map(|name| {
                let count = Arc::new(AtomicUsize::new(0));
                let callback_count = count.clone();
                receiver
                    .by_name(name)
                    .unwrap()
                    .connect("handoff", false, move |values| {
                        let buffer = values[1].get::<gst::Buffer>().unwrap();
                        if buffer.size() > 0 {
                            callback_count.fetch_add(1, Ordering::Relaxed);
                        }
                        None
                    });
                count
            })
            .collect();
        receiver.set_state(gst::State::Playing).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while counts
            .iter()
            .any(|count| count.load(Ordering::Relaxed) < 10)
            && Instant::now() < deadline
        {
            for pipeline in [&sender, &receiver] {
                if let Some(error) = pipeline
                    .bus()
                    .unwrap()
                    .pop_filtered(&[gst::MessageType::Error])
                {
                    panic!("NDI pipeline failed: {error:?}");
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let received: Vec<_> = counts
            .iter()
            .map(|count| count.load(Ordering::Relaxed))
            .collect();
        assert!(
            received.iter().all(|count| *count >= 10),
            "received video/audio buffers: {received:?}"
        );
        eprintln!("NDI loopback received video/audio buffers: {received:?}");
    }

    #[test]
    fn raw_branches_negotiate_the_requested_format() {
        pipeline::init().unwrap();
        let description = description(
            &pipeline::VideoSource::Test,
            pipeline::AudioSource::Silence,
            (1280, 720),
            30,
        )
        .replace(
            "ndisinkcombiner name=ndi-combine ! ndisink name=ndi-output sync=true",
            "",
        )
        .replace("ndi-combine.video", "fakesink sync=false")
        .replace("ndi-combine.audio", "fakesink sync=false")
        .replace("is-live=true", "num-buffers=3");
        let pipeline = gst::parse::launch(&description)
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
        let _guard = PipelineGuard::new(pipeline.clone());
        pipeline.set_state(gst::State::Playing).unwrap();
        let message = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(5),
                &[gst::MessageType::Error, gst::MessageType::Eos],
            )
            .expect("finite raw branches finish");
        assert_eq!(message.type_(), gst::MessageType::Eos, "{message:?}");
    }
}
