//! Forming a Wi-Fi Direct group with a receiver and tearing it down.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

use zbus::proxy::CacheProperties;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, Value};

use nd_core::{NdError, Result};

use super::{GROUP_STATE_POLL, P2pDevice, WFD_SOURCE_IES, classify, is_gone};
use crate::networkmanager::{ActiveConnectionProxy, DeviceProxy, Ip4ConfigProxy};

/// The state of a NetworkManager active connection (`NMActiveConnectionState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveState {
    Unknown,
    Activating,
    Activated,
    Deactivating,
    Deactivated,
}

impl From<u32> for ActiveState {
    fn from(v: u32) -> Self {
        match v {
            1 => ActiveState::Activating,
            2 => ActiveState::Activated,
            3 => ActiveState::Deactivating,
            4 => ActiveState::Deactivated,
            _ => ActiveState::Unknown,
        }
    }
}

/// Did NetworkManager refuse a teardown because the group already went down?
///
/// Any other refusal (a policy denial, a bus failure) leaves the group up and
/// has to reach the caller.
fn already_inactive(e: &zbus::Error) -> bool {
    is_gone(e)
        || matches!(
            e,
            zbus::Error::MethodError(name, _, _)
                if name.as_str() == "org.freedesktop.NetworkManager.ConnectionNotActive"
        )
}

impl P2pDevice {
    /// Forms a Wi-Fi Direct group with the peer (a Miracast sink) by
    /// activating a `wifi-p2p` connection with us in the WFD source role.
    /// Returns the active connection's path (follow it with
    /// [`active_state`](Self::active_state)).
    pub async fn connect(&self, peer_path: &str) -> Result<OwnedObjectPath> {
        let mut connection: HashMap<&str, Value<'_>> = HashMap::new();
        connection.insert("type", Value::from("wifi-p2p"));
        connection.insert("id", Value::from("BigNetScreen Miracast"));
        if let Some(user) = current_username() {
            connection.insert("permissions", Value::from(vec![format!("user:{user}:")]));
        }

        let mut wifi_p2p: HashMap<&str, Value<'_>> = HashMap::new();
        wifi_p2p.insert("wfd-ies", Value::from(WFD_SOURCE_IES.to_vec()));

        // Never route through this connection: it is only the P2P link for
        // the cast. Without `never-default`, the machine's normal traffic
        // would start trying to leave over the link to the TV.
        let mut ipv4: HashMap<&str, Value<'_>> = HashMap::new();
        ipv4.insert("method", Value::from("auto"));
        ipv4.insert("never-default", Value::from(true));

        let mut ipv6: HashMap<&str, Value<'_>> = HashMap::new();
        ipv6.insert("method", Value::from("auto"));
        ipv6.insert("never-default", Value::from(true));
        ipv6.insert("may-fail", Value::from(true));

        let mut settings: HashMap<&str, HashMap<&str, Value<'_>>> = HashMap::new();
        settings.insert("connection", connection);
        settings.insert("wifi-p2p", wifi_p2p);
        settings.insert("ipv4", ipv4);
        settings.insert("ipv6", ipv6);

        let mut options: HashMap<&str, Value<'_>> = HashMap::new();
        options.insert("bind-activation", Value::from("dbus-client"));
        options.insert("persist", Value::from("volatile"));

        let peer = ObjectPath::try_from(peer_path).map_err(NdError::network)?;

        let (_conn_path, active, _result) = self
            .nm
            .add_and_activate_connection2(settings, &self.path.as_ref(), &peer, options)
            .await
            .map_err(|e| classify(e, "AddAndActivateConnection2"))?;

        Ok(active)
    }

    /// Tears down the P2P group.
    pub async fn disconnect(&self, active: &OwnedObjectPath) -> Result<()> {
        match self.nm.deactivate_connection(&active.as_ref()).await {
            Ok(()) => Ok(()),
            Err(e) if already_inactive(&e) => Ok(()),
            Err(e) => Err(classify(e, "DeactivateConnection")),
        }
    }

    async fn active_proxy(&self, active: &OwnedObjectPath) -> Result<ActiveConnectionProxy<'_>> {
        ActiveConnectionProxy::builder(&self.conn)
            .path(active.clone())
            .map_err(NdError::network)?
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(NdError::network)
    }

    /// Reads an active connection's state.
    ///
    /// If the object vanished (the connection dropped, common on the Realtek
    /// driver), it returns `Deactivated`. Other D-Bus errors are propagated,
    /// so "the group dropped" stays distinct from "the bus is in trouble".
    pub async fn active_state(&self, active: &OwnedObjectPath) -> Result<ActiveState> {
        let proxy = match self.active_proxy(active).await {
            Ok(p) => p,
            Err(_) => return Ok(ActiveState::Deactivated),
        };
        match proxy.state().await {
            Ok(state) => Ok(ActiveState::from(state)),
            // The object was removed: the connection really did end.
            Err(e) if is_gone(&e) => Ok(ActiveState::Deactivated),
            Err(e) => Err(NdError::network(e)),
        }
    }

    /// The local IPv4 addresses assigned to an active connection (our IP on
    /// the P2P link, where the WFD RTSP server will listen).
    pub async fn addresses(&self, active: &OwnedObjectPath) -> Result<Vec<IpAddr>> {
        let ac = self.active_proxy(active).await?;
        // The connection can vanish between reading the state and this call
        // (the Realtek driver drops the group as soon as it forms). That is
        // not an error: it means "there is no address yet", and the caller
        // should keep trying rather than aborting the whole retry.
        let ip4 = match ac.ip4_config().await {
            Ok(path) => path,
            Err(e) if is_gone(&e) => return Ok(Vec::new()),
            Err(e) => return Err(NdError::network(e)),
        };
        if ip4.as_str() == "/" {
            return Ok(Vec::new());
        }

        let ip4_proxy = Ip4ConfigProxy::builder(&self.conn)
            .path(ip4)
            .map_err(NdError::network)?
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(NdError::network)?;
        let address_data = match ip4_proxy.address_data().await {
            Ok(address_data) => address_data,
            Err(e) if is_gone(&e) => return Ok(Vec::new()),
            Err(e) => return Err(NdError::network(e)),
        };

        let mut addresses = Vec::new();
        for entry in &address_data {
            if let Some(value) = entry.get("address")
                && let Ok(text) = String::try_from(value.clone())
            {
                match text.parse::<IpAddr>() {
                    Ok(ip) => addresses.push(ip),
                    Err(e) => tracing::warn!(%text, %e, "invalid address from NM"),
                }
            }
        }
        Ok(addresses)
    }

    /// The P2P group's network interface name (e.g. `p2p-wlan0-0`).
    ///
    /// Needed to ask firewalld to open the port only on that link, rather
    /// than on the whole machine.
    pub async fn interface(&self, active: &OwnedObjectPath) -> Result<Option<String>> {
        let ac = self.active_proxy(active).await?;
        let devices = match ac.devices().await {
            Ok(devices) => devices,
            Err(e) if is_gone(&e) => return Ok(None),
            Err(e) => return Err(NdError::network(e)),
        };
        for dev in devices {
            let device = DeviceProxy::builder(&self.conn)
                .path(dev)
                .map_err(NdError::network)?
                .cache_properties(CacheProperties::No)
                .build()
                .await
                .map_err(NdError::network)?;
            // `IpInterface` is the link's real name; `Interface` serves as a
            // fallback while the device is still coming up.
            let mut name = device.ip_interface().await.ok().filter(|n| !n.is_empty());
            if name.is_none() {
                name = device.interface().await.ok().filter(|n| !n.is_empty());
            }
            if let Some(name) = name {
                return Ok(Some(name));
            }
        }
        Ok(None)
    }

    /// How long to let NetworkManager finish removing an activation.
    ///
    /// The object leaves the bus within about a second, and a new activation
    /// started before that inherits the removal.
    const TEARDOWN_SETTLE: Duration = Duration::from_secs(2);

    /// Forms the group and waits until there is a usable local IP, retrying
    /// because the Realtek driver often drops the group right after forming it.
    pub async fn connect_and_wait(
        &self,
        peer_path: &str,
        attempts: u32,
        per_attempt: Duration,
    ) -> Result<(OwnedObjectPath, IpAddr)> {
        let (_tx, mut cancel) = tokio::sync::watch::channel(false);
        self.connect_and_wait_cancelled(peer_path, attempts, per_attempt, &mut cancel)
            .await
    }

    /// [`Self::connect_and_wait`] that gives up, tearing the group down, once
    /// `cancel` turns true or its sender is dropped.
    pub async fn connect_and_wait_cancelled(
        &self,
        peer_path: &str,
        attempts: u32,
        per_attempt: Duration,
        cancel: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<(OwnedObjectPath, IpAddr)> {
        let mut last = NdError::Network("could not form the P2P group".into());

        for attempt in 1..=attempts.max(1) {
            if *cancel.borrow() {
                return Err(NdError::Cancelled);
            }
            tracing::info!(attempt, "forming the Wi-Fi Direct group");
            let active = match self.connect(peer_path).await {
                Ok(a) => a,
                Err(e) => {
                    // Logged per attempt: "the receiver did not answer" and
                    // "the card refused" call for opposite actions.
                    tracing::warn!(attempt, err = %e, "could not start the group");
                    last = e;
                    continue;
                }
            };

            let failure = match self.wait_for_address(&active, per_attempt, cancel).await {
                Ok(ip) => return Ok((active, ip)),
                Err(failure) => failure,
            };
            if let Err(err) = self.disconnect(&active).await {
                tracing::warn!(%err, "could not tear down the Wi-Fi Direct group");
            }
            if matches!(failure, NdError::Cancelled) {
                return Err(failure);
            }
            tracing::warn!(attempt, err = %failure, "the Wi-Fi Direct group attempt failed");
            last = failure;
            // Let the teardown finish. Asking for a new group while the old
            // activation is still being removed is how an attempt ends up
            // watching an object that is on its way out.
            if cancelled_during(cancel, Self::TEARDOWN_SETTLE).await {
                return Err(NdError::Cancelled);
            }
        }

        Err(last)
    }

    /// Watches one activation until it has a local address, for at most
    /// `per_attempt`; leaves tearing it down to the caller.
    async fn wait_for_address(
        &self,
        active: &OwnedObjectPath,
        per_attempt: Duration,
        cancel: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<IpAddr> {
        let deadline = tokio::time::Instant::now() + per_attempt;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(NdError::Network(
                    "the Wi-Fi Direct group was not ready in time".into(),
                ));
            }
            if cancelled_during(cancel, GROUP_STATE_POLL).await {
                return Err(NdError::Cancelled);
            }

            // A vanished activation is this attempt failing, not a fatal
            // error: NetworkManager removes the object as soon as it tears an
            // activation down, and asking about it then answers
            // `UnknownMethod`. The caller's retries exist for exactly that.
            let state = self.active_state(active).await.map_err(|err| {
                NdError::Network(format!(
                    "the Wi-Fi Direct group disappeared while it was being formed: {err}"
                ))
            })?;
            match state {
                ActiveState::Activated => {
                    let addresses = self.addresses(active).await.map_err(|err| {
                        NdError::Network(format!(
                            "the Wi-Fi Direct group disappeared before it had an address: {err}"
                        ))
                    })?;
                    if let Some(ip) = addresses.into_iter().next() {
                        tracing::info!(%ip, "P2P group formed");
                        return Ok(ip);
                    }
                    // Activated but no DHCP yet: keep waiting.
                }
                ActiveState::Deactivated => {
                    return Err(NdError::Network(
                        "the Wi-Fi Direct group dropped right after being formed \
                         (a known instability on Realtek cards)"
                            .into(),
                    ));
                }
                ActiveState::Unknown | ActiveState::Activating | ActiveState::Deactivating => {}
            }
        }
    }
}

/// Waits `duration` unless `cancel` asks to stop first; `true` means stop.
///
/// A dropped sender also means stop: nobody is left to use the group, and a
/// closed channel answers `changed()` at once, which would otherwise spin.
async fn cancelled_during(
    cancel: &mut tokio::sync::watch::Receiver<bool>,
    duration: Duration,
) -> bool {
    let sleep = tokio::time::sleep(duration);
    tokio::pin!(sleep);
    loop {
        if *cancel.borrow_and_update() {
            return true;
        }
        tokio::select! {
            _ = &mut sleep => return false,
            changed = cancel.changed() => if changed.is_err() {
                return true;
            }
        }
    }
}

/// The current user's name.
///
/// `$USER` may be missing under a service manager, so the uid is looked up in
/// `/etc/passwd` as a last resort. With no name the connection carries no
/// permissions, which NetworkManager accepts.
fn current_username() -> Option<String> {
    if let Ok(user) = std::env::var("USER")
        && !user.is_empty()
    {
        return Some(user);
    }
    if let Ok(user) = std::env::var("LOGNAME")
        && !user.is_empty()
    {
        return Some(user);
    }
    // A last resort with no extra dependency: resolve the uid in /etc/passwd.
    let uid = current_uid();
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let _passwd = fields.next()?;
        let entry_uid: u32 = fields.next()?.parse().ok()?;
        (entry_uid == uid).then(|| name.to_string())
    })
}

/// `getuid(2)` without pulling the `libc` crate into the dependency graph.
fn current_uid() -> u32 {
    // SAFETY: getuid takes no arguments, touches no memory and cannot fail,
    // so declaring it safe holds for every call.
    unsafe extern "C" {
        safe fn getuid() -> u32;
    }
    getuid()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn method_error(name: &str) -> zbus::Error {
        let call = zbus::Message::method_call("/", "DeactivateConnection")
            .unwrap()
            .build(&())
            .unwrap();
        zbus::Message::error(&call.header(), name)
            .unwrap()
            .build(&("detail",))
            .unwrap()
            .into()
    }

    #[test]
    fn torn_down_means_already_down() {
        assert!(already_inactive(&method_error(
            "org.freedesktop.NetworkManager.ConnectionNotActive"
        )));
        assert!(already_inactive(&method_error(
            "org.freedesktop.DBus.Error.UnknownObject"
        )));
        // A refusal leaves the group up; reporting success would hide it.
        assert!(!already_inactive(&method_error(
            "org.freedesktop.NetworkManager.PermissionDenied"
        )));
        assert!(!already_inactive(&method_error(
            "org.freedesktop.DBus.Error.AccessDenied"
        )));
    }

    #[tokio::test]
    async fn dropped_sender_ends_wait() {
        let (sender, mut cancel) = tokio::sync::watch::channel(false);
        drop(sender);
        let wait = cancelled_during(&mut cancel, Duration::from_millis(200));
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), wait).await,
            Ok(true)
        );
    }

    #[tokio::test]
    async fn wait_ends_early_on_cancel() {
        let (sender, mut cancel) = tokio::sync::watch::channel(false);
        assert!(!cancelled_during(&mut cancel, Duration::from_millis(10)).await);
        sender.send_replace(true);
        assert!(cancelled_during(&mut cancel, Duration::from_secs(60)).await);
    }

    #[test]
    fn active_state_mapping() {
        assert_eq!(ActiveState::from(0), ActiveState::Unknown);
        assert_eq!(ActiveState::from(1), ActiveState::Activating);
        assert_eq!(ActiveState::from(2), ActiveState::Activated);
        assert_eq!(ActiveState::from(3), ActiveState::Deactivating);
        assert_eq!(ActiveState::from(4), ActiveState::Deactivated);
        assert_eq!(ActiveState::from(99), ActiveState::Unknown);
    }
}
