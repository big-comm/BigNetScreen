//! Low-latency Cast mirroring (Cast Streaming, app 0F5096E8).
//!
//!   cargo run -p nd-chromecast --example cc_mirror -- <IP>            # pattern
//!   cargo run -p nd-chromecast --example cc_mirror -- <IP> screen 60  # screen
//!
//! Unlike `cc_cast` (Default Media Receiver + HTTP), there is no container and
//! no media player on the other end — the delay is the negotiated
//! `targetDelay`, not seconds of pre-buffering.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use nd_chromecast::mirror_session;
use nd_core::pipeline::VideoSource;
use nd_core::sink::{SinkState, SinkStatus};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,nd_chromecast=debug")),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let ip: IpAddr = args
        .next()
        .ok_or("uso: cc_mirror <IP> [screen] [segundos]")?
        .parse()?;
    let rest: Vec<String> = args.collect();
    let use_screen = rest.iter().any(|a| a == "screen");
    let secs: u64 = rest.iter().find_map(|a| a.parse().ok()).unwrap_or(60);

    let status = Arc::new(SinkStatus::new());
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

    let stopper = cancel_tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(secs)).await;
        eprintln!("\n>>> deadline reached; shutting down…");
        let _ = stopper.send(true);
    });
    let on_signal = cancel_tx.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = on_signal.send(true);
        }
    });

    let watched = status.clone();
    let reporter = tokio::spawn(async move {
        let mut last: Option<SinkState> = None;
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let state = watched.state();
            if last != Some(state) {
                eprintln!("    estado: {state:?}");
                last = Some(state);
            }
        }
    });

    let capture = if use_screen {
        let backend = nd_capture::select_backend().await;
        let source = backend.start(nd_core::capture::SourceType::Monitor).await?;
        eprintln!("capture: node {} {:?}", source.node_id, source.size);
        Some((backend, source))
    } else {
        None
    };
    let (video, size) = match &capture {
        Some((_, source)) => (source.video_source(), source.size_or((1920, 1080))),
        // An animated pattern with a clock: you can tell from across the room
        // whether the picture is live or frozen, and comparing the clock with
        // the local screen measures the delay with no instrumentation at all.
        // `WxH` on the command line drives the test pattern's size, which is
        // how a resolution can be exercised without a screen that has it.
        None => (
            VideoSource::Diagnostic,
            rest.iter()
                .find_map(|a| {
                    let (w, h) = a.split_once('x')?;
                    Some((w.parse().ok()?, h.parse().ok()?))
                })
                .unwrap_or((1280, 720)),
        ),
    };

    eprintln!("mirroring to {ip} for up to {secs}s…");
    let result = mirror_session::run(
        ip,
        nd_chromecast::cast::PORT,
        video,
        size,
        &status,
        cancel_rx,
    )
    .await;
    reporter.abort();

    if let Some((backend, source)) = capture {
        let _ = backend.stop().await;
        drop(source);
    }

    match result {
        Ok(()) => {
            eprintln!("session ended.");
            Ok(())
        }
        Err(err) => {
            eprintln!("failed: {err}");
            if let Some(detail) = status.message() {
                eprintln!("detalhe: {detail}");
            }
            Err(err.into())
        }
    }
}
