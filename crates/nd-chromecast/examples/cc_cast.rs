//! A full cast to a **real** Chromecast.
//!
//! ```sh
//! # test pattern (opens no capture dialog)
//! cargo run -p nd-chromecast --example cc_cast -- 192.168.68.108
//!
//! # the actual screen (the portal asks for permission the first time)
//! cargo run -p nd-chromecast --example cc_cast -- 192.168.68.108 screen
//!
//! # duration in seconds (default 60)
//! cargo run -p nd-chromecast --example cc_cast -- 192.168.68.108 screen 30
//! ```
//!
//! ⚠️ This **takes over the receiver**: the app on the TV/projector is started
//! and shows whatever is streamed. At the end (deadline or Ctrl-C) the session
//! is closed and the app is shut down.
//!
//! Find the IP with `cargo run -p nd-chromecast --example mdns_scan`.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use nd_chromecast::session;
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
    let Some(ip) = args.next() else {
        eprintln!("uso: cc_cast <IP do Chromecast> [screen] [segundos]");
        eprintln!("     (find the IP with the mdns_scan example)");
        return Ok(());
    };
    let ip: IpAddr = ip.parse()?;

    let rest: Vec<String> = args.collect();
    let use_screen = rest.iter().any(|a| a == "screen");
    let secs: u64 = rest.iter().find_map(|a| a.parse().ok()).unwrap_or(60);

    let status = Arc::new(SinkStatus::new());
    let (handle, cancel) = session::cancellation();

    // Ends on its own after the deadline, so the receiver is not left stuck.
    let stopper = handle.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(secs)).await;
        eprintln!("\n>>> deadline reached; closing the session…");
        stopper.stop();
    });

    // Ctrl-C exits cleanly (shuts the app on the TV down instead of leaving it stuck).
    let on_signal = handle.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\n>>> interrupted; closing the session…");
            on_signal.stop();
        }
    });

    // Shows the state transitions while the session runs.
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

    eprintln!("streaming to {ip} for up to {secs}s…");

    let result = if use_screen {
        let backend = nd_capture::select_backend().await;
        let source = backend.start(nd_core::capture::SourceType::Monitor).await?;
        eprintln!(
            "capture started (node {}, {:?})",
            source.node_id, source.size
        );
        let outcome = session::run(ip, source, &status, cancel).await;
        let _ = backend.stop().await;
        outcome
    } else {
        eprintln!("streaming a 1280x720 test pattern");
        session::run_with_video(ip, VideoSource::Test, (1280, 720), &status, cancel).await
    };

    reporter.abort();

    match result {
        Ok(()) => {
            eprintln!("session ended normally.");
            Ok(())
        }
        Err(err) => {
            eprintln!("falhou: {err}");
            if let Some(detail) = status.message() {
                eprintln!("detalhe: {detail}");
            }
            Err(err.into())
        }
    }
}
