//! Agregador de providers.
//!
//! Funde os streams de descoberta de vários providers (Chromecast, WFD-P2P,
//! WFD-MICE) num único stream que a GUI consome. Providers que falham ao
//! iniciar (ex.: WFD sob Flatpak, sem NetworkManager) são apenas registrados e
//! ignorados — o app degrada graciosamente em vez de falhar por inteiro.

use std::sync::Arc;

use futures::stream::{select_all, BoxStream, StreamExt};

use crate::provider::{DiscoveryEvent, Provider};

/// Combina múltiplos [`Provider`] num só stream observável.
pub struct MetaProvider {
    providers: Vec<Arc<dyn Provider>>,
}

impl MetaProvider {
    /// Cria o agregador a partir de uma lista de providers.
    pub fn new(providers: Vec<Arc<dyn Provider>>) -> Self {
        Self { providers }
    }

    /// Inicia a descoberta em todos os providers e devolve o stream fundido.
    pub async fn discover(&self) -> BoxStream<'static, DiscoveryEvent> {
        let mut streams = Vec::with_capacity(self.providers.len());
        for provider in &self.providers {
            match provider.discover().await {
                Ok(stream) => {
                    tracing::info!(provider = provider.id(), "descoberta iniciada");
                    streams.push(stream);
                }
                Err(err) => {
                    tracing::warn!(provider = provider.id(), %err, "provider indisponível");
                }
            }
        }

        if streams.is_empty() {
            return futures::stream::empty().boxed();
        }
        select_all(streams).boxed()
    }
}
