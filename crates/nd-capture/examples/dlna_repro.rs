//! Reproduces the DLNA "negotiation problem" with a real screen capture.
//!
//! Builds exactly what `nd-dlna` builds for a screen (VA-API encoder, `UpTo`
//! target, MPEG-TS over `multisocketsink`) and runs it for a few seconds,
//! printing what the pipeline reports.
//!
//! ```sh
//! GST_DEBUG=2 cargo run -p nd-capture --example dlna_repro
//! ```

use std::time::Duration;

use gst::prelude::*;
use gstreamer as gst;

use nd_capture::select_backend;
use nd_core::capture::SourceType;
use nd_core::pipeline::{self, StreamConfig, VideoTarget};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    pipeline::init()?;
    let encoder: pipeline::H264Encoder = match std::env::var("REPRO_ENCODER").as_deref() {
        Ok("x264enc") => pipeline::H264Encoder::X264,
        _ => pipeline::H264Encoder::VaH264,
    };
    let target = match std::env::var("REPRO_TARGET").as_deref() {
        Ok("exact") => VideoTarget::Exact((1920, 1080)),
        _ => VideoTarget::UpTo((1920, 1080)),
    };
    let backend = select_backend().await;
    let source = backend.start(SourceType::Monitor).await?;
    println!("capture: node {} size {:?}", source.node_id, source.size);
    let cfg = StreamConfig {
        width: 1920,
        height: 1080,
        fps: 30,
        encoder,
        audio: pipeline::AudioSource::Silence,
        ..Default::default()
    };
    let desc = pipeline::ts_http_pipeline_description(
        &cfg,
        &source.video_source(),
        target,
        Some(8_000_000),
    );
    // `REPRO_NO_SQUARE=1` leaves the compositor's pixel aspect as it came, to
    // show the failure the normaliser exists for.
    let desc = if std::env::var_os("REPRO_NO_SQUARE").is_some() {
        desc.replace(&format!(" ! {}", pipeline::SQUARE_PIXELS), "")
    } else {
        desc
    };
    println!("description:\n{desc}\n");
    let (pl, mut events) = pipeline::build_pipeline(&desc, cfg.latency_ms())?;
    pl.set_state(gst::State::Playing)?;
    let outcome = tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(6)) => "ran 6 s without an error".to_string(),
        Some(event) = futures::StreamExt::next(&mut events) => format!("pipeline event: {event:?}"),
    };
    println!("{outcome}");
    if let Some(pad) = pl
        .by_name(pipeline::CAPTURE_SOURCE_NAME)
        .and_then(|e| e.static_pad("src"))
    {
        println!(
            "capture caps: {:?}",
            pad.current_caps().map(|c| c.to_string())
        );
    }
    if let Some(pad) = pl.by_name("scale-caps").and_then(|e| e.static_pad("src")) {
        println!(
            "scale caps: {:?}",
            pad.current_caps().map(|c| c.to_string())
        );
    }
    let _ = pl.set_state(gst::State::Null);
    backend.stop().await?;
    drop(source);
    Ok(())
}
