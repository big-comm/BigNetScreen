//! Unified error type for the core.
//!
//! In Rust, error handling goes through `Result`, which rules out by
//! construction the "GError by value / NULL-deref" class of bug that used to
//! crash the daemon in the C project (see the audit, `nd-daemon.c:86`).

use thiserror::Error;

/// An error from any core layer.
#[derive(Debug, Error)]
pub enum NdError {
    /// Failure coming from GStreamer (init, pipeline parsing, state change).
    #[error("gstreamer: {0}")]
    Gst(String),

    /// Screen capture failure (portal/Mutter).
    #[error("capture: {0}")]
    Capture(String),

    /// Protocol failure (WFD/RTSP negotiation, Cast channel).
    #[error("protocol: {0}")]
    Protocol(String),

    /// Network failure (D-Bus, NetworkManager, firewalld, sockets).
    #[error("network: {0}")]
    Network(String),

    /// Feature unavailable in the current environment (e.g. WFD under Flatpak).
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// The receiver joined (or was paired with) and then never opened the
    /// connection back.
    ///
    /// Its own variant because it is the one failure worth **retrying**: a
    /// Miracast receiver that pairs and goes quiet almost always accepts a
    /// freshly formed group. It carries the capture back so that retrying does
    /// not put a second permission dialog in front of someone who already
    /// agreed to the first.
    // The field is `capture`, not `source`: `thiserror` reads a field called
    // `source` as the underlying error and demands it implement `Error`.
    #[error("protocol: {reason}")]
    SinkNeverConnected {
        capture: Box<crate::capture::CaptureSource>,
        reason: String,
    },

    /// Operation cancelled (e.g. the user disconnected mid-negotiation).
    #[error("cancelled")]
    Cancelled,
}

/// The crate's standard alias.
pub type Result<T> = std::result::Result<T, NdError>;
