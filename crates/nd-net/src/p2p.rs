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

mod group;
mod neighbours;

use std::collections::HashMap;
use std::time::Duration;

use futures::stream::{Stream, StreamExt};
use zbus::Connection;
use zbus::proxy::CacheProperties;
use zbus::zvariant::{OwnedObjectPath, Value};

use nd_core::{NdError, Result};

pub use crate::network_manager::local_ipv4_addresses;
use crate::network_manager::{DeviceProxy, NetworkManagerProxy, WifiP2pPeerProxy, WifiP2pProxy};
pub use group::ActiveState;
pub use neighbours::peers_on_link;

/// The WFD Information Elements advertising this PC as a Wi-Fi Display
/// **source** with its RTSP server on port 7236.
///
/// Subelement 0 (Device Information), length 6:
/// `0x0090` = device type source + WSD; `0x1c44` = 7236 (the RTSP port);
/// `0x00c8` = 200 (the declared maximum throughput, in Mbps).
const WFD_SOURCE_IES: &[u8] = &[0x00, 0x00, 0x06, 0x00, 0x90, 0x1c, 0x44, 0x00, 0xc8];

/// The duration of each scan requested from NetworkManager.
///
/// NM accepts 1–600 s. A short, **renewed** window is used (see
/// [`P2pDevice::keep_finding`]) rather than one long one: that way the radio
/// returns to the normal network's channel periodically and the main Wi-Fi
/// connection suffers less.
const FIND_TIMEOUT: Duration = Duration::from_secs(30);

/// The scan renewal interval (shorter than [`FIND_TIMEOUT`], so there is no
/// dead window between one scan and the next).
const FIND_RENEW_INTERVAL: Duration = Duration::from_secs(20);

/// How often a forming group's state is read.
const GROUP_STATE_POLL: Duration = Duration::from_millis(500);

/// `NM_DEVICE_TYPE_WIFI_P2P`.
/// NetworkManager's device type for an ordinary Wi-Fi station.
const DEVICE_TYPE_WIFI: u32 = 2;
const DEVICE_TYPE_WIFI_P2P: u32 = 30;

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
             (expected under Flatpak)"
        ))
    } else {
        NdError::Network(format!("{context}: {e}"))
    }
}

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

        // Noted while walking the list: telling "no wireless hardware" apart
        // from "wireless that cannot do Wi-Fi Direct" needs to know whether an
        // ordinary Wi-Fi device exists at all.
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

        // The three causes are indistinguishable from here, and they call for
        // very different actions, so all three are named. Reporting only "the
        // card does not support it" sent people shopping for a Wi-Fi adapter
        // they did not need.
        // A machine with no wireless hardware at all is the one case where the
        // usual advice is actively wrong: there is no card to blame, no driver
        // to update and no backend to switch. Tested on a desktop wired by
        // Ethernet, where the message sent the person looking for a Wi-Fi
        // setting that does not exist.
        if !has_wifi {
            return Err(NdError::Unsupported(
                "no Wi-Fi adapter on this computer. Miracast is a direct radio link \
                 between the two devices, so it needs one — casting over the network \
                 (Chromecast) works without it"
                    .into(),
            ));
        }

        Err(NdError::Unsupported(
            "no Wi-Fi P2P device in NetworkManager. Either this Wi-Fi card does not \
             support Wi-Fi Direct, or the driver does not expose it, or NetworkManager \
             is running with the `iwd` backend, which has no P2P support — `nmcli device` \
             should list a `p2p-dev-*` device alongside the Wi-Fi one"
                .into(),
        ))
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
            .map_err(|e| classify(e, "Peers property"))?;

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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
