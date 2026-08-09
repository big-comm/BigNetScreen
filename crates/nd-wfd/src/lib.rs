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

/// Provider de descoberta WFD-P2P (Miracast) via Wi-Fi Direct.
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
                        tracing::warn!(%err, "descoberta P2P falhou; tentando de novo");
                        if tx
                            .unbounded_send(DiscoveryEvent::ProviderUnavailable {
                                provider: "wfd-p2p",
                                reason: format!("{err} — tentando reconectar"),
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
                "o NetworkManager parou de enviar eventos de peer".into(),
            ));
        };

        let message = match event {
            PeerEvent::Added(peer) => {
                // Only peers advertising Wi-Fi Display are Miracast receivers.
                if !peer.is_wfd || !known.insert(peer.path.clone()) {
                    continue;
                }
                tracing::info!(name = %peer.name, mac = %peer.hw_address, "sink Miracast encontrado");
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
    const ACCEPT_TIMEOUT: Duration = Duration::from_secs(40);

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

        status.set(SinkState::Connecting);
        let (active, our_ip) = device
            .connect_and_wait(sink.peer_path(), CONNECT_ATTEMPTS, CONNECT_TIMEOUT)
            .await?;

        // Radio silence **only once the group is formed**: forming it depends
        // on the radio being in a searching state, and silencing it earlier
        // would be pulling the bridge out from under us. From here on, going
        // on looking for peers would only disturb the link we just established
        // (`nd_core::radio`).
        let _radio = nd_core::radio::quiet();

        // Always tear the group down on exit, errors and cancellation included.
        let result = run_session(&device, &active, our_ip, source, status, cancel).await;
        let _ = device.disconnect(&active).await;
        result
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

        let result = serve(our_ip, source, status, cancel).await;

        firewall::release(lease).await;
        result
    }

    async fn serve(
        our_ip: IpAddr,
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
        tracing::info!(%our_ip, "servidor RTSP/WFD no ar; aguardando o sink");

        let accepted = tokio::select! {
            result = tokio::time::timeout(ACCEPT_TIMEOUT, listener.accept()) => result,
            _ = cancel.changed() => return Ok(()),
        };
        let (stream, addr) = accepted
            .map_err(|_| {
                NdError::Protocol(
                    "the receiver did not open the RTSP connection — check that it is still \
                     in Screen Mirroring mode"
                        .into(),
                )
            })?
            .map_err(|e| NdError::Network(e.to_string()))?;

        // We only accept the peer from our own P2P link.
        if !addr.ip().is_ipv4() && !addr.ip().is_ipv6() {
            return Err(NdError::Protocol("invalid source address".into()));
        }
        tracing::info!(%addr, "sink conectou");

        status.set(SinkState::WaitStreaming);
        let driver = nd_net::detect_gpu_driver();
        let encoder = pipeline::best_encoder(driver)?;
        // The captured screen's aspect ratio guides the WFD mode choice.
        let cfg = WfdCastConfig::new(our_ip, addr.ip(), source.video_source(), encoder)
            .with_source_size(source.size_or((1920, 1080)));

        status.set(SinkState::Streaming);
        // The RTSP session runs until TEARDOWN, an error, or the user stopping it.
        let result = tokio::select! {
            result = cast_to_sink(stream, cfg) => result,
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
