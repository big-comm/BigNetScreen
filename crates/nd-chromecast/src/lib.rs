//! Suporte a Google Chromecast.
//!
//! Pipeline do protocolo (Fase 2):
//! 1. **Descoberta** `_googlecast._tcp` via mDNS (`mdns-sd`).
//! 2. **Canal Cast** na porta 8009 sobre TLS (`tokio-rustls`) com mensagens
//!    protobuf (`prost`). Diferente do C, a validação de certificado **não**
//!    será um `return TRUE` cego — aceitaremos só `UNKNOWN_CA`/`BAD_IDENTITY`.
//! 3. **Servidor HTTP** (`hyper`) servindo o stream MKV num path com token
//!    UUID aleatório + allowlist de IP do receptor (mantém as boas práticas
//!    de segurança já presentes no C).

use async_trait::async_trait;
use nd_core::provider::{DiscoveryEvent, Provider};
use nd_core::{NdError, Result};

/// Provider de descoberta Chromecast. **TODO Fase 2.**
pub struct ChromecastProvider;

#[async_trait]
impl Provider for ChromecastProvider {
    fn id(&self) -> &'static str {
        "chromecast"
    }
    async fn start_discovery(&self) -> Result<()> {
        Err(NdError::Unsupported("ChromecastProvider: a implementar (Fase 2)".into()))
    }
    async fn stop_discovery(&self) -> Result<()> {
        Ok(())
    }
    fn subscribe(&self) -> Box<dyn futures::Stream<Item = DiscoveryEvent> + Send + Unpin> {
        Box::new(futures::stream::empty())
    }
}
