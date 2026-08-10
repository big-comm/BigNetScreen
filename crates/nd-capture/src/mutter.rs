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

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use gstreamer::prelude::*;
use tokio::sync::Mutex;
use zbus::zvariant::{OwnedObjectPath, Value};
use zbus::Connection;

use nd_core::capture::{CaptureBackend, CaptureSource, SourceType};
use nd_core::{NdError, Result};

/// How long to wait for the virtual monitor to appear in the display
/// configuration.
///
/// Generous, and measured rather than guessed. The monitor only exists once
/// PipeWire has negotiated a size, which happens when the pipeline connects —
/// and on the Miracast path the pipeline only starts after the whole M1–M7
/// negotiation with the receiver. Timed in the field: 19 s between creating the
/// session and the first frame. A 15 s limit expired first, the layout was
/// never applied, and the user was left with the compositor's default scale on
/// their own screen.
const VIRTUAL_MONITOR_TIMEOUT: Duration = Duration::from_secs(120);

/// How many times to try connecting to the freshly announced virtual stream.
const KEEP_ALIVE_ATTEMPTS: u32 = 15;

/// The wait between those attempts.
const KEEP_ALIVE_RETRY_DELAY: Duration = Duration::from_millis(400);

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

/// An active Mutter session.
struct ActiveSession {
    /// The connection has to live alongside it: Mutter only exposes the
    /// session object to the connection that created it, and tears everything
    /// down when that connection drops.
    _conn: Connection,
    session: OwnedObjectPath,
    conn_for_stop: Connection,
    /// The `RecordVirtual` session that created the extra screen, when the
    /// stream handed to the caller comes from `RecordMonitor` instead.
    ///
    /// Two sessions are needed because Mutter draws the pointer into a monitor
    /// stream and **not** into a virtual one — measured, with the pointer
    /// present in one capture and absent from the other. The virtual session
    /// creates the screen; the monitor session is what gets streamed.
    virtual_session: Option<OwnedObjectPath>,
    /// Keeps the virtual stream consumed. The screen exists only while someone
    /// is reading it, so a `fakesink` holds it open while the real pipeline
    /// reads the monitor stream.
    keep_alive: Option<gstreamer::Pipeline>,
}

/// Backend baseado em `org.gnome.Mutter.ScreenCast`.
pub struct MutterBackend {
    active: Mutex<Option<ActiveSession>>,
    /// The desired resolution for the virtual monitor.
    virtual_size: Mutex<(u32, u32)>,
    /// The cursor mode to request (see [`MutterBackend::set_cursor_mode`]).
    cursor_mode: Mutex<u32>,
    /// A specific connector to capture, instead of the primary monitor.
    monitor_connector: Mutex<Option<String>>,
    /// The desktop arrangement as it was before the virtual monitor appeared.
    ///
    /// Kept so it can be restored: Mutter regenerates a default layout every
    /// time the monitor set changes, and that default overrides the scale and
    /// refresh mode the user chose (see [`crate::display_config`]).
    layout: Mutex<Option<crate::display_config::LayoutSnapshot>>,
}

impl MutterBackend {
    pub fn new() -> Self {
        Self {
            active: Mutex::new(None),
            virtual_size: Mutex::new((1920, 1080)),
            cursor_mode: Mutex::new(CURSOR_MODE_EMBEDDED),
            monitor_connector: Mutex::new(None),
            layout: Mutex::new(None),
        }
    }

    /// Captures a specific connector instead of the primary monitor.
    ///
    /// The reason this exists: Mutter draws the pointer into a `RecordMonitor`
    /// stream but **not** into a `RecordVirtual` one — measured, with the
    /// pointer visible in one capture and absent from the other. Since a
    /// virtual monitor becomes a real connector once created, it can be
    /// captured as a monitor, cursor included.
    pub async fn set_monitor_connector(&self, connector: Option<String>) {
        *self.monitor_connector.lock().await = connector;
    }

    /// Overrides the cursor mode (0 hidden, 1 embedded, 2 metadata).
    ///
    /// Exists for the `cursor_probe` diagnostic, which captures the same frame
    /// with the cursor off and on and compares the bytes: identical frames mean
    /// the compositor is not drawing it, which no amount of reading can settle.
    pub async fn set_cursor_mode(&self, mode: u32) {
        *self.cursor_mode.lock().await = mode;
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
    /// Holds the virtual stream open with a minimal pipeline.
    ///
    /// A virtual monitor exists only while something consumes its stream. The
    /// caller reads the *monitor* stream instead, so without this the screen
    /// would vanish the moment it was created.
    async fn keep_virtual_stream_alive(&self, node_id: u32) -> Result<gstreamer::Pipeline> {
        let source = crate::pipeline_source(node_id);
        // Two details here, both of them measured.
        //
        // `videoconvert` is not decoration: without it the pipeline failed to
        // negotiate and `gstpipewiresrc` reported "target not found".
        //
        // `videorate drop-only` in front of it is what keeps this cheap. These
        // frames exist only to keep the screen alive — they are thrown away —
        // and converting 1920x1080 sixty times a second to discard the result
        // is pure load. With the machine already at 148% CPU for this app, that
        // waste showed up as stuttering audio.
        let desc = format!(
            "{source} ! videorate drop-only=true ! video/x-raw,framerate=1/1 \
             ! videoconvert ! fakesink sync=false"
        );

        // Retried, because the node is announced before it can be connected
        // to: the first attempt fails with "target not found" from
        // `gstpipewiresrc`. Each failed pipeline is taken to `NULL` before
        // being dropped — leaving one in `READY` produced a `CRITICAL` and then
        // a segfault at process exit.
        let mut last = String::new();
        for attempt in 1..=KEEP_ALIVE_ATTEMPTS {
            let (pipeline, _events) = nd_core::pipeline::build_pipeline(&desc, 0)?;
            match pipeline.set_state(gstreamer::State::Playing) {
                // `set_state` returning Ok only means the change was accepted;
                // asking for the state waits for it to happen, or to fail.
                Ok(_) => match pipeline.state(gstreamer::ClockTime::from_seconds(3)).0 {
                    Ok(_) => return Ok(pipeline),
                    Err(err) => last = err.to_string(),
                },
                Err(err) => last = err.to_string(),
            }
            let _ = pipeline.set_state(gstreamer::State::Null);
            tracing::debug!(attempt, %last, "the virtual stream is not connectable yet");
            tokio::time::sleep(KEEP_ALIVE_RETRY_DELAY).await;
        }

        Err(NdError::Capture(format!(
            "could not hold the virtual screen open after {KEEP_ALIVE_ATTEMPTS} attempts: {last}"
        )))
    }

    /// Opens a second screen-cast session recording a specific connector.
    ///
    /// Returns the session path and the PipeWire node to read.
    async fn record_connector(
        &self,
        conn: &Connection,
        connector: &str,
    ) -> Result<(OwnedObjectPath, u32)> {
        let screen_cast = ScreenCastProxy::new(conn).await.map_err(cap_err)?;
        let session_path = screen_cast
            .create_session(std::collections::HashMap::new())
            .await
            .map_err(cap_err)?;
        let session = ScreenCastSessionProxy::builder(conn)
            .path(session_path.clone())
            .map_err(cap_err)?
            .build()
            .await
            .map_err(cap_err)?;

        let mut props: std::collections::HashMap<&str, Value<'_>> =
            std::collections::HashMap::new();
        props.insert("cursor-mode", Value::from(*self.cursor_mode.lock().await));
        let stream_path = session
            .record_monitor(connector, props)
            .await
            .map_err(cap_err)?;

        let stream = ScreenCastStreamProxy::builder(conn)
            .path(stream_path)
            .map_err(cap_err)?
            .build()
            .await
            .map_err(cap_err)?;
        let mut added = stream
            .receive_pipe_wire_stream_added()
            .await
            .map_err(cap_err)?;

        session.start().await.map_err(cap_err)?;

        match tokio::time::timeout(STREAM_TIMEOUT, added.next()).await {
            Ok(Some(signal)) => Ok((session_path, signal.args().map_err(cap_err)?.node_id)),
            _ => {
                let _ = session.stop().await;
                Err(NdError::Capture(
                    "the extra screen did not announce a PipeWire node".into(),
                ))
            }
        }
    }

    /// The primary monitor's connector.
    ///
    /// Delegated to [`crate::display_config`], which is the one place that
    /// declares the D-Bus types. The duplicate declaration that used to live
    /// here got the logical-monitor tuple wrong — an extra leading `u32`, where
    /// the interface says `a(iiduba(ssss)a{sv})` — so every read failed, the
    /// `.ok()?` swallowed it, and capturing a monitor through Mutter always
    /// came back "could not identify the primary monitor".
    async fn primary_connector(conn: &Connection) -> Option<String> {
        crate::display_config::primary_connector(conn).await
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

        // Snapshot the arrangement *before* the monitor set changes. Mutter
        // regenerates a default layout the moment the virtual monitor appears,
        // and that default replaces the user's chosen scale and refresh mode.
        let (layout_before, connectors_before) = if source_type == SourceType::Virtual {
            (
                crate::display_config::LayoutSnapshot::capture(&conn).await,
                crate::display_config::connectors(&conn).await,
            )
        } else {
            (None, Vec::new())
        };

        let mut props: std::collections::HashMap<&str, Value<'_>> =
            std::collections::HashMap::new();
        props.insert("cursor-mode", Value::from(*self.cursor_mode.lock().await));

        let size = *self.virtual_size.lock().await;
        let stream_path = match source_type {
            SourceType::Monitor => {
                let connector = match self.monitor_connector.lock().await.clone() {
                    Some(chosen) => chosen,
                    None => Self::primary_connector(&conn).await.ok_or_else(|| {
                        NdError::Capture("could not identify the primary monitor".into())
                    })?,
                };
                tracing::info!(%connector, "capturing a monitor through Mutter");
                session
                    .record_monitor(&connector, props)
                    .await
                    .map_err(cap_err)?
            }
            SourceType::Virtual => {
                // `is-platform` is what makes the new screen part of the
                // desktop layout — an extension you can drag windows onto —
                // rather than a detached stream.
                props.insert("is-platform", Value::from(true));

                // `modes` is how the size is set, and the interface is explicit
                // about it: "when the modes property is configured, the
                // PipeWire stream becomes non-resizable, and size is controlled
                // by the compositor as if it was a regular monitor".
                //
                // Without it the size is left to PipeWire negotiation, and
                // nothing downstream constrains it — `videoscale` accepts any
                // input — so the compositor handed over its minimum: a **16x16**
                // monitor, stretched across the whole receiver. That is what a
                // blurry picture and a screen the pointer cannot reach look
                // like from the outside.
                let mut mode: HashMap<&str, Value<'_>> = HashMap::new();
                mode.insert("size", Value::from((size.0, size.1)));
                mode.insert("refresh-rate", Value::from(60.0f64));
                mode.insert("is-preferred", Value::from(true));
                props.insert("modes", Value::from(vec![mode]));

                tracing::info!(w = size.0, h = size.1, "creating a virtual monitor");
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

        let node_id_result = tokio::time::timeout(STREAM_TIMEOUT, added.next()).await;
        let node_id = match node_id_result {
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

        // A virtual monitor needs two sessions, and the reason is the pointer.
        //
        // Measured on this machine: Mutter draws the cursor into a
        // `RecordMonitor` stream and **not** into a `RecordVirtual` one. The
        // captured frames prove it — the pointer is there in one and absent in
        // the other, same code, same `cursor-mode: embedded`.
        //
        // So the virtual session's job is only to *create* the screen. Once it
        // exists it is a connector like any other, and a second session records
        // it as a monitor — cursor included. The virtual stream still has to be
        // consumed, because the screen lives only while someone reads it, so a
        // `fakesink` holds it open.
        if source_type == SourceType::Virtual {
            let keep_alive = self.keep_virtual_stream_alive(node_id).await?;

            let connector = crate::display_config::wait_for_virtual_connector(
                &conn,
                &connectors_before,
                VIRTUAL_MONITOR_TIMEOUT,
            )
            .await;

            let Some(connector) = connector else {
                let _ = keep_alive.set_state(gstreamer::State::Null);
                let _ = session.stop().await;
                return Err(NdError::Capture(
                    "the virtual monitor never appeared in the display configuration".into(),
                ));
            };

            if let Some((mode_id, width, height)) =
                crate::display_config::preferred_mode(&conn, &connector).await
            {
                // A tiny monitor is the difference between a usable screen and
                // a blurry postage stamp stretched over the receiver.
                if width < 640 || height < 480 {
                    tracing::warn!(
                        width,
                        height,
                        "the virtual monitor came out far too small; the receiver will \
                         upscale it and the picture will look blurry"
                    );
                }
                if let Some(layout) = layout_before.clone() {
                    match layout.extend_with(&conn, &connector, &mode_id).await {
                        Ok(()) => tracing::info!(
                            %connector, width, height,
                            "extra screen placed to the right; the existing monitors keep \
                             their own mode and scale"
                        ),
                        Err(err) => tracing::warn!(%err, "could not arrange the extra screen"),
                    }
                }
            }
            *self.layout.lock().await = layout_before;

            // Second session: the same screen, recorded as a monitor.
            let (monitor_session, monitor_node) = self
                .record_connector(&conn, &connector)
                .await
                .inspect_err(|_| {
                    let _ = keep_alive.set_state(gstreamer::State::Null);
                })?;

            *self.active.lock().await = Some(ActiveSession {
                _conn: conn.clone(),
                session: monitor_session,
                conn_for_stop: conn,
                virtual_session: Some(session_path),
                keep_alive: Some(keep_alive),
            });

            tracing::info!(
                node_id = monitor_node,
                %connector,
                "extra screen ready, captured as a monitor so the pointer is included"
            );
            return Ok(CaptureSource {
                pipewire_fd: None,
                node_id: monitor_node,
                source_type,
                size: Some(size),
            });
        }

        *self.active.lock().await = Some(ActiveSession {
            _conn: conn.clone(),
            session: session_path,
            conn_for_stop: conn,
            virtual_session: None,
            keep_alive: None,
        });

        tracing::info!(node_id, "Mutter capture started");
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

        // The keep-alive pipeline and the virtual session go last: dropping
        // them first would remove the screen out from under the session that
        // was recording it.
        if let Some(keep_alive) = active.keep_alive {
            let _ = keep_alive.set_state(gstreamer::State::Null);
        }
        if let Some(virtual_session) = active.virtual_session {
            if let Ok(builder) =
                ScreenCastSessionProxy::builder(&active.conn_for_stop).path(virtual_session)
            {
                if let Ok(proxy) = builder.build().await {
                    let _ = proxy.stop().await;
                }
            }
        }

        // Removing the monitor changes the set again, so Mutter regenerates a
        // default layout a second time. Restoring puts the user's own screen
        // back exactly as they had it.
        if let Some(layout) = self.layout.lock().await.take() {
            if let Err(err) = layout.restore(&active.conn_for_stop).await {
                tracing::warn!(%err, "could not restore the previous desktop layout");
            } else {
                tracing::info!("desktop layout restored");
            }
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
