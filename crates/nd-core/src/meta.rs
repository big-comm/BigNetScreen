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
        let streams = self.providers.iter().cloned().map(|provider| {
            futures::stream::once(async move {
                let id = provider.id();
                match tokio::time::timeout(std::time::Duration::from_secs(15), provider.discover())
                    .await
                {
                    Ok(Ok(stream)) => {
                        tracing::info!(provider = id, "discovery started");
                        // The "ready" event comes before the provider's findings.
                        let ready = futures::stream::once(async move {
                            DiscoveryEvent::ProviderReady { provider: id }
                        });
                        ready.chain(stream).boxed()
                    }
                    result => {
                        let reason = match result {
                            Ok(Err(err)) => err.to_string(),
                            Err(_) => "discovery initialization timed out".to_string(),
                            Ok(Ok(_)) => unreachable!(),
                        };
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
        // Regression: the failure used to be just a `warn` and the UI sat on
        // "Searching…" forever, with no explanation.
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
    async fn one_failing_provider_does_not_kill_the_others() {
        let meta = MetaProvider::new(vec![Arc::new(FailingProvider), Arc::new(OneSinkProvider)]);
        let events = meta.discover().await.collect::<Vec<_>>().await;
        assert!(events
            .iter()
            .any(|e| matches!(e, DiscoveryEvent::ProviderUnavailable { .. })));
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
    async fn a_stalled_provider_does_not_delay_other_receivers() {
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
