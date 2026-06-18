//! Testa a CONEXÃO Wi-Fi Direct (Fase 3b) com um sink Miracast.
//!
//!   cargo run -p nd-net --example p2p_connect
//!
//! Coloque a TV/projetor em "Espelhamento de Tela / Screen Mirroring" ANTES.
//! O teste: descobre o peer WFD, forma o grupo P2P e acompanha o estado da
//! conexão (2 = ACTIVATED = grupo formado).

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
    eprintln!("procurando sink Miracast (TV em Espelhamento de Tela)…");

    let mut peer = None;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if let Some(p) = device.peers().await?.into_iter().find(|p| p.is_wfd) {
            eprintln!("peer WFD: {} [{}] {}", p.name, p.hw_address, p.path);
            peer = Some(p.path);
            break;
        }
        eprintln!("  …ainda nenhum peer WFD");
    }

    let peer = match peer {
        Some(p) => p,
        None => {
            eprintln!("nenhum sink Miracast encontrado — a TV está em Espelhamento de Tela?");
            return Ok(());
        }
    };

    eprintln!("formando grupo Wi-Fi Direct com {peer}…");
    let active = device.connect(&peer).await?;
    eprintln!("conexão ativa: {active}");

    for _ in 0..15 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let state = device.active_state(&active).await.unwrap_or(ActiveState::Unknown);
        eprintln!("  estado: {state:?}");
        if state == ActiveState::Activated {
            let ips = device.addresses(&active).await.unwrap_or_default();
            eprintln!("✅ grupo P2P formado! nosso IP no link: {ips:?}");
            eprintln!("   (servidor RTSP/WFD escutaria na 7236 desse IP — Fase 3c)");
            eprintln!("   mantendo o grupo vivo por 15s (veja o projetor seguir em 'conectando')…");
            tokio::time::sleep(Duration::from_secs(15)).await;
            break;
        }
        if state == ActiveState::Deactivated {
            eprintln!("❌ conexão falhou/desativada");
            break;
        }
    }

    Ok(())
}
