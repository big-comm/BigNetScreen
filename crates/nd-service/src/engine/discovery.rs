//! Finding receivers: which providers run, and how their events reach the
//! receiver list.

use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::mpsc;

use nd_chromecast::MdnsProvider;
use nd_core::capture::SourceType;
use nd_core::meta::MetaProvider;
use nd_core::provider::{DiscoveryEvent, Provider};
use nd_core::settings::Protocol;
use nd_wfd::WfdP2pProvider;

use super::{EMPTY_HINT_AFTER, Engine, Event};
use crate::wire::{Issue, Status};

impl Engine {
    pub(super) fn discovery_event(&mut self, event: DiscoveryEvent) {
        match event {
            DiscoveryEvent::Added(sink) | DiscoveryEvent::Updated(sink) => {
                let id = sink.info().id;
                if !self.registry.contains_key(&id)
                    && self.registry.len() >= nd_core::provider::MAX_RECEIVERS
                {
                    return;
                }
                if !self.order.contains(&id) {
                    self.order.push(id.clone());
                }
                // An existing receiver is updated, never replaced: swapping the
                // `Arc` mid-session would lose the connection running on it.
                if self.active_cast.as_ref() != Some(&id) {
                    self.registry.insert(id, sink);
                }
                self.searching = false;
                self.refresh_status();
            }
            DiscoveryEvent::Removed(id) => {
                // Never remove the receiver that is streaming: a momentary mDNS
                // dropout would take the running session's row away.
                if self.active_cast.as_deref() == Some(id.as_str()) {
                    return;
                }
                self.registry.remove(&id);
                self.order.retain(|listed| listed != &id);
                self.refresh_status();
            }
            DiscoveryEvent::ProviderUnavailable { provider, reason } => {
                let issue = Issue {
                    provider: provider.to_string(),
                    reason,
                };
                self.issues.retain(|old| old.provider != issue.provider);
                self.issues.push(issue);
            }
            DiscoveryEvent::ProviderReady { provider } => {
                self.issues.retain(|issue| issue.provider != provider);
            }
        }
    }

    pub(super) fn abort_discovery(&mut self) {
        if let Some(task) = self.discovery.take() {
            task.abort();
        }
        self.generation += 1;
    }

    pub(super) fn apply_discovery_preference(&mut self) {
        if self.settings.auto_discovery {
            self.rescan();
        } else {
            self.abort_discovery();
            self.searching = false;
            self.refresh_status();
        }
    }

    pub(super) fn rescan(&mut self) {
        if matches!(self.status.kind.as_str(), "error" | "ndi-runtime-missing") {
            self.status = Status::of("idle");
        }
        self.abort_discovery();
        self.issues.clear();
        self.searching = true;
        self.registry
            .retain(|id, _| self.active_cast.as_ref() == Some(id));
        self.order
            .retain(|id| self.active_cast.as_ref() == Some(id));
        self.refresh_status();
        self.start_discovery();
    }

    pub(super) fn start_discovery(&mut self) {
        let (handle, registration) = futures::future::AbortHandle::new_pair();
        let generation = self.generation;
        let protocol = self.settings.protocol;
        let events = self.events.clone();
        tokio::spawn(async move {
            let _ = futures::future::Abortable::new(
                run_discovery(events, generation, protocol),
                registration,
            )
            .await;
        });
        let events = self.events.clone();
        tokio::spawn(async move {
            tokio::time::sleep(EMPTY_HINT_AFTER).await;
            let _ = events.send(Event::SearchTimedOut(generation)).await;
        });
        self.discovery = Some(handle);
    }

    pub(super) fn probe_virtual_support(&mut self) {
        let events = self.events.clone();
        tokio::spawn(async move {
            // No sink yet: this only asks whether a virtual screen is possible.
            let backend = nd_capture::select_backend_for(SourceType::Virtual, None).await;
            let supported = backend.supported_sources().await;
            let _ = events
                .send(Event::VirtualSupported(
                    supported.contains(&SourceType::Virtual),
                ))
                .await;
        });
    }
}

/// Runs discovery across the chosen providers and reports to the engine.
async fn run_discovery(events: mpsc::Sender<Event>, generation: u64, protocol: Protocol) {
    let mut providers: Vec<Arc<dyn Provider>> = Vec::new();

    // A preference for one protocol is honoured by not starting the other.
    // Filtering the list afterwards would leave the radio scanning for Wi-Fi
    // Direct peers during a Cast session, which is the contention the
    // preference exists to avoid.
    if matches!(protocol, Protocol::Auto | Protocol::Cast) {
        // A single mDNS daemon serves both Chromecast and AirPlay.
        match MdnsProvider::all_media_receivers() {
            Ok(provider) => providers.push(Arc::new(provider)),
            Err(err) => {
                let _ = events
                    .send(Event::Discovery(
                        DiscoveryEvent::ProviderUnavailable {
                            provider: "mdns",
                            reason: err.to_string(),
                        },
                        generation,
                    ))
                    .await;
            }
        }
    }
    if matches!(protocol, Protocol::Auto | Protocol::Cast) {
        // SSDP, not mDNS: the two discovery protocols share nothing, so a
        // DLNA television is invisible to the browse above and this one is
        // invisible to a Chromecast. Both have to run.
        providers.push(Arc::new(nd_dlna::DlnaProvider));
    }
    if matches!(protocol, Protocol::Auto | Protocol::Miracast) {
        providers.push(Arc::new(WfdP2pProvider));
    }
    if std::env::var_os("NETWORK_DISPLAYS_DUMMY").is_some() {
        providers.push(Arc::new(nd_core::dummy::DummyProvider));
    }

    let meta = MetaProvider::new(providers);
    let mut stream = meta.discover().await;
    while let Some(event) = stream.next().await {
        if events
            .send(Event::Discovery(event, generation))
            .await
            .is_err()
        {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tests::{engine, sink};

    #[test]
    fn discovery_bounds_new_receivers_and_replaces_provider_diagnostics() {
        let mut engine = engine();
        for index in 0..nd_core::provider::MAX_RECEIVERS + 10 {
            engine.discovery_event(DiscoveryEvent::Added(sink(&index.to_string())));
            engine.discovery_event(DiscoveryEvent::ProviderUnavailable {
                provider: "test",
                reason: index.to_string(),
            });
        }
        assert_eq!(engine.registry.len(), nd_core::provider::MAX_RECEIVERS);
        assert_eq!(engine.order.len(), nd_core::provider::MAX_RECEIVERS);
        assert_eq!(engine.issues.len(), 1);
        engine.discovery_event(DiscoveryEvent::Removed("0".into()));
        engine.discovery_event(DiscoveryEvent::Added(sink("new")));
        assert!(engine.registry.contains_key("new"));
    }

    #[test]
    fn the_streaming_receiver_survives_a_discovery_dropout() {
        let mut engine = engine();
        engine.order.push("a".into());
        engine.registry.insert("a".into(), sink("a"));
        engine.active_cast = Some("a".into());
        engine.active_sink = Some(sink("a"));
        engine.discovery_event(DiscoveryEvent::Removed("a".into()));
        assert!(engine.order.contains(&"a".to_string()));
    }
}
