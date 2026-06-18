//! Teste da negociação WFD (Fase 3c, parte 1): forma o grupo Wi-Fi Direct,
//! sobe o servidor RTSP na 7236 e negocia M1–M3 com o sink, imprimindo as
//! capacidades anunciadas (resolução/codecs/porta RTP).
//!
//!   cargo run -p nd-wfd --example wfd_negotiate
//!
//! Requer a TV/projetor em "Espelhamento de Tela".

use std::time::Duration;

use nd_net::p2p::{ActiveState, P2pDevice};
use nd_wfd::rtsp::negotiate_caps;
use tokio::net::TcpListener;

const RTSP_PORT: u16 = 7236;

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
    eprintln!("procurando sink Miracast (TV em Espelhamento de Tela)…");

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
    let active = device.connect(&peer).await?;

    let mut our_ip = None;
    for _ in 0..15 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if device.active_state(&active).await? == ActiveState::Activated {
            our_ip = device.addresses(&active).await?.into_iter().next();
            break;
        }
    }
    let ip = our_ip.ok_or("grupo não obteve IP local")?;
    eprintln!("grupo formado; nosso IP: {ip}");

    let listener = TcpListener::bind((ip.as_str(), RTSP_PORT)).await?;
    eprintln!("servidor RTSP/WFD em {ip}:{RTSP_PORT} — aguardando o sink conectar (até 40s)…");

    let (stream, addr) = tokio::time::timeout(Duration::from_secs(40), listener.accept()).await??;
    eprintln!("sink conectou de {addr}! negociando WFD M1–M3…");

    let caps = negotiate_caps(stream).await?;
    eprintln!("✅ capacidades do sink:");
    eprintln!("   wfd_video_formats: {:?}", caps.video_formats);
    eprintln!("   wfd_audio_codecs:  {:?}", caps.audio_codecs);
    eprintln!("   rtp_port:          {}", caps.rtp_port);
    eprintln!("(próximo: M4–M7 + pipeline MPEG-TS/RTP → imagem no projetor)");

    Ok(())
}
