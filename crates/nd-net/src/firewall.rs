//! Opening the WFD ports in firewalld for the length of a Miracast session.
//!
//! firewalld is usually active on Manjaro/BigLinux, and without these rules the
//! receiver cannot reach TCP 7236 and the cast fails silently.
//!
//! - The rules are runtime only: a reboot or `firewall-cmd --reload` discards
//!   them, and nothing is written to the permanent configuration.
//! - They go in the P2P interface's zone, not the whole machine.
//! - Without firewalld (ufw, raw nftables, no firewall) everything is a no-op.
//! - The open rules are recorded on disk until released, so a run that dies
//!   without releasing them (crash, `SIGKILL`) does not leave the ports open
//!   until the next reboot: the next run closes them first ([`release_stale`]).

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

/// The ports a session needs, as firewalld names them.
fn wfd_ports() -> [(String, String); 2] {
    [
        (RTSP_PORT.to_string(), "tcp".to_string()),
        (RTP_PORTS.to_string(), "udp".to_string()),
    ]
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
    ///
    /// Only the WFD ports are accepted: the record is a file anyone with the
    /// user's rights can edit, and it must not be able to close other rules.
    fn from_file(contents: &str) -> Option<Self> {
        let mut lines = contents.lines();
        let zone = lines.next()?.trim();
        if zone.is_empty() {
            return None;
        }
        let known = wfd_ports();
        let ports = lines
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let mut parts = line.split_whitespace();
                let rule = (parts.next()?.to_string(), parts.next()?.to_string());
                (parts.next().is_none() && known.contains(&rule)).then_some(rule)
            })
            .collect::<Option<Vec<_>>>()?;
        if ports.len() > 2 || (ports.len() == 2 && ports[0] == ports[1]) {
            return None;
        }
        Some(Self {
            zone: zone.to_string(),
            ports,
        })
    }
}

/// Where the open rules are remembered between runs.
fn lease_path() -> Option<PathBuf> {
    nd_core::persistence::app_dir(nd_core::persistence::BaseDir::State)
        .map(|dir| dir.join("firewall-lease"))
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
    if let Err(err) = std::fs::remove_file(&path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(%err, "could not remove the firewall lease record");
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
/// A missing firewalld is not an error. It fails only on an error while
/// applying a rule, after undoing the rules this call had applied.
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
    let Some(zone) = zone_of(&conn, &zone_proxy, interface).await? else {
        return Ok(FirewallLease::noop());
    };

    let mut lease = FirewallLease {
        zone,
        ports: Vec::new(),
    };
    for (port, proto) in wfd_ports() {
        let zone = &lease.zone;
        // Already open by the user's configuration? Then it is not ours to close.
        let opened = match zone_proxy.query_port(zone, &port, &proto).await {
            Ok(true) => {
                tracing::debug!(%zone, %port, %proto, "port was already open");
                continue;
            }
            Ok(false) => zone_proxy
                .add_port(zone, &port, &proto, 0)
                .await
                .map_err(|err| format!("firewalld could not open {port}/{proto}: {err}")),
            Err(err) => Err(format!("firewalld queryPort failed: {err}")),
        };
        if let Err(message) = opened {
            release(lease).await;
            return Err(NdError::Network(message));
        }
        tracing::info!(%zone, %port, %proto, "port opened in firewalld");
        lease.ports.push((port, proto));
        // Recorded after each rule, so a crash between the two leaves the
        // first one on record.
        remember(&lease);
    }
    Ok(lease)
}

/// The zone `interface` is in, falling back to the default zone; `None` when
/// firewalld is not there.
///
/// A freshly created interface usually has no explicit zone and lands in the
/// default one. zbus proxies are lazy, so firewalld's absence only shows up on
/// these first calls.
async fn zone_of(
    conn: &Connection,
    zone_proxy: &FirewallZoneProxy<'_>,
    interface: Option<&str>,
) -> Result<Option<String>> {
    if let Some(interface) = interface
        && let Ok(zone) = zone_proxy.get_zone_of_interface(interface).await
        && !zone.is_empty()
    {
        return Ok(Some(zone));
    }
    default_zone(conn).await
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
/// Errors are logged and the journal is retained for retry. A failed D-Bus
/// cleanup must never erase the only record of a port this process opened.
pub async fn release(lease: FirewallLease) {
    if lease.is_noop() {
        return;
    }
    // Any early return keeps the on-disk record for the next run.
    let Some(conn) = connect().await else {
        return;
    };
    let zone_proxy = match FirewallZoneProxy::new(&conn).await {
        Ok(proxy) => proxy,
        Err(err) => {
            tracing::warn!(%err, zone = %lease.zone, "firewalld unreachable; ports left open for the next run");
            return;
        }
    };

    let mut complete = true;
    for (port, proto) in &lease.ports {
        // A reload may already have removed the runtime rule. Absence is a
        // successful cleanup, unlike a failed query or permission denial.
        match zone_proxy.query_port(&lease.zone, port, proto).await {
            Ok(false) => continue,
            Ok(true) => {}
            Err(err) => {
                complete = false;
                tracing::warn!(zone = %lease.zone, %port, %proto, %err, "could not verify firewall cleanup");
                continue;
            }
        }
        match zone_proxy.remove_port(&lease.zone, port, proto).await {
            Ok(_) => tracing::info!(zone = %lease.zone, %port, %proto, "port closed"),
            Err(err) => {
                complete = false;
                tracing::warn!(zone = %lease.zone, %port, %proto, %err, "failed to close the port; retaining lease");
            }
        }
    }
    if complete {
        // Do not erase a different lease that another operation has written.
        if let Some(path) = lease_path()
            && std::fs::read_to_string(path)
                .ok()
                .as_deref()
                .and_then(FirewallLease::from_file)
                .as_ref()
                == Some(&lease)
        {
            forget();
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
            ports: wfd_ports().to_vec(),
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
    async fn absent_firewalld_is_a_no_op() {
        // Most desktop users do not run firewalld.
        let result = ensure_ports_open(Some("p2p-wlan0-0")).await;
        let lease = result.expect("firewall integration");
        release(lease).await;
    }
    #[test]
    fn lease_cleans_only_its_ports() {
        for text in [
            "public\n22 tcp\n",
            "public\n7236 udp\n",
            "public\n7236 tcp extra\n",
            "public\n7236 tcp\n7236 tcp\n",
        ] {
            assert!(
                FirewallLease::from_file(text).is_none(),
                "accepted {text:?}"
            );
        }
    }
}
