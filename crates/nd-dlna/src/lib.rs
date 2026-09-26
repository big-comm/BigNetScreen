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
//! DLNA reaches receivers over the existing LAN without a Wi-Fi Direct
//! adapter. End-to-end delay depends on both sender and receiver buffering;
//! one television's measured delay is not a protocol-wide floor. See
//! `docs/dlna.md` for the transport choices and their validation limits.

pub mod avtransport;
pub mod renderer;
pub mod session;
pub mod ssdp;
pub mod upnp;

use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;

use nd_core::capture::CaptureSource;
use nd_core::sink::{Sink, SinkInfo, SinkKind, SinkState, SinkStatus, sanitize_name};
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
    /// `RenderingControl` and `ConnectionManager`, when it announced them.
    rendering_control: Option<Endpoint>,
    connection_manager: Option<Endpoint>,
    address: IpAddr,
    status: SinkStatus,
    /// Cancels the running session, when there is one, with the number that
    /// tells it apart from the next.
    session: Mutex<Option<(u64, tokio::sync::watch::Sender<bool>)>>,
    sessions: std::sync::atomic::AtomicU64,
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

        let service = |kind: &str| {
            upnp::control_url(&description, kind).and_then(|path| location.resolve(&path))
        };
        let rendering_control = service("RenderingControl:1");
        let connection_manager = service("ConnectionManager:1");

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
            rendering_control,
            connection_manager,
            status: SinkStatus::new(),
            session: Mutex::new(None),
            sessions: std::sync::atomic::AtomicU64::new(0),
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

    fn upnp_renderer(&self) -> Option<nd_core::sink::UpnpRenderer> {
        Some(nd_core::sink::UpnpRenderer {
            address: self.address,
            av_transport: self.control.url(),
            rendering_control: self.rendering_control.as_ref().map(Endpoint::url),
            connection_manager: self.connection_manager.as_ref().map(Endpoint::url),
        })
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
        let (cancel_tx, cancel) = tokio::sync::watch::channel(false);
        let number = self
            .sessions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        {
            let mut slot = self.session.lock().unwrap_or_else(PoisonError::into_inner);
            // A second session would take over the same television and leave
            // the first one serving a screen nobody is watching.
            if slot.as_ref().is_some_and(|(_, tx)| !*tx.borrow()) {
                return Err(NdError::Protocol(
                    "this screen is already receiving a transmission".into(),
                ));
            }
            *slot = Some((number, cancel_tx));
        }
        self.status.reset();

        let result = session::run(self.address, &self.control, source, &self.status, cancel).await;
        // Over, however it ended. Left in place, a session that ended on its
        // own — the file finished, the viewer stopped it on the remote — kept
        // refusing every later one until the service restarted. Only ours:
        // a stopped session may still be finishing when the next one starts.
        {
            let mut slot = self.session.lock().unwrap_or_else(PoisonError::into_inner);
            if slot.as_ref().is_some_and(|(current, _)| *current == number) {
                *slot = None;
            }
        }

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
            .map(|(_, cancel)| cancel)
        {
            let _ = cancel.send(true);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_session_that_ended_on_its_own_lets_the_next_one_start() {
        nd_core::pipeline::init().unwrap();
        let previous = nd_core::settings::current();
        nd_core::settings::set_in_memory(&nd_core::settings::Settings {
            port: 0,
            system_audio: false,
            microphone: false,
            hardware_encoding: false,
            ..Default::default()
        });
        // Nobody answers there: each session ends by itself, with an error.
        let control = Endpoint::parse("http://127.0.0.1:1/control").unwrap();
        let sink = DlnaSink {
            info: SinkInfo {
                id: "test".into(),
                display_name: "Test".into(),
                kind: SinkKind::Dlna,
                address: Some("127.0.0.1".into()),
            },
            location: String::new(),
            address: control.addr.ip(),
            control,
            rendering_control: None,
            connection_manager: None,
            status: SinkStatus::new(),
            session: Mutex::new(None),
            sessions: std::sync::atomic::AtomicU64::new(0),
        };
        let source = || {
            CaptureSource::media_file(
                nd_core::capture::MediaPlayback {
                    control: None,
                    start: None,
                    source: nd_core::media::MediaSource::File("/nonexistent.mkv".into()),
                    kind: nd_core::media::MediaKind::Video,
                    title: "gone".into(),
                },
                (320, 180),
            )
        };
        for attempt in 0..2 {
            let err = sink.start_stream(source()).await.unwrap_err().to_string();
            assert!(
                !err.contains("already receiving"),
                "attempt {attempt}: {err}"
            );
        }
        nd_core::settings::set_in_memory(&previous);
    }
}
