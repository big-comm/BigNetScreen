//! The session service.
//!
//! Started by D-Bus when a client asks for it, and gone again a few minutes
//! after the last thing it was doing finished. Run it by hand to watch it
//! work; `--foreground` only changes whether it exits when idle.

use futures::StreamExt;
use std::time::Duration;

/// How long to stay after the last session ends.
///
/// Long enough that closing a window and opening it again does not pay for a
/// fresh start; short enough that a forgotten service is not a resident cost.
const IDLE_GRACE: Duration = Duration::from_secs(180);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let foreground = std::env::args().any(|arg| arg == "--foreground");

    // Useful in a bug report, and cheap: which encoder this machine will use.
    let driver = nd_net::detect_gpu_driver();
    match nd_core::pipeline::best_encoder(driver, nd_core::pipeline::Acceleration::preferred()) {
        Ok(encoder) => tracing::info!(?driver, ?encoder, "video encoding"),
        Err(err) => tracing::error!(?driver, %err, "no H.264 encoder available"),
    }

    let engine = nd_service::engine::start();
    let connection = nd_service::dbus::serve(engine.clone()).await?;
    tracing::info!(name = nd_service::BUS_NAME, "session service ready");

    let mut messages = zbus::MessageStream::from(&connection);
    // Being asked to stop (the session ending, the service manager, Ctrl+C) goes
    // through the same shutdown as going idle. A signal that killed the process
    // outright skipped it, and with it the removal of the virtual sound card.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = async { while let Some(Ok(_)) = messages.next().await {} } => {
            tracing::info!("session bus disconnected; stopping the service");
        }
        _ = terminate.recv() => {
            tracing::info!("asked to stop; stopping the service");
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("interrupted; stopping the service");
        }
        _ = nd_service::dbus::idle_after(engine.subscribe(), IDLE_GRACE), if !foreground => {
            tracing::info!("idle; exiting until something asks again");
        }
    }
    let mut snapshots = engine.subscribe();
    let _ = engine.send(nd_service::engine::Command::Shutdown).await;
    // Keep Tokio alive until the engine's session owners finish STOP/CLOSE.
    while snapshots.changed().await.is_ok() {}
    Ok(())
}
