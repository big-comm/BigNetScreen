//! Provider aggregator.
//!
//! Merges the discovery streams of several providers (Chromecast, WFD-P2P,
//! WFD-MICE) into the single stream the GUI consumes.
//!
//! Providers that fail to start (e.g. WFD under Flatpak, with no
//! NetworkManager) are **not silenced**: the failure becomes a
//! [`DiscoveryEvent::ProviderUnavailable`] carrying readable text, so the UI
//! can explain to the user why Miracast is missing. The app degrades
//! gracefully instead of failing outright — without hiding the reason.

use std::sync::Arc;

use futures::stream::{select_all, BoxStream, StreamExt};

use crate::provider::{DiscoveryEvent, Provider};

/// Combines several [`Provider`]s into a single observable stream.
pub struct MetaProvider {
    providers: Vec<Arc<dyn Provider>>,
}

impl MetaProvider {
    /// Builds the aggregator from a list of providers.
    pub fn new(providers: Vec<Arc<dyn Provider>>) -> Self {
        Self { providers }
    }

    /// How many providers are registered.
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Starts discovery on every provider and returns the merged stream.
    pub async fn discover(&self) -> BoxStream<'static, DiscoveryEvent> {
        let mut streams: Vec<BoxStream<'static, DiscoveryEvent>> =
            Vec::with_capacity(self.providers.len());

        for provider in &self.providers {
            let id = provider.id();
            match provider.discover().await {
                Ok(stream) => {
                    tracing::info!(provider = id, "discovery started");
                    // The "ready" event comes before the provider's findings.
                    let ready = futures::stream::once(async move {
                        DiscoveryEvent::ProviderReady { provider: id }
                    });
                    streams.push(ready.chain(stream).boxed());
                }
                Err(err) => {
                    let reason = err.to_string();
                    tracing::warn!(provider = id, %reason, "provider unavailable");
                    streams.push(
                        futures::stream::once(async move {
                            DiscoveryEvent::ProviderUnavailable {
                                provider: id,
                                reason,
                            }
                        })
                        .boxed(),
                    );
                }
            }
        }

        if streams.is_empty() {
            return futures::stream::empty().boxed();
        }
        select_all(streams).boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::CaptureSource;
    use crate::sink::{Sink, SinkInfo, SinkKind, SinkState};
    use crate::{NdError, Result};
    use async_trait::async_trait;

    struct FailingProvider;

    #[async_trait]
    impl Provider for FailingProvider {
        fn id(&self) -> &'static str {
            "failing"
        }
        async fn discover(&self) -> Result<BoxStream<'static, DiscoveryEvent>> {
            Err(NdError::Unsupported("no NetworkManager".into()))
        }
    }

    struct OneSinkProvider;
    struct TinySink;

    #[async_trait]
    impl Sink for TinySink {
        fn info(&self) -> SinkInfo {
            SinkInfo {
                id: "s1".into(),
                display_name: "S1".into(),
                kind: SinkKind::Dummy,
                address: None,
            }
        }
        fn state(&self) -> SinkState {
            SinkState::Disconnected
        }
        async fn start_stream(&self, _s: CaptureSource) -> Result<()> {
            Ok(())
        }
        async fn stop_stream(&self) -> Result<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl Provider for OneSinkProvider {
        fn id(&self) -> &'static str {
            "ok"
        }
        async fn discover(&self) -> Result<BoxStream<'static, DiscoveryEvent>> {
            let sink: Arc<dyn Sink> = Arc::new(TinySink);
            Ok(futures::stream::once(async move { DiscoveryEvent::Added(sink) }).boxed())
        }
    }

    #[test]
    fn failure_becomes_a_visible_event() {
        // Regression: the failure used to be just a `warn` and the UI sat on
        // "Searching…" forever, with no explanation.
        let rt = futures::executor::block_on(async {
            let meta = MetaProvider::new(vec![Arc::new(FailingProvider)]);
            meta.discover().await.collect::<Vec<_>>().await
        });
        assert_eq!(rt.len(), 1);
        match &rt[0] {
            DiscoveryEvent::ProviderUnavailable { provider, reason } => {
                assert_eq!(*provider, "failing");
                assert!(reason.contains("NetworkManager"), "{reason}");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn healthy_provider_announces_ready_then_sinks() {
        let events = futures::executor::block_on(async {
            let meta = MetaProvider::new(vec![Arc::new(OneSinkProvider)]);
            meta.discover().await.collect::<Vec<_>>().await
        });
        assert!(matches!(
            events[0],
            DiscoveryEvent::ProviderReady { provider: "ok" }
        ));
        assert!(matches!(events[1], DiscoveryEvent::Added(_)));
    }

    #[test]
    fn one_failing_provider_does_not_kill_the_others() {
        let events = futures::executor::block_on(async {
            let meta =
                MetaProvider::new(vec![Arc::new(FailingProvider), Arc::new(OneSinkProvider)]);
            meta.discover().await.collect::<Vec<_>>().await
        });
        assert!(events
            .iter()
            .any(|e| matches!(e, DiscoveryEvent::ProviderUnavailable { .. })));
        assert!(events.iter().any(|e| matches!(e, DiscoveryEvent::Added(_))));
    }
}
