//! # nd-core
//!
//! The GUI-free core of BigNetScreen, shared by every protocol crate, the
//! session service and the window:
//!
//! - the traits that isolate protocols and environments:
//!   [`provider::Provider`] discovers receivers, [`sink::Sink`] runs a session
//!   to one, and [`capture::CaptureBackend`] opens the screen through the
//!   portal or Mutter, so native and Flatpak builds share the same code;
//! - GStreamer pipeline construction ([`pipeline`]), where the encoding and
//!   latency policy for every transmission path lives;
//! - the preferences ([`settings`], [`latency`]) and the HTTP server Cast and
//!   DLNA receivers fetch the stream from ([`stream_server`]).

pub mod capture;
pub mod dummy;
pub mod error;
pub mod latency;
pub mod media;
pub mod meta;
pub mod persistence;
pub mod pipeline;
pub mod provider;
pub mod radio;
pub mod settings;
pub mod sink;
pub mod stream_server;
pub mod virtual_sink;

pub use error::{NdError, Result};
