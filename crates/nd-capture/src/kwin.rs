//! Screens and virtual screens over `zkde_screencast_unstable_v1`, KWin's
//! protocol, which big-compositor serves too.
//!
//! An existing screen is streamed by its `wl_output`, chosen by connector
//! name, with no portal dialog: the compositor grants the protocol to the
//! installed service alone.
//!
//! The ScreenCast portal has no way to say how big a virtual screen should be,
//! so `xdg-desktop-portal-kde` picks for everyone — with a literal:
//!
//! ```c++
//! // src/screencast.cpp, v6.7.4:234
//! stream = WaylandIntegration::startStreamingVirtual(
//!     OutputsModel::virtualScreenIdForApp(session->appId()), outputName,
//!     {1920, 1080}, cursorMode);
//! ```
//!
//! KWin's own `zkde_screencast_unstable_v1` does take a size, and it is what
//! the portal calls underneath, so on KWin this crate asks the compositor
//! directly, as it asks Mutter on GNOME.
//!
//! The screen KWin creates runs at 60 Hz (`OutputModeline(size, 60000)` in
//! `drm_virtual_output.cpp`) and the protocol has no refresh-rate argument, so
//! a higher fps setting cannot be honoured here. KWin's capture is driven by
//! damage, not by a clock (`OutputScreenCastSource` follows
//! `OutputLayer::repaintScheduled`): the frame rate is the rate the screen is
//! painted at, so a 24 fps video streams at 24. A virtual screen's vblank is a
//! `QTimer` (`SoftwareVsyncMonitor`, truncating to whole milliseconds) feeding
//! a second `QTimer` in the screencast's pacing, which keeps it below 60 even
//! when the screen repaints constantly; closing that gap means patching KWin.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::Mutex;
use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};

use nd_core::capture::{CaptureBackend, CaptureSource, SourceType};
use nd_core::{NdError, Result};

use crate::cap_err;

/// The generated client side of the protocol.
pub mod protocol {
    #![allow(clippy::too_many_arguments, missing_docs)]
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/zkde-screencast-unstable-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/zkde-screencast-unstable-v1.xml");
}

use protocol::zkde_screencast_stream_unstable_v1::{
    Event as StreamEvent, ZkdeScreencastStreamUnstableV1 as Stream,
};
use protocol::zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1 as Manager;

/// `stream_virtual_output_with_description` arrived in version 4. Version 2
/// has the request without a description, but a screen the user cannot name in
/// their display settings is not worth a second code path.
const MIN_VERSION: u32 = 4;
/// The highest version this code knows how to read. Version 6 adds `serial`,
/// which is what [`CaptureSource::pipewire_serial`] wants.
const MAX_VERSION: u32 = 6;

/// The buffer pool to insist on, at the top of what KWin offers.
///
/// `screencaststream.cpp` advertises `SPA_POD_CHOICE_RANGE_Int(3, 2, 4)`. Left
/// to settle on its own the count varies per stream and stays for its life,
/// and a stream that lands low stutters while the compositor sits idle.
///
/// Four and not more: asking beyond what the producer offers fails the
/// allocation outright rather than being clamped.
const KWIN_BUFFER_POOL: u32 = 4;

/// Draw the cursor into the frames (`pointer.embedded`).
const POINTER_EMBEDDED: u32 = 2;

/// How long to wait for KWin to create the screen and its PipeWire node.
const CREATE_TIMEOUT: Duration = Duration::from_secs(5);

/// `wl_output` gained its `name` (the connector) in version 4.
const OUTPUT_VERSION: u32 = 4;

/// A screen the compositor can stream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Screen {
    /// The connector, as the output names itself (`DP-1`).
    pub name: String,
    /// What the compositor tells the person it is, often make and model.
    pub description: String,
}

/// Whatever the compositor has told us so far.
#[derive(Default)]
struct StreamState {
    node: Option<u32>,
    serial: Option<u64>,
    failed: Option<String>,
    outputs: Vec<(WlOutput, Screen)>,
}

impl Dispatch<WlOutput, usize> for StreamState {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some((_, screen)) = state.outputs.get_mut(*index) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => screen.name = name,
            wl_output::Event::Description { description } => screen.description = description,
            _ => {}
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for StreamState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<Manager, ()> for StreamState {
    fn event(
        _: &mut Self,
        _: &Manager,
        _: <Manager as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<Stream, ()> for StreamState {
    fn event(
        state: &mut Self,
        _: &Stream,
        event: StreamEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            StreamEvent::Created { node } => state.node = Some(node),
            StreamEvent::Serial {
                object_serial_hi,
                object_serial_low,
            } => state.serial = Some(object_serial(object_serial_hi, object_serial_low)),
            StreamEvent::Failed { error } => state.failed = Some(error),
            // The compositor dropped the screen. `stop` is what closes our
            // side; there is nothing to undo here.
            StreamEvent::Closed => {}
        }
    }
}

/// The protocol splits the 64-bit PipeWire serial into two `uint` halves.
fn object_serial(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

/// The PipeWire node and serial the compositor announced, or why there is none.
fn announced_target(state: StreamState) -> Result<(u32, Option<u64>)> {
    if let Some(error) = state.failed {
        return Err(NdError::Capture(format!(
            "the compositor refused the screen: {error}"
        )));
    }
    let node = state
        .node
        .ok_or_else(|| NdError::Capture("the compositor announced no PipeWire node".into()))?;
    if !crate::is_valid_pipewire_target(node, state.serial) {
        return Err(NdError::Capture(
            "the compositor announced an invalid PipeWire target".into(),
        ));
    }
    Ok((node, state.serial))
}

/// A live stream. Dropping the connection ends it, and a virtual screen with it.
struct ActiveStream {
    connection: Connection,
    stream: Stream,
}

/// Backend based on `zkde_screencast_unstable_v1`: existing and virtual
/// screens.
pub struct KWinBackend {
    size: Mutex<(u32, u32)>,
    screen: Mutex<Option<String>>,
    active: Mutex<Option<ActiveStream>>,
}

impl Default for KWinBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl KWinBackend {
    pub fn new() -> Self {
        Self {
            size: Mutex::new((1920, 1080)),
            screen: Mutex::new(None),
            active: Mutex::new(None),
        }
    }

    /// Sets the virtual screen's size before [`CaptureBackend::start`].
    pub async fn set_virtual_size(&self, width: u32, height: u32) {
        *self.size.lock().await = (width, height);
    }

    /// Chooses the screen a [`SourceType::Monitor`] capture streams, by
    /// connector name; `None` takes the only one there is.
    pub async fn set_screen(&self, name: Option<String>) {
        *self.screen.lock().await = name;
    }
}

/// Binds the `zkde_screencast_unstable_v1` global, or says why it could not.
///
/// Also serves as the availability probe: a compositor without the protocol,
/// or one that keeps it from this process, does not advertise this global.
fn bind_screencast(
    connection: &Connection,
) -> Result<(GlobalList, Manager, EventQueue<StreamState>)> {
    let (globals, queue) = registry_queue_init::<StreamState>(connection).map_err(cap_err)?;
    let screencast = globals
        .bind::<Manager, _, _>(&queue.handle(), MIN_VERSION..=MAX_VERSION, ())
        .map_err(|err| {
            NdError::Capture(format!("zkde_screencast_unstable_v1 unavailable: {err}"))
        })?;
    Ok((globals, screencast, queue))
}

/// Binds every output that names its connector and reads the names.
fn read_outputs(
    globals: &GlobalList,
    queue: &mut EventQueue<StreamState>,
    state: &mut StreamState,
) -> Result<()> {
    for global in globals.contents().clone_list() {
        if global.interface != "wl_output" || global.version < OUTPUT_VERSION {
            continue;
        }
        let index = state.outputs.len();
        let output = globals.registry().bind::<WlOutput, _, _>(
            global.name,
            OUTPUT_VERSION,
            &queue.handle(),
            index,
        );
        state.outputs.push((output, Screen::default()));
    }
    queue.roundtrip(state).map_err(cap_err)?;
    state.outputs.retain(|(_, screen)| !screen.name.is_empty());
    Ok(())
}

/// The screens the compositor will stream, in its own order.
///
/// Blocking: a short Wayland round trip.
pub(crate) fn screens() -> Result<Vec<Screen>> {
    let connection = Connection::connect_to_env().map_err(cap_err)?;
    let (globals, _, mut queue) = bind_screencast(&connection)?;
    let mut state = StreamState::default();
    read_outputs(&globals, &mut queue, &mut state)?;
    Ok(state
        .outputs
        .into_iter()
        .map(|(_, screen)| screen)
        .collect())
}

/// The output to stream: the one named, or the only one there is.
fn chosen_output(outputs: &[(WlOutput, Screen)], name: Option<&str>) -> Result<WlOutput> {
    let found = match name {
        Some(name) => outputs.iter().find(|(_, screen)| screen.name == name),
        None if outputs.len() == 1 => outputs.first(),
        None => None,
    };
    if let Some((output, _)) = found {
        return Ok(output.clone());
    }
    let names: Vec<&str> = outputs
        .iter()
        .map(|(_, screen)| screen.name.as_str())
        .collect();
    Err(NdError::Capture(match name {
        Some(name) => format!(
            "there is no screen {name}; the screens are {}",
            names.join(", ")
        ),
        None => format!(
            "several screens are connected; choose one of {}",
            names.join(", ")
        ),
    }))
}

/// Waits for the stream's PipeWire node, or the compositor's refusal.
fn wait_for_target(
    queue: &mut EventQueue<StreamState>,
    state: &mut StreamState,
) -> Result<(u32, Option<u64>)> {
    let deadline = Instant::now() + CREATE_TIMEOUT;
    while state.node.is_none() && state.failed.is_none() {
        if Instant::now() >= deadline {
            return Err(NdError::Capture(
                "the compositor did not open the stream in time".into(),
            ));
        }
        queue.roundtrip(state).map_err(cap_err)?;
        if state.node.is_none() && state.failed.is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    announced_target(std::mem::take(state))
}

/// Streams an existing screen and waits for its PipeWire node.
///
/// Blocking, like [`open_virtual_screen`].
fn open_screen(name: Option<&str>) -> Result<(ActiveStream, u32, Option<u64>)> {
    let connection = Connection::connect_to_env().map_err(cap_err)?;
    let (globals, screencast, mut queue) = bind_screencast(&connection)?;
    let mut state = StreamState::default();
    read_outputs(&globals, &mut queue, &mut state)?;
    let output = chosen_output(&state.outputs, name)?;
    let stream = screencast.stream_output(&output, POINTER_EMBEDDED, &queue.handle(), ());
    let (node, serial) = wait_for_target(&mut queue, &mut state)?;
    Ok((ActiveStream { connection, stream }, node, serial))
}

/// Creates the screen and waits for its PipeWire node.
///
/// Blocking: `wayland-client` has no async surface, and this runs once per
/// session on a `spawn_blocking` thread.
fn open_virtual_screen(width: u32, height: u32) -> Result<(ActiveStream, u32, Option<u64>)> {
    let connection = Connection::connect_to_env().map_err(cap_err)?;
    let (_, screencast, mut queue) = bind_screencast(&connection)?;

    let stream = screencast.stream_virtual_output_with_description(
        "BigNetScreen".to_string(),
        "BigNetScreen".to_string(),
        width as i32,
        height as i32,
        1.0,
        POINTER_EMBEDDED,
        &queue.handle(),
        (),
    );

    let (node, serial) = wait_for_target(&mut queue, &mut StreamState::default())?;
    Ok((ActiveStream { connection, stream }, node, serial))
}

#[async_trait]
impl CaptureBackend for KWinBackend {
    fn id(&self) -> &'static str {
        "kwin"
    }

    async fn is_available(&self) -> bool {
        if crate::is_sandboxed() {
            return false;
        }
        tokio::task::spawn_blocking(|| {
            Connection::connect_to_env()
                .map_err(cap_err)
                .and_then(|connection| bind_screencast(&connection))
                .is_ok()
        })
        .await
        .unwrap_or(false)
    }

    async fn supported_sources(&self) -> Vec<SourceType> {
        if self.is_available().await {
            vec![SourceType::Monitor, SourceType::Virtual]
        } else {
            Vec::new()
        }
    }

    async fn start(&self, source_type: SourceType) -> Result<CaptureSource> {
        self.stop().await?;
        match source_type {
            SourceType::Virtual => self.start_virtual().await,
            SourceType::Monitor => self.start_screen().await,
            SourceType::Window => Err(NdError::Capture(
                "the zkde_screencast backend does not stream windows".into(),
            )),
        }
    }

    async fn stop(&self) -> Result<()> {
        let Some(active) = self.active.lock().await.take() else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || {
            active.stream.close();
            let _ = active.connection.flush();
        })
        .await
        .map_err(cap_err)
    }
}

impl KWinBackend {
    async fn start_screen(&self) -> Result<CaptureSource> {
        let name = self.screen.lock().await.clone();
        let (active, node_id, pipewire_serial) =
            tokio::task::spawn_blocking(move || open_screen(name.as_deref()))
                .await
                .map_err(cap_err)??;
        tracing::info!(node_id, "the compositor is streaming the screen");
        *self.active.lock().await = Some(active);
        Ok(CaptureSource {
            pipewire_fd: None,
            node_id,
            pipewire_serial,
            source_type: SourceType::Monitor,
            size: None,
            min_buffers: Some(KWIN_BUFFER_POOL),
            media: None,
        })
    }

    async fn start_virtual(&self) -> Result<CaptureSource> {
        let (width, height) = *self.size.lock().await;
        let (active, node_id, pipewire_serial) =
            tokio::task::spawn_blocking(move || open_virtual_screen(width, height))
                .await
                .map_err(cap_err)??;

        tracing::info!(
            width,
            height,
            node_id,
            "the compositor opened the virtual screen at the requested size"
        );
        *self.active.lock().await = Some(active);

        Ok(CaptureSource {
            pipewire_fd: None,
            node_id,
            pipewire_serial,
            source_type: SourceType::Virtual,
            size: Some((width, height)),
            // KWin offers two to four buffers and prefers three, and where the
            // negotiation lands is fixed for the life of the stream. Pin it to
            // the top: the compositor can only paint into a buffer we have
            // given back, and this pipeline holds one by design.
            min_buffers: Some(KWIN_BUFFER_POOL),
            media: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stop_without_start_is_a_noop() {
        assert!(KWinBackend::new().stop().await.is_ok());
    }

    #[tokio::test]
    async fn only_virtual_screens_are_this_backend_s_business() {
        let backend = KWinBackend::new();
        assert!(backend.start(SourceType::Monitor).await.is_err());
        assert!(backend.start(SourceType::Window).await.is_err());
    }

    #[test]
    fn the_serial_event_rebuilds_a_64_bit_value_from_its_halves() {
        assert_eq!(object_serial(1, 0xffff_fffe), 0x0000_0001_ffff_fffe);
    }

    fn announced(node: Option<u32>, serial: Option<u64>) -> Result<(u32, Option<u64>)> {
        announced_target(StreamState {
            node,
            serial,
            ..Default::default()
        })
    }

    #[test]
    fn a_stream_is_captured_only_by_a_valid_target() {
        let serial = u64::from(u32::MAX) + 7;
        assert_eq!(
            announced(Some(42), Some(serial)).unwrap(),
            (42, Some(serial))
        );
        assert_eq!(announced(Some(42), None).unwrap(), (42, None));
        // A malformed serial fails closed rather than falling back to the node.
        assert!(announced(Some(42), Some(0)).is_err());
        assert!(announced(Some(0), None).is_err());
        assert!(announced(Some(u32::MAX), None).is_err());
        assert!(announced(None, Some(serial)).is_err());
        let refused = StreamState {
            node: Some(42),
            serial: Some(serial),
            failed: Some("no".into()),
            ..Default::default()
        };
        assert!(announced_target(refused).is_err());
    }
}
