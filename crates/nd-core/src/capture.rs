//! Screen capture abstraction.
//!
//! Two concrete implementations (in the `nd-capture` crate) behind the same
//! trait, picked at runtime according to the environment:
//!
//! - **Portal** (`ashpd` / xdg-desktop-portal `ScreenCast`): mandatory under
//!   Flatpak, and works on any desktop (KDE, GNOME, …).
//! - **Mutter directly** (`org.gnome.Mutter.ScreenCast` over D-Bus): the
//!   lowest-latency path on native GNOME, and the one that can capture a
//!   **virtual monitor**.
//!
//! This split is what makes the "native + Flatpak" goal possible without
//! `#ifdef`s scattered through the code.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use async_trait::async_trait;

use crate::pipeline::VideoSource;
use crate::Result;

/// What is going to be captured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceType {
    /// A whole physical monitor.
    Monitor,
    /// One specific window (needs a backend with window-capture support).
    Window,
    /// A **virtual** monitor created on demand (Mutter path; ideal for WFD).
    Virtual,
}

/// A PipeWire stream ready to feed the encoding pipeline.
///
/// `node_id` is **always** required: it is what identifies the node to capture.
/// `pipewire_fd` is the remote descriptor handed over by the portal — under
/// Flatpak there is no other way. Mutter directly hands over no descriptor: the
/// node lives in the session's own PipeWire daemon, and the field is `None`.
#[derive(Debug)]
pub struct CaptureSource {
    /// The PipeWire socket descriptor, when the backend provides one.
    ///
    /// Ownership stays here: `pipewiresrc` dups the descriptor when it starts,
    /// but this value has to stay alive while the pipeline is being built.
    pub pipewire_fd: Option<OwnedFd>,
    /// The PipeWire node to consume.
    pub node_id: u32,
    /// The effective source type.
    pub source_type: SourceType,
    /// Known dimensions, when the backend reports them (None = negotiate).
    pub size: Option<(u32, u32)>,
}

impl CaptureSource {
    /// The raw descriptor, for building the `pipewiresrc` description.
    pub fn raw_fd(&self) -> Option<RawFd> {
        self.pipewire_fd.as_ref().map(|fd| fd.as_raw_fd())
    }

    /// The matching video source, with `fd` and `path` already filled in.
    ///
    /// Always going through this constructor rules out the class of bug where
    /// the pipeline was assembled without `fd=`/`path=` and captured some
    /// arbitrary node from the PipeWire daemon instead of the stream the user
    /// authorised.
    pub fn video_source(&self) -> VideoSource {
        VideoSource::PipeWire {
            fd: self.raw_fd(),
            node_id: self.node_id,
        }
    }

    /// The known resolution, or the given guess.
    pub fn size_or(&self, default: (u32, u32)) -> (u32, u32) {
        self.size.unwrap_or(default)
    }
}

/// A capture backend. Implementations must be thread-safe.
#[async_trait]
pub trait CaptureBackend: Send + Sync {
    /// Short identifier for logs/telemetry (`"portal"`, `"mutter"`).
    fn id(&self) -> &'static str;

    /// Checks whether this backend can operate in the current environment.
    async fn is_available(&self) -> bool;

    /// The source types this backend can capture in this environment.
    ///
    /// Not every portal exposes `Virtual` or `Window`; asking first avoids
    /// requesting something that makes the portal close the session without
    /// explanation.
    async fn supported_sources(&self) -> Vec<SourceType> {
        vec![SourceType::Monitor]
    }

    /// Starts the capture and returns the PipeWire stream.
    async fn start(&self, source_type: SourceType) -> Result<CaptureSource>;

    /// Ends the capture session and releases resources in the compositor.
    async fn stop(&self) -> Result<()>;
}
