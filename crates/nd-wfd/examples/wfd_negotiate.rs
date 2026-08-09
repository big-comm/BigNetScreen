//! WFD negotiation diagnostic: forms the Wi-Fi Direct group, brings the RTSP
//! server up on 7236 and negotiates M1–M3, printing **the decoded video
//! modes** from the CEA/VESA/HH tables — and which one would be chosen.
//!
//!   cargo run -p nd-wfd --example wfd_negotiate
//!
//! Requires the TV/projector to be in "Screen Mirroring" mode.

use std::time::Duration;

use nd_net::p2p::P2pDevice;
use nd_wfd::rtsp::{negotiate_caps, RTSP_PORT};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,nd_wfd=debug")),
        )
        .init();

    let device = P2pDevice::open().await?;
    device.start_find().await?;
    eprintln!("looking for a Miracast sink (TV in Screen Mirroring mode)…");

    let mut peer = None;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if let Some(p) = device.peers().await?.into_iter().find(|p| p.is_wfd) {
            eprintln!("peer WFD: {} [{}]", p.name, p.hw_address);
            peer = Some(p.path);
            break;
        }
    }
    let peer = peer.ok_or("nenhum sink Miracast encontrado")?;

    eprintln!("formando grupo Wi-Fi Direct…");
    let (_active, ip) = device
        .connect_and_wait(&peer, 4, Duration::from_secs(20))
        .await?;
    eprintln!("grupo formado; nosso IP: {ip}");

    let listener = TcpListener::bind((ip, RTSP_PORT)).await?;
    eprintln!("RTSP/WFD on {ip}:{RTSP_PORT} — waiting for the sink (up to 40s)…");

    let (stream, addr) = tokio::time::timeout(Duration::from_secs(40), listener.accept()).await??;
    eprintln!("sink conectou de {addr}! negociando M1–M3…");

    let caps = negotiate_caps(stream).await?;
    println!("\ncapacidades do sink");
    println!("  wfd_video_formats: {:?}", caps.video_formats);
    println!("  wfd_audio_codecs:  {:?}", caps.audio_codecs);
    println!("  porta RTP:         {}", caps.rtp_port);
    println!(
        "  profile/level:     {:#04x}/{:#04x}",
        caps.profile, caps.level
    );
    println!("\n  {} modos suportados:", caps.modes.len());
    for mode in &caps.modes {
        println!(
            "    {:>4}x{:<4} @{:>2}Hz {:?}{}",
            mode.width,
            mode.height,
            mode.fps,
            mode.table,
            if mode.interlaced { " (interlaced)" } else { "" }
        );
    }
    match caps.best_mode_for((1920, 1080), (1920, 1080)) {
        Some(mode) => println!(
            "\n  chosen for a 1920x1080 screen: {}x{}@{}Hz",
            mode.width, mode.height, mode.fps
        ),
        None => println!("\n  no usable mode!"),
    }

    Ok(())
}
