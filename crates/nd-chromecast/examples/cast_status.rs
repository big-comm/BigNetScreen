//! Testa o canal de controle Cast contra um receptor real.
//!
//!   cargo run -p nd-chromecast --example cast_status -- <IP> [launch]
//!
//! Sem `launch`: conecta e imprime o status (read-only, seguro).
//! Com `launch`: também dá LAUNCH no Default Media Receiver — a tela "pronto
//! para transmitir" aparece no projetor.

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
    let ip: IpAddr = args
        .next()
        .unwrap_or_else(|| "192.168.68.101".to_string())
        .parse()?;
    let do_launch = args.next().as_deref() == Some("launch");

    println!("conectando a {ip}:8009…");
    let mut channel = CastChannel::connect(ip).await?;
    println!("TLS + CONNECT ok");

    channel.request_status().await?;
    if do_launch {
        println!("LAUNCH {DEFAULT_MEDIA_RECEIVER}…");
        channel.launch(DEFAULT_MEDIA_RECEIVER).await?;
    }

    for _ in 0..15 {
        match tokio::time::timeout(Duration::from_secs(6), channel.next_event()).await {
            Ok(Ok((namespace, payload))) => {
                let kind = payload
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                println!("[{namespace}] {kind}: {payload}");
            }
            Ok(Err(err)) => {
                eprintln!("erro: {err}");
                break;
            }
            Err(_) => {
                println!("(sem mais eventos)");
                break;
            }
        }
    }

    Ok(())
}
