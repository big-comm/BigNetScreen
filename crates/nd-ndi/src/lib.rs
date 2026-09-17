//! Optional NDI sender. Vendor binaries are neither linked nor bundled.

use std::sync::Mutex;

use async_trait::async_trait;
use futures::StreamExt;
use gstreamer::{self as gst, prelude::*};
use nd_core::capture::CaptureSource;
use nd_core::pipeline::{self, PipelineEvent, PipelineGuard, StreamConfig};
use nd_core::sink::{Sink, SinkInfo, SinkKind, SinkState, SinkStatus, StreamLink};
use nd_core::{NdError, Result};

pub const ID: &str = "local:ndi-publisher";

/// Verify plugin presence without starting capture or publishing a source.
pub fn available() -> bool {
    pipeline::init().is_ok()
        && ["ndisink", "ndisinkcombiner"]
            .iter()
            .all(|name| gst::ElementFactory::find(name).is_some())
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
            return Err(NdError::Unsupported("NDI requires gst-plugin-ndi and the separately installed NDI runtime (NDI_RUNTIME_DIR_V6 or NDI_RUNTIME_DIR_V5)".into()));
        }
        let _radio = nd_core::radio::quiet();
        let preferences = nd_core::settings::current();
        let size =
            StreamConfig::fit_within(source.size_or((1920, 1080)), preferences.resolution_limit());
        let fps = preferences.fps;
        let description = description(&source.video_source(), source.audio_source(), size, fps);
        let (pipeline, mut events) = pipeline::build_configured_pipeline(&description, 0, |pipeline| {
            let sink = pipeline.by_name("ndi-output").ok_or_else(|| NdError::Gst("NDI sink missing".into()))?;
            // Configure before READY starts the NDI sender; avoid launch-string interpolation.
            sink.set_property("ndi-name", format!("BigNetScreen — {}", self.name));
            Ok(())
        }).map_err(|err| NdError::Gst(format!("NDI initialization failed; check gst-plugin-ndi and the NDI runtime installation: {err}")))?;
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
