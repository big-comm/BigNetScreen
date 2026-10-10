//! Capture through `org.gnome.Mutter.ScreenCast` on native GNOME.
//!
//! It exists for the virtual monitor: a screen at exactly the resolution the
//! sink negotiated, with no scaling and no change to the user's physical
//! layout, which the portal cannot size. [`crate::select_backend_for`] picks
//! it for that case and falls back to the portal when it fails.
//!
//! Differences from the portal:
//! - there is no PipeWire descriptor: the node lives in the session's own
//!   PipeWire daemon, so [`CaptureSource::pipewire_fd`] is `None`, and there
//!   is no serial, so the node id is the target;
//! - the session is only visible to the D-Bus connection that created it,
//!   which is why the [`Connection`] is kept alongside the paths;
//! - unavailable under Flatpak, whose sandbox does not reach Mutter.
//!
//! A virtual monitor emits no frames until caps are negotiated, so
//! `pipewiresrc` needs the high `keepalive-time` `nd_core::pipeline` sets.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use gstreamer::prelude::*;
use tokio::sync::Mutex;
use zbus::Connection;
use zbus::zvariant::{OwnedObjectPath, Value};

use nd_core::capture::{CaptureBackend, CaptureSource, SourceType};
use nd_core::{NdError, Result};

use crate::cap_err;

/// How long to wait for the virtual monitor to appear in the display
/// configuration.
///
/// Generous on purpose. The monitor only exists once PipeWire has negotiated
/// a size, which happens when the pipeline connects, and on the Miracast path
/// that waits for the whole M1–M7 negotiation with the receiver. A limit that
/// expires first leaves the layout unapplied and the user with the
/// compositor's default scale on their own screen.
const VIRTUAL_MONITOR_TIMEOUT: Duration = Duration::from_secs(120);

/// How many times to try connecting to the freshly announced virtual stream.
const KEEP_ALIVE_ATTEMPTS: u32 = 15;

/// The wait between those attempts.
const KEEP_ALIVE_RETRY_DELAY: Duration = Duration::from_millis(400);

/// How long to wait for `PipeWireStreamAdded`.
const STREAM_TIMEOUT: Duration = Duration::from_secs(10);

/// Mutter's `CursorMode`: 0 = hidden, 1 = embedded in the video, 2 = metadata.
const CURSOR_MODE_EMBEDDED: u32 = 1;

/// The lowest interface version this code speaks.
const MIN_API_VERSION: i32 = 3;
/// The highest version this code was written against (GNOME 49 exposes 4).
/// The interface is private to GNOME and may change without notice, so newer
/// versions go to the portal.
const MAX_API_VERSION: i32 = 4;

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
    /// Mutter only exposes a session to the connection that created it, and
    /// tears everything down when that connection drops.
    conn: Connection,
    session: OwnedObjectPath,
    /// The `RecordVirtual` session that created the extra screen, when the
    /// stream handed to the caller comes from `RecordMonitor` instead.
    ///
    /// Two sessions are needed because Mutter draws the pointer into a monitor
    /// stream and not into a virtual one. The virtual session creates the
    /// screen; the monitor session is what gets streamed.
    virtual_session: Option<OwnedObjectPath>,
    /// Keeps the virtual stream consumed. The screen exists only while someone
    /// is reading it, so a `fakesink` holds it open while the real pipeline
    /// reads the monitor stream.
    keep_alive: Option<gstreamer::Pipeline>,
    virtual_connector: Option<String>,
}

impl ActiveSession {
    fn new(conn: Connection, session: OwnedObjectPath) -> Self {
        Self {
            conn,
            session,
            virtual_session: None,
            keep_alive: None,
            virtual_connector: None,
        }
    }
}

/// Backend based on `org.gnome.Mutter.ScreenCast`.
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
    /// Creating the screen at the resolution the sink negotiated avoids a
    /// `videoscale` on the hot path.
    pub async fn set_virtual_size(&self, width: u32, height: u32) {
        *self.virtual_size.lock().await = (width, height);
    }

    async fn recording_properties(&self) -> HashMap<&'static str, Value<'static>> {
        HashMap::from([("cursor-mode", Value::from(*self.cursor_mode.lock().await))])
    }

    /// `RecordVirtual` properties for a screen of `size`.
    async fn virtual_properties(&self, size: (u32, u32)) -> HashMap<&'static str, Value<'static>> {
        let mut properties = self.recording_properties().await;
        // `is-platform` makes the new screen part of the desktop layout, an
        // extension windows can be dragged onto, rather than a detached stream.
        properties.insert("is-platform", Value::from(true));
        // `modes` sets the size, and makes the stream non-resizable "as if it
        // was a regular monitor". Without it the size is left to PipeWire
        // negotiation, nothing downstream constrains it, and the compositor
        // hands over its minimum: a 16x16 monitor stretched over the receiver.
        let mode = HashMap::from([
            ("size", Value::from(size)),
            ("refresh-rate", Value::from(60.0f64)),
            ("is-preferred", Value::from(true)),
        ]);
        properties.insert("modes", Value::from(vec![mode]));
        properties
    }

    /// Opens a session recording a monitor and returns the PipeWire node to
    /// read. Leaves `active` alone, and stops the session itself on failure.
    async fn open_monitor(
        &self,
        conn: &Connection,
        connector: &str,
    ) -> Result<(OwnedObjectPath, u32)> {
        let (session_path, session) = create_session(conn).await?;
        let properties = self.recording_properties().await;
        let recorded = async {
            let stream_path = session
                .record_monitor(connector, properties)
                .await
                .map_err(cap_err)?;
            start_and_wait_for_node(conn, &session, stream_path).await
        }
        .await;
        match recorded {
            Ok(node_id) => Ok((session_path, node_id)),
            Err(err) => {
                let _ = session.stop().await;
                Err(err)
            }
        }
    }

    /// Creates a virtual monitor and returns its stream.
    ///
    /// Two sessions, because Mutter draws the pointer into a `RecordMonitor`
    /// stream and not into a `RecordVirtual` one, with the same
    /// `cursor-mode: embedded`. The virtual session only creates the screen;
    /// once it exists it is a connector like any other, and a second session
    /// records it as a monitor, pointer included.
    ///
    /// Everything opened is recorded in `active` and `layout` as it happens,
    /// so a failure part-way is undone by [`CaptureBackend::stop`].
    async fn open_virtual(&self, conn: Connection) -> Result<CaptureSource> {
        let size = *self.virtual_size.lock().await;
        // Taken before the monitor set changes: Mutter regenerates a default
        // layout the moment the virtual monitor appears, and that default
        // replaces the user's chosen scale and refresh mode.
        let layout_before = crate::display_config::LayoutSnapshot::capture(&conn).await;
        let connectors_before = crate::display_config::connectors(&conn).await;

        let (session_path, session) = create_session(&conn).await?;
        *self.active.lock().await = Some(ActiveSession::new(conn.clone(), session_path.clone()));

        tracing::info!(w = size.0, h = size.1, "creating a virtual monitor");
        let stream_path = session
            .record_virtual(self.virtual_properties(size).await)
            .await
            .map_err(cap_err)?;
        let node_id = start_and_wait_for_node(&conn, &session, stream_path).await?;

        *self.layout.lock().await = layout_before.clone();
        let keep_alive = keep_virtual_stream_alive(node_id).await?;
        if let Some(active) = self.active.lock().await.as_mut() {
            active.keep_alive = Some(keep_alive.clone());
        }

        let connector = crate::display_config::wait_for_virtual_connector(
            &conn,
            &connectors_before,
            VIRTUAL_MONITOR_TIMEOUT,
        )
        .await
        .ok_or_else(|| {
            NdError::Capture(
                "the virtual monitor never appeared in the display configuration".into(),
            )
        })?;
        if let Some(layout) = layout_before {
            arrange_virtual_screen(&conn, &connector, &layout).await;
        }

        let (monitor_session, monitor_node) = self.open_monitor(&conn, &connector).await?;
        *self.active.lock().await = Some(ActiveSession {
            conn,
            session: monitor_session,
            virtual_session: Some(session_path),
            keep_alive: Some(keep_alive),
            virtual_connector: Some(connector.clone()),
        });

        tracing::info!(
            node_id = monitor_node,
            %connector,
            "extra screen ready, captured as a monitor so the pointer is included"
        );
        Ok(CaptureSource {
            pipewire_fd: None,
            node_id: monitor_node,
            pipewire_serial: None,
            source_type: SourceType::Virtual,
            size: Some(size),
            // Mutter's advertised buffer range is not in front of us the way
            // KWin's is, so this keeps the element's own default rather than
            // guessing at a floor that could fail the allocation.
            min_buffers: None,
            media: None,
        })
    }

    async fn open(&self, source_type: SourceType) -> Result<CaptureSource> {
        let conn = Connection::session().await.map_err(cap_err)?;
        if source_type == SourceType::Virtual {
            return self.open_virtual(conn).await;
        }

        let connector = match self.monitor_connector.lock().await.clone() {
            Some(chosen) => chosen,
            None => crate::display_config::primary_connector(&conn)
                .await
                .ok_or_else(|| NdError::Capture("could not identify the primary monitor".into()))?,
        };
        tracing::info!(%connector, "capturing a monitor through Mutter");
        let (session, node_id) = self.open_monitor(&conn, &connector).await?;
        *self.active.lock().await = Some(ActiveSession::new(conn, session));

        tracing::info!(node_id, "Mutter capture started");
        Ok(CaptureSource {
            // Mutter publishes into the user session's PipeWire daemon: there
            // is no descriptor to hand over.
            pipewire_fd: None,
            node_id,
            pipewire_serial: None,
            source_type,
            size: None,
            min_buffers: None,
            media: None,
        })
    }
}

impl Default for MutterBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Creates a screen-cast session on `conn`.
async fn create_session(
    conn: &Connection,
) -> Result<(OwnedObjectPath, ScreenCastSessionProxy<'static>)> {
    let screen_cast = ScreenCastProxy::new(conn).await.map_err(cap_err)?;
    let session_path = screen_cast
        .create_session(HashMap::new())
        .await
        .map_err(|e| {
            NdError::Capture(format!(
                "Mutter refused to create the capture session ({e}) — \
                 use the portal (BIGNETSCREEN_CAPTURE=portal)"
            ))
        })?;
    let session = ScreenCastSessionProxy::builder(conn)
        .path(session_path.clone())
        .map_err(cap_err)?
        .build()
        .await
        .map_err(cap_err)?;
    Ok((session_path, session))
}

/// Starts the session and returns the PipeWire node its stream announces.
///
/// Subscribes before `Start`: the signal arrives immediately, and a late
/// subscription would miss the node id.
async fn start_and_wait_for_node(
    conn: &Connection,
    session: &ScreenCastSessionProxy<'_>,
    stream_path: OwnedObjectPath,
) -> Result<u32> {
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
        Ok(Some(signal)) => Ok(signal.args().map_err(cap_err)?.node_id),
        Ok(None) => Err(NdError::Capture(
            "Mutter closed the stream before announcing the PipeWire node".into(),
        )),
        Err(_) => Err(NdError::Capture(format!(
            "Mutter did not announce the PipeWire node within {}s",
            STREAM_TIMEOUT.as_secs()
        ))),
    }
}

/// Holds the virtual stream open with a minimal pipeline.
///
/// A virtual monitor exists only while something consumes its stream. The
/// caller reads the monitor stream instead, so without this the screen would
/// vanish the moment it was created.
async fn keep_virtual_stream_alive(node_id: u32) -> Result<gstreamer::Pipeline> {
    let source = crate::pipeline_source(node_id);
    // Without `videoconvert` the pipeline fails to negotiate and
    // `gstpipewiresrc` reports "target not found". `videorate drop-only` in
    // front of it keeps this cheap: the frames are thrown away, and
    // converting every one of them only to discard it costs enough CPU to
    // make the audio stutter.
    let desc = format!(
        "{source} ! videorate drop-only=true ! video/x-raw,framerate=1/1 \
         ! videoconvert ! fakesink sync=false"
    );

    // Retried, because the node is announced before it can be connected to:
    // the first attempt fails with "target not found" from `gstpipewiresrc`.
    // Each failed pipeline is taken to the null state before being dropped;
    // one left ready logs a GLib critical and then segfaults at process exit.
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

/// Puts the new screen to the right of the saved layout, at its preferred
/// mode, leaving the existing monitors' mode and scale as they were.
async fn arrange_virtual_screen(
    conn: &Connection,
    connector: &str,
    layout: &crate::display_config::LayoutSnapshot,
) {
    let Some((mode_id, width, height)) =
        crate::display_config::preferred_mode(conn, connector).await
    else {
        return;
    };
    if width < 640 || height < 480 {
        tracing::warn!(
            width,
            height,
            "the virtual monitor came out far too small; the receiver will \
             upscale it and the picture will look blurry"
        );
    }
    match layout.extend_with(conn, connector, &mode_id).await {
        Ok(()) => tracing::info!(
            %connector, width, height,
            "extra screen placed to the right; the existing monitors keep \
             their own mode and scale"
        ),
        Err(err) => tracing::warn!(%err, "could not arrange the extra screen"),
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
        // A private GNOME API: past the versions this code was written
        // against, the portal is the safer path.
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
        if source_type == SourceType::Window {
            // Mutter requires the window id; picking one is the portal's job.
            return Err(NdError::Unsupported(
                "window capture through Mutter directly is not supported; \
                 use the portal"
                    .into(),
            ));
        }
        self.stop().await?;
        let started = self.open(source_type).await;
        if started.is_err() {
            let _ = self.stop().await;
        }
        started
    }

    async fn stop(&self) -> Result<()> {
        let Some(active) = self.active.lock().await.take() else {
            return Ok(());
        };
        if let Some(connector) = &active.virtual_connector
            && let Some(current) =
                crate::display_config::LayoutSnapshot::capture(&active.conn).await
        {
            *self.layout.lock().await = current.without(connector);
        }
        if let Ok(builder) = ScreenCastSessionProxy::builder(&active.conn).path(active.session)
            && let Ok(session) = builder.build().await
        {
            let _ = session.stop().await;
        }

        // The keep-alive pipeline and the virtual session go last: dropping
        // them first would remove the screen out from under the session that
        // was recording it.
        if let Some(keep_alive) = active.keep_alive {
            let _ = keep_alive.set_state(gstreamer::State::Null);
        }
        if let Some(virtual_session) = active.virtual_session
            && let Ok(builder) = ScreenCastSessionProxy::builder(&active.conn).path(virtual_session)
            && let Ok(proxy) = builder.build().await
        {
            let _ = proxy.stop().await;
        }

        // Removing the monitor changes the set again, so Mutter regenerates a
        // default layout a second time. Restoring puts the user's own screen
        // back exactly as they had it.
        if let Some(layout) = self.layout.lock().await.take() {
            if let Err(err) = layout.restore(&active.conn).await {
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
    async fn stop_without_start_is_a_noop() {
        let backend = MutterBackend::new();
        assert!(backend.stop().await.is_ok());
    }
}
