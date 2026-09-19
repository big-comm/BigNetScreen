//! Publishes a test pattern to web browsers and prints the address and PIN.
//!
//! For trying the page on a real TV, phone or browser without the GUI:
//!
//! ```text
//! cargo run -p nd-webrtc --example web_publish
//! ```
//!
//! Runs until Ctrl+C.

use nd_core::pipeline::{AudioSource, GpuDriver, VideoSource};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,nd_webrtc=debug".into()),
        )
        .init();
    let text = nd_webrtc::PageText {
        title: "BigNetScreen test pattern".into(),
        prompt: "Enter the PIN printed in the terminal.".into(),
        join: "Watch".into(),
        wrong_pin: "That PIN is not right.".into(),
        locked: "Too many attempts; wait half a minute.".into(),
        connecting: "Connecting…".into(),
        failed: "Could not connect.".into(),
        ended: "The sharing has ended.".into(),
        fullscreen_hint: "Tap or click the picture for full screen.".into(),
    };
    let mut session = nd_webrtc::Session::start(
        &VideoSource::Test,
        Some(AudioSource::Silence),
        (1280, 720),
        30,
        GpuDriver::Unknown,
        &text,
    )
    .await?;
    println!("URL: {}", session.access().url);
    println!("PIN: {}", session.access().pin);
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = ticker.tick() => println!("receivers: {}", session.receivers()),
            event = futures::StreamExt::next(session.events()) => {
                println!("pipeline event: {event:?}");
                if event.is_none() { break; }
            }
        }
    }
    session.close().await;
    Ok(())
}
