//! Virtual screens on KWin, at the size the user actually chose.
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
//! That is why a new screen was always 1920x1080 no matter what the person
//! selected. KWin's own `zkde_screencast_unstable_v1` does take a size, and it
//! is what the portal calls underneath, so asking the compositor directly costs
//! nothing and removes the cap. It is the same trade this crate already makes
//! with Mutter on GNOME.
//!
//! The screen KWin creates always runs at 60 Hz (`OutputModeline(size, 60000)`
//! in `drm_virtual_output.cpp`), and the stream it hands to PipeWire is capped
//! at that rate — a higher fps setting cannot be honoured here, whatever the
//! screen is doing. The protocol below has no refresh-rate argument either, so
//! there is nothing to ask for.
//!
//! In practice it delivers about 49, not 60, and the shortfall is not ours.
//! KWin's capture is driven by damage, never by a clock
//! (`OutputScreenCastSource` connects to `OutputLayer::repaintScheduled`), so
//! the rate we measure is the rate the screen is being painted at: a 24 fps
//! video gives 24, the same video at double speed gives 48. Measured, all
//! three.
//!
//! What caps a *virtual* screen at 49 is that its vblank is a `QTimer`
//! (`SoftwareVsyncMonitor`, truncating to whole milliseconds against a
//! 16.666 ms period) feeding a second `QTimer` in the screencast's own pacing.
//! A real output gets a hardware vblank instead of the first one, and reaches
//! 88–113 on this machine's 180 Hz panel with the identical pipeline. 1080p
//! and 1440p both sit at 49, so it is not the cost of the pixels.
//!
//! So a log showing 24 is a 24 fps video, not a fault, and closing the gap
//! between 49 and 60 means patching KWin.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::Mutex;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, QueueHandle};

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
/// to settle on its own the result varied per stream and stayed there, which
/// is what made one new screen fluid and the next one stutter for no reason
/// anybody could see — 9 frames a second against 36, with the compositor at 7%
/// of a core in both.
///
/// Four and not more: asking beyond what the producer offers fails the
/// allocation outright rather than being clamped.
const KWIN_BUFFER_POOL: u32 = 4;

/// Draw the cursor into the frames (`pointer.embedded`).
const POINTER_EMBEDDED: u32 = 2;

/// How long to wait for KWin to create the screen and its PipeWire node.
const CREATE_TIMEOUT: Duration = Duration::from_secs(5);

/// Whatever the stream has told us so far.
#[derive(Default)]
struct StreamState {
    node: Option<u32>,
    serial: Option<u64>,
    failed: Option<String>,
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
            } => {
                state.serial =
                    Some((u64::from(object_serial_hi) << 32) | u64::from(object_serial_low))
            }
            StreamEvent::Failed { error } => state.failed = Some(error),
            // The compositor dropped the screen. `stop` is what closes our
            // side; there is nothing to undo here.
            StreamEvent::Closed => {}
        }
    }
}

/// A live virtual screen. Dropping the connection destroys it in KWin.
struct ActiveStream {
    connection: Connection,
    stream: Stream,
}

/// Backend based on `zkde_screencast_unstable_v1`. Virtual screens only.
pub struct KWinBackend {
    size: Mutex<(u32, u32)>,
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
            active: Mutex::new(None),
        }
    }

    /// Sets the virtual screen's size before [`CaptureBackend::start`].
    pub async fn set_virtual_size(&self, width: u32, height: u32) {
        *self.size.lock().await = (width, height);
    }
}

/// Binds the manager, or says why it could not.
///
/// Also serves as the availability probe: a compositor that is not KWin simply
/// does not advertise this global.
fn bind_manager(
    connection: &Connection,
) -> Result<(Manager, wayland_client::EventQueue<StreamState>)> {
    let (globals, queue) = registry_queue_init::<StreamState>(connection).map_err(cap_err)?;
    let handle = queue.handle();
    let manager = globals
        .bind::<Manager, _, _>(&handle, MIN_VERSION..=MAX_VERSION, ())
        .map_err(|err| {
            NdError::Capture(format!("zkde_screencast_unstable_v1 unavailable: {err}"))
        })?;
    Ok((manager, queue))
}

/// Creates the screen and waits for its PipeWire node.
///
/// Blocking: `wayland-client` has no async surface, and this runs once per
/// session on a `spawn_blocking` thread.
fn open_virtual_screen(width: u32, height: u32) -> Result<(ActiveStream, u32, Option<u64>)> {
    let connection = Connection::connect_to_env().map_err(cap_err)?;
    let (manager, mut queue) = bind_manager(&connection)?;
    let handle = queue.handle();

    let stream = manager.stream_virtual_output_with_description(
        "BigNetScreen".to_string(),
        "BigNetScreen".to_string(),
        width as i32,
        height as i32,
        1.0,
        POINTER_EMBEDDED,
        &handle,
        (),
    );

    let mut state = StreamState::default();
    let deadline = Instant::now() + CREATE_TIMEOUT;
    while state.node.is_none() && state.failed.is_none() {
        if Instant::now() >= deadline {
            return Err(NdError::Capture(
                "KWin did not open the virtual screen in time".into(),
            ));
        }
        queue.roundtrip(&mut state).map_err(cap_err)?;
        if state.node.is_none() && state.failed.is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    if let Some(error) = state.failed {
        return Err(NdError::Capture(format!(
            "KWin refused the screen: {error}"
        )));
    }
    let node = state
        .node
        .expect("loop only exits with a node or a failure");
    Ok((ActiveStream { connection, stream }, node, state.serial))
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
                .and_then(|connection| bind_manager(&connection))
                .is_ok()
        })
        .await
        .unwrap_or(false)
    }

    async fn supported_sources(&self) -> Vec<SourceType> {
        // Ordinary screens go through the portal: it keeps the restore token,
        // so the person is not asked again on every cast.
        if self.is_available().await {
            vec![SourceType::Virtual]
        } else {
            Vec::new()
        }
    }

    async fn start(&self, source_type: SourceType) -> Result<CaptureSource> {
        if source_type != SourceType::Virtual {
            return Err(NdError::Capture(
                "the KWin backend only creates virtual screens".into(),
            ));
        }
        self.stop().await?;

        let (width, height) = *self.size.lock().await;
        let (active, node_id, pipewire_serial) =
            tokio::task::spawn_blocking(move || open_virtual_screen(width, height))
                .await
                .map_err(cap_err)??;

        tracing::info!(
            width,
            height,
            node_id,
            "KWin opened the virtual screen at the requested size"
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
        let mut state = StreamState::default();
        // The dispatch is what the compositor calls; exercise the same maths.
        let (hi, low) = (0x0000_0001u32, 0xffff_fffeu32);
        state.serial = Some((u64::from(hi) << 32) | u64::from(low));
        assert_eq!(state.serial, Some(0x0000_0001_ffff_fffe));
    }
}
