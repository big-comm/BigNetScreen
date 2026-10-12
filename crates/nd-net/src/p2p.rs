//! Wi-Fi Direct (P2P) discovery through NetworkManager, the radio half of
//! Miracast; `nd-wfd` drives it and speaks RTSP over the group it forms.
//!
//! NetworkManager exposes a `p2p-dev-wlpXsY` device with the
//! `org.freedesktop.NetworkManager.Device.WifiP2P` interface (`StartFind`,
//! `StopFind`, the `Peers` property and the `PeerAdded`/`PeerRemoved`
//! signals). A peer with non-empty `WfdIEs` is a Miracast sink: a TV or
//! projector in "Screen Mirroring" mode.
//!
//! - `StartFind` expires after its timeout and NetworkManager then stops
//!   scanning without a word, so [`P2pDevice::keep_finding`] renews it for a
//!   bounded window.
//! - The peer list follows the signals; re-reading `Peers` would cost one
//!   proxy, a `GetAll` and a match rule per peer on every read.
//! - [`group`] forms the group with a chosen sink; [`neighbours`] tells
//!   whether the sink joined it.
//!
//! Unavailable under Flatpak, which has no system bus.

mod group;
mod neighbours;

use std::collections::HashMap;
use std::time::Duration;

use futures::stream::{Stream, StreamExt};
use zbus::Connection;
use zbus::proxy::CacheProperties;
use zbus::zvariant::{OwnedObjectPath, Value};

use nd_core::{NdError, Result};

pub use crate::networkmanager::LanAddresses;
use crate::networkmanager::{DeviceProxy, NetworkManagerProxy, WifiP2pPeerProxy, WifiP2pProxy};
pub use group::ActiveState;
pub use neighbours::peers_on_link;

/// The WFD Information Elements advertising this PC as a Wi-Fi Display
/// source with its RTSP server on port 7236.
///
/// Subelement 0 (Device Information), length 6:
/// `0x0090` = device type source + WSD; `0x1c44` = 7236 (the RTSP port);
/// `0x00c8` = 200 (the declared maximum throughput, in Mbps).
const WFD_SOURCE_IES: &[u8] = &[0x00, 0x00, 0x06, 0x00, 0x90, 0x1c, 0x44, 0x00, 0xc8];

/// The duration of each scan requested from NetworkManager.
///
/// NM accepts 1–600 s. A short, renewed window is used (see
/// [`P2pDevice::keep_finding`]) rather than one long one: that way the radio
/// returns to the normal network's channel periodically and the main Wi-Fi
/// connection suffers less.
const FIND_TIMEOUT: Duration = Duration::from_secs(30);

/// The scan renewal interval (shorter than [`FIND_TIMEOUT`], so there is no
/// dead window between one scan and the next).
const FIND_RENEW_INTERVAL: Duration = Duration::from_secs(20);

/// How long [`P2pDevice::keep_finding`] keeps renewing the scan.
///
/// Scanning hops channels and slows the user's own Wi-Fi, so it runs for a
/// window after discovery starts; a rescan starts a new window.
const FIND_WINDOW: Duration = Duration::from_secs(120);

/// How often a forming group's state is read.
const GROUP_STATE_POLL: Duration = Duration::from_millis(500);

/// `NM_DEVICE_TYPE_WIFI`: an ordinary Wi-Fi station.
const DEVICE_TYPE_WIFI: u32 = 2;
/// `NM_DEVICE_TYPE_WIFI_P2P`.
const DEVICE_TYPE_WIFI_P2P: u32 = 30;

/// wpa_supplicant's global interface.
///
/// NetworkManager drives discovery, but the WFD Information Elements live one
/// layer below, on the supplicant itself, and NM exposes no way to set them
/// for the search, only for a connection.
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

/// A discovered Wi-Fi Direct peer.
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

/// Why there is no Wi-Fi P2P device, in terms of what the person can do.
fn missing_p2p_device(has_wifi: bool) -> NdError {
    // Without any Wi-Fi the usual advice (card, driver, backend) is wrong,
    // so that case gets its own message. With Wi-Fi but no P2P device the
    // three causes cannot be told apart from here and call for different
    // actions, so all three are named.
    if !has_wifi {
        return NdError::Unsupported(
            "no Wi-Fi adapter on this computer. Miracast is a direct radio link \
             between the two devices, so it needs one — casting over the network \
             (Chromecast) works without it"
                .into(),
        );
    }

    NdError::Unsupported(
        "no Wi-Fi P2P device in NetworkManager. Either this Wi-Fi card does not \
         support Wi-Fi Direct, or the driver does not expose it, or NetworkManager \
         is running with the `iwd` backend, which has no P2P support — `nmcli device` \
         should list a `p2p-dev-*` device alongside the Wi-Fi one"
            .into(),
    )
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

/// Tells "the service does not even exist here" from "the call failed", so
/// the interface can say whether support is missing (Flatpak, a card without
/// P2P) or a call failed and may succeed later.
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
             (expected under Flatpak)"
        ))
    } else {
        NdError::Network(format!("{context}: {e}"))
    }
}

/// A handle to NetworkManager's Wi-Fi P2P device, holding its proxies for
/// the whole discovery or session.
pub struct P2pDevice {
    conn: Connection,
    path: OwnedObjectPath,
    p2p: WifiP2pProxy<'static>,
    nm: NetworkManagerProxy<'static>,
}

impl P2pDevice {
    /// Opens the system bus and finds the Wi-Fi P2P device.
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

        // Telling "no wireless hardware" from "wireless that cannot do Wi-Fi
        // Direct" needs to know whether an ordinary Wi-Fi device exists.
        let mut has_wifi = false;

        for dev in devices {
            let device = DeviceProxy::builder(&conn)
                .path(dev.clone())
                .map_err(err)?
                .cache_properties(CacheProperties::No)
                .build()
                .await
                .map_err(err)?;
            let kind = device.device_type().await.unwrap_or(0);
            if kind == DEVICE_TYPE_WIFI {
                has_wifi = true;
            }
            if kind == DEVICE_TYPE_WIFI_P2P {
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

        Err(missing_p2p_device(has_wifi))
    }

    /// Advertises this machine as a Wi-Fi Display source during discovery.
    ///
    /// A sink decides whether to answer with its own WFD Information Elements
    /// based on what the probe request carries. Several receivers, Amazon Fire
    /// TV among them, stay silent for a peer that does not identify itself as
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

        // A failed read can mean either missing support or restricted D-Bus access.
        if let Err(err) = supplicant.wfd_ies().await {
            tracing::info!(
                %err,
                "could not read wpa_supplicant WFDIEs; support could not be checked \
                 (D-Bus policy may restrict access); pickier receivers may not appear"
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
    /// stops scanning silently, leaving the peer list frozen.
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

    /// Keeps the scan alive for [`FIND_WINDOW`], renewing it before it
    /// expires, so a receiver switched on shortly after still appears.
    ///
    /// While a stream is running, scanning stops (see [`nd_core::radio`]).
    /// Searching and streaming compete for the same antenna, and the scan's
    /// channel hopping turns into choppy audio and delay on the other end.
    pub async fn keep_finding(&self) {
        let deadline = tokio::time::Instant::now() + FIND_WINDOW;
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
            if tokio::time::Instant::now() >= deadline {
                tracing::debug!("P2P scan window over");
                return;
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
            .map_err(|e| classify(e, "Peers property"))?;

        let mut peers = Vec::with_capacity(paths.len());
        for path in &paths {
            match self.read_peer(path).await {
                Ok(peer) => peers.push(peer),
                Err(err) => tracing::debug!(%path, %err, "the peer vanished while being read"),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wfd_ies_use_firewall_port() {
        // Subelement 0 (Device Information); bytes 1-2 the length of what
        // follows, or the sink discards the element without a word.
        assert_eq!(WFD_SOURCE_IES[0], 0x00);
        let declared = u16::from_be_bytes([WFD_SOURCE_IES[1], WFD_SOURCE_IES[2]]);
        assert_eq!(usize::from(declared), WFD_SOURCE_IES.len() - 3);
        // Bits 0-1 of the device bitmap: 00 = WFD source.
        let bitmap = u16::from_be_bytes([WFD_SOURCE_IES[3], WFD_SOURCE_IES[4]]);
        assert_eq!(bitmap & 0b11, 0);
        let port = u16::from_be_bytes([WFD_SOURCE_IES[5], WFD_SOURCE_IES[6]]);
        assert_eq!(port, crate::firewall::RTSP_PORT);
    }

    #[test]
    fn find_renew_precedes_expiry() {
        // If the renewal does not come before expiry, a window opens in which
        // the radio is not scanning and new sinks stay invisible.
        assert!(FIND_RENEW_INTERVAL < FIND_TIMEOUT);
    }
}
