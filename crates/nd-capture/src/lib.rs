//! Backends de captura de tela.
//!
//! - [`PortalBackend`] — `ashpd` / xdg-desktop-portal `ScreenCast`. Único
//!   caminho viável sob Flatpak; funciona em qualquer desktop.
//! - [`MutterBackend`] — `org.gnome.Mutter.ScreenCast` via D-Bus. Menor
//!   latência no GNOME nativo e suporta **monitor virtual** (ideal p/ WFD).
//!
//! [`select_backend`] escolhe em runtime: Mutter quando disponível e fora de
//! sandbox; senão Portal.

use async_trait::async_trait;
use nd_core::capture::{CaptureBackend, CaptureSource, SourceType};
use nd_core::{NdError, Result};

/// Backend baseado em xdg-desktop-portal (`ashpd`). **TODO Fase 0.**
pub struct PortalBackend;

#[async_trait]
impl CaptureBackend for PortalBackend {
    fn id(&self) -> &'static str {
        "portal"
    }
    async fn is_available(&self) -> bool {
        true // o portal está presente em todo desktop moderno
    }
    async fn start(&self, _source_type: SourceType) -> Result<CaptureSource> {
        Err(NdError::Unsupported("PortalBackend: a implementar (Fase 0)".into()))
    }
    async fn stop(&self) -> Result<()> {
        Ok(())
    }
}

/// Backend baseado em `org.gnome.Mutter.ScreenCast`. **TODO Fase 1.**
pub struct MutterBackend;

#[async_trait]
impl CaptureBackend for MutterBackend {
    fn id(&self) -> &'static str {
        "mutter"
    }
    async fn is_available(&self) -> bool {
        false // implementar: checar o nome D-Bus + não estar em sandbox
    }
    async fn start(&self, _source_type: SourceType) -> Result<CaptureSource> {
        Err(NdError::Unsupported("MutterBackend: a implementar (Fase 1)".into()))
    }
    async fn stop(&self) -> Result<()> {
        Ok(())
    }
}

/// Seleciona o melhor backend disponível no ambiente atual.
pub async fn select_backend() -> Box<dyn CaptureBackend> {
    let mutter = MutterBackend;
    if mutter.is_available().await {
        Box::new(mutter)
    } else {
        Box::new(PortalBackend)
    }
}
