//! Creates a **virtual monitor** and reports what came back.
//!
//! ```sh
//! cargo run -p nd-capture --example virtual_monitor [width] [height] [seconds] [fps]
//! ```
//!
//! A virtual monitor is an extra desktop that exists only for the cast: the
//! receiver shows a second screen instead of duplicating the laptop's. It goes
//! through whatever [`nd_capture::select_backend_for`] picks here — Mutter on
//! GNOME, `zkde_screencast_unstable_v1` on KWin, the portal otherwise — so the
//! size below is also the check that the chosen backend honours a size at all.
//!
//! The example exists because this path is easy to write and hard to trust: it
//! either creates a screen the compositor really renders to, or it succeeds on
//! D-Bus and produces a node that never emits a frame. Only running it tells
//! the two apart, so it also **counts the frames** that come out.

use std::time::Duration;

use gstreamer::prelude::*;

use nd_core::capture::SourceType;
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
    // The frame count below is only worth reading against a rate that was
    // asked for: a screen that renders at 60 and a pipeline that asks for 30
    // both produce 30.
    let fps: u32 = args.next().and_then(|a| a.parse().ok()).unwrap_or(30);

    // `VM_MODE=1280x720` asks for a monitor of a different size than the one
    // the pipeline requests. That is what tells "the compositor honoured the
    // size we asked for" apart from "PipeWire negotiated the pipeline's size
    // and the two happened to match".
    let (mode_w, mode_h) = std::env::var("VM_MODE")
        .ok()
        .and_then(|spec| {
            let (w, h) = spec.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((width, height));

    // The backends read the size from the preferences, same as the app does.
    let mut chosen = nd_core::settings::current();
    chosen.quality = nd_core::settings::Quality::Custom;
    chosen.custom_width = mode_w;
    chosen.custom_height = mode_h;
    nd_core::settings::set_in_memory(&chosen);

    let backend = nd_capture::select_backend_for(SourceType::Virtual, None).await;
    eprintln!("backend: {}", backend.id());
    if (mode_w, mode_h) != (width, height) {
        eprintln!("monitor mode requested: {mode_w}x{mode_h} (pipeline asks {width}x{height})");
    }
    eprintln!("creating a {mode_w}x{mode_h} virtual monitor…");

    let source = backend.start(SourceType::Virtual).await?;
    eprintln!(
        "virtual monitor created: node {} size {:?}",
        source.node_id, source.size
    );
    eprintln!("it should now show up in Settings › Displays as an extra screen");

    // A node with no frames is the failure mode worth catching: D-Bus says yes
    // and nothing is ever rendered.
    // The encoder production would pick. Counting frames through x264 says
    // nothing about a machine that casts with NVENC.
    let driver = nd_net::detect_gpu_driver();
    let encoder = pipeline::best_encoder(driver, pipeline::Acceleration::preferred())?;
    eprintln!("encoder: {encoder:?} (driver {driver:?})");
    let cfg = pipeline::StreamConfig {
        width,
        height,
        fps,
        encoder,
        ..Default::default()
    };
    let desc = pipeline::mirror_pipeline_description(
        &cfg,
        &source.video_source(),
        pipeline::CHROMECAST_MAX_RESOLUTION,
        false,
    );
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

    eprintln!("\n{frames} frames captured in {secs}s (asked for {fps} fps)");
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
