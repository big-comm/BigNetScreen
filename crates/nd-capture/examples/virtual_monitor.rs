//! Creates a **virtual monitor** and reports what came back.
//!
//! ```sh
//! cargo run -p nd-capture --example virtual_monitor [width] [height] [seconds]
//! ```
//!
//! A virtual monitor is an extra desktop that exists only for the cast: the
//! receiver shows a second screen instead of duplicating the laptop's. It needs
//! `org.gnome.Mutter.ScreenCast` — most desktop portals do not offer it.
//!
//! The example exists because this path is easy to write and hard to trust: it
//! either creates a screen the compositor really renders to, or it succeeds on
//! D-Bus and produces a node that never emits a frame. Only running it tells
//! the two apart, so it also **counts the frames** that come out.

use std::time::Duration;

use gstreamer::prelude::*;

use nd_capture::MutterBackend;
use nd_core::capture::{CaptureBackend, SourceType};
use nd_core::pipeline::{self, PipelineEvent};

#[tokio::main]
async fn main() -> nd_core::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,nd_capture=debug".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let width: u32 = args.next().and_then(|a| a.parse().ok()).unwrap_or(1920);
    let height: u32 = args.next().and_then(|a| a.parse().ok()).unwrap_or(1080);
    let secs: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(10);

    let backend = MutterBackend::new();
    if !backend.is_available().await {
        eprintln!(
            "org.gnome.Mutter.ScreenCast is not on the bus — a virtual monitor \
             needs native GNOME (it is unavailable under Flatpak, KDE, Sway…)"
        );
        return Ok(());
    }

    let supported = backend.supported_sources().await;
    eprintln!("source types offered: {supported:?}");
    if !supported.contains(&SourceType::Virtual) {
        eprintln!("this compositor does not offer a virtual monitor");
        return Ok(());
    }

    // `VM_MODE=1280x720` asks for a monitor of a different size than the one
    // the pipeline requests. That is what tells "the compositor honoured the
    // `modes` property" apart from "PipeWire negotiated the pipeline's size and
    // the two happened to match".
    let (mode_w, mode_h) = std::env::var("VM_MODE")
        .ok()
        .and_then(|spec| {
            let (w, h) = spec.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((width, height));
    backend.set_virtual_size(mode_w, mode_h).await;
    if (mode_w, mode_h) != (width, height) {
        eprintln!("monitor mode requested: {mode_w}x{mode_h} (pipeline asks {width}x{height})");
    }
    eprintln!("creating a {width}x{height} virtual monitor…");

    let source = backend.start(SourceType::Virtual).await?;
    eprintln!(
        "virtual monitor created: node {} size {:?}",
        source.node_id, source.size
    );
    eprintln!("it should now show up in Settings › Displays as an extra screen");

    // A node with no frames is the failure mode worth catching: D-Bus says yes
    // and nothing is ever rendered.
    let cfg = pipeline::StreamConfig {
        width,
        height,
        ..Default::default()
    };
    let desc = pipeline::mirror_pipeline_description(&cfg, &source.video_source());
    let (gst_pipeline, mut events) = pipeline::build_pipeline(&desc, cfg.latency_ms())?;
    gst_pipeline
        .set_state(gstreamer::State::Playing)
        .map_err(|e| nd_core::NdError::Gst(e.to_string()))?;

    let sink = gst_pipeline
        .by_name(pipeline::MIRROR_VIDEO_SINK)
        .and_then(|e| e.downcast::<gstreamer_app::AppSink>().ok())
        .ok_or_else(|| nd_core::NdError::Gst("video appsink not found".into()))?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let mut frames = 0u64;
    while tokio::time::Instant::now() < deadline {
        if let Ok(PipelineEvent::Error { message, debug }) = events.try_recv() {
            eprintln!("pipeline failed: {message}\n{debug}");
            break;
        }
        if sink
            .try_pull_sample(gstreamer::ClockTime::from_mseconds(200))
            .is_some()
        {
            frames += 1;
        }
    }

    let _ = gst_pipeline.set_state(gstreamer::State::Null);
    drop(source);
    backend.stop().await?;

    eprintln!("\n{frames} frames captured in {secs}s");
    if frames == 0 {
        eprintln!(
            "the monitor was created but produced nothing — this is the silent failure \
             the example exists to catch"
        );
    } else {
        eprintln!("virtual monitor working; it has been removed on exit");
    }
    Ok(())
}
