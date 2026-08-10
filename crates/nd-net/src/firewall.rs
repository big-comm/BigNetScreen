//! Opening the WFD ports in firewalld.
//!
//! On Manjaro/BigLinux firewalld is usually active, and in that case the
//! Miracast sink **cannot reach TCP 7236** — the cast used to fail with no
//! message at all, because `ensure_firewall_zone()` was an empty `Ok(())`.
//!
//! The strategy:
//! - the rules are **runtime only**, never written to the permanent
//!   configuration: a reboot or a `firewall-cmd --reload` discards them;
//! - the scope is the **P2P interface's zone**, not the whole machine;
//! - if firewalld is not installed/active, everything becomes a silent no-op
//!   (the user may be on ufw, raw nftables, or no firewall at all).

use zbus::Connection;

use nd_core::{NdError, Result};

/// Porta do servidor RTSP do WFD.
pub const RTSP_PORT: u16 = 7236;
/// Portas RTP/RTCP locais (o RTCP de volta do sink chega em `+1`).
pub const RTP_PORTS: &str = "16384-16385";

#[zbus::proxy(
    interface = "org.fedoraproject.FirewallD1",
    default_service = "org.fedoraproject.FirewallD1",
    default_path = "/org/fedoraproject/FirewallD1"
)]
trait FirewallD {
    fn get_default_zone(&self) -> zbus::Result<String>;
}

#[zbus::proxy(
    interface = "org.fedoraproject.FirewallD1.zone",
    default_service = "org.fedoraproject.FirewallD1",
    default_path = "/org/fedoraproject/FirewallD1"
)]
trait FirewallZone {
    fn get_zone_of_interface(&self, interface: &str) -> zbus::Result<String>;

    fn add_port(
        &self,
        zone: &str,
        port: &str,
        protocol: &str,
        timeout: i32,
    ) -> zbus::Result<String>;

    fn remove_port(&self, zone: &str, port: &str, protocol: &str) -> zbus::Result<String>;

    fn query_port(&self, zone: &str, port: &str, protocol: &str) -> zbus::Result<bool>;
}

/// The rules applied, so they can be undone exactly as they were made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirewallLease {
    zone: String,
    ports: Vec<(String, String)>,
}

impl FirewallLease {
    /// No rules applied (firewalld absent): `release` does nothing.
    pub fn noop() -> Self {
        Self {
            zone: String::new(),
            ports: Vec::new(),
        }
    }

    pub fn is_noop(&self) -> bool {
        self.ports.is_empty()
    }

    /// The zone the rules were applied in.
    pub fn zone(&self) -> &str {
        &self.zone
    }
}

async fn connect() -> Option<Connection> {
    match Connection::system().await {
        Ok(conn) => Some(conn),
        Err(err) => {
            tracing::debug!(%err, "no system bus; firewalld skipped");
            None
        }
    }
}

/// Opens the WFD ports in the given interface's zone.
///
/// `interface` is the P2P link's name (e.g. `p2p-wlan0-0`); when `None`, the
/// default zone is used. Returns the "lease" to hand to [`release`].
///
/// It never fails because firewalld is missing — only on a real error while
/// applying a rule.
pub async fn ensure_ports_open(interface: Option<&str>) -> Result<FirewallLease> {
    let Some(conn) = connect().await else {
        return Ok(FirewallLease::noop());
    };

    let zone_proxy = match FirewallZoneProxy::new(&conn).await {
        Ok(p) => p,
        Err(err) => {
            tracing::info!(%err, "firewalld is not on the bus; nothing to do");
            return Ok(FirewallLease::noop());
        }
    };

    // Find out which zone the P2P interface is in. A freshly created
    // interface usually has no explicit zone: it lands in the default one.
    //
    // zbus proxies are lazy: firewalld's absence only shows up here, on the
    // first call — which is why the degradation to a no-op happens at this
    // point rather than when the proxy is created.
    let zone = match interface {
        Some(iface) => match zone_proxy.get_zone_of_interface(iface).await {
            Ok(z) if !z.is_empty() => z,
            _ => match default_zone(&conn).await? {
                Some(z) => z,
                None => return Ok(FirewallLease::noop()),
            },
        },
        None => match default_zone(&conn).await? {
            Some(z) => z,
            None => return Ok(FirewallLease::noop()),
        },
    };

    let wanted = [
        (RTSP_PORT.to_string(), "tcp".to_string()),
        (RTP_PORTS.to_string(), "udp".to_string()),
    ];

    let mut applied = Vec::new();
    for (port, proto) in wanted {
        // Already open by the user's configuration? Then it is not ours to close.
        if zone_proxy
            .query_port(&zone, &port, &proto)
            .await
            .unwrap_or(false)
        {
            tracing::debug!(%zone, %port, %proto, "port was already open");
            continue;
        }
        match zone_proxy.add_port(&zone, &port, &proto, 0).await {
            Ok(_) => {
                tracing::info!(%zone, %port, %proto, "porta aberta no firewalld");
                applied.push((port, proto));
            }
            Err(err) => {
                // Missing authorisation (polkit) is the common case: warn and
                // carry on — the cast may work if the firewall already allows it.
                tracing::warn!(%zone, %port, %proto, %err, "could not open the port");
            }
        }
    }

    Ok(FirewallLease {
        zone,
        ports: applied,
    })
}

/// firewalld's default zone, or `None` when firewalld is not there.
async fn default_zone(conn: &Connection) -> Result<Option<String>> {
    let proxy = match FirewallDProxy::new(conn).await {
        Ok(p) => p,
        Err(err) => {
            tracing::debug!(%err, "firewalld absent");
            return Ok(None);
        }
    };
    match proxy.get_default_zone().await {
        Ok(zone) => Ok(Some(zone)),
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown"
                || name.as_str() == "org.freedesktop.DBus.Error.NameHasNoOwner" =>
        {
            tracing::info!("firewalld is not installed/active; no ports to open");
            Ok(None)
        }
        Err(e) => Err(NdError::Network(format!("firewalld getDefaultZone: {e}"))),
    }
}

/// Removes exactly the rules [`ensure_ports_open`] applied.
///
/// Idempotent and forgiving: an error here is logged, never propagated —
/// failing to *close* a port must not break the session teardown.
pub async fn release(lease: FirewallLease) {
    if lease.is_noop() {
        return;
    }
    let Some(conn) = connect().await else {
        return;
    };
    let Ok(zone_proxy) = FirewallZoneProxy::new(&conn).await else {
        return;
    };

    for (port, proto) in &lease.ports {
        match zone_proxy.remove_port(&lease.zone, port, proto).await {
            Ok(_) => tracing::info!(zone = %lease.zone, %port, %proto, "porta fechada"),
            Err(err) => {
                tracing::warn!(zone = %lease.zone, %port, %proto, %err, "failed to close the port")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_lease_releases_cleanly() {
        let lease = FirewallLease::noop();
        assert!(lease.is_noop());
        futures::executor::block_on(release(lease));
    }

    #[tokio::test]
    async fn absent_firewalld_is_not_an_error() {
        // On a machine without firewalld this has to return Ok (a no-op), not
        // Err: most desktop users do not run firewalld.
        let result = ensure_ports_open(Some("p2p-wlan0-0")).await;
        assert!(result.is_ok(), "{result:?}");
    }
}
