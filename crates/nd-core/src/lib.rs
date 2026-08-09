//! # nd-core
//!
//! GUI-independent core of BigNetScreen. It holds:
//!
//! - The **abstractions** that isolate protocols and environments:
//!   - [`provider::Provider`] — receiver discovery (Chromecast/WFD).
//!   - [`sink::Sink`] — lifecycle of a cast session (a state machine).
//!   - [`capture::CaptureBackend`] — screen capture (portal vs. Mutter directly),
//!     which is what lets **native and Flatpak** share the same code.
//! - The **GStreamer pipeline construction** ([`pipeline`]), where all the
//!   low-latency tuning lives. Those values were ported from the reference C
//!   project (GNOME Network Displays), which this rewrite replaces.
//!
//! Nothing here depends on GTK: everything is testable in isolation.

pub mod capture;
pub mod dummy;
pub mod error;
pub mod meta;
pub mod pipeline;
pub mod provider;
pub mod radio;
pub mod sink;

pub use error::{NdError, Result};
