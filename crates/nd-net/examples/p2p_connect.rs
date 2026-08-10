//! Exercises the Wi-Fi Direct CONNECTION (phase 3b) with a Miracast sink.
//!
//!   cargo run -p nd-net --example p2p_connect
//!
//! Put the TV/projector into "Screen Mirroring" FIRST. The test discovers the
//! WFD peer, forms the P2P group and follows the connection state
//! (2 = ACTIVATED = group formed).

use std::time::Duration;

use nd_net::p2p::{ActiveState, P2pDevice};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let device = P2pDevice::open().await?;
    device.start_find().await?;
    eprintln!("looking for a Miracast sink (TV in Screen Mirroring mode)…");

    let mut peer = None;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if let Some(p) = device.peers().await?.into_iter().find(|p| p.is_wfd) {
            eprintln!("peer WFD: {} [{}] {}", p.name, p.hw_address, p.path);
            peer = Some(p.path);
            break;
        }
        eprintln!("  …no WFD peer yet");
    }

    let peer = match peer {
        Some(p) => p,
        None => {
            eprintln!("no Miracast sink found — is the TV in Screen Mirroring mode?");
            return Ok(());
        }
    };

    eprintln!("forming a Wi-Fi Direct group with {peer}…");
    let active = device.connect(&peer).await?;
    eprintln!("active connection: {active}");

    for _ in 0..15 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let state = device
            .active_state(&active)
            .await
            .unwrap_or(ActiveState::Unknown);
        eprintln!("  estado: {state:?}");
        if state == ActiveState::Activated {
            let ips = device.addresses(&active).await.unwrap_or_default();
            eprintln!("✅ P2P group formed! our IP on the link: {ips:?}");
            eprintln!("   (the RTSP/WFD server would listen on 7236 of that IP)");
            eprintln!(
                "   keeping the group alive for 15s (watch the projector sit on 'connecting')…"
            );
            tokio::time::sleep(Duration::from_secs(15)).await;
            break;
        }
        if state == ActiveState::Deactivated {
            eprintln!("❌ connection failed/deactivated");
            break;
        }
    }

    Ok(())
}
