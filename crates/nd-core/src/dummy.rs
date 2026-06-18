//! Provider/sink falsos para desenvolvimento e teste de UI.
//!
//! Equivalente ao `NETWORK_DISPLAYS_DUMMY=1` do projeto C: injeta receptores
//! fictícios para exercitar a lista da GUI sem hardware na rede.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};

use crate::capture::CaptureSource;
use crate::provider::{DiscoveryEvent, Provider};
use crate::sink::{Sink, SinkInfo, SinkKind, SinkState};
use crate::{NdError, Result};

struct DummySink {
    info: SinkInfo,
}

#[async_trait]
impl Sink for DummySink {
    fn info(&self) -> SinkInfo {
        self.info.clone()
    }
    fn state(&self) -> SinkState {
        SinkState::Disconnected
    }
    async fn start_stream(&self, _source: CaptureSource) -> Result<()> {
        Err(NdError::Unsupported("sink dummy não transmite".into()))
    }
    async fn stop_stream(&self) -> Result<()> {
        Ok(())
    }
}

/// Provider que anuncia alguns receptores fictícios uma vez.
pub struct DummyProvider;

#[async_trait]
impl Provider for DummyProvider {
    fn id(&self) -> &'static str {
        "dummy"
    }

    async fn discover(&self) -> Result<BoxStream<'static, DiscoveryEvent>> {
        let make = |id: &str, name: &str, kind: SinkKind, addr: &str| {
            let sink = DummySink {
                info: SinkInfo {
                    id: id.to_string(),
                    display_name: name.to_string(),
                    kind,
                    address: Some(addr.to_string()),
                },
            };
            DiscoveryEvent::Added(Arc::new(sink) as Arc<dyn Sink>)
        };

        let events = vec![
            make("dummy-cc", "Chromecast de teste", SinkKind::Chromecast, "192.168.0.50"),
            make("dummy-wfd", "Miracast de teste", SinkKind::WfdP2p, "AA:BB:CC:DD:EE:FF"),
        ];

        Ok(futures::stream::iter(events).boxed())
    }
}
