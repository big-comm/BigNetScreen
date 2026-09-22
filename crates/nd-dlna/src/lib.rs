//! Casting to DLNA/UPnP renderers — the televisions that predate Chromecast.
//!
//! ```text
//!   SSDP M-SEARCH ──► renderer answers with its description
//!                            │
//!   GET description ─────────┘  → friendly name + AVTransport control URL
//!                            │
//!   capture ──► pipeline ──► multisocketsink
//!                                  ▲
//!                                  │ socket handed over by the stream server
//!   SetAVTransportURI(url) ── Play ─┘  → the television opens the GET
//! ```
//!
//! The media half is not new: the transport stream, the HTTP server and the
//! socket handover are the same [`nd_core::stream_server`] and
//! [`nd_core::pipeline::ts_http_pipeline_description`] the Cast fallback uses.
//! What this crate adds is the two halves DLNA does differently — finding the
//! device ([`ssdp`]) and driving it ([`avtransport`]).
//!
//! ## What this protocol is for, and what it is not for
//!
//! **The picture arrives about a second and a half late, and nothing here can
//! make it arrive sooner.** That is not a defect to be fixed later: a DLNA
//! renderer decides its own prebuffer and exposes no way to ask for a smaller
//! one — the whole UPnP surface of a real television was read action by action
//! and there is no buffer, delay or latency control in any of its three
//! services. Cast is different only because it negotiates a `targetDelay` that
//! we set to zero.
//!
//! So this is for **watching** on a big screen, not for working on one. Its
//! value is reach: it runs over ordinary Ethernet, needs no Wi-Fi adapter, and
//! covers a large installed base of televisions that have neither Chromecast
//! nor AirPlay nor Miracast.

pub mod avtransport;
pub mod session;
pub mod ssdp;
pub mod upnp;

use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;

use nd_core::capture::CaptureSource;
use nd_core::sink::{sanitize_name, Sink, SinkInfo, SinkKind, SinkState, SinkStatus};
use nd_core::{NdError, Result};

use crate::ssdp::Announcement;
use crate::upnp::Endpoint;

pub use ssdp::DlnaProvider;

/// One discovered renderer.
pub struct DlnaSink {
    info: SinkInfo,
    /// The description URL it announced, kept so a sweep can tell a device
    /// that merely re-announced from one that actually moved.
    location: String,
    /// Where `AVTransport` is driven.
    control: Endpoint,
    address: IpAddr,
    status: SinkStatus,
    /// Cancels the running session, when there is one.
    session: Mutex<Option<tokio::sync::watch::Sender<bool>>>,
}

impl DlnaSink {
    /// Fetches a renderer's description and builds a sink from it.
    ///
    /// Fails for a device that cannot be played to. That is the point: SSDP
    /// answers come from anything that speaks UPnP, and offering the user a
    /// row that can only ever produce an error is worse than not listing it.
    pub async fn describe(announcement: &Announcement) -> Result<Self> {
        let location = Endpoint::parse(&announcement.location).ok_or_else(|| {
            NdError::Protocol("the renderer announced an unusable address".into())
        })?;
        let description = upnp::describe(&location).await?;

        let control_path = upnp::control_url(&description, "AVTransport:1").ok_or_else(|| {
            NdError::Unsupported("the device has no AVTransport service to drive".into())
        })?;
        let control = location.resolve(&control_path).ok_or_else(|| {
            NdError::Protocol(format!(
                "the AVTransport URL makes no sense: {control_path}"
            ))
        })?;

        // Off the network and straight into a label, so it is sanitised like
        // every other announced name.
        let display_name =
            sanitize_name(upnp::tag_text(&description, "friendlyName").unwrap_or("Tela DLNA"));

        Ok(Self {
            info: SinkInfo {
                // The USN, not the address: a television that comes back on a
                // new DHCP lease is the same television.
                id: announcement.usn.clone(),
                display_name,
                kind: SinkKind::Dlna,
                address: Some(location.addr.ip().to_string()),
            },
            location: announcement.location.clone(),
            address: location.addr.ip(),
            control,
            status: SinkStatus::new(),
            session: Mutex::new(None),
        })
    }

    /// The description URL this sink was built from.
    pub fn location(&self) -> &str {
        &self.location
    }
}

#[async_trait]
impl Sink for DlnaSink {
    fn info(&self) -> SinkInfo {
        self.info.clone()
    }

    fn control_endpoint(&self) -> Option<std::net::SocketAddr> {
        Some(self.control.addr)
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

    fn max_source_size(&self) -> Option<(u32, u32)> {
        Some(nd_core::pipeline::DLNA_MAX_RESOLUTION)
    }

    async fn start_stream(&self, source: CaptureSource) -> Result<()> {
        let (cancel_tx, cancel) = tokio::sync::watch::channel(false);
        {
            let mut slot = self.session.lock().unwrap_or_else(PoisonError::into_inner);
            // A second session would take over the same television and leave
            // the first one serving a screen nobody is watching.
            if slot.as_ref().is_some_and(|tx| !*tx.borrow()) {
                return Err(NdError::Protocol(
                    "this screen is already receiving a transmission".into(),
                ));
            }
            *slot = Some(cancel_tx);
        }
        self.status.reset();

        let result = session::run(self.address, &self.control, source, &self.status, cancel).await;

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
        // Signal only: `session::run` is what tells the renderer to stop and
        // what tears the pipeline down, and it has to finish doing that before
        // anything reports the sink idle.
        if let Some(cancel) = self
            .session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            let _ = cancel.send(true);
        }
        Ok(())
    }
}
