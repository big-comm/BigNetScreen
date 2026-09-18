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

/// What was found out about the vendor runtime, for telling the user the
/// right thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeCheck {
    /// No `libndi` on this system (or the plugin failed to register).
    Missing,
    /// The library is there but refuses to initialise on this processor.
    ///
    /// Installing it again would not help; the message must say so.
    CpuUnsupported { version: Option<String> },
    /// Ready, with the version string the runtime reports about itself.
    Ready { version: Option<String> },
}

/// Checks the optional runtime without creating a sender or starting capture.
///
/// Three outcomes, because they call for three different messages: the
/// library is absent (offer to install it), it is present but this CPU is not
/// supported (do not offer to install it), or it works (log its version so a
/// bug report says which one).
pub fn runtime_check() -> RuntimeCheck {
    if !available() {
        return RuntimeCheck::Missing;
    }
    match gstndi::runtime_info() {
        gstndi::RuntimeInfo::Missing(reason) => {
            tracing::info!(%reason, "NDI runtime not found");
            RuntimeCheck::Missing
        }
        gstndi::RuntimeInfo::CpuUnsupported { version } => {
            tracing::warn!(?version, "the NDI runtime does not support this CPU");
            RuntimeCheck::CpuUnsupported { version }
        }
        gstndi::RuntimeInfo::Ready { version } => {
            tracing::info!(?version, "NDI runtime ready");
            RuntimeCheck::Ready { version }
        }
    }
}

/// `true` when publishing can be attempted (see [`runtime_check`]).
pub fn runtime_available() -> bool {
    matches!(runtime_check(), RuntimeCheck::Ready { .. })
}

/// How often the sink is asked how many receivers are connected.
const RECEIVERS_POLL: std::time::Duration = std::time::Duration::from_secs(1);

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
        let audio = match source.audio_source() {
            // No audio was asked for: the video goes straight to the sink and
            // skips the combiner, which would add two frames of delay only
            // to line the picture up with silence.
            pipeline::AudioSource::Silence => None,
            audio => Some(audio),
        };
        let description = description(&source.video_source(), audio, size, fps);
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
            receivers: Some(0),
        });
        self.status.set(SinkState::Streaming);
        // NDI publishes to whoever asks, so "streaming" alone does not mean
        // anyone is watching. The sink counts its receivers; that count is
        // what the interface shows.
        let sink = pipeline.by_name("ndi-output");
        let mut poll = tokio::time::interval(RECEIVERS_POLL);
        let outcome = loop {
            tokio::select! {
                _ = cancelled.changed() => break Ok(()),
                _ = poll.tick() => {
                    if let Some(count) = sink.as_ref().map(|sink| sink.property::<i32>("connections")) {
                        if count >= 0 {
                            self.status.set_receivers(count as u32);
                        }
                    }
                }
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

/// The pixel formats the sink hands to the runtime as they are.
///
/// Listing them all lets `videoconvert` pass the captured frames through
/// untouched: the screen arrives as BGRx and the runtime takes BGRx, so
/// converting it to UYVY first only spent a full pass over every frame on the
/// CPU. The runtime folds whatever conversion it still needs into its own
/// compression step. UYVY leads the list so that a format outside it is
/// converted to the runtime's native one.
const NATIVE_FORMATS: &str = "{ UYVY, BGRx, BGRA, RGBx, RGBA, NV12, I420 }";

fn video_branch(video: &pipeline::VideoSource, size: (u32, u32), fps: u32) -> String {
    format!(
        "{} ! videorate ! videoscale add-borders=true ! videoconvert ! \
         video/x-raw,format={NATIVE_FORMATS},width={},height={},framerate={}/1 ! \
         queue max-size-buffers=2 max-size-bytes=0 max-size-time=0 leaky=downstream",
        video.description(),
        size.0,
        size.1,
        fps
    )
}

/// The pipeline for publishing.
///
/// With audio, video and sound meet in `ndisinkcombiner`, which holds the
/// picture back until the matching sound has arrived (two frames). Without
/// audio the picture goes straight into the sink and pays none of that.
fn description(
    video: &pipeline::VideoSource,
    audio: Option<pipeline::AudioSource>,
    size: (u32, u32),
    fps: u32,
) -> String {
    let video = video_branch(video, size, fps);
    match audio {
        None => format!("{video} ! ndisink name=ndi-output sync=true"),
        Some(audio) => format!(
            "ndisinkcombiner name=ndi-combine ! ndisink name=ndi-output sync=true \
             {video} ! ndi-combine.video \
             {} ! audioconvert ! audioresample ! \
             audio/x-raw,format=F32LE,layout=interleaved,rate=48000,channels=2 ! \
             queue max-size-buffers=0 max-size-bytes=0 max-size-time=200000000 ! ndi-combine.audio",
            audio.description()
        ),
    }
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
            Some(pipeline::AudioSource::Silence),
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

        // The sender must have noticed its receiver: this is what the
        // interface shows as "1 receiver". The count refreshes once a second
        // while frames are rendered, so allow it a moment.
        let output = sender.by_name("ndi-output").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let connections = loop {
            let count = output.property::<i32>("connections");
            if count >= 1 || Instant::now() >= deadline {
                break count;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        assert!(connections >= 1, "sender saw {connections} receiver(s)");
        eprintln!("NDI sender reports {connections} receiver(s) connected");
    }

    #[test]
    fn without_audio_the_video_skips_the_combiner() {
        let silent = description(&pipeline::VideoSource::Test, None, (1280, 720), 30);
        assert!(!silent.contains("ndisinkcombiner"), "{silent}");
        assert!(silent.contains("! ndisink name=ndi-output"), "{silent}");

        let with_audio = description(
            &pipeline::VideoSource::Test,
            Some(pipeline::AudioSource::Silence),
            (1280, 720),
            30,
        );
        assert!(with_audio.contains("ndisinkcombiner"), "{with_audio}");
        assert!(with_audio.contains("ndi-combine.audio"), "{with_audio}");
    }

    #[test]
    fn captured_frames_pass_through_in_their_own_format() {
        // The screen arrives as BGRx; forcing UYVY cost a conversion pass per
        // frame. With the sink's formats listed, `videoconvert` must leave a
        // BGRx stream alone and only convert what the sink cannot take.
        pipeline::init().unwrap();
        for (source_format, expected) in [("BGRx", "BGRx"), ("NV12", "NV12"), ("YUY2", "UYVY")] {
            let description = video_branch(&pipeline::VideoSource::Test, (320, 240), 30).replace(
                "videotestsrc is-live=true",
                &format!("videotestsrc num-buffers=2 ! video/x-raw,format={source_format}"),
            ) + " ! fakesink name=out sync=false";
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
                .expect("the finite branch finishes");
            assert_eq!(message.type_(), gst::MessageType::Eos, "{message:?}");
            let caps = pipeline
                .by_name("out")
                .unwrap()
                .static_pad("sink")
                .unwrap()
                .current_caps()
                .unwrap();
            let format = caps.structure(0).unwrap().get::<&str>("format").unwrap();
            assert_eq!(format, expected, "source {source_format}: {caps}");
        }
    }

    /// CPU spent per frame with the old forced-UYVY branch versus the native
    /// pass-through, both feeding a real NDI sender (so the runtime's own
    /// compression is part of the measurement). Prints the numbers; run with
    /// `--ignored --nocapture`.
    #[test]
    #[ignore = "Requires the NDI runtime; prints a measurement"]
    fn bench_native_vs_forced_uyvy() {
        fn cpu_seconds() -> f64 {
            let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
            let after_comm = stat.rsplit(')').next().unwrap();
            let fields: Vec<&str> = after_comm.split_whitespace().collect();
            // Fields after the command: state is index 0, utime 11, stime 12.
            let ticks: f64 =
                fields[11].parse::<f64>().unwrap() + fields[12].parse::<f64>().unwrap();
            ticks / 100.0
        }
        fn run(label: &str, caps_format: &str, frames: u32) {
            let description = format!(
                "videotestsrc num-buffers={frames} pattern=smpte ! \
                 video/x-raw,format=BGRx,width=1920,height=1080,framerate=60/1 ! \
                 videoconvert ! video/x-raw,format={caps_format} ! \
                 ndisink name=ndi-output sync=false"
            );
            let pipeline = gst::parse::launch(&description)
                .unwrap()
                .downcast::<gst::Pipeline>()
                .unwrap();
            pipeline.by_name("ndi-output").unwrap().set_property(
                "ndi-name",
                format!("BigNetScreen bench {}", std::process::id()),
            );
            let _guard = PipelineGuard::new(pipeline.clone());
            let cpu_before = cpu_seconds();
            let wall = std::time::Instant::now();
            pipeline.set_state(gst::State::Playing).unwrap();
            let message = pipeline
                .bus()
                .unwrap()
                .timed_pop_filtered(
                    gst::ClockTime::from_seconds(120),
                    &[gst::MessageType::Error, gst::MessageType::Eos],
                )
                .expect("finite run finishes");
            assert_eq!(message.type_(), gst::MessageType::Eos, "{message:?}");
            let wall = wall.elapsed().as_secs_f64();
            let cpu = cpu_seconds() - cpu_before;
            eprintln!(
                "{label:<28} frames={frames} wall={wall:.2}s cpu={cpu:.2}s cpu/frame={:.2}ms",
                cpu * 1000.0 / frames as f64
            );
        }
        assert!(available());
        let frames = 600;
        for _ in 0..2 {
            run("forced UYVY (old)", "UYVY", frames);
            run("native BGRx (new)", NATIVE_FORMATS, frames);
        }
    }

    #[test]
    fn raw_branches_negotiate_the_requested_format() {
        pipeline::init().unwrap();
        let description = description(
            &pipeline::VideoSource::Test,
            Some(pipeline::AudioSource::Silence),
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
