//! Descoberta de receptores via mDNS.
//!
//! [`MdnsProvider`] varre um tipo de serviço mDNS e expõe cada instância como
//! um [`Sink`]. Dois construtores prontos:
//! - [`MdnsProvider::chromecast`] → `_googlecast._tcp.local.`
//! - [`MdnsProvider::airplay`] → `_airplay._tcp.local.` (apenas descoberta;
//!   transmissão AirPlay está fora de escopo)
//!
//! A transmissão Chromecast em si (canal Cast TLS+protobuf + servidor HTTP) é a
//! Fase 2; por ora [`MdnsSink::start_stream`] é stub.

pub mod cast;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

use nd_core::capture::CaptureSource;
use nd_core::provider::{DiscoveryEvent, Provider};
use nd_core::sink::{Sink, SinkInfo, SinkKind, SinkState};
use nd_core::{NdError, Result};

fn net_err<E: std::fmt::Display>(e: E) -> NdError {
    NdError::Network(e.to_string())
}

/// Provider de descoberta baseado em mDNS, parametrizado pelo tipo de serviço.
pub struct MdnsProvider {
    daemon: ServiceDaemon,
    id: &'static str,
    service_type: &'static str,
    kind: SinkKind,
}

impl MdnsProvider {
    fn new(id: &'static str, service_type: &'static str, kind: SinkKind) -> Result<Self> {
        let daemon = ServiceDaemon::new().map_err(net_err)?;
        Ok(Self {
            daemon,
            id,
            service_type,
            kind,
        })
    }

    /// Descobre Chromecasts (`_googlecast._tcp.local.`).
    pub fn chromecast() -> Result<Self> {
        Self::new("chromecast", "_googlecast._tcp.local.", SinkKind::Chromecast)
    }

    /// Descobre receptores AirPlay (`_airplay._tcp.local.`).
    pub fn airplay() -> Result<Self> {
        Self::new("airplay", "_airplay._tcp.local.", SinkKind::AirPlay)
    }
}

#[async_trait]
impl Provider for MdnsProvider {
    fn id(&self) -> &'static str {
        self.id
    }

    async fn discover(&self) -> Result<BoxStream<'static, DiscoveryEvent>> {
        // `browse` devolve um canal flume síncrono; uma thread dedicada faz a
        // ponte para um stream async. O `ServiceDaemon` (em `self`) precisa
        // continuar vivo: quando dropado, o canal fecha e a thread encerra.
        let events = self.daemon.browse(self.service_type).map_err(net_err)?;
        let kind = self.kind;
        let (tx, rx) = futures::channel::mpsc::unbounded::<DiscoveryEvent>();

        std::thread::spawn(move || {
            while let Ok(event) = events.recv() {
                let msg = match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let sink = MdnsSink::from_service(&info, kind);
                        tracing::info!(name = %sink.info.display_name, ?kind, "receptor encontrado");
                        Some(DiscoveryEvent::Added(Arc::new(sink) as Arc<dyn Sink>))
                    }
                    ServiceEvent::ServiceRemoved(_ty, fullname) => {
                        tracing::info!(%fullname, "receptor removido");
                        Some(DiscoveryEvent::Removed(fullname))
                    }
                    _ => None,
                };
                if let Some(msg) = msg {
                    if tx.unbounded_send(msg).is_err() {
                        break; // stream consumidor foi dropado
                    }
                }
            }
        });

        Ok(rx.boxed())
    }
}

/// Receptor descoberto via mDNS. A transmissão entra nas Fases 2/3.
pub struct MdnsSink {
    info: SinkInfo,
    state: Mutex<SinkState>,
}

impl MdnsSink {
    fn from_service(service: &ServiceInfo, kind: SinkKind) -> Self {
        let fullname = service.get_fullname().to_string();
        // O nome amigável vem do TXT `fn` (Chromecast); senão, a parte da
        // instância do fullname (caso do AirPlay: "Samsung Projector LSP3").
        let display_name = service
            .get_property_val_str("fn")
            .map(|s| s.to_string())
            .unwrap_or_else(|| fullname.split('.').next().unwrap_or(&fullname).to_string());
        let address = service
            .get_addresses_v4()
            .into_iter()
            .next()
            .map(|ip| ip.to_string());

        Self {
            info: SinkInfo {
                id: fullname,
                display_name,
                kind,
                address,
            },
            state: Mutex::new(SinkState::Disconnected),
        }
    }
}

#[async_trait]
impl Sink for MdnsSink {
    fn info(&self) -> SinkInfo {
        self.info.clone()
    }
    fn state(&self) -> SinkState {
        *self.state.lock().unwrap()
    }
    async fn start_stream(&self, _source: CaptureSource) -> Result<()> {
        match self.info.kind {
            SinkKind::Chromecast => Err(NdError::Unsupported(
                "cast Chromecast a implementar (Fase 2)".into(),
            )),
            _ => Err(NdError::Unsupported(
                "transmissão AirPlay não é suportada".into(),
            )),
        }
    }
    async fn stop_stream(&self) -> Result<()> {
        Ok(())
    }
}
