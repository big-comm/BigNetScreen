//! Diagnostic: runs the **real** WFD pipeline pointed at loopback.
//!
//! It answers "is what we are sending valid?" without depending on the
//! projector. In another terminal:
//!
//! ```sh
//! ffprobe -v error -show_streams rtp://127.0.0.1:19000
//! ffplay rtp://127.0.0.1:19000
//! ```
//!
//!   cargo run -p nd-wfd --example wfd_loopback [width] [height] [fps]

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use gst::prelude::*;
use gstreamer as gst;

use nd_core::pipeline::{self, StreamConfig, VideoSource, WfdTransport};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let width: u32 = args.next().and_then(|a| a.parse().ok()).unwrap_or(1920);
    let height: u32 = args.next().and_then(|a| a.parse().ok()).unwrap_or(1080);
    let fps: u32 = args.next().and_then(|a| a.parse().ok()).unwrap_or(60);

    let encoder = pipeline::best_encoder(pipeline::GpuDriver::Unknown)?;
    let cfg = StreamConfig {
        width,
        height,
        fps,
        encoder,
        ..Default::default()
    };
    let transport = WfdTransport::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 19000);
    let desc = pipeline::wfd_pipeline_description(&cfg, &VideoSource::Test, &transport);

    println!("encoder: {encoder:?}  modo: {width}x{height}@{fps}");
    println!("\n--- pipeline description ---\n{desc}\n---\n");
    println!("read it with:  ffprobe -v error -show_streams rtp://127.0.0.1:19000\n");

    let (gst_pipeline, _events) = pipeline::build_pipeline(&desc, cfg.latency_ms())?;
    gst_pipeline.set_state(gst::State::Playing)?;
    tokio::time::sleep(Duration::from_secs(25)).await;
    gst_pipeline.set_state(gst::State::Null)?;
    Ok(())
}
