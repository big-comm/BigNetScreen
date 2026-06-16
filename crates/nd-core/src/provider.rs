//! Abstração de **provider**: descoberta de receptores na rede.
//!
//! Cada protocolo tem seu provider (Chromecast via mDNS, WFD-P2P via
//! NetworkManager, WFD-MICE via mDNS). Um agregador [`MetaProvider`] (a
//! implementar no crate da GUI / núcleo) funde todos numa única lista
//! observável, exatamente como o `NdMetaProvider` do C — porém sem o truque
//! confuso de emitir `sink-added` com `NULL`.

use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;

use crate::sink::Sink;
use crate::Result;

/// Evento de descoberta emitido por um provider.
#[derive(Clone)]
pub enum DiscoveryEvent {
    /// Um novo receptor apareceu.
    Added(Arc<dyn Sink>),
    /// Um receptor sumiu (por id).
    Removed(String),
}

/// Fonte de descoberta de receptores. Implementações devem ser thread-safe.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Identificador curto para logs (`"chromecast"`, `"wfd-p2p"`, …).
    fn id(&self) -> &'static str;

    /// Inicia a varredura. Os receptores chegam pelo stream de [`subscribe`].
    ///
    /// [`subscribe`]: Provider::subscribe
    async fn start_discovery(&self) -> Result<()>;

    /// Interrompe a varredura.
    async fn stop_discovery(&self) -> Result<()>;

    /// Stream de eventos de descoberta. Múltiplos assinantes são permitidos.
    fn subscribe(&self) -> Box<dyn Stream<Item = DiscoveryEvent> + Send + Unpin>;
}
