//! Fake provider/sink for development and UI testing.
//!
//! The equivalent of the C project's `NETWORK_DISPLAYS_DUMMY=1`: it injects
//! made-up receivers so the GUI list can be exercised with no hardware on the
//! network.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};

use crate::capture::CaptureSource;
use crate::provider::{DiscoveryEvent, Provider};
use crate::sink::{Sink, SinkInfo, SinkKind, SinkState, SinkStatus};
use crate::{NdError, Result};

struct DummySink {
    info: SinkInfo,
    status: SinkStatus,
}

#[async_trait]
impl Sink for DummySink {
    fn info(&self) -> SinkInfo {
        self.info.clone()
    }
    fn state(&self) -> SinkState {
        self.status.state()
    }
    fn error_message(&self) -> Option<String> {
        self.status.message()
    }
    async fn start_stream(&self, _source: CaptureSource) -> Result<()> {
        self.status.fail("a test sink does not stream");
        Err(NdError::Unsupported(
            "the dummy sink does not stream".into(),
        ))
    }
    async fn stop_stream(&self) -> Result<()> {
        self.status.reset();
        Ok(())
    }
}

/// A provider that announces a few made-up receivers once.
pub struct DummyProvider;

#[async_trait]
impl Provider for DummyProvider {
    fn id(&self) -> &'static str {
        "dummy"
    }

    fn display_name(&self) -> &'static str {
        "Test"
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
                status: SinkStatus::new(),
            };
            DiscoveryEvent::Added(Arc::new(sink) as Arc<dyn Sink>)
        };

        let events = vec![
            make(
                "dummy-cc",
                "Test Chromecast",
                SinkKind::Chromecast,
                "192.168.0.50",
            ),
            make(
                "dummy-wfd",
                "Test Miracast",
                SinkKind::WfdP2p,
                "AA:BB:CC:DD:EE:FF",
            ),
        ];

        Ok(futures::stream::iter(events).boxed())
    }
}
