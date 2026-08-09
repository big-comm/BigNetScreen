//! Capture through `org.gnome.Mutter.ScreenCast` (native GNOME).
//!
//! It exists for one specific reason: the **virtual monitor**. On most
//! backends the `ScreenCast` portal only exposes real monitors and windows,
//! and WFD benefits a great deal from a virtual monitor at exactly the
//! resolution the sink negotiated (no scaling, no change to the user's
//! physical layout).
//!
//! Differences from the portal:
//! - **there is no PipeWire descriptor**: the node lives in the session's own
//!   PipeWire daemon, so [`CaptureSource::pipewire_fd`] is `None`;
//! - the session is only visible to the D-Bus connection that created it —
//!   which is why the [`Connection`] is kept alongside the paths;
//! - unavailable under Flatpak (the sandbox does not talk to Mutter).
//!
//! A quirk carried over from the C code: a virtual monitor **emits no frames**
//! until caps are negotiated; `pipewiresrc` needs a high `keepalive-time`,
//! which `nd_core::pipeline` already provides.

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::Mutex;
use zbus::zvariant::{OwnedObjectPath, Value};
use zbus::Connection;

use nd_core::capture::{CaptureBackend, CaptureSource, SourceType};
use nd_core::{NdError, Result};

/// How long to wait for `PipeWireStreamAdded`.
const STREAM_TIMEOUT: Duration = Duration::from_secs(10);

/// Mutter's `CursorMode`: 0 = hidden, 1 = embedded in the video, 2 = metadata.
const CURSOR_MODE_EMBEDDED: u32 = 1;

/// The lowest interface version we know how to speak.
const MIN_API_VERSION: i32 = 3;
/// The highest validated version (GNOME 49 exposes 4). Above that we prefer
/// the portal: the interface is private to GNOME and may change without
/// notice.
const MAX_API_VERSION: i32 = 4;

fn cap_err<E: std::fmt::Display>(e: E) -> NdError {
    NdError::Capture(e.to_string())
}

#[zbus::proxy(
    interface = "org.gnome.Mutter.ScreenCast",
    default_service = "org.gnome.Mutter.ScreenCast",
    default_path = "/org/gnome/Mutter/ScreenCast"
)]
trait ScreenCast {
    fn create_session(
        &self,
        properties: std::collections::HashMap<&str, Value<'_>>,
    ) -> zbus::Result<OwnedObjectPath>;

    #[zbus(property)]
    fn version(&self) -> zbus::Result<i32>;
}

#[zbus::proxy(
    interface = "org.gnome.Mutter.ScreenCast.Session",
    default_service = "org.gnome.Mutter.ScreenCast"
)]
trait ScreenCastSession {
    fn record_monitor(
        &self,
        connector: &str,
        properties: std::collections::HashMap<&str, Value<'_>>,
    ) -> zbus::Result<OwnedObjectPath>;

    fn record_virtual(
        &self,
        properties: std::collections::HashMap<&str, Value<'_>>,
    ) -> zbus::Result<OwnedObjectPath>;

    fn record_window(
        &self,
        properties: std::collections::HashMap<&str, Value<'_>>,
    ) -> zbus::Result<OwnedObjectPath>;

    fn start(&self) -> zbus::Result<()>;

    fn stop(&self) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.gnome.Mutter.ScreenCast.Stream",
    default_service = "org.gnome.Mutter.ScreenCast"
)]
trait ScreenCastStream {
    #[zbus(signal)]
    fn pipe_wire_stream_added(&self, node_id: u32) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.gnome.Mutter.DisplayConfig",
    default_service = "org.gnome.Mutter.DisplayConfig",
    default_path = "/org/gnome/Mutter/DisplayConfig"
)]
trait DisplayConfig {
    #[allow(clippy::type_complexity)]
    fn get_current_state(
        &self,
    ) -> zbus::Result<(
        u32,
        // monitores: ((connector, vendor, product, serial), modos, propriedades)
        Vec<(
            (String, String, String, String),
            Vec<(
                String,
                i32,
                i32,
                f64,
                f64,
                Vec<f64>,
                std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
            )>,
            std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
        )>,
        Vec<(
            u32,
            i32,
            i32,
            f64,
            u32,
            bool,
            Vec<(String, String, String, String)>,
            std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
        )>,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    )>;
}

/// An active Mutter session.
struct ActiveSession {
    /// The connection has to live alongside it: Mutter only exposes the
    /// session object to the connection that created it, and tears everything
    /// down when that connection drops.
    _conn: Connection,
    session: OwnedObjectPath,
    conn_for_stop: Connection,
}

/// Backend baseado em `org.gnome.Mutter.ScreenCast`.
pub struct MutterBackend {
    active: Mutex<Option<ActiveSession>>,
    /// The desired resolution for the virtual monitor.
    virtual_size: Mutex<(u32, u32)>,
}

impl MutterBackend {
    pub fn new() -> Self {
        Self {
            active: Mutex::new(None),
            virtual_size: Mutex::new((1920, 1080)),
        }
    }

    /// Sets the virtual monitor's resolution before [`CaptureBackend::start`].
    ///
    /// This is the concrete win of this backend: creating the screen already
    /// at the resolution the sink negotiated avoids a `videoscale` on the hot
    /// path.
    pub async fn set_virtual_size(&self, width: u32, height: u32) {
        *self.virtual_size.lock().await = (width, height);
    }

    /// The primary monitor's connector name (e.g. `eDP-1`, `HDMI-1`).
    async fn primary_connector(conn: &Connection) -> Option<String> {
        let proxy = DisplayConfigProxy::new(conn).await.ok()?;
        let (_serial, monitors, logical, _props) = proxy.get_current_state().await.ok()?;

        // Prefer the monitor of the first logical monitor flagged as primary.
        for (_x, _y, _scale, _transform, primary, assigned, _props) in logical
            .iter()
            .map(|l| (l.1, l.2, l.3, l.4, l.5, &l.6, &l.7))
        {
            if primary {
                if let Some((connector, ..)) = assigned.first() {
                    return Some(connector.clone());
                }
            }
        }
        monitors.first().map(|m| m.0 .0.clone())
    }
}

impl Default for MutterBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CaptureBackend for MutterBackend {
    fn id(&self) -> &'static str {
        "mutter"
    }

    async fn is_available(&self) -> bool {
        if crate::is_sandboxed() {
            return false;
        }
        let Ok(conn) = Connection::session().await else {
            return false;
        };
        let Ok(proxy) = ScreenCastProxy::new(&conn).await else {
            return false;
        };
        // The property only answers if the service really exists (zbus proxies
        // are lazy).
        let Ok(version) = proxy.version().await else {
            return false;
        };
        // A version guard: this is a private GNOME API. If it evolves beyond
        // what we know how to speak, letting the portal take over beats
        // risking undefined behaviour.
        if !(MIN_API_VERSION..=MAX_API_VERSION).contains(&version) {
            tracing::info!(
                version,
                supported = format!("{MIN_API_VERSION}..={MAX_API_VERSION}"),
                "org.gnome.Mutter.ScreenCast version outside the known range; using the portal"
            );
            return false;
        }
        true
    }

    async fn supported_sources(&self) -> Vec<SourceType> {
        if self.is_available().await {
            vec![SourceType::Monitor, SourceType::Window, SourceType::Virtual]
        } else {
            Vec::new()
        }
    }

    async fn start(&self, source_type: SourceType) -> Result<CaptureSource> {
        // Close any previous session before opening another.
        self.stop().await?;

        let conn = Connection::session().await.map_err(cap_err)?;
        let screencast = ScreenCastProxy::new(&conn).await.map_err(cap_err)?;

        let session_path = screencast
            .create_session(Default::default())
            .await
            .map_err(|e| {
                NdError::Capture(format!(
                    "Mutter refused to create the capture session ({e}) — \
                     use the portal (BIGNETSCREEN_CAPTURE=portal)"
                ))
            })?;

        let session = ScreenCastSessionProxy::builder(&conn)
            .path(session_path.clone())
            .map_err(cap_err)?
            .build()
            .await
            .map_err(cap_err)?;

        let mut props: std::collections::HashMap<&str, Value<'_>> =
            std::collections::HashMap::new();
        props.insert("cursor-mode", Value::from(CURSOR_MODE_EMBEDDED));

        let size = *self.virtual_size.lock().await;
        let stream_path = match source_type {
            SourceType::Monitor => {
                let connector = Self::primary_connector(&conn).await.ok_or_else(|| {
                    NdError::Capture("could not identify the primary monitor".into())
                })?;
                tracing::info!(%connector, "capturando monitor via Mutter");
                session
                    .record_monitor(&connector, props)
                    .await
                    .map_err(cap_err)?
            }
            SourceType::Virtual => {
                props.insert("is-platform", Value::from(true));
                tracing::info!(w = size.0, h = size.1, "criando monitor virtual");
                session.record_virtual(props).await.map_err(cap_err)?
            }
            SourceType::Window => {
                // Mutter requires the window id; picking one is the portal's job.
                return Err(NdError::Unsupported(
                    "window capture through Mutter directly is not supported; \
                     use the portal"
                        .into(),
                ));
            }
        };

        // Subscribe BEFORE Start(): the signal arrives immediately and a
        // late subscription would miss the node id.
        let stream = ScreenCastStreamProxy::builder(&conn)
            .path(stream_path.clone())
            .map_err(cap_err)?
            .build()
            .await
            .map_err(cap_err)?;
        let mut added = stream
            .receive_pipe_wire_stream_added()
            .await
            .map_err(cap_err)?;

        session.start().await.map_err(cap_err)?;

        let node_id = match tokio::time::timeout(STREAM_TIMEOUT, added.next()).await {
            Ok(Some(signal)) => signal.args().map_err(cap_err)?.node_id,
            Ok(None) => {
                let _ = session.stop().await;
                return Err(NdError::Capture(
                    "Mutter closed the stream before announcing the PipeWire node".into(),
                ));
            }
            Err(_) => {
                let _ = session.stop().await;
                return Err(NdError::Capture(format!(
                    "Mutter did not announce the PipeWire node within {}s",
                    STREAM_TIMEOUT.as_secs()
                )));
            }
        };

        *self.active.lock().await = Some(ActiveSession {
            _conn: conn.clone(),
            session: session_path,
            conn_for_stop: conn,
        });

        tracing::info!(node_id, "captura via Mutter iniciada");
        Ok(CaptureSource {
            // Mutter publishes into the user session's PipeWire daemon: there
            // is no descriptor to hand over.
            pipewire_fd: None,
            node_id,
            source_type,
            size: (source_type == SourceType::Virtual).then_some(size),
        })
    }

    async fn stop(&self) -> Result<()> {
        let Some(active) = self.active.lock().await.take() else {
            return Ok(());
        };
        let session = ScreenCastSessionProxy::builder(&active.conn_for_stop)
            .path(active.session)
            .map_err(cap_err)?
            .build()
            .await
            .map_err(cap_err)?;
        if let Err(err) = session.stop().await {
            tracing::debug!(%err, "the Mutter session was already closed");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn availability_check_never_panics() {
        // Runs both on GNOME and in CI without a graphical session.
        let backend = MutterBackend::new();
        let available = backend.is_available().await;
        tracing::info!(available, "is Mutter ScreenCast available?");
    }

    #[tokio::test]
    async fn stop_without_start_is_a_noop() {
        let backend = MutterBackend::new();
        assert!(backend.stop().await.is_ok());
    }
}
