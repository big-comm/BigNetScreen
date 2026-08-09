//! Integration test for the stream path, with no Chromecast required.
//!
//! It exists because of a real bug: the client socket was handed to
//! `multisocketsink` while the pipeline was still in `Null`, GStreamer refused
//! it with nothing but a `WARNING` on the bus, and the receiver got **zero
//! bytes**. No unit test could see that — only running the whole path could.
//!
//! Here the complete path is exercised: HTTP server → request triage → handing
//! the descriptor to GStreamer → real Matroska bytes on the connection.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use gst::prelude::*;
use gstreamer as gst;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use nd_chromecast::http::StreamServer;
use nd_core::pipeline::{self, StreamConfig, VideoSource, CHROMECAST_SINK_NAME};

/// The EBML signature that opens every Matroska/WebM file.
const EBML_MAGIC: [u8; 4] = [0x1A, 0x45, 0xDF, 0xA3];

/// Are the required plugins present on this machine?
fn media_stack_available() -> bool {
    if pipeline::init().is_err() {
        return false;
    }
    let needed = [
        "matroskamux",
        "multisocketsink",
        "avenc_aac",
        "videotestsrc",
    ];
    needed
        .iter()
        .all(|name| gst::ElementFactory::find(name).is_some())
        && !pipeline::probe_encoders().is_empty()
}

async fn read_http_response(stream: &mut TcpStream, want: usize) -> (String, Vec<u8>) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];

    // Headers plus at least `want` bytes of body, with a deadline.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline {
        let read = match tokio::time::timeout(Duration::from_secs(3), stream.read(&mut chunk)).await
        {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => n,
            Ok(Err(_)) => break,
            Err(_) => continue,
        };
        buffer.extend_from_slice(&chunk[..read]);

        if let Some(split) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            if buffer.len() >= split + 4 + want {
                break;
            }
        }
    }

    let split = buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(buffer.len());
    let headers = String::from_utf8_lossy(&buffer[..split]).into_owned();
    let body = buffer[split..].to_vec();
    (headers, body)
}

#[tokio::test(flavor = "multi_thread")]
async fn serves_real_matroska_bytes_to_the_receiver() {
    if !media_stack_available() {
        eprintln!("media plugins missing; test skipped");
        return;
    }

    let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let server = StreamServer::bind(loopback)
        .await
        .expect("servidor do stream");
    let url = server.url();
    let addr = server.local_addr();

    let encoder = pipeline::best_encoder(pipeline::GpuDriver::Unknown).expect("algum encoder");
    let cfg = StreamConfig {
        width: 640,
        height: 480,
        encoder,
        ..Default::default()
    };
    let desc = pipeline::chromecast_pipeline_description(&cfg, &VideoSource::Test);
    let (gst_pipeline, _events) =
        pipeline::build_pipeline(&desc, cfg.latency_ms()).expect("pipeline");

    // A direct regression for the bug: `build_pipeline` has to leave `Null`,
    // otherwise `multisocketsink` refuses the client and the stream is empty.
    let (_, state, _) = gst_pipeline.state(gst::ClockTime::from_seconds(1));
    assert_ne!(
        state,
        gst::State::Null,
        "the pipeline must not come back in Null: the sink would refuse the client"
    );

    let sink = gst_pipeline
        .by_name(CHROMECAST_SINK_NAME)
        .expect("multisocketsink presente e nomeado");

    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let play = gst_pipeline.clone();
    let serving = tokio::spawn(async move {
        server
            .serve(
                sink,
                move || {
                    play.set_state(gst::State::Playing)
                        .map(|_| ())
                        .map_err(|e| nd_core::NdError::Gst(e.to_string()))
                },
                cancel_rx,
            )
            .await
    });

    // A client doing what the Chromecast would do.
    let path = url.rsplit_once('/').map(|(_, t)| format!("/{t}")).unwrap();
    let mut client = TcpStream::connect(addr).await.expect("conectar");
    client
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes())
        .await
        .expect("enviar GET");

    let (headers, body) = read_http_response(&mut client, 8192).await;

    assert!(headers.starts_with("HTTP/1.1 200 OK"), "{headers}");
    assert!(headers.contains("video/x-matroska"), "{headers}");
    // A body terminated by EOF: with no declared length the stream can run
    // forever.
    assert!(!headers.contains("Content-Length"), "{headers}");

    assert!(
        body.len() >= 8192,
        "only {} bytes of media received (the Null-pipeline bug gave 0)",
        body.len()
    );
    assert_eq!(
        &body[..4],
        &EBML_MAGIC,
        "the body should start with Matroska's EBML signature"
    );

    let _ = cancel_tx.send(true);
    drop(client);
    let _ = tokio::time::timeout(Duration::from_secs(5), serving).await;
    let _ = gst_pipeline.set_state(gst::State::Null);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_token_gets_nothing() {
    if !media_stack_available() {
        return;
    }

    let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let server = StreamServer::bind(loopback).await.expect("servidor");
    let addr = server.local_addr();

    let cfg = StreamConfig::default();
    let desc = pipeline::chromecast_pipeline_description(&cfg, &VideoSource::Test);
    let (gst_pipeline, _events) =
        pipeline::build_pipeline(&desc, cfg.latency_ms()).expect("pipeline");
    let sink = gst_pipeline.by_name(CHROMECAST_SINK_NAME).expect("sink");

    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let serving = tokio::spawn(async move { server.serve(sink, || Ok(()), cancel_rx).await });

    let mut client = TcpStream::connect(addr).await.expect("conectar");
    client
        .write_all(b"GET /token-errado HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .expect("GET");

    let (headers, body) = read_http_response(&mut client, 0).await;
    // 404 rather than 403: a portscan should not even confirm a server is here.
    assert!(headers.starts_with("HTTP/1.1 404"), "{headers}");
    assert!(body.is_empty(), "leaked {} bytes of media", body.len());

    let _ = cancel_tx.send(true);
    drop(client);
    let _ = tokio::time::timeout(Duration::from_secs(5), serving).await;
    let _ = gst_pipeline.set_state(gst::State::Null);
}
