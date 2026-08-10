//! Proves the media stack end to end, already using the production tuning.
//!
//! Screen capture → `nd_core::pipeline` (encoder picked from the registry,
//! with the real low-latency properties) → an MP4 file.
//!
//! ```sh
//! cargo run -p nd-capture --example spike_capture_encode
//! ```
//!
//! Needs a graphical session: the portal opens a dialog to pick the monitor
//! (only the first time — after that the restore token skips it).
//! Output: `/tmp/bignetscreen-spike.mp4` (≈ 8 s of recording).

use std::time::Duration;

use gst::prelude::*;
use gstreamer as gst;

use nd_capture::select_backend;
use nd_core::capture::SourceType;
use nd_core::pipeline::{self, StreamConfig};

const OUTPUT: &str = "/tmp/bignetscreen-spike.mp4";
const RECORD_SECS: u64 = 8;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    pipeline::init()?;

    // The same encoder selection production uses: registry + GPU driver.
    let driver = nd_net_driver();
    let encoder = pipeline::best_encoder(driver)?;
    println!("encoder escolhido: {encoder:?} (driver {driver:?})");

    println!("requesting capture from the portal…");
    let backend = select_backend().await;
    let source = backend.start(SourceType::Monitor).await?;
    let (width, height) = source.size_or((1920, 1080));
    println!(
        "PipeWire stream acquired: node {} ({width}x{height})",
        source.node_id
    );

    let cfg = StreamConfig {
        width,
        height,
        encoder,
        ..Default::default()
    };

    // `source.video_source()` guarantees the right `fd=`/`path=` — building
    // the description by hand was exactly the source of the "captures the
    // wrong node" bug.
    let desc = format!(
        "{src} ! {enc} ! h264parse ! mp4mux ! filesink location={out}",
        src = describe_source(&cfg, &source),
        enc = encoder.encoder_description(&cfg),
        out = OUTPUT,
    );

    let (pipeline, mut events) = pipeline::build_pipeline(&desc, cfg.latency_ms())?;
    pipeline.set_state(gst::State::Playing)?;
    println!("recording for {RECORD_SECS}s…");

    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(RECORD_SECS)) => {}
        Some(event) = futures::StreamExt::next(&mut events) => {
            eprintln!("evento do pipeline: {event:?}");
        }
    }

    // EOS so mp4mux closes the container properly.
    pipeline.send_event(gst::event::Eos::new());
    if let Some(bus) = pipeline.bus() {
        // The sync handler already drains the bus; a brief wait is enough for
        // the muxer to finish writing the index.
        let _ = bus;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    pipeline.set_state(gst::State::Null)?;
    backend.stop().await?;
    drop(source); // only now is it safe to close the fd

    println!("pronto: {OUTPUT}");
    Ok(())
}

/// The source + conversion fragment, mirroring the start of the production pipeline.
fn describe_source(cfg: &StreamConfig, source: &nd_core::capture::CaptureSource) -> String {
    let video = source.video_source();
    // Reuses the core's conversion logic through a throwaway full pipeline,
    // so the rules are not duplicated here.
    let full = pipeline::chromecast_pipeline_description(cfg, &video);
    // Take everything up to the encoder (exclusive).
    let cut = full
        .find(cfg.encoder.element())
        .expect("the description contains the encoder");
    full[..cut].trim_end().trim_end_matches('!').to_string()
}

/// The `nd-net` crate only exists in the native build; detect it directly here.
fn nd_net_driver() -> nd_core::pipeline::GpuDriver {
    use nd_core::pipeline::GpuDriver;
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return GpuDriver::Unknown;
    };
    let mut names: Vec<_> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("card") && n["card".len()..].chars().all(|c| c.is_ascii_digit()))
        .collect();
    names.sort();
    for card in names {
        let link = format!("/sys/class/drm/{card}/device/driver");
        if let Ok(target) = std::fs::read_link(&link) {
            if let Some(module) = target.file_name().and_then(|n| n.to_str()) {
                let driver = GpuDriver::from_kernel_module(module);
                if driver != GpuDriver::Unknown {
                    return driver;
                }
            }
        }
    }
    GpuDriver::Unknown
}
