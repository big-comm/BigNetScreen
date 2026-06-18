//! # nd-core
//!
//! Núcleo independente de GUI do BigNetScreen. Concentra:
//!
//! - As **abstrações** que isolam protocolos e ambientes:
//!   - [`provider::Provider`] — descoberta de receptores (Chromecast/WFD).
//!   - [`sink::Sink`] — ciclo de vida de uma sessão de cast (máquina de estados).
//!   - [`capture::CaptureBackend`] — captura de tela (portal vs Mutter direto),
//!     necessária para suportar **nativo e Flatpak** com o mesmo código.
//! - A **construção dos pipelines GStreamer** ([`pipeline`]), onde mora todo o
//!   tuning de baixa latência. Esses valores são portados do projeto C de
//!   referência (`./bkp`, branch HEAD — antes da regressão do working tree).
//!
//! Nada aqui depende de GTK: tudo é testável de forma isolada.

pub mod capture;
pub mod dummy;
pub mod error;
pub mod meta;
pub mod pipeline;
pub mod provider;
pub mod sink;

pub use error::{NdError, Result};
