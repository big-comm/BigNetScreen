//! Exercises the Cast control channel against a real receiver.
//!
//!   cargo run -p nd-chromecast --example cast_status -- <IP> [launch]
//!
//! Without `launch`: connects and prints the status (read-only, safe).
//! With `launch`: also starts the Default Media Receiver — the "ready to cast"
//! screen appears on the projector. Unlike the earlier version, a refusal from
//! the receiver now becomes a visible error instead of going unnoticed.

use std::net::IpAddr;
use std::time::Duration;

use nd_chromecast::cast::{CastChannel, DEFAULT_MEDIA_RECEIVER};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let ip: IpAddr = match args.next() {
        Some(arg) => arg.parse()?,
        None => {
            eprintln!("usage: cast_status <receiver IP> [launch]");
            return Ok(());
        }
    };
    // `launch` uses the Default Media Receiver; any other value is treated as
    // an App ID to start (e.g. 0F5096E8, the mirroring app).
    let launch_arg = args.next();
    let app_to_launch = match launch_arg.as_deref() {
        Some("launch") => Some(DEFAULT_MEDIA_RECEIVER.to_string()),
        Some(other) => Some(other.to_string()),
        None => None,
    };

    println!("conectando a {ip}:8009…");
    let channel = CastChannel::connect(ip).await?;
    println!("TLS + CONNECT ok (certificado validado: cadeia e prazo)");

    // The reply is now correlated by requestId: this genuinely waits.
    let status = channel.status().await?;
    println!("receiver status: {status:#}");

    if let Some(app_id) = &app_to_launch {
        println!("iniciando {app_id}…");
        match channel.launch(app_id).await {
            Ok(app) => {
                println!(
                    "app started: transport={} session={}",
                    app.transport_id, app.session_id
                );
                // The namespaces reveal which protocol the app speaks — that
                // is how one finds out whether the mirroring path is there.
                if let Ok(status) = channel.status().await {
                    println!("status after the launch: {status:#}");
                }
            }
            Err(err) => println!("falhou ao iniciar {app_id}: {err}"),
        }
    }

    println!("waiting for spontaneous events (10s)…");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(3), channel.next_event()).await {
            Ok(Some(event)) => {
                let kind = event
                    .payload
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                println!("[{}] {kind}", event.namespace);
            }
            Ok(None) => {
                println!("(channel closed by the receiver)");
                break;
            }
            Err(_) => {} // sem eventos nesta janela; o heartbeat segue sozinho
        }
    }

    Ok(())
}
