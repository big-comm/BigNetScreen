//! NetworkManager's D-Bus interfaces, and the LAN addresses it reports.
//!
//! Shared by Wi-Fi Direct ([`crate::p2p`]) and by DLNA discovery, which binds
//! one SSDP source per active LAN connection.

use std::collections::HashMap;

use zbus::Connection;
use zbus::proxy::CacheProperties;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
pub(crate) trait NetworkManager {
    fn get_all_devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    #[zbus(property)]
    fn active_connections(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    #[allow(clippy::type_complexity)]
    fn add_and_activate_connection2(
        &self,
        connection: HashMap<&str, HashMap<&str, Value<'_>>>,
        device: &ObjectPath<'_>,
        specific_object: &ObjectPath<'_>,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<(
        OwnedObjectPath,
        OwnedObjectPath,
        HashMap<String, OwnedValue>,
    )>;

    fn deactivate_connection(&self, active: &ObjectPath<'_>) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Device",
    default_service = "org.freedesktop.NetworkManager"
)]
pub(crate) trait Device {
    #[zbus(property)]
    fn device_type(&self) -> zbus::Result<u32>;

    #[zbus(property)]
    fn ip_interface(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn interface(&self) -> zbus::Result<String>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Device.WifiP2P",
    default_service = "org.freedesktop.NetworkManager"
)]
pub(crate) trait WifiP2p {
    fn start_find(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;

    fn stop_find(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn peers(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    #[zbus(signal)]
    fn peer_added(&self, peer: OwnedObjectPath) -> zbus::Result<()>;

    #[zbus(signal)]
    fn peer_removed(&self, peer: OwnedObjectPath) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.WifiP2PPeer",
    default_service = "org.freedesktop.NetworkManager"
)]
pub(crate) trait WifiP2pPeer {
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn hw_address(&self) -> zbus::Result<String>;

    #[zbus(property, name = "WfdIEs")]
    fn wfd_ies(&self) -> zbus::Result<Vec<u8>>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Connection.Active",
    default_service = "org.freedesktop.NetworkManager"
)]
pub(crate) trait ActiveConnection {
    #[zbus(property, name = "Type")]
    fn connection_type(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;

    #[zbus(property)]
    fn ip4_config(&self) -> zbus::Result<OwnedObjectPath>;

    #[zbus(property)]
    fn devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.IP4Config",
    default_service = "org.freedesktop.NetworkManager"
)]
pub(crate) trait Ip4Config {
    #[zbus(property)]
    fn address_data(&self) -> zbus::Result<Vec<HashMap<String, OwnedValue>>>;
}

/// One IPv4 source per active LAN connection, including non-default routes.
/// Refreshing this list on each SSDP sweep follows Wi-Fi/Ethernet changes.
pub async fn local_ipv4_addresses() -> zbus::Result<Vec<std::net::Ipv4Addr>> {
    let conn = Connection::system().await?;
    let nm = NetworkManagerProxy::new(&conn).await?;
    let mut addresses = Vec::new();
    for path in nm.active_connections().await?.into_iter().take(64) {
        let read = async {
            let active = ActiveConnectionProxy::builder(&conn)
                .path(path)?
                .cache_properties(CacheProperties::No)
                .build()
                .await?;
            // Tunnels and loopback must not divert LAN discovery onto a VPN.
            if active.state().await? != 2
                || !matches!(
                    active.connection_type().await?.as_str(),
                    "802-3-ethernet" | "802-11-wireless" | "bridge" | "bond" | "vlan"
                )
            {
                return Ok(None);
            }
            let ip4 = Ip4ConfigProxy::builder(&conn)
                .path(active.ip4_config().await?)?
                .cache_properties(CacheProperties::No)
                .build()
                .await?;
            Ok::<_, zbus::Error>(ip4.address_data().await?.iter().take(64).find_map(|entry| {
                let address = <&str>::try_from(entry.get("address")?)
                    .ok()?
                    .parse::<std::net::Ipv4Addr>()
                    .ok()?;
                (!address.is_loopback() && !address.is_unspecified() && !address.is_multicast())
                    .then_some(address)
            }))
        };
        // One disappearing connection must not hide the other active networks.
        if let Ok(Some(address)) = read.await
            && !addresses.contains(&address)
        {
            addresses.push(address);
        }
    }
    Ok(addresses)
}
