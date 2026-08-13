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
    /// When set, this is not a capture at all: it is a file being played to the
    /// receiver in place of the screen.
    ///
    /// Modelled here rather than as a separate kind of session because every
    /// protocol already knows how to send "a source" — making a file one of
    /// those means Miracast, Cast mirroring, the resolution caps and the stop
    /// button all keep working with no changes of their own.
    pub media: Option<MediaPlayback>,
}

/// A file being played to a receiver instead of a screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaPlayback {
    pub path: std::path::PathBuf,
    pub kind: crate::media::MediaKind,
    /// The name to show when the file has no picture of its own.
    pub title: String,
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
    /// A source that plays a file rather than capturing anything.
    ///
    /// `size` is what the file will be scaled to; the receiver's own limit and
    /// the person's quality preference still apply on top of it.
    pub fn media_file(playback: MediaPlayback, size: (u32, u32)) -> Self {
        Self {
            pipewire_fd: None,
            node_id: 0,
            source_type: SourceType::Monitor,
            size: Some(size),
            media: Some(playback),
        }
    }

    /// The sound that goes with this source.
    ///
    /// A file brings its own; a screen capture takes whatever the preferences
    /// say. Deciding it here keeps the two from disagreeing — sending a film
    /// with the computer's system audio over it is not what anyone meant.
    pub fn audio_source(&self) -> crate::pipeline::AudioSource {
        match &self.media {
            Some(_) => crate::pipeline::AudioSource::MediaFile,
            None => crate::pipeline::AudioSource::detect(),
        }
    }

    pub fn video_source(&self) -> VideoSource {
        if let Some(playback) = &self.media {
            return VideoSource::MediaFile {
                path: playback.path.clone(),
                kind: playback.kind,
                title: playback.title.clone(),
            };
        }
        VideoSource::PipeWire {
            fd: self.raw_fd(),
            node_id: self.node_id,
            // A virtual monitor has no panel of its own: whatever the pipeline
            // negotiates *is* the screen's resolution, so it has to be asked
            // for explicitly. A real monitor keeps its own.
            size: (self.source_type == SourceType::Virtual)
                .then_some(self.size)
                .flatten(),
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
