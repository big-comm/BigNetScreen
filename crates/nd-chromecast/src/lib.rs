//! Receiver discovery over mDNS, and streaming to Chromecast.
//!
//! [`MdnsProvider`] browses **several service types with a single
//! `ServiceDaemon`**. There used to be one daemon per protocol: two sockets on
//! 5353, two copies of the multicast traffic and two bridging threads — and
//! none of them was shut down on drop (mdns-sd's `ServiceDaemon` requires an
//! explicit `shutdown()`).

pub mod cast;
pub mod http;
pub mod mirror;
pub mod mirror_session;
pub mod rtcp;
pub mod rtp;
pub mod session;

use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent};

use nd_core::capture::CaptureSource;
use nd_core::provider::{DiscoveryEvent, Provider};
use nd_core::sink::{sanitize_name, Sink, SinkInfo, SinkKind, SinkState, SinkStatus};
use nd_core::{NdError, Result};

const CHROMECAST_SERVICE: &str = "_googlecast._tcp.local.";
const AIRPLAY_SERVICE: &str = "_airplay._tcp.local.";

fn net_err<E: std::fmt::Display>(e: E) -> NdError {
    NdError::Network(e.to_string())
}

/// An mDNS service type to browse and the protocol it stands for.
#[derive(Clone, Copy, Debug)]
pub struct ServiceKind {
    pub service_type: &'static str,
    pub kind: SinkKind,
}

impl ServiceKind {
    pub const CHROMECAST: Self = Self {
        service_type: CHROMECAST_SERVICE,
        kind: SinkKind::Chromecast,
    };
    pub const AIRPLAY: Self = Self {
        service_type: AIRPLAY_SERVICE,
        kind: SinkKind::AirPlay,
    };
}

/// An mDNS discovery provider able to browse several services at once.
pub struct MdnsProvider {
    daemon: ServiceDaemon,
    id: &'static str,
    display_name: &'static str,
    services: Vec<ServiceKind>,
}

impl MdnsProvider {
    /// Creates a provider that browses the given services with one daemon.
    pub fn new(
        id: &'static str,
        display_name: &'static str,
        services: Vec<ServiceKind>,
    ) -> Result<Self> {
        let daemon = ServiceDaemon::new().map_err(net_err)?;
        Ok(Self {
            daemon,
            id,
            display_name,
            services,
        })
    }

    /// Browses Chromecast **and** AirPlay on a single daemon (the app's case).
    pub fn all_media_receivers() -> Result<Self> {
        Self::new(
            "mdns",
            "Chromecast/AirPlay",
            vec![ServiceKind::CHROMECAST, ServiceKind::AIRPLAY],
        )
    }

    /// Chromecast only (`_googlecast._tcp.local.`).
    pub fn chromecast() -> Result<Self> {
        Self::new("chromecast", "Chromecast", vec![ServiceKind::CHROMECAST])
    }

    /// AirPlay only (`_airplay._tcp.local.`) — discovery alone.
    pub fn airplay() -> Result<Self> {
        Self::new("airplay", "AirPlay", vec![ServiceKind::AIRPLAY])
    }
}

impl Drop for MdnsProvider {
    fn drop(&mut self) {
        // mdns-sd's `ServiceDaemon` keeps its background thread alive until an
        // explicit `shutdown()`: without this, every provider created leaked a
        // thread and a multicast socket for the rest of the process.
        for service in &self.services {
            let _ = self.daemon.stop_browse(service.service_type);
        }
        match self.daemon.shutdown() {
            Ok(_) => tracing::debug!(provider = self.id, "daemon mDNS encerrado"),
            Err(err) => tracing::debug!(provider = self.id, %err, "mDNS daemon already shut down"),
        }
    }
}

#[async_trait]
impl Provider for MdnsProvider {
    fn id(&self) -> &'static str {
        self.id
    }

    fn display_name(&self) -> &'static str {
        self.display_name
    }

    async fn discover(&self) -> Result<BoxStream<'static, DiscoveryEvent>> {
        let (tx, rx) = futures::channel::mpsc::unbounded::<DiscoveryEvent>();

        for service in &self.services {
            // `browse` returns a synchronous flume channel; one thread per
            // service bridges it to the async stream. When the daemon shuts
            // down the channel closes and the thread ends on its own.
            let events = self.daemon.browse(service.service_type).map_err(net_err)?;
            let kind = service.kind;
            let tx = tx.clone();

            std::thread::Builder::new()
                .name(format!("mdns-{}", service.kind.as_str()))
                .spawn(move || {
                    while let Ok(event) = events.recv() {
                        let msg = match event {
                            ServiceEvent::ServiceResolved(info) => {
                                let sink = MdnsSink::from_service(&info, kind);
                                // Each device announces itself several times,
                                // and an announcement with no usable address
                                // must not replace a working one in the list.
                                if sink.address.is_none() {
                                    tracing::debug!(
                                        name = %sink.info.display_name,
                                        "announcement with no usable address; ignored"
                                    );
                                    continue;
                                }
                                tracing::info!(
                                    name = %sink.info.display_name,
                                    address = ?sink.info.address,
                                    ?kind,
                                    "receiver found"
                                );
                                Some(DiscoveryEvent::Added(Arc::new(sink) as Arc<dyn Sink>))
                            }
                            ServiceEvent::ServiceRemoved(_ty, fullname) => {
                                tracing::info!(%fullname, "receiver removed");
                                Some(DiscoveryEvent::Removed(fullname))
                            }
                            _ => None,
                        };
                        if let Some(msg) = msg {
                            if tx.unbounded_send(msg).is_err() {
                                break; // consumidor do stream foi dropado
                            }
                        }
                    }
                })
                .map_err(net_err)?;
        }

        Ok(rx.boxed())
    }
}

/// Picks, among the announced addresses, one that can actually be connected to.
///
/// IPv4 first (most receivers announce nothing else), then routable IPv6 —
/// IPv6-only networks used to make the receiver invisible.
///
/// **Link-local addresses are excluded.** An `fe80::…` is only usable together
/// with the interface index, which does not survive the conversion to
/// `IpAddr`; connecting without it fails immediately with `EINVAL`. The same
/// goes for IPv4's `169.254.0.0/16`. Since the same device usually announces
/// itself more than once, each announcement carrying a different set of
/// addresses, what this looked like in practice was a receiver that worked and
/// then, on the next announcement, started returning "Invalid argument".
fn best_address(addresses: impl Iterator<Item = IpAddr>) -> Option<IpAddr> {
    addresses
        .filter(|ip| !ip.is_loopback() && !is_link_local(ip))
        .min_by_key(|ip| if ip.is_ipv4() { 0 } else { 1 })
}

/// Is the address only valid within a link (and does it need the interface index)?
fn is_link_local(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        // `is_unicast_link_local` is not stable for general use here yet.
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// A receiver discovered over mDNS.
pub struct MdnsSink {
    info: SinkInfo,
    /// The already resolved address, ready for the Cast channel.
    address: Option<IpAddr>,
    port: u16,
    status: SinkStatus,
    /// Handle to the running session, so `stop_stream` can end it.
    session: Mutex<Option<session::SessionHandle>>,
}

impl MdnsSink {
    fn from_service(service: &ResolvedService, kind: SinkKind) -> Self {
        let fullname = service.get_fullname().to_string();

        // The friendly name from the `fn` TXT record (Chromecast); failing
        // that, the instance part of the fullname (the AirPlay case).
        // Sanitised: it comes off the network and ends up in UI labels and,
        // further along, in other processes' arguments.
        let raw_name = service
            .get_property_val_str("fn")
            .map(|s| s.to_string())
            .unwrap_or_else(|| fullname.split('.').next().unwrap_or(&fullname).to_string());
        let display_name = sanitize_name(&raw_name);

        let address = best_address(service.get_addresses().iter().map(|s| s.to_ip_addr()));

        Self {
            info: SinkInfo {
                id: fullname,
                display_name,
                kind,
                address: address.map(|ip| ip.to_string()),
            },
            address,
            port: service.get_port(),
            status: SinkStatus::new(),
            session: Mutex::new(None),
        }
    }

    /// The receiver's resolved IP address.
    pub fn ip(&self) -> Option<IpAddr> {
        self.address
    }

    /// Porta anunciada no mDNS (8009 nos Chromecasts).
    pub fn port(&self) -> u16 {
        self.port
    }
}

#[async_trait]
impl Sink for MdnsSink {
    fn info(&self) -> SinkInfo {
        self.info.clone()
    }

    fn state(&self) -> SinkState {
        self.status.state()
    }

    fn error_message(&self) -> Option<String> {
        self.status.message()
    }

    async fn start_stream(&self, source: CaptureSource) -> Result<()> {
        if self.info.kind != SinkKind::Chromecast {
            let msg = "streaming to AirPlay is not supported; this receiver \
                       shows up in discovery only";
            self.status.fail(msg);
            return Err(NdError::Unsupported(msg.into()));
        }

        let ip = self.address.ok_or_else(|| {
            let msg = "the receiver was announced with no usable IP address";
            self.status.fail(msg);
            NdError::Network(msg.into())
        })?;

        let (handle, cancel) = session::cancellation();
        *self.session.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);

        // Mirroring first: direct RTP, no media player in the middle, with a
        // delay of hundreds of milliseconds rather than seconds.
        //
        // Not every Cast device ships the mirroring app (some speakers and
        // older TVs only have the Default Media Receiver), so the fall back to
        // the HTTP path is automatic — and only happens if the **negotiation**
        // fails, never in the middle of a stream that already started.
        let video = source.video_source();
        let size = source.size_or((1920, 1080));

        let result = match mirror_session::run(ip, video, size, &self.status, cancel.clone()).await
        {
            Err(err) if mirror_session::is_unsupported(&err) => {
                tracing::info!(
                    %err,
                    "this receiver does not accept mirroring; using the HTTP path"
                );
                self.status.reset();
                session::run(ip, source, &self.status, cancel).await
            }
            other => {
                drop(source);
                other
            }
        };

        *self.session.lock().unwrap_or_else(PoisonError::into_inner) = None;
        match result {
            Ok(()) => {
                self.status.set(SinkState::Disconnected);
                Ok(())
            }
            Err(err) => {
                self.status.fail(err.to_string());
                Err(err)
            }
        }
    }

    async fn stop_stream(&self) -> Result<()> {
        // Signals the running session; `start_stream` returns on its own.
        if let Some(handle) = self
            .session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            handle.stop();
        }
        self.status.reset();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn link_local_addresses_are_never_chosen() {
        use std::net::{Ipv4Addr, Ipv6Addr};

        let rota: IpAddr = Ipv4Addr::new(192, 168, 68, 114).into();
        let ll_v6: IpAddr = "fe80::25af:8855:5a42:286b"
            .parse::<Ipv6Addr>()
            .unwrap()
            .into();
        let ll_v4: IpAddr = Ipv4Addr::new(169, 254, 1, 2).into();

        // With a routable address in the list, link-local must not win.
        assert_eq!(best_address([ll_v6, rota].into_iter()), Some(rota));
        assert_eq!(best_address([ll_v4, rota].into_iter()), Some(rota));

        // And with link-local alone the answer is "none": connecting would
        // give EINVAL, and an error in the user's face is worse than ignoring
        // the announcement and waiting for the next one, which carries the
        // good address.
        assert_eq!(best_address([ll_v6, ll_v4].into_iter()), None);
    }

    #[test]
    fn ipv4_wins_over_routable_ipv6_but_ipv6_still_works() {
        use std::net::{Ipv4Addr, Ipv6Addr};

        let v4: IpAddr = Ipv4Addr::new(192, 168, 68, 114).into();
        let v6: IpAddr = "2001:db8::1".parse::<Ipv6Addr>().unwrap().into();

        assert_eq!(best_address([v6, v4].into_iter()), Some(v4));
        // On an IPv6-only network the receiver must not vanish from the list.
        assert_eq!(best_address([v6].into_iter()), Some(v6));
    }

    use super::*;

    #[test]
    fn service_kinds_use_the_right_mdns_types() {
        assert_eq!(
            ServiceKind::CHROMECAST.service_type,
            "_googlecast._tcp.local."
        );
        assert_eq!(ServiceKind::AIRPLAY.service_type, "_airplay._tcp.local.");
    }

    #[test]
    fn one_daemon_serves_both_protocols() {
        // A performance regression: two providers created two ServiceDaemons,
        // duplicating 5353 sockets and multicast traffic.
        let Ok(provider) = MdnsProvider::all_media_receivers() else {
            return; // sem rede no ambiente de teste
        };
        assert_eq!(provider.services.len(), 2);
    }
}
