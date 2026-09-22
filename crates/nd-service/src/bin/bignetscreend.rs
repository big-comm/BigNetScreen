//! The session service.
//!
//! Started by D-Bus when a client asks for it, and gone again a few minutes
//! after the last thing it was doing finished. Run it by hand to watch it
//! work; `--foreground` only changes whether it exits when idle.

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
    match nd_core::pipeline::best_encoder(driver) {
        Ok(encoder) => tracing::info!(?driver, ?encoder, "video encoding"),
        Err(err) => tracing::error!(?driver, %err, "no H.264 encoder available"),
    }

    let engine = nd_service::engine::start();
    let _connection = nd_service::dbus::serve(engine.clone()).await?;
    tracing::info!(name = nd_service::BUS_NAME, "session service ready");

    if foreground {
        std::future::pending::<()>().await;
        return Ok(());
    }
    nd_service::dbus::idle_after(engine.subscribe(), IDLE_GRACE).await;
    tracing::info!("idle; exiting until something asks again");
    let _ = engine.send(nd_service::engine::Command::Shutdown).await;
    Ok(())
}
