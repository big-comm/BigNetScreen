//! Validates the stream server **without needing a Chromecast**.
//!
//! It brings up the HTTP server and the pipeline (a test pattern) listening on
//! `127.0.0.1`, prints the URL and serves it. In another terminal:
//!
//! ```sh
//! curl -s -o /tmp/cc.mkv --max-time 5 '<printed URL>'
//! ffprobe /tmp/cc.mkv        # should show H.264 + AAC in Matroska
//! ```
//!
//! This exercises the riskiest step of the path: handing `multisocketsink` a
//! socket descriptor whose headers have already been written.
//!
//!   cargo run -p nd-chromecast --example cc_stream_local

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use gst::prelude::*;
use gstreamer as gst;

use nd_chromecast::http::StreamServer;
use nd_core::pipeline::{self, StreamConfig, VideoSource, CHROMECAST_SINK_NAME};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,nd_chromecast=debug")),
        )
        .init();

    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(20);

    // The "receiver" is localhost itself.
    let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let server = StreamServer::bind(loopback).await?;

    let encoder = pipeline::best_encoder(pipeline::GpuDriver::Unknown)?;
    let cfg = StreamConfig {
        width: 1280,
        height: 720,
        encoder,
        ..Default::default()
    };
    let desc = pipeline::chromecast_pipeline_description(&cfg, &VideoSource::Test);
    let (gst_pipeline, _events) = pipeline::build_pipeline(&desc, cfg.latency_ms())?;
    let sink = gst_pipeline
        .by_name(CHROMECAST_SINK_NAME)
        .expect("multisocketsink no pipeline");

    println!("\nencoder: {encoder:?}");
    println!("URL:     {}", server.url());
    println!(
        "\nteste:   curl -s -o /tmp/cc.mkv --max-time 5 '{}'",
        server.url()
    );
    println!("         ffprobe /tmp/cc.mkv\n");
    println!("servindo por {secs}s…\n");

    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let play = gst_pipeline.clone();

    let serving = tokio::spawn(async move {
        server
            .serve(
                sink,
                move || {
                    println!(">>> cliente conectado; pipeline em Playing");
                    play.set_state(gst::State::Playing)
                        .map(|_| ())
                        .map_err(|e| nd_core::NdError::Gst(e.to_string()))
                },
                cancel_rx,
            )
            .await
    });

    tokio::time::sleep(Duration::from_secs(secs)).await;
    let _ = cancel_tx.send(true);
    let _ = serving.await;
    let _ = gst_pipeline.set_state(gst::State::Null);

    println!("fim.");
    Ok(())
}
