//! Provider aggregator.
//!
//! Merges the discovery streams of several providers (Chromecast, WFD-P2P,
//! WFD-MICE) into the single stream the GUI consumes.
//!
//! A provider that fails to start (WFD under Flatpak, with no NetworkManager)
//! becomes a [`DiscoveryEvent::ProviderUnavailable`] carrying readable text,
//! so the UI can say why Miracast is missing while the other providers go on.

use std::sync::Arc;

use futures::stream::{BoxStream, StreamExt, select_all};

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
        let streams = self.providers.iter().cloned().map(|provider| {
            futures::stream::once(async move {
                let id = provider.id();
                let timeout = std::time::Duration::from_secs(15);
                let started = match tokio::time::timeout(timeout, provider.discover()).await {
                    Ok(started) => started.map_err(|err| err.to_string()),
                    Err(_) => Err("discovery initialization timed out".to_string()),
                };
                match started {
                    Ok(stream) => {
                        tracing::info!(provider = id, "discovery started");
                        // The "ready" event comes before the provider's findings.
                        let ready = futures::stream::once(async move {
                            DiscoveryEvent::ProviderReady { provider: id }
                        });
                        ready.chain(stream).boxed()
                    }
                    Err(reason) => {
                        tracing::warn!(provider = id, %reason, "provider unavailable");
                        futures::stream::once(async move {
                            DiscoveryEvent::ProviderUnavailable {
                                provider: id,
                                reason,
                            }
                        })
                        .boxed()
                    }
                }
            })
            .flatten()
            .boxed()
        });
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

    #[tokio::test]
    async fn failure_becomes_a_visible_event() {
        // A provider that cannot start must say so, or the UI waits on
        // "Searching…" with no explanation.
        let meta = MetaProvider::new(vec![Arc::new(FailingProvider)]);
        let rt = meta.discover().await.collect::<Vec<_>>().await;
        assert_eq!(rt.len(), 1);
        match &rt[0] {
            DiscoveryEvent::ProviderUnavailable { provider, reason } => {
                assert_eq!(*provider, "failing");
                assert!(reason.contains("NetworkManager"), "{reason}");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn healthy_provider_announces_ready_then_sinks() {
        let meta = MetaProvider::new(vec![Arc::new(OneSinkProvider)]);
        let events = meta.discover().await.collect::<Vec<_>>().await;
        assert!(matches!(
            events[0],
            DiscoveryEvent::ProviderReady { provider: "ok" }
        ));
        assert!(matches!(events[1], DiscoveryEvent::Added(_)));
    }

    #[tokio::test]
    async fn failing_provider_spares_others() {
        let meta = MetaProvider::new(vec![Arc::new(FailingProvider), Arc::new(OneSinkProvider)]);
        let events = meta.discover().await.collect::<Vec<_>>().await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, DiscoveryEvent::ProviderUnavailable { .. }))
        );
        assert!(events.iter().any(|e| matches!(e, DiscoveryEvent::Added(_))));
    }

    struct StalledProvider;

    #[async_trait]
    impl Provider for StalledProvider {
        fn id(&self) -> &'static str {
            "stalled"
        }
        async fn discover(&self) -> Result<BoxStream<'static, DiscoveryEvent>> {
            futures::future::pending().await
        }
    }

    #[tokio::test]
    async fn stalled_provider_delays_nobody() {
        let meta = MetaProvider::new(vec![Arc::new(StalledProvider), Arc::new(OneSinkProvider)]);
        let mut events = meta.discover().await;
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            assert!(matches!(
                events.next().await,
                Some(DiscoveryEvent::ProviderReady { provider: "ok" })
            ));
            assert!(matches!(
                events.next().await,
                Some(DiscoveryEvent::Added(_))
            ));
        })
        .await
        .expect("healthy discovery must not wait for another provider");
    }
}
