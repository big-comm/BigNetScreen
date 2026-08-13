//! A full Miracast cast: forms the Wi-Fi Direct group, opens the port in
//! firewalld, negotiates WFD (M1–M7) and streams to the sink.
//!
//!   cargo run -p nd-wfd --example wfd_cast              # test pattern
//!   cargo run -p nd-wfd --example wfd_cast -- screen    # capture the screen
//!   cargo run -p nd-wfd --example wfd_cast -- virtual   # an extra desktop
//!
//! Requires the TV/projector to be in "Screen Mirroring" mode.

use std::time::Duration;

use nd_core::pipeline::{self, VideoSource};
use nd_net::firewall;
use nd_net::p2p::P2pDevice;
use nd_wfd::rtsp::{cast_to_sink, WfdCastConfig, RTSP_PORT};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,nd_wfd=debug")),
        )
        .init();

    let mode = std::env::args().nth(1).unwrap_or_default();
    let source_type = match mode.as_str() {
        "screen" => Some(nd_core::capture::SourceType::Monitor),
        // An extra desktop rather than a copy of this one.
        "virtual" => Some(nd_core::capture::SourceType::Virtual),
        _ => None,
    };

    let device = P2pDevice::open().await?;
    device.start_find().await?;
    eprintln!("looking for a Miracast sink…");

    let mut peer = None;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let peers = device.peers().await?;
        if let Some(p) = peers.iter().find(|p| p.is_wfd) {
            eprintln!("peer WFD: {} [{}]", p.name, p.hw_address);
            peer = Some(p.path.clone());
            break;
        }
    }
    let peer = peer.ok_or("no Miracast sink found")?;

    // O retry por instabilidade de driver agora mora na biblioteca.
    let (active, our_ip) = device
        .connect_and_wait(&peer, 4, Duration::from_secs(20))
        .await?;
    eprintln!("grupo formado; nosso IP: {our_ip}");

    // Without this, with firewalld active the sink cannot reach 7236.
    let interface = device.interface(&active).await.unwrap_or(None);
    let lease = firewall::ensure_ports_open(interface.as_deref()).await?;

    let listener = TcpListener::bind((our_ip, RTSP_PORT)).await?;
    eprintln!("RTSP/WFD on {our_ip}:{RTSP_PORT} — waiting for the sink (up to 40s)…");
    let (stream, addr) = tokio::time::timeout(Duration::from_secs(40), listener.accept()).await??;
    eprintln!("the sink connected from {addr}! negotiating + streaming…");

    let driver = nd_net::detect_gpu_driver();
    let encoder = pipeline::best_encoder(driver)?;
    eprintln!("encoder: {encoder:?} (driver {driver:?})");

    // Real capture or a test pattern, depending on the argument.
    let capture = match source_type {
        Some(source_type) => {
            let backend = nd_capture::select_backend_for(source_type).await;
            Some(backend.start(source_type).await?)
        }
        None => None,
    };
    let (video, source_size) = match &capture {
        Some(source) => (source.video_source(), source.size_or((1920, 1080))),
        None => (VideoSource::Test, (1920, 1080)),
    };

    let mut cfg =
        WfdCastConfig::new(our_ip, addr.ip(), video, encoder).with_source_size(source_size);
    // `WFD_MAX_RES=1280x720` caps the negotiated mode — handy for bisecting
    // interoperability problems with a specific sink.
    if let Ok(spec) = std::env::var("WFD_MAX_RES") {
        if let Some((w, h)) = spec.split_once('x') {
            if let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>()) {
                eprintln!("capping the mode at {w}x{h} (WFD_MAX_RES)");
                cfg.max_resolution = (w, h);
                cfg.source_size = (w, h);
            }
        }
    }
    // The example has no interface to report to, so it hands the session a
    // status of its own and throws it away.
    let status = nd_core::sink::SinkStatus::new();
    let result = cast_to_sink(stream, cfg, &status).await;

    firewall::release(lease).await;
    let _ = device.disconnect(&active).await;
    drop(capture);

    result?;
    eprintln!("session ended.");
    Ok(())
}
