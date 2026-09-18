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
//!   (the user may be on ufw, raw nftables, or no firewall at all);
//! - the rules applied are **remembered on disk** while they are open, so a
//!   run that dies without releasing them (crash, `SIGKILL`) does not leave
//!   the ports open until the next reboot: the next run closes them first
//!   (see [`release_stale`]).

use std::path::PathBuf;

use zbus::Connection;

use nd_core::{NdError, Result};

/// Port of the WFD RTSP server.
pub const RTSP_PORT: u16 = 7236;
/// Local RTP/RTCP ports (the RTCP coming back from the sink arrives at `+1`).
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

    /// The on-disk form: the zone, then one `port protocol` per line.
    fn to_file(&self) -> String {
        let mut out = format!("{}\n", self.zone);
        for (port, proto) in &self.ports {
            out.push_str(port);
            out.push(' ');
            out.push_str(proto);
            out.push('\n');
        }
        out
    }

    /// Reads back what [`Self::to_file`] wrote; `None` if the file makes no
    /// sense (there is nothing safe to do with a half-written zone name).
    fn from_file(contents: &str) -> Option<Self> {
        let mut lines = contents.lines();
        let zone = lines.next()?.trim();
        if zone.is_empty() {
            return None;
        }
        let ports = lines
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let mut parts = line.split_whitespace();
                Some((parts.next()?.to_string(), parts.next()?.to_string()))
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            zone: zone.to_string(),
            ports,
        })
    }
}

/// Where the open rules are remembered between runs.
fn lease_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    Some(base.join("bignetscreen").join("firewall-lease"))
}

/// Writes the lease down so a later run can undo it if this one cannot.
fn remember(lease: &FirewallLease) {
    if lease.is_noop() {
        return;
    }
    let Some(path) = lease_path() else {
        return;
    };
    if let Err(err) = nd_core::persistence::write_private(&path, lease.to_file().as_bytes()) {
        tracing::warn!(%err, path = %path.display(), "could not record the firewall lease");
    }
}

/// The lease was released: nothing left to remember.
fn forget() {
    let Some(path) = lease_path() else {
        return;
    };
    if let Err(err) = std::fs::remove_file(&path) {
        if err.kind() != std::io::ErrorKind::NotFound {
            tracing::debug!(%err, "could not remove the firewall lease record");
        }
    }
}

/// Closes the ports a previous run left open and forgets them.
///
/// Meant for startup and for right before opening new ports; both are no-ops
/// when nothing was left behind.
pub async fn release_stale() {
    let Some(path) = lease_path() else {
        return;
    };
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(_) => return,
    };
    let Some(lease) = FirewallLease::from_file(&contents) else {
        tracing::warn!(path = %path.display(), "unreadable firewall lease record; discarding it");
        forget();
        return;
    };
    tracing::info!(zone = %lease.zone, "closing firewall ports left open by a previous run");
    release(lease).await;
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
    release_stale().await;

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
        let already_open = match zone_proxy.query_port(&zone, &port, &proto).await {
            Ok(open) => open,
            Err(err) => {
                release(FirewallLease {
                    zone: zone.clone(),
                    ports: applied,
                })
                .await;
                return Err(NdError::Network(format!(
                    "firewalld queryPort failed: {err}"
                )));
            }
        };
        if already_open {
            tracing::debug!(%zone, %port, %proto, "port was already open");
            continue;
        }
        match zone_proxy.add_port(&zone, &port, &proto, 0).await {
            Ok(_) => {
                tracing::info!(%zone, %port, %proto, "port opened in firewalld");
                applied.push((port, proto));
            }
            Err(err) => {
                release(FirewallLease {
                    zone: zone.clone(),
                    ports: applied,
                })
                .await;
                return Err(NdError::Network(format!(
                    "firewalld could not open {port}/{proto}: {err}"
                )));
            }
        }
    }

    let lease = FirewallLease {
        zone,
        ports: applied,
    };
    remember(&lease);
    Ok(lease)
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
    // Forgotten before the attempt rather than after: a record that keeps
    // pointing at ports firewalld already dropped would only make the next
    // run log spurious failures.
    forget();
    let Some(conn) = connect().await else {
        return;
    };
    let Ok(zone_proxy) = FirewallZoneProxy::new(&conn).await else {
        return;
    };

    for (port, proto) in &lease.ports {
        match zone_proxy.remove_port(&lease.zone, port, proto).await {
            Ok(_) => tracing::info!(zone = %lease.zone, %port, %proto, "port closed"),
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
    fn lease_record_round_trips() {
        let lease = FirewallLease {
            zone: "public".into(),
            ports: vec![
                (RTSP_PORT.to_string(), "tcp".into()),
                (RTP_PORTS.to_string(), "udp".into()),
            ],
        };
        assert_eq!(FirewallLease::from_file(&lease.to_file()), Some(lease));
        assert_eq!(FirewallLease::from_file(""), None);
        assert_eq!(FirewallLease::from_file("\n7236 tcp\n"), None);
        assert_eq!(FirewallLease::from_file("public\n7236\n"), None);
        assert_eq!(
            FirewallLease::from_file("home\n"),
            Some(FirewallLease {
                zone: "home".into(),
                ports: Vec::new(),
            })
        );
    }

    #[test]
    fn noop_lease_releases_cleanly() {
        let lease = FirewallLease::noop();
        assert!(lease.is_noop());
        futures::executor::block_on(release(lease));
    }

    #[tokio::test]
    #[ignore = "requires explicit host firewall access"]
    async fn absent_firewalld_is_not_an_error() {
        // On a machine without firewalld this has to return Ok (a no-op), not
        // Err: most desktop users do not run firewalld.
        let result = ensure_ports_open(Some("p2p-wlan0-0")).await;
        let lease = result.expect("firewall integration");
        release(lease).await;
    }
}
