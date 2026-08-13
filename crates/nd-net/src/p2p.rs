//! Wi-Fi Direct (P2P) discovery and connection through NetworkManager — the
//! foundation of Miracast/WFD.
//!
//! NetworkManager exposes a dedicated `p2p-dev-wlpXsY` device with the
//! `org.freedesktop.NetworkManager.Device.WifiP2P` interface
//! (`StartFind`/`StopFind`, the `Peers` property and the
//! `PeerAdded`/`PeerRemoved` signals). Every peer with non-empty `WfdIEs` is a
//! **Miracast sink** (a TV/projector in "Screen Mirroring" mode).
//!
//! ## Two important fixes over the earlier version
//!
//! 1. **`StartFind` expires.** NM's default is 30 s; without renewal, a sink
//!    switched on after that never showed up. See [`P2pDevice::start_find`]
//!    and [`FIND_TIMEOUT`].
//! 2. **Signals instead of polling.** The peer list used to be re-read every
//!    2 s, creating one D-Bus proxy per peer (with `GetAll` + a match rule) on
//!    every cycle. Now `PeerAdded`/`PeerRemoved` are used, which removes both
//!    the traffic and the up-to-2 s detection delay.
//!
//! Unavailable under Flatpak (system bus).

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

use futures::stream::{Stream, StreamExt};
use zbus::proxy::CacheProperties;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::Connection;

use nd_core::{NdError, Result};

/// The WFD Information Elements advertising this PC as a Wi-Fi Display
/// **source** with its RTSP server on port 7236.
///
/// Subelement 0 (Device Information), length 6:
/// `0x0090` = device type source + WSD; `0x1c44` = 7236 (the RTSP port);
/// `0x00c8` = 200 (the declared maximum throughput, in Mbps).
pub const WFD_SOURCE_IES: &[u8] = &[0x00, 0x00, 0x06, 0x00, 0x90, 0x1c, 0x44, 0x00, 0xc8];

/// The duration of each scan requested from NetworkManager.
///
/// NM accepts 1–600 s. A short, **renewed** window is used (see
/// [`P2pDevice::keep_finding`]) rather than one long one: that way the radio
/// returns to the normal network's channel periodically and the main Wi-Fi
/// connection suffers less.
pub const FIND_TIMEOUT: Duration = Duration::from_secs(30);

/// The scan renewal interval (shorter than [`FIND_TIMEOUT`], so there is no
/// dead window between one scan and the next).
pub const FIND_RENEW_INTERVAL: Duration = Duration::from_secs(20);

/// `NM_DEVICE_TYPE_WIFI_P2P`.
const DEVICE_TYPE_WIFI_P2P: u32 = 30;

// ---------------------------------------------------------------------------
// Proxies tipados
// ---------------------------------------------------------------------------

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
trait NetworkManager {
    fn get_all_devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

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
trait Device {
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
trait WifiP2p {
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
trait WifiP2pPeer {
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
trait ActiveConnection {
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
trait Ip4Config {
    #[zbus(property)]
    fn address_data(&self) -> zbus::Result<Vec<HashMap<String, OwnedValue>>>;
}

/// wpa_supplicant's global interface.
///
/// NetworkManager drives discovery, but the WFD Information Elements live one
/// layer below, on the supplicant itself — and NM exposes no way to set them
/// for the *search*, only for a connection.
#[zbus::proxy(
    interface = "fi.w1.wpa_supplicant1",
    default_service = "fi.w1.wpa_supplicant1",
    default_path = "/fi/w1/wpa_supplicant1"
)]
trait WpaSupplicant {
    #[zbus(property, name = "WFDIEs")]
    fn wfd_ies(&self) -> zbus::Result<Vec<u8>>;

    #[zbus(property, name = "WFDIEs")]
    fn set_wfd_ies(&self, value: &[u8]) -> zbus::Result<()>;
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

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

/// Um peer Wi-Fi Direct descoberto.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct P2pPeer {
    /// D-Bus path (a stable identifier).
    pub path: String,
    /// The advertised friendly name.
    pub name: String,
    /// The peer's MAC address.
    pub hw_address: String,
    /// `true` if it advertises Wi-Fi Display (it is a Miracast sink).
    pub is_wfd: bool,
}

/// A change in the peer list, coming from NetworkManager's signals.
#[derive(Clone, Debug)]
pub enum PeerEvent {
    Added(P2pPeer),
    Removed { path: String },
}

fn err<E: std::fmt::Display>(e: E) -> NdError {
    NdError::Network(e.to_string())
}

/// Has the D-Bus object ceased to exist?
///
/// Telling "it vanished" from "it failed" is what lets the group-formation
/// retry survive driver instability: the active connection disappears midway,
/// and that must not abort the remaining attempts.
fn is_gone(e: &zbus::Error) -> bool {
    matches!(
        e,
        zbus::Error::MethodError(name, _, _)
            if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod"
                || name.as_str() == "org.freedesktop.DBus.Error.UnknownObject"
                || name.as_str() == "org.freedesktop.DBus.Error.UnknownInterface"
    )
}

/// Tells "the service does not even exist here" from "the call failed".
///
/// Previously every D-Bus error became the same `Deactivated`/`Network`, which
/// kept the UI from telling the user whether the problem was missing support
/// (Flatpak, a card without P2P) or a transient failure.
fn classify(e: zbus::Error, context: &str) -> NdError {
    let unavailable = matches!(
        &e,
        zbus::Error::MethodError(name, _, _)
            if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown"
                || name.as_str() == "org.freedesktop.DBus.Error.NameHasNoOwner"
    );
    if unavailable {
        NdError::Unsupported(format!(
            "{context}: NetworkManager is not available on the system bus \
             (esperado sob Flatpak)"
        ))
    } else {
        NdError::Network(format!("{context}: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------------

/// A handle to NetworkManager's Wi-Fi P2P device.
///
/// The proxies are created **once** and reused; the earlier version rebuilt
/// the device and NM proxies on every call, plus one proxy per peer on every
/// polling cycle.
pub struct P2pDevice {
    conn: Connection,
    path: OwnedObjectPath,
    p2p: WifiP2pProxy<'static>,
    nm: NetworkManagerProxy<'static>,
}

impl P2pDevice {
    /// Abre o barramento de sistema e localiza o device Wi-Fi P2P.
    pub async fn open() -> Result<Self> {
        let conn = Connection::system().await.map_err(|e| {
            NdError::Unsupported(format!(
                "no access to the system bus ({e}) — Miracast requires the native build"
            ))
        })?;

        let nm = NetworkManagerProxy::new(&conn)
            .await
            .map_err(|e| classify(e, "NetworkManager"))?;
        let devices = nm
            .get_all_devices()
            .await
            .map_err(|e| classify(e, "GetAllDevices"))?;

        for dev in devices {
            let device = DeviceProxy::builder(&conn)
                .path(dev.clone())
                .map_err(err)?
                .cache_properties(CacheProperties::No)
                .build()
                .await
                .map_err(err)?;
            if device.device_type().await.unwrap_or(0) == DEVICE_TYPE_WIFI_P2P {
                let p2p = WifiP2pProxy::builder(&conn)
                    .path(dev.clone())
                    .map_err(err)?
                    .build()
                    .await
                    .map_err(err)?;
                tracing::info!(path = %dev, "Wi-Fi P2P device found");

                // Before any search: several sinks only answer with their WFD
                // Information Elements to a peer that already identifies
                // itself as a Wi-Fi Display source.
                Self::advertise_as_wfd_source(&conn).await;

                return Ok(Self {
                    conn,
                    path: dev,
                    p2p,
                    nm,
                });
            }
        }

        // The three causes are indistinguishable from here, and they call for
        // very different actions, so all three are named. Reporting only "the
        // card does not support it" sent people shopping for a Wi-Fi adapter
        // they did not need.
        Err(NdError::Unsupported(
            "no Wi-Fi P2P device in NetworkManager. Either this Wi-Fi card does not \
             support Wi-Fi Direct, or the driver does not expose it, or NetworkManager \
             is running with the `iwd` backend, which has no P2P support — `nmcli device` \
             should list a `p2p-dev-*` device alongside the Wi-Fi one"
                .into(),
        ))
    }

    /// The device's D-Bus path.
    pub fn path(&self) -> &str {
        self.path.as_str()
    }

    /// Advertises this machine as a Wi-Fi Display **source** during discovery.
    ///
    /// A sink decides whether to answer with its own WFD Information Elements
    /// based on what the probe request carries. Several receivers — Amazon Fire
    /// TV among them — stay silent for a peer that does not identify itself as
    /// a WFD device, and the search then finds a plain P2P peer with no WFD
    /// IEs, which discovery filters out. The device is right there and never
    /// appears in the list.
    ///
    /// NetworkManager only applies the `wfd-ies` of a *connection*, which is
    /// too late: by then the sink already has to have been found. The property
    /// lives one layer below, on wpa_supplicant itself.
    ///
    /// Best effort by design. It fails, harmlessly, when:
    /// - wpa_supplicant is not on the bus (NetworkManager running with `iwd`);
    /// - it was built without `CONFIG_WIFI_DISPLAY`, and the property does not
    ///   exist;
    /// - D-Bus policy refuses the write to a non-root caller.
    ///
    /// In every one of those cases discovery goes on working for the sinks that
    /// advertise unprompted; only the pickier ones stay hidden, and the log
    /// says why.
    async fn advertise_as_wfd_source(conn: &Connection) {
        let supplicant = match WpaSupplicantProxy::new(conn).await {
            Ok(proxy) => proxy,
            Err(err) => {
                tracing::debug!(%err, "wpa_supplicant is not on the bus; not advertising WFD IEs");
                return;
            }
        };

        // Reading first tells "the property does not exist" (no
        // CONFIG_WIFI_DISPLAY) apart from "the write was refused" (D-Bus
        // policy). They call for different actions from whoever is debugging.
        if let Err(err) = supplicant.wfd_ies().await {
            tracing::info!(
                %err,
                "wpa_supplicant has no WFDIEs property (built without CONFIG_WIFI_DISPLAY); \
                 receivers that only answer WFD sources will not appear"
            );
            return;
        }

        match supplicant.set_wfd_ies(WFD_SOURCE_IES).await {
            Ok(()) => tracing::debug!("advertising this machine as a Wi-Fi Display source"),
            Err(err) => tracing::info!(
                %err,
                "could not set the WFD IEs on wpa_supplicant (a D-Bus policy usually \
                 restricts this to root); pickier receivers may not appear"
            ),
        }
    }

    /// Starts (or renews) the scan for Wi-Fi Direct peers.
    ///
    /// The `timeout` is explicit: without it NetworkManager uses 30 s and then
    /// **stops scanning silently**, leaving the peer list frozen.
    pub async fn start_find(&self) -> Result<()> {
        let timeout = FIND_TIMEOUT.as_secs().clamp(1, 600) as i32;
        let mut options: HashMap<&str, Value<'_>> = HashMap::new();
        options.insert("timeout", Value::from(timeout));
        self.p2p
            .start_find(options)
            .await
            .map_err(|e| classify(e, "StartFind"))
    }

    /// Stops the scan.
    pub async fn stop_find(&self) -> Result<()> {
        self.p2p
            .stop_find()
            .await
            .map_err(|e| classify(e, "StopFind"))
    }

    /// Keeps the scan alive indefinitely, renewing it before it expires.
    ///
    /// Runs until cancelled (the future being dropped). It is the piece that
    /// was missing for a receiver switched on after the app to appear in the
    /// list.
    ///
    /// While a stream is running, scanning **stops** — see [`nd_core::radio`].
    /// Searching and streaming compete for the same antenna, and the scan's
    /// channel hopping turns into choppy audio and delay on the other end.
    pub async fn keep_finding(&self) {
        let mut ticker = tokio::time::interval(FIND_RENEW_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            if nd_core::radio::is_quiet() {
                if let Err(err) = self.stop_find().await {
                    tracing::debug!(%err, "failed to pause P2P scanning");
                }
                tracing::info!("P2P scanning paused while streaming");
                nd_core::radio::until_free().await;
                tracing::info!("P2P scanning resumed");
            } else {
                // Do not wait for the next tick to pause: a stream may start
                // right after a renewal, which would leave 20 s of scanning on
                // top of it.
                tokio::select! {
                    _ = ticker.tick() => {}
                    _ = nd_core::radio::until_quiet() => continue,
                }
            }

            if let Err(err) = self.start_find().await {
                tracing::warn!(%err, "failed to renew the P2P scan");
            } else {
                tracing::debug!("P2P scan renewed");
            }
        }
    }

    async fn read_peer(&self, path: &OwnedObjectPath) -> Result<P2pPeer> {
        // No cache: these are ephemeral objects; zbus's cache would issue a
        // `GetAll` plus a PropertiesChanged match rule per peer.
        let peer = WifiP2pPeerProxy::builder(&self.conn)
            .path(path.clone())
            .map_err(err)?
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(err)?;

        let name = peer.name().await.unwrap_or_default();
        let hw_address = peer.hw_address().await.unwrap_or_default();
        let wfd_ies = peer.wfd_ies().await.unwrap_or_default();

        Ok(P2pPeer {
            path: path.to_string(),
            name,
            hw_address,
            is_wfd: !wfd_ies.is_empty(),
        })
    }

    /// Reads the current list of discovered peers.
    ///
    /// Peers that vanish mid-read are skipped rather than aborting the whole
    /// scan.
    pub async fn peers(&self) -> Result<Vec<P2pPeer>> {
        let paths = self
            .p2p
            .peers()
            .await
            .map_err(|e| classify(e, "propriedade Peers"))?;

        let mut peers = Vec::with_capacity(paths.len());
        for path in &paths {
            match self.read_peer(path).await {
                Ok(peer) => peers.push(peer),
                Err(err) => tracing::debug!(%path, %err, "peer sumiu durante a leitura"),
            }
        }
        Ok(peers)
    }

    /// A stream of peer-list changes, driven by NetworkManager's signals (no
    /// polling).
    ///
    /// It emits the already known peers first, then the changes. The stream
    /// lives for as long as it is consumed.
    pub async fn peer_events(&self) -> Result<impl Stream<Item = PeerEvent> + '_> {
        let added = self.p2p.receive_peer_added().await.map_err(err)?;
        let removed = self.p2p.receive_peer_removed().await.map_err(err)?;

        // Initial snapshot: peers discovered before we subscribed to signals.
        let initial = futures::stream::iter(self.peers().await?).map(PeerEvent::Added);

        let added = added.filter_map(move |signal| async move {
            let args = signal.args().ok()?;
            let path = args.peer;
            self.read_peer(&path).await.ok().map(PeerEvent::Added)
        });

        let removed = removed.filter_map(|signal| async move {
            let args = signal.args().ok()?;
            Some(PeerEvent::Removed {
                path: args.peer.to_string(),
            })
        });

        Ok(initial.chain(futures::stream::select(added, removed)))
    }

    /// Forms a Wi-Fi Direct group with the peer (a Miracast sink) by
    /// activating a `wifi-p2p` connection with us in the WFD **source** role.
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

        let peer = ObjectPath::try_from(peer_path).map_err(err)?;

        let (_conn_path, active, _result) = self
            .nm
            .add_and_activate_connection2(settings, &self.path.as_ref(), &peer, options)
            .await
            .map_err(|e| classify(e, "AddAndActivateConnection2"))?;

        Ok(active)
    }

    /// Desfaz o grupo P2P.
    pub async fn disconnect(&self, active: &OwnedObjectPath) -> Result<()> {
        match self.nm.deactivate_connection(&active.as_ref()).await {
            Ok(()) => Ok(()),
            // It already went down on its own: not an error.
            Err(zbus::Error::MethodError(..)) => Ok(()),
            Err(e) => Err(err(e)),
        }
    }

    async fn active_proxy(&self, active: &OwnedObjectPath) -> Result<ActiveConnectionProxy<'_>> {
        ActiveConnectionProxy::builder(&self.conn)
            .path(active.clone())
            .map_err(err)?
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(err)
    }

    /// Reads an active connection's state.
    ///
    /// If the object vanished (the connection dropped — common on the Realtek
    /// driver), it returns `Deactivated`. **Other** D-Bus errors are
    /// propagated: masking everything as `Deactivated` made it impossible to
    /// tell "the group dropped" from "the bus is in trouble".
    pub async fn active_state(&self, active: &OwnedObjectPath) -> Result<ActiveState> {
        let proxy = match self.active_proxy(active).await {
            Ok(p) => p,
            Err(_) => return Ok(ActiveState::Deactivated),
        };
        match proxy.state().await {
            Ok(state) => Ok(ActiveState::from(state)),
            // The object was removed: the connection really did end.
            Err(e) if is_gone(&e) => Ok(ActiveState::Deactivated),
            Err(e) => Err(err(e)),
        }
    }

    /// The local IPv4 addresses assigned to an active connection (our IP on
    /// the P2P link — where the WFD RTSP server will listen).
    pub async fn addresses(&self, active: &OwnedObjectPath) -> Result<Vec<IpAddr>> {
        let ac = self.active_proxy(active).await?;
        // The connection can vanish between reading the state and this call
        // (the Realtek driver drops the group as soon as it forms). That is
        // not an error: it means "there is no address yet", and the caller
        // should keep trying rather than aborting the whole retry.
        let ip4 = match ac.ip4_config().await {
            Ok(path) => path,
            Err(e) if is_gone(&e) => return Ok(Vec::new()),
            Err(e) => return Err(err(e)),
        };
        if ip4.as_str() == "/" {
            return Ok(Vec::new());
        }

        let ip4_proxy = Ip4ConfigProxy::builder(&self.conn)
            .path(ip4)
            .map_err(err)?
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(err)?;
        let data = match ip4_proxy.address_data().await {
            Ok(data) => data,
            Err(e) if is_gone(&e) => return Ok(Vec::new()),
            Err(e) => return Err(err(e)),
        };

        let mut addresses = Vec::new();
        for entry in &data {
            if let Some(value) = entry.get("address") {
                if let Ok(text) = String::try_from(value.clone()) {
                    match text.parse::<IpAddr>() {
                        Ok(ip) => addresses.push(ip),
                        Err(e) => tracing::warn!(%text, %e, "invalid address from NM"),
                    }
                }
            }
        }
        Ok(addresses)
    }

    /// The P2P group's network interface name (e.g. `p2p-wlan0-0`).
    ///
    /// Needed to ask firewalld to open the port **only** on that link, rather
    /// than on the whole machine.
    pub async fn interface(&self, active: &OwnedObjectPath) -> Result<Option<String>> {
        let ac = self.active_proxy(active).await?;
        let devices = match ac.devices().await {
            Ok(devices) => devices,
            Err(e) if is_gone(&e) => return Ok(None),
            Err(e) => return Err(err(e)),
        };
        for dev in devices {
            let device = DeviceProxy::builder(&self.conn)
                .path(dev)
                .map_err(err)?
                .cache_properties(CacheProperties::No)
                .build()
                .await
                .map_err(err)?;
            // `IpInterface` is the link's real name; `Interface` serves as a
            // fallback while the device is still coming up.
            let mut name = device.ip_interface().await.ok().flatten_empty();
            if name.is_none() {
                name = device.interface().await.ok().flatten_empty();
            }
            if let Some(name) = name {
                return Ok(Some(name));
            }
        }
        Ok(None)
    }

    /// How long to let NetworkManager finish removing an activation.
    ///
    /// Measured against the failure it prevents: the object is gone from the
    /// bus within a second, and a new activation started before that inherits
    /// the removal.
    const TEARDOWN_SETTLE: Duration = Duration::from_secs(2);

    /// Forms the group and waits until there is a usable local IP.
    ///
    /// It gathers in one place the retry that used to be copy-pasted across
    /// the examples: the Realtek driver frequently drops the group right after
    /// forming it.
    pub async fn connect_and_wait(
        &self,
        peer_path: &str,
        attempts: u32,
        per_attempt: Duration,
    ) -> Result<(OwnedObjectPath, IpAddr)> {
        let mut last = NdError::Network("could not form the P2P group".into());

        for attempt in 1..=attempts.max(1) {
            tracing::info!(attempt, "forming the Wi-Fi Direct group");
            let active = match self.connect(peer_path).await {
                Ok(a) => a,
                Err(e) => {
                    // The reason matters: "the receiver did not answer" and
                    // "the card refused" call for opposite actions from
                    // whoever is standing in front of the TV. Storing it
                    // without logging left the log with nothing but the
                    // attempt number.
                    tracing::warn!(attempt, err = %e, "could not start the group");
                    last = e;
                    continue;
                }
            };

            let deadline = tokio::time::Instant::now() + per_attempt;
            loop {
                if tokio::time::Instant::now() >= deadline {
                    last = NdError::Network("the Wi-Fi Direct group was not ready in time".into());
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;

                // A vanished activation is **this attempt failing**, not a
                // fatal error. NetworkManager removes the object as soon as it
                // tears an activation down, and asking about it then answers
                // `UnknownMethod`. Propagating that with `?` abandoned the
                // remaining attempts over the very condition they exist for.
                let state = match self.active_state(&active).await {
                    Ok(state) => state,
                    Err(err) => {
                        last = NdError::Network(format!(
                            "the Wi-Fi Direct group disappeared while it was being formed: {err}"
                        ));
                        break;
                    }
                };

                match state {
                    ActiveState::Activated => {
                        let addresses = match self.addresses(&active).await {
                            Ok(addresses) => addresses,
                            Err(err) => {
                                last = NdError::Network(format!(
                                    "the Wi-Fi Direct group disappeared before it had an \
                                     address: {err}"
                                ));
                                break;
                            }
                        };
                        if let Some(ip) = addresses.into_iter().next() {
                            tracing::info!(%ip, "grupo P2P formado");
                            return Ok((active, ip));
                        }
                        // Activated but no DHCP yet: keep waiting.
                    }
                    ActiveState::Deactivated => {
                        last = NdError::Network(
                            "the Wi-Fi Direct group dropped right after being formed \
                             (a known instability on Realtek cards)"
                                .into(),
                        );
                        break;
                    }
                    _ => {}
                }
            }

            tracing::warn!(attempt, err = %last, "the Wi-Fi Direct group attempt failed");
            let _ = self.disconnect(&active).await;
            // Let the teardown finish. Asking for a new group while the old
            // activation is still being removed is how an attempt ends up
            // watching an object that is on its way out.
            tokio::time::sleep(Self::TEARDOWN_SETTLE).await;
        }

        Err(last)
    }
}

/// Pequeno auxiliar: `Ok("")` vira `None`.
trait FlattenEmpty {
    fn flatten_empty(self) -> Option<String>;
}

impl FlattenEmpty for Option<String> {
    fn flatten_empty(self) -> Option<String> {
        self.filter(|s| !s.is_empty())
    }
}

impl FlattenEmpty for String {
    fn flatten_empty(self) -> Option<String> {
        if self.is_empty() {
            None
        } else {
            Some(self)
        }
    }
}

/// The current user's name.
///
/// `$USER` does not exist under systemd/services; we fall back to `getpwuid`
/// through `/etc/passwd` only when necessary. If nothing works, the
/// connection's permissions are omitted (NM accepts that).
fn current_username() -> Option<String> {
    if let Ok(user) = std::env::var("USER") {
        if !user.is_empty() {
            return Some(user);
        }
    }
    if let Ok(user) = std::env::var("LOGNAME") {
        if !user.is_empty() {
            return Some(user);
        }
    }
    // A last resort with no extra dependency: resolve the uid in /etc/passwd.
    let uid = unsafe { libc_getuid() };
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
unsafe fn libc_getuid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_state_mapping() {
        assert_eq!(ActiveState::from(0), ActiveState::Unknown);
        assert_eq!(ActiveState::from(1), ActiveState::Activating);
        assert_eq!(ActiveState::from(2), ActiveState::Activated);
        assert_eq!(ActiveState::from(3), ActiveState::Deactivating);
        assert_eq!(ActiveState::from(4), ActiveState::Deactivated);
        assert_eq!(ActiveState::from(99), ActiveState::Unknown);
    }

    #[test]
    fn wfd_source_ies_announce_port_7236() {
        // Subelement 0, len 6; bytes 5-6 = the RTSP port, big-endian.
        assert_eq!(WFD_SOURCE_IES[0], 0x00, "the Device Information subelement");
        assert_eq!(
            u16::from_be_bytes([WFD_SOURCE_IES[1], WFD_SOURCE_IES[2]]),
            6,
            "the declared length"
        );
        let port = u16::from_be_bytes([WFD_SOURCE_IES[5], WFD_SOURCE_IES[6]]);
        assert_eq!(port, 7236, "the advertised RTSP port");
        // Bits 0-1 of the bitmap: 00 = WFD source.
        let bitmap = u16::from_be_bytes([WFD_SOURCE_IES[3], WFD_SOURCE_IES[4]]);
        assert_eq!(bitmap & 0b11, 0, "the device type must be source");
    }

    #[test]
    fn the_same_ies_are_used_for_searching_and_for_connecting() {
        // These bytes go out in two very different places: the wpa_supplicant
        // property during discovery, and the `wfd-ies` of the NetworkManager
        // connection when the group is formed. Announcing one thing while
        // searching and another while connecting is how a sink answers the
        // probe and then refuses the session.
        //
        // The byte sequence is the one the reference implementation documents;
        // a receiver that only answers WFD sources compares it field by field.
        assert_eq!(
            WFD_SOURCE_IES,
            &[0x00, 0x00, 0x06, 0x00, 0x90, 0x1c, 0x44, 0x00, 0xc8],
            "the IEs must stay byte-identical to the Wi-Fi Display spec"
        );

        // The declared length has to match what actually follows it, or the
        // sink discards the whole element without a word.
        let declared = u16::from_be_bytes([WFD_SOURCE_IES[1], WFD_SOURCE_IES[2]]) as usize;
        assert_eq!(
            declared,
            WFD_SOURCE_IES.len() - 3,
            "the declared length must match the subelement's body"
        );
    }

    #[test]
    fn find_renew_precedes_expiry() {
        // If the renewal does not come before expiry, a window opens in which
        // the radio is not scanning and new sinks stay invisible.
        assert!(FIND_RENEW_INTERVAL < FIND_TIMEOUT);
    }

    #[test]
    fn username_resolution_has_a_fallback() {
        // It must not panic even without $USER.
        let _ = current_username();
    }
}

/// Which peers took an address on this link, according to the kernel.
///
/// Used to tell two very different failures apart when the receiver never
/// opens the RTSP connection:
///
/// - **nothing here**: the receiver completed the pairing and then never
///   joined the group. Re-forming the group is worth a try; telling the person
///   to check their mirroring menu is not, because the receiver never got far
///   enough for that to matter.
/// - **something here**: it joined, took an address, and then went quiet —
///   which usually means the mirroring screen was closed on the device.
///
/// The limit is worth stating: an entry appears once there has been traffic
/// with that peer, so an empty result is evidence, not proof.
pub fn peers_on_link(interface: &str) -> Vec<String> {
    match std::fs::read_to_string("/proc/net/arp") {
        Ok(table) => parse_arp(&table, interface),
        Err(err) => {
            tracing::debug!(%err, "could not read the neighbour table");
            Vec::new()
        }
    }
}

/// Parses `/proc/net/arp`, keeping the peers on one interface.
///
/// Columns: address, HW type, flags, HW address, mask, device. A flags value
/// of `0x0` is an *incomplete* entry — an address the kernel asked about and
/// never got an answer for — so it is not a peer that is there.
fn parse_arp(table: &str, interface: &str) -> Vec<String> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let address = fields.next()?;
            let _hw_type = fields.next()?;
            let flags = fields.next()?;
            let _hw_address = fields.next()?;
            let _mask = fields.next()?;
            let device = fields.next()?;
            (device == interface && flags != "0x0").then(|| address.to_string())
        })
        .collect()
}

#[cfg(test)]
mod arp_tests {
    use super::parse_arp;

    const TABLE: &str =
        "IP address       HW type     Flags       HW address            Mask     Device\n\
192.168.68.115   0x1         0x2         dc:a3:a2:08:73:2f     *        wlp2s0\n\
10.42.0.215      0x1         0x2         56:44:a3:49:c2:ae     *        p2p-wlp2s0-0\n\
10.42.0.99       0x1         0x0         00:00:00:00:00:00     *        p2p-wlp2s0-0\n";

    #[test]
    fn only_peers_on_the_asked_for_link_are_returned() {
        // The machine is on its home network at the same time; a receiver
        // there is not a peer of the Wi-Fi Direct group.
        assert_eq!(super::parse_arp(TABLE, "p2p-wlp2s0-0"), vec!["10.42.0.215"]);
        assert_eq!(parse_arp(TABLE, "wlp2s0"), vec!["192.168.68.115"]);
    }

    #[test]
    fn an_unanswered_entry_is_not_a_peer() {
        // Flags `0x0` is an address the kernel asked about and never heard
        // back from. Counting it would report a receiver that joined when
        // nothing did, and turn a useful retry into a misleading message.
        assert!(!parse_arp(TABLE, "p2p-wlp2s0-0").contains(&"10.42.0.99".to_string()));
    }

    #[test]
    fn a_link_with_nobody_on_it_is_empty() {
        assert!(parse_arp(TABLE, "p2p-wlp2s0-9").is_empty());
        assert!(parse_arp("", "p2p-wlp2s0-0").is_empty());
        // A truncated line must not panic.
        assert!(parse_arp("IP address\n10.42.0.1 0x1\n", "p2p-wlp2s0-0").is_empty());
    }
}
