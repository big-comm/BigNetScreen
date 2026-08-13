//! Wi-Fi Display (Miracast) support.
//!
//! **Discovery**: every Wi-Fi Direct peer that advertises Wi-Fi Display
//! (`WfdIEs`) becomes a [`Sink`] in the list. The scan is driven by
//! NetworkManager **signals** and is **renewed** periodically (NM's
//! `StartFind` expires after 30 s), reconnecting automatically after a
//! transient failure.
//!
//! **Streaming** ([`rtsp`]): P2P group → firewalld → RTSP server on 7236 →
//! M1–M7 negotiation → the MPEG-TS/RTP pipeline from `nd_core::pipeline`.
//!
//! Unavailable under Flatpak (it depends on NetworkManager on the system bus);
//! in that case [`Provider::discover`] returns `Err` with an explanatory
//! message that `MetaProvider` turns into a banner visible in the UI.

pub mod rtsp;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};

use nd_core::capture::CaptureSource;
use nd_core::provider::{DiscoveryEvent, Provider};
use nd_core::sink::{sanitize_name, Sink, SinkInfo, SinkKind, SinkState, SinkStatus};
use nd_core::{NdError, Result};
use nd_net::p2p::{P2pDevice, P2pPeer, PeerEvent};

/// Initial wait between attempts to reopen the device after a failure.
const RETRY_BASE: Duration = Duration::from_secs(2);
/// Ceiling of the exponential reconnection backoff.
const RETRY_MAX: Duration = Duration::from_secs(30);

/// WFD-P2P (Miracast) discovery provider over Wi-Fi Direct.
pub struct WfdP2pProvider;

#[async_trait]
impl Provider for WfdP2pProvider {
    fn id(&self) -> &'static str {
        "wfd-p2p"
    }

    fn display_name(&self) -> &'static str {
        "Miracast"
    }

    async fn discover(&self) -> Result<BoxStream<'static, DiscoveryEvent>> {
        // The first open is synchronous: if the environment has no Miracast
        // support (Flatpak, a card without Wi-Fi Direct), the error surfaces
        // right away and MetaProvider turns it into a message for the user.
        let device = P2pDevice::open().await?;
        device.start_find().await?;

        let (tx, rx) = futures::channel::mpsc::unbounded::<DiscoveryEvent>();

        tokio::spawn(async move {
            let mut device = device;
            let mut backoff = RETRY_BASE;
            let mut known: HashSet<String> = HashSet::new();

            loop {
                match run_discovery(&device, &tx, &mut known).await {
                    // The consumer went away: shut down for good.
                    Ok(()) => {
                        let _ = device.stop_find().await;
                        return;
                    }
                    Err(err) => {
                        // A fixed regression: a single D-Bus failure used to
                        // end discovery silently, leaving the user on
                        // "Searching…" forever.
                        tracing::warn!(%err, "P2P discovery failed; retrying");
                        if tx
                            .unbounded_send(DiscoveryEvent::ProviderUnavailable {
                                provider: "wfd-p2p",
                                reason: format!("{err} — reconnecting"),
                            })
                            .is_err()
                        {
                            return;
                        }

                        // Known sinks may no longer exist after reconnecting:
                        // clear the UI list.
                        for id in known.drain() {
                            if tx.unbounded_send(DiscoveryEvent::Removed(id)).is_err() {
                                return;
                            }
                        }

                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(RETRY_MAX);

                        match P2pDevice::open().await {
                            Ok(fresh) => {
                                if fresh.start_find().await.is_ok() {
                                    device = fresh;
                                    backoff = RETRY_BASE;
                                    let _ = tx.unbounded_send(DiscoveryEvent::ProviderReady {
                                        provider: "wfd-p2p",
                                    });
                                }
                            }
                            Err(err) => tracing::debug!(%err, "P2P device still unavailable"),
                        }
                    }
                }
            }
        });

        Ok(rx.boxed())
    }
}

/// The discovery loop: NM signals plus scan renewal.
///
/// Returns `Ok(())` when the stream's consumer went away, and `Err` when
/// communication with NetworkManager failed (the caller retries).
async fn run_discovery(
    device: &P2pDevice,
    tx: &futures::channel::mpsc::UnboundedSender<DiscoveryEvent>,
    known: &mut HashSet<String>,
) -> Result<()> {
    use futures::future::FutureExt;

    // Peers already reported as "seen, but not offering themselves". Kept so
    // the log carries one line per device rather than one per announcement.
    let mut seen_without_wfd: HashSet<String> = HashSet::new();

    let mut events = Box::pin(device.peer_events().await?);
    // NetworkManager's scan expires; renewing it is what makes a receiver
    // switched on after the app appear in the list.
    let mut renew = Box::pin(device.keep_finding().fuse());

    loop {
        let event = futures::select! {
            event = events.next().fuse() => event,
            () = renew => unreachable!("keep_finding runs forever"),
        };

        let Some(event) = event else {
            return Err(NdError::Network(
                "NetworkManager stopped sending peer events".into(),
            ));
        };

        let message = match event {
            PeerEvent::Added(peer) => {
                // Only peers advertising Wi-Fi Display are Miracast receivers.
                if !peer.is_wfd {
                    // Logged rather than dropped in silence. A receiver that is
                    // switched on but not in mirroring mode shows up here, and
                    // this line is the only way to tell "the TV was never seen"
                    // from "the TV was seen and is not offering itself". Amazon
                    // Fire TV only advertises WFD while its Display Mirroring
                    // screen is open, and that had people concluding the app
                    // could not see their device at all.
                    if seen_without_wfd.insert(peer.path.clone()) {
                        tracing::info!(
                            name = %peer.name,
                            mac = %peer.hw_address,
                            "Wi-Fi Direct peer found without Wi-Fi Display: it is not \
                             offering itself as a receiver. On a TV or projector, open the \
                             screen mirroring mode (on Fire TV: Settings › Display & Sounds \
                             › Display Mirroring)"
                        );
                    }
                    continue;
                }
                if !known.insert(peer.path.clone()) {
                    continue;
                }
                tracing::info!(name = %peer.name, mac = %peer.hw_address, "Miracast sink found");
                DiscoveryEvent::Added(Arc::new(WfdSink::new(&peer)) as Arc<dyn Sink>)
            }
            PeerEvent::Removed { path } => {
                if !known.remove(&path) {
                    continue;
                }
                DiscoveryEvent::Removed(path)
            }
        };

        if tx.unbounded_send(message).is_err() {
            return Ok(());
        }
    }
}

/// Sink Miracast descoberto.
pub struct WfdSink {
    info: SinkInfo,
    peer_path: String,
    status: SinkStatus,
    /// Signals that a running session should end.
    ///
    /// Without this, `stop_stream` only cleared the visual state: the Wi-Fi
    /// Direct group, the RTSP server and the pipeline stayed up, and the
    /// interface's stop button stopped nothing.
    cancel: std::sync::Mutex<Option<tokio::sync::watch::Sender<bool>>>,
}

impl WfdSink {
    fn new(peer: &P2pPeer) -> Self {
        // The name comes off the network: sanitise it before displaying or
        // passing it on.
        let display_name = if peer.name.trim().is_empty() {
            peer.hw_address.clone()
        } else {
            sanitize_name(&peer.name)
        };
        Self {
            info: SinkInfo {
                id: peer.path.clone(),
                display_name,
                kind: SinkKind::WfdP2p,
                address: Some(peer.hw_address.clone()),
            },
            peer_path: peer.path.clone(),
            status: SinkStatus::new(),
            cancel: std::sync::Mutex::new(None),
        }
    }

    /// The peer's D-Bus path (for forming the P2P group).
    pub fn peer_path(&self) -> &str {
        &self.peer_path
    }
}

#[async_trait]
impl Sink for WfdSink {
    fn info(&self) -> SinkInfo {
        self.info.clone()
    }

    fn state(&self) -> SinkState {
        self.status.state()
    }

    fn error_message(&self) -> Option<String> {
        self.status.message()
    }

    fn link(&self) -> Option<nd_core::sink::StreamLink> {
        self.status.link()
    }

    async fn start_stream(&self, source: CaptureSource) -> Result<()> {
        self.status.set(SinkState::Connecting);

        let (tx, rx) = tokio::sync::watch::channel(false);
        *self
            .cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);

        let result = cast::run(self, source, &self.status, rx).await;

        *self
            .cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;

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
        // Tells the session; it tears the P2P group down, closes the firewall
        // and stops the pipeline on its own, and `start_stream` returns.
        if let Some(tx) = self
            .cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = tx.send(true);
        }
        self.status.reset();
        Ok(())
    }
}

/// Orchestration of a complete Miracast session.
pub mod cast {
    use super::*;
    use std::net::IpAddr;
    use std::time::Duration;

    use nd_core::pipeline;
    use nd_net::firewall;
    use tokio::net::TcpListener;

    use crate::rtsp::{cast_to_sink, WfdCastConfig, RTSP_PORT};

    /// Tentativas de formar o grupo P2P (drivers Realtek derrubam o primeiro).
    const CONNECT_ATTEMPTS: u32 = 4;
    /// Time per attempt until the group has an IP.
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
    /// How long to wait for the sink to open the RTSP connection.
    ///
    /// Measured, not guessed: in the sessions that worked, the receiver
    /// associated one second after the group started and opened RTSP within
    /// about ten. Waiting forty seconds only made a failure take forty seconds
    /// — and the failure that does happen is not cured by waiting, it is cured
    /// by forming the group again.
    const ACCEPT_TIMEOUT: Duration = Duration::from_secs(20);

    /// How long to leave the radio alone between session attempts.
    const RETRY_SETTLE: Duration = Duration::from_secs(3);

    /// How many times to form the group and wait for the receiver.
    ///
    /// A receiver that completes the pairing and then never joins is the
    /// common Miracast failure, and it clears on a fresh group. Doing that
    /// automatically is the difference between "it did not work" and "it took
    /// a moment".
    const SESSION_ATTEMPTS: u32 = 3;

    /// Runs the whole session: P2P group → firewall → RTSP → streaming.
    ///
    /// It gathers what previously only existed copy-pasted inside the
    /// examples.
    pub async fn run(
        sink: &WfdSink,
        source: CaptureSource,
        status: &SinkStatus,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let device = P2pDevice::open().await?;
        let mut source = source;
        let mut last: Option<NdError> = None;

        for attempt in 1..=SESSION_ATTEMPTS {
            status.set(SinkState::Connecting);
            let (active, our_ip) = device
                .connect_and_wait(sink.peer_path(), CONNECT_ATTEMPTS, CONNECT_TIMEOUT)
                .await?;

            // Radio silence **only once the group is formed**: forming it
            // depends on the radio being in a searching state, and silencing it
            // earlier would be pulling the bridge out from under us. From here
            // on, going on looking for peers would only disturb the link we
            // just established (`nd_core::radio`).
            let quiet = nd_core::radio::quiet();

            let outcome =
                run_session(&device, &active, our_ip, source, status, cancel.clone()).await;
            // Always tear the group down, errors and cancellation included.
            // The receiver has to see it go away before it will accept a new
            // one, which is exactly what the next attempt depends on.
            let _ = device.disconnect(&active).await;
            drop(quiet);

            match outcome {
                Ok(()) => return Ok(()),
                Err(NdError::SinkNeverConnected { capture, reason })
                    if attempt < SESSION_ATTEMPTS =>
                {
                    tracing::warn!(attempt, %reason, "the receiver never connected; trying again");
                    status.set(SinkState::Connecting);
                    // Two waits, not one delay for both: NetworkManager needs
                    // a moment to finish removing the activation, and the
                    // receiver needs to notice the group has gone before it
                    // will accept a new one.
                    tokio::time::sleep(RETRY_SETTLE).await;
                    // The capture is handed back so the next attempt can use
                    // it: asking the portal again would put a permission
                    // dialog in front of someone who already agreed.
                    source = *capture;
                    last = Some(NdError::Protocol(reason));
                }
                Err(NdError::SinkNeverConnected { reason, .. }) => {
                    return Err(NdError::Protocol(reason))
                }
                Err(err) => return Err(err),
            }
        }

        Err(last.unwrap_or_else(|| {
            NdError::Protocol("the receiver never opened the connection".into())
        }))
    }

    async fn run_session(
        device: &P2pDevice,
        active: &zbus_types::ActivePath,
        our_ip: IpAddr,
        source: CaptureSource,
        status: &SinkStatus,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        status.set(SinkState::EnsuringFirewall);
        let interface = device.interface(active).await.unwrap_or(None);
        // Without this, with firewalld active (the default on BigLinux) the
        // sink cannot reach TCP 7236 and the cast fails with no message at
        // all.
        let lease = firewall::ensure_ports_open(interface.as_deref()).await?;

        let result = serve(our_ip, interface.as_deref(), source, status, cancel).await;

        firewall::release(lease).await;
        result
    }

    async fn serve(
        our_ip: IpAddr,
        interface: Option<&str>,
        source: CaptureSource,
        status: &SinkStatus,
        mut cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        status.set(SinkState::WaitSocket);
        // Listen **only** on the P2P link's IP: this does not expose the RTSP
        // port to the rest of the network.
        let listener = TcpListener::bind((our_ip, RTSP_PORT)).await.map_err(|e| {
            NdError::Network(format!("could not listen on {our_ip}:{RTSP_PORT}: {e}"))
        })?;
        tracing::info!(%our_ip, "RTSP/WFD server listening; waiting for the sink");

        let accepted = tokio::select! {
            result = tokio::time::timeout(ACCEPT_TIMEOUT, listener.accept()) => result,
            _ = cancel.changed() => return Ok(()),
        };
        let (stream, addr) = match accepted {
            Ok(result) => result.map_err(|e| NdError::Network(e.to_string()))?,
            Err(_) => {
                // Which step failed changes the advice, so look before
                // speaking: did anything at all take an address on this link?
                let peers = interface
                    .map(nd_net::p2p::peers_on_link)
                    .unwrap_or_default();
                let reason = if peers.is_empty() {
                    tracing::warn!(
                        ?interface,
                        "nothing joined the Wi-Fi Direct link; the receiver paired and dropped"
                    );
                    "the receiver paired and then never joined — trying again with a fresh \
                     connection"
                        .to_string()
                } else {
                    tracing::warn!(?peers, "the receiver joined but never opened RTSP");
                    "the receiver joined but never started mirroring — check that Screen \
                     Mirroring is still open on it"
                        .to_string()
                };
                return Err(NdError::SinkNeverConnected {
                    capture: Box::new(source),
                    reason,
                });
            }
        };

        // We only accept the peer from our own P2P link.
        if !addr.ip().is_ipv4() && !addr.ip().is_ipv6() {
            return Err(NdError::Protocol("invalid source address".into()));
        }
        tracing::info!(%addr, "the sink connected");

        status.set(SinkState::WaitStreaming);
        let driver = nd_net::detect_gpu_driver();
        let encoder = pipeline::best_encoder(driver)?;
        // The captured screen's aspect ratio guides the WFD mode choice.
        let cfg = WfdCastConfig::new(our_ip, addr.ip(), source.video_source(), encoder)
            .with_audio(source.audio_source())
            .with_source_size(source.size_or((1920, 1080)));

        status.set(SinkState::Streaming);
        // The RTSP session runs until TEARDOWN, an error, or the user stopping it.
        let result = tokio::select! {
            result = cast_to_sink(stream, cfg, status) => result,
            _ = cancel.changed() => {
                tracing::info!("Miracast session ended at the user's request");
                Ok(())
            }
        };
        // `source` has to live this long: the PipeWire fd is used by the
        // pipeline for the whole session.
        drop(source);
        result
    }

    /// An alias for the D-Bus path type, to keep `zbus` out of the signature.
    pub mod zbus_types {
        pub use zbus::zvariant::OwnedObjectPath as ActivePath;
    }
}
