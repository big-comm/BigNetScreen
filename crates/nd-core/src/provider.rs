//! The **provider** abstraction: discovering receivers on the network.
//!
//! Every protocol has its own provider (Chromecast over mDNS, WFD-P2P over
//! NetworkManager, WFD-MICE over mDNS). The aggregator
//! [`crate::meta::MetaProvider`] merges them all into a single observable
//! stream.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::sink::Sink;
use crate::Result;

/// A discovery event emitted by a provider.
///
/// The diagnostic variants exist so that **failures are visible**: before them,
/// a provider that failed to start became a `warn` in the log while the user
/// stared at "Searching…" forever, with no way to tell "there is no receiver on
/// this network" from "Miracast never even started".
#[derive(Clone)]
pub enum DiscoveryEvent {
    /// A new receiver showed up.
    Added(Arc<dyn Sink>),
    /// An already known receiver had its data refreshed (name, IP).
    ///
    /// Kept apart from `Added` so the UI updates the existing row instead of
    /// replacing the instance — swapping the `Arc` mid-session would lose the
    /// connection state.
    Updated(Arc<dyn Sink>),
    /// A receiver went away (identified by `SinkInfo::id`).
    Removed(String),
    /// A provider could not start, or stopped working.
    ProviderUnavailable {
        provider: &'static str,
        /// Text ready to be shown to the user, not just logged.
        reason: String,
    },
    /// A provider started (or recovered) and is scanning.
    ProviderReady { provider: &'static str },
}

impl std::fmt::Debug for DiscoveryEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiscoveryEvent::Added(s) => write!(f, "Added({})", s.info().id),
            DiscoveryEvent::Updated(s) => write!(f, "Updated({})", s.info().id),
            DiscoveryEvent::Removed(id) => write!(f, "Removed({id})"),
            DiscoveryEvent::ProviderUnavailable { provider, reason } => {
                write!(f, "ProviderUnavailable({provider}: {reason})")
            }
            DiscoveryEvent::ProviderReady { provider } => write!(f, "ProviderReady({provider})"),
        }
    }
}

/// A source of receiver discovery. Implementations must be thread-safe.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Short identifier for logs (`"chromecast"`, `"wfd-p2p"`, …).
    fn id(&self) -> &'static str;

    /// Protocol name to show the user (`"Miracast"`, `"Chromecast"`).
    fn display_name(&self) -> &'static str {
        self.id()
    }

    /// Starts discovery and returns a continuous stream of events.
    ///
    /// The stream lives for as long as it is consumed; when dropped, the
    /// underlying discovery is torn down (e.g. the mDNS `ServiceDaemon` is
    /// released).
    async fn discover(&self) -> Result<BoxStream<'static, DiscoveryEvent>>;
}
