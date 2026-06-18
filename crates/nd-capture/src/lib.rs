//! Backends de captura de tela.
//!
//! - [`PortalBackend`] — `ashpd` / xdg-desktop-portal `ScreenCast`. Único
//!   caminho viável sob Flatpak; funciona em qualquer desktop. **Implementado.**
//! - [`MutterBackend`] — `org.gnome.Mutter.ScreenCast` via D-Bus. Menor
//!   latência no GNOME nativo e suporta **monitor virtual**. *TODO Fase 1.*
//!
//! [`select_backend`] escolhe em runtime: Mutter quando disponível e fora de
//! sandbox; senão Portal.

use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType as PortalSourceType};
use ashpd::desktop::{PersistMode, Session};
use ashpd::WindowIdentifier;
use async_trait::async_trait;
use tokio::sync::Mutex;

use nd_core::capture::{CaptureBackend, CaptureSource, SourceType};
use nd_core::{NdError, Result};

fn cap_err<E: std::fmt::Display>(e: E) -> NdError {
    NdError::Capture(e.to_string())
}

/// Backend de captura via xdg-desktop-portal (`ashpd`).
///
/// Guarda a `Session` do portal entre [`start`](CaptureBackend::start) e
/// [`stop`](CaptureBackend::stop). A `Session` é `(Proxy, PhantomData)` — não
/// retém o proxy criador, por isso pode viver com lifetime `'static`.
pub struct PortalBackend {
    session: Mutex<Option<Session<'static, Screencast<'static>>>>,
}

impl PortalBackend {
    pub fn new() -> Self {
        Self {
            session: Mutex::new(None),
        }
    }
}

impl Default for PortalBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CaptureBackend for PortalBackend {
    fn id(&self) -> &'static str {
        "portal"
    }

    async fn is_available(&self) -> bool {
        Screencast::new().await.is_ok()
    }

    async fn start(&self, source_type: SourceType) -> Result<CaptureSource> {
        let portal_type = match source_type {
            SourceType::Monitor => PortalSourceType::Monitor,
            SourceType::Window => PortalSourceType::Window,
            SourceType::Virtual => PortalSourceType::Virtual,
        };

        let proxy = Screencast::new().await.map_err(cap_err)?;
        let session = proxy.create_session().await.map_err(cap_err)?;

        proxy
            .select_sources(
                &session,
                CursorMode::Embedded,
                portal_type.into(),
                false, // multiple
                None,  // restore_token
                PersistMode::DoNot,
            )
            .await
            .map_err(cap_err)?;

        let streams = proxy
            .start(&session, &WindowIdentifier::default())
            .await
            .map_err(cap_err)?
            .response()
            .map_err(cap_err)?;

        let stream = streams
            .streams()
            .first()
            .ok_or_else(|| NdError::Capture("o portal não retornou nenhum stream".into()))?;
        let node_id = stream.pipe_wire_node_id();
        let size = stream.size().map(|(w, h)| (w as u32, h as u32));

        let pipewire_fd = proxy
            .open_pipe_wire_remote(&session)
            .await
            .map_err(cap_err)?;

        // Sessão precisa sobreviver à captura; guardada para o stop().
        *self.session.lock().await = Some(session);

        tracing::info!(node_id, ?size, "captura via portal iniciada");
        Ok(CaptureSource {
            pipewire_fd,
            node_id,
            source_type,
            size,
        })
    }

    async fn stop(&self) -> Result<()> {
        if let Some(session) = self.session.lock().await.take() {
            session.close().await.map_err(cap_err)?;
        }
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
        Err(NdError::Unsupported(
            "MutterBackend: a implementar (Fase 1)".into(),
        ))
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
        Box::new(PortalBackend::new())
    }
}
