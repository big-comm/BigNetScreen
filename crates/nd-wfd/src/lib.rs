//! Suporte a Wi-Fi Display (Miracast).
//!
//! A parte mais difícil do projeto — onde o código C de referência (`./bkp`)
//! é insubstituível. Pipeline do protocolo (Fase 3):
//! 1. **Descoberta/conexão P2P** via NetworkManager ([`nd_net`]) — Wi-Fi
//!    Direct group owner/cliente.
//! 2. **Servidor RTSP** (porta 7236, `gstreamer-rtsp-server`) com a negociação
//!    WFD M1–M7 (capabilities, resolução, codec) — portar de `wfd-client.c` e
//!    `wfd-media-factory.c`, **reabilitando a seleção de resolução/60 Hz** que
//!    o working tree do C deixou em `#if 0`.
//! 3. **firewalld** ([`nd_net`]) para isolar a porta na interface P2P.
//!
//! Indisponível sob Flatpak (depende de NetworkManager/firewalld no system
//! bus); o app deve degradar para "apenas Chromecast" nesse ambiente.

use async_trait::async_trait;
use nd_core::provider::{DiscoveryEvent, Provider};
use nd_core::{NdError, Result};

/// Provider de descoberta WFD-P2P. **TODO Fase 3.**
pub struct WfdP2pProvider;

#[async_trait]
impl Provider for WfdP2pProvider {
    fn id(&self) -> &'static str {
        "wfd-p2p"
    }
    async fn start_discovery(&self) -> Result<()> {
        Err(NdError::Unsupported("WfdP2pProvider: a implementar (Fase 3)".into()))
    }
    async fn stop_discovery(&self) -> Result<()> {
        Ok(())
    }
    fn subscribe(&self) -> Box<dyn futures::Stream<Item = DiscoveryEvent> + Send + Unpin> {
        Box::new(futures::stream::empty())
    }
}
