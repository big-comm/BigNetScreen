//! Suporte a Wi-Fi Display (Miracast).
//!
//! Fase 3a (esta): **descoberta** de sinks Miracast via Wi-Fi Direct
//! ([`nd_net::p2p`]). Cada peer P2P que anuncia Wi-Fi Display (`WfdIEs`) vira um
//! [`Sink`] na lista.
//!
//! Fases 3b/3c (próximas, código C de referência em `bkp/src/wfd/`):
//! - conectar P2P (formar grupo Wi-Fi Direct) → IP na interface p2p;
//! - servidor RTSP (porta 7236) + negociação WFD (M1–M7) + pipeline GStreamer
//!   (tuning em [`nd_core::pipeline`]), reabilitando a seleção de resolução/60 Hz.
//!
//! Indisponível sob Flatpak (depende de NetworkManager no system bus); o
//! [`Provider::discover`] devolve `Err` e o `MetaProvider` ignora graciosamente.

pub mod rtsp;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};

use nd_core::capture::CaptureSource;
use nd_core::provider::{DiscoveryEvent, Provider};
use nd_core::sink::{Sink, SinkInfo, SinkKind, SinkState};
use nd_core::{NdError, Result};
use nd_net::p2p::{P2pDevice, P2pPeer};

/// Intervalo de varredura da lista de peers P2P.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Provider de descoberta WFD-P2P (Miracast) via Wi-Fi Direct.
pub struct WfdP2pProvider;

#[async_trait]
impl Provider for WfdP2pProvider {
    fn id(&self) -> &'static str {
        "wfd-p2p"
    }

    async fn discover(&self) -> Result<BoxStream<'static, DiscoveryEvent>> {
        // Falha aqui (sem hardware P2P, sem NM, Flatpak) => MetaProvider ignora.
        let device = P2pDevice::open().await?;
        device.start_find().await?;

        let (tx, rx) = futures::channel::mpsc::unbounded::<DiscoveryEvent>();

        // Varre periodicamente e emite diffs (Added/Removed) só de sinks WFD.
        tokio::spawn(async move {
            let mut known: HashSet<String> = HashSet::new();
            'poll: loop {
                tokio::time::sleep(POLL_INTERVAL).await;

                let peers = match device.peers().await {
                    Ok(peers) => peers,
                    Err(err) => {
                        tracing::warn!(%err, "falha ao ler peers P2P; encerrando varredura");
                        break;
                    }
                };

                let current: HashSet<String> = peers
                    .iter()
                    .filter(|p| p.is_wfd)
                    .map(|p| p.path.clone())
                    .collect();

                // Novos sinks Miracast.
                for peer in peers.iter().filter(|p| p.is_wfd && !known.contains(&p.path)) {
                    tracing::info!(name = %peer.name, mac = %peer.hw_address, "sink Miracast encontrado");
                    let sink = Arc::new(WfdSink::new(peer)) as Arc<dyn Sink>;
                    if tx.unbounded_send(DiscoveryEvent::Added(sink)).is_err() {
                        break 'poll;
                    }
                }

                // Sinks que sumiram.
                for gone in known.difference(&current) {
                    if tx.unbounded_send(DiscoveryEvent::Removed(gone.clone())).is_err() {
                        break 'poll;
                    }
                }

                known = current;
            }

            let _ = device.stop_find().await;
        });

        Ok(rx.boxed())
    }
}

/// Sink Miracast descoberto. A conexão/streaming entram nas Fases 3b/3c.
pub struct WfdSink {
    info: SinkInfo,
    state: Mutex<SinkState>,
}

impl WfdSink {
    fn new(peer: &P2pPeer) -> Self {
        let display_name = if peer.name.is_empty() {
            peer.hw_address.clone()
        } else {
            peer.name.clone()
        };
        Self {
            info: SinkInfo {
                id: peer.path.clone(),
                display_name,
                kind: SinkKind::WfdP2p,
                address: Some(peer.hw_address.clone()),
            },
            state: Mutex::new(SinkState::Disconnected),
        }
    }
}

#[async_trait]
impl Sink for WfdSink {
    fn info(&self) -> SinkInfo {
        self.info.clone()
    }
    fn state(&self) -> SinkState {
        *self.state.lock().unwrap()
    }
    async fn start_stream(&self, _source: CaptureSource) -> Result<()> {
        Err(NdError::Unsupported(
            "Miracast: conexão P2P e streaming a implementar (Fase 3b/3c)".into(),
        ))
    }
    async fn stop_stream(&self) -> Result<()> {
        Ok(())
    }
}
