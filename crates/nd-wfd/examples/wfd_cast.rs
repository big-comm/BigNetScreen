//! Cast Miracast completo (Fase 3c): forma o grupo Wi-Fi Direct, negocia WFD
//! (M1–M7) e transmite um padrão de barras 1080p para o sink.
//!
//!   cargo run -p nd-wfd --example wfd_cast
//!
//! Requer a TV/projetor em "Espelhamento de Tela". Se der certo, o padrão de
//! barras aparece no projetor.

use std::time::Duration;

use nd_net::p2p::{ActiveState, P2pDevice};
use nd_wfd::rtsp::cast_to_sink;
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
    eprintln!("procurando sink Miracast…");

    let mut peer = None;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let peers = device.peers().await?;
        // Mira no "Projector" se houver (há também uma TV WFD na rede); senão
        // o primeiro sink WFD.
        let chosen = peers
            .iter()
            .find(|p| p.is_wfd && p.name.contains("Projector"))
            .or_else(|| peers.iter().find(|p| p.is_wfd));
        if let Some(p) = chosen {
            eprintln!("peer WFD: {} [{}]", p.name, p.hw_address);
            peer = Some(p.path.clone());
            break;
        }
    }
    let peer = peer.ok_or("nenhum sink Miracast encontrado")?;

    // O driver Realtek às vezes derruba o grupo na hora; tentamos algumas vezes.
    let mut our_ip = None;
    'retry: for attempt in 1..=4 {
        eprintln!("formando grupo Wi-Fi Direct (tentativa {attempt}/4)…");
        let active = device.connect(&peer).await?;

        for _ in 0..15 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            match device.active_state(&active).await? {
                ActiveState::Activated => {
                    for _ in 0..12 {
                        if let Some(ip) = device.addresses(&active).await?.into_iter().next() {
                            our_ip = Some(ip);
                            break 'retry;
                        }
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
                ActiveState::Deactivated => {
                    eprintln!("  grupo caiu (instabilidade Realtek) — nova tentativa…");
                    break;
                }
                _ => {}
            }
        }
    }
    let our_ip: std::net::IpAddr = our_ip
        .ok_or("não consegui formar um grupo P2P estável (driver Realtek)")?
        .parse()?;
    eprintln!("grupo formado; nosso IP: {our_ip}");

    let listener = TcpListener::bind((our_ip, RTSP_PORT)).await?;
    eprintln!("RTSP/WFD em {our_ip}:{RTSP_PORT} — aguardando o sink (até 40s)…");
    let (stream, addr) = tokio::time::timeout(Duration::from_secs(40), listener.accept()).await??;
    eprintln!("sink conectou de {addr}! negociando + transmitindo…");

    cast_to_sink(stream, our_ip, addr.ip()).await?;
    eprintln!("sessão encerrada.");
    Ok(())
}
