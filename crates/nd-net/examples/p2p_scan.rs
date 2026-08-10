//! Exercises Wi-Fi Direct (P2P) discovery through NetworkManager.
//!
//!   cargo run -p nd-net --example p2p_scan
//!
//! Put the TV/projector into "Screen Mirroring" so it shows up as a peer with
//! `wfd=true` (a Miracast sink).

use std::time::Duration;

use nd_net::p2p::P2pDevice;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    println!("locating the Wi-Fi P2P device…");
    let device = P2pDevice::open().await?;
    println!("StartFind…");
    device.start_find().await?;

    for i in 1..=15 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let peers = device.peers().await?;
        println!("[{}s] {} peer(s):", i * 2, peers.len());
        for peer in &peers {
            println!(
                "   {:<24} {}  wfd={}",
                peer.name, peer.hw_address, peer.is_wfd
            );
        }
    }

    device.stop_find().await?;
    println!("StopFind. fim.");
    Ok(())
}
