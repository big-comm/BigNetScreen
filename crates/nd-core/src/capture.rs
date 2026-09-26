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

use crate::Result;
use crate::pipeline::VideoSource;

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
/// Prefer `pipewire_serial` when provided by the portal. The legacy node ID
/// is retained for older portals and the direct Mutter API.
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
    /// Stable PipeWire object.serial returned by ScreenCast portal v6+.
    pub pipewire_serial: Option<u64>,
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
    /// The smallest PipeWire buffer pool to accept, where the producer's range
    /// is known. See [`crate::pipeline::VideoSource::PipeWire::min_buffers`].
    pub min_buffers: Option<u32>,
}

/// A file being played to a receiver instead of a screen.
#[derive(Clone, Debug)]
pub struct MediaPlayback {
    pub control: Option<crate::media::FilePlaybackControl>,
    pub start: Option<crate::media::PlaybackStart>,
    pub source: crate::media::MediaSource,
    pub kind: crate::media::MediaKind,
    /// The name to show when the file has no picture of its own.
    pub title: String,
}

impl CaptureSource {
    /// The raw descriptor, for building the `pipewiresrc` description.
    pub fn raw_fd(&self) -> Option<RawFd> {
        self.pipewire_fd.as_ref().map(|fd| fd.as_raw_fd())
    }

    /// A source that plays a file rather than capturing anything.
    ///
    /// `size` is what the file will be scaled to; the receiver's own limit and
    /// the person's quality preference still apply on top of it.
    pub fn media_file(playback: MediaPlayback, size: (u32, u32)) -> Self {
        Self {
            pipewire_fd: None,
            node_id: 0,
            pipewire_serial: None,
            source_type: SourceType::Monitor,
            min_buffers: None,
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

    /// Preserve the portal-authorized descriptor and stable stream identity.
    /// A serial is never truncated or replaced by a reusable node ID.
    pub fn video_source(&self) -> VideoSource {
        if let Some(playback) = &self.media {
            return VideoSource::Media {
                source: playback.source.clone(),
                kind: playback.kind,
                title: playback.title.clone(),
            };
        }
        VideoSource::PipeWire {
            fd: self.raw_fd(),
            node_id: self.node_id,
            serial: self.pipewire_serial,
            // A virtual monitor has no panel of its own: whatever the pipeline
            // negotiates *is* the screen's resolution, so it has to be asked
            // for explicitly. A real monitor keeps its own.
            size: (self.source_type == SourceType::Virtual)
                .then_some(self.size)
                .flatten(),
            min_buffers: self.min_buffers,
        }
    }

    /// The size the portal announced, or the given guess.
    ///
    /// Known to be wrong under fractional scaling, and knowingly used anyway.
    /// The portal announces a size in the compositor's coordinate space, which
    /// the spec warns "may not be equivalent to a size in a pixel coordinate
    /// space": measured on a 2560x1440 panel at 110%, announced 2328x1310 and
    /// delivered 2560x1440. Encoding at the announced size resamples every
    /// frame away from the size it arrived in.
    ///
    /// Only the negotiated caps carry the truth, and they do not exist until
    /// the pipeline runs — after the Cast OFFER has committed to a size. A
    /// second PipeWire connection cannot be used to ask early: the portal's
    /// node is not resolvable from one, which nineteen consecutive attempts
    /// per session established before this went back to the announced size.
    /// Cast mirroring and DLNA instead use the pipeline's negotiated caps
    /// for the encoded dimensions.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_serial_reaches_pipeline_without_legacy_node_target() {
        let source = CaptureSource {
            pipewire_fd: None,
            node_id: 42,
            pipewire_serial: Some(u64::from(u32::MAX) + 123),
            source_type: SourceType::Monitor,
            min_buffers: None,
            size: Some((1920, 1080)),
            media: None,
        };
        let description = source.video_source().description();
        assert!(
            description.contains("target-object=4294967418"),
            "{description}"
        );
        assert!(!description.contains("path="), "{description}");
    }

    #[test]
    fn a_declared_size_is_preferred_over_the_caller_s_guess() {
        let source = CaptureSource::media_file(
            MediaPlayback {
                control: None,
                start: None,
                source: crate::media::MediaSource::File("/nonexistent.mp4".into()),
                kind: crate::media::MediaKind::Video,
                title: String::new(),
            },
            (1280, 720),
        );
        assert_eq!(source.size_or((1920, 1080)), (1280, 720));
    }
}
