//! Abstração de **provider**: descoberta de receptores na rede.
//!
//! Cada protocolo tem seu provider (Chromecast via mDNS, WFD-P2P via
//! NetworkManager, WFD-MICE via mDNS). O agregador [`crate::meta::MetaProvider`]
//! funde todos num único stream observável.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::sink::Sink;
use crate::Result;

/// Evento de descoberta emitido por um provider.
#[derive(Clone)]
pub enum DiscoveryEvent {
    /// Um novo receptor apareceu.
    Added(Arc<dyn Sink>),
    /// Um receptor sumiu (identificado pelo `SinkInfo::id`).
    Removed(String),
}

/// Fonte de descoberta de receptores. Implementações devem ser thread-safe.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Identificador curto para logs (`"chromecast"`, `"wfd-p2p"`, …).
    fn id(&self) -> &'static str;

    /// Inicia a descoberta e devolve um stream contínuo de eventos.
    ///
    /// O stream vive enquanto for consumido; ao ser dropado, a descoberta
    /// subjacente é encerrada (ex.: o `ServiceDaemon` do mDNS é liberado).
    async fn discover(&self) -> Result<BoxStream<'static, DiscoveryEvent>>;
}
