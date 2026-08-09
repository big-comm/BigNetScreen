//! Screen capture backends.
//!
//! - [`PortalBackend`] — `ashpd` / xdg-desktop-portal `ScreenCast`. The only
//!   viable path under Flatpak, and it works on any desktop.
//! - [`MutterBackend`] — `org.gnome.Mutter.ScreenCast` over D-Bus. Used for
//!   the **virtual monitor** on native GNOME, where the portal is weak.
//!
//! [`select_backend`] chooses at runtime.
//!
//! ## The restore token (a big win in perceived latency)
//!
//! The earlier version used `PersistMode::DoNot`, so the portal dialog came up
//! **every single time** the user started a cast. Now we ask for
//! `ExplicitlyRevoked` and keep the returned token; on later sessions capture
//! starts with no dialog at all. The token lives in
//! `$XDG_DATA_HOME/bignetscreen/restore-token` (mode 0600).

pub mod mutter;

use std::path::PathBuf;

use ashpd::desktop::screencast::{
    CursorMode, Screencast, SelectSourcesOptions, SourceType as PortalSourceType, StartCastOptions,
};
use ashpd::desktop::{PersistMode, Session};
use ashpd::WindowIdentifier;
use async_trait::async_trait;
use tokio::sync::Mutex;

use nd_core::capture::{CaptureBackend, CaptureSource, SourceType};
use nd_core::{NdError, Result};

pub use mutter::MutterBackend;

fn cap_err<E: std::fmt::Display>(e: E) -> NdError {
    NdError::Capture(e.to_string())
}

/// Path of the portal session's restore token.
fn restore_token_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("bignetscreen").join("restore-token"))
}

fn load_restore_token() -> Option<String> {
    let path = restore_token_path()?;
    let token = std::fs::read_to_string(path).ok()?;
    let token = token.trim().to_string();
    (!token.is_empty()).then_some(token)
}

fn store_restore_token(token: &str) {
    let Some(path) = restore_token_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        if let Err(err) = std::fs::create_dir_all(dir) {
            tracing::warn!(%err, "could not create the token directory");
            return;
        }
    }
    if let Err(err) = std::fs::write(&path, token) {
        tracing::warn!(%err, "could not save the restore token");
        return;
    }
    // The token authorises dialog-free screen capture: it must not be
    // readable by other users on the machine.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    tracing::debug!("restore token saved");
}

/// Discards the token (the portal rejected it, or the user revoked permission).
fn clear_restore_token() {
    if let Some(path) = restore_token_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// The window identifier for the portal dialog.
///
/// Without it the dialog may open parentless, non-modal and behind the app's
/// own window.
#[derive(Clone, Debug, Default)]
pub struct ParentWindow(Option<String>);

impl ParentWindow {
    /// Handle exportado via `xdg-foreign` (Wayland) ou XID (X11).
    pub fn from_handle(handle: impl Into<String>) -> Self {
        Self(Some(handle.into()))
    }

    fn identifier(&self) -> Option<WindowIdentifier> {
        self.0
            .clone()
            .map(WindowIdentifier::from_xdg_foreign_exported)
    }
}

/// Backend de captura via xdg-desktop-portal (`ashpd`).
pub struct PortalBackend {
    session: Mutex<Option<Session<Screencast>>>,
    parent: ParentWindow,
}

impl PortalBackend {
    pub fn new() -> Self {
        Self {
            session: Mutex::new(None),
            parent: ParentWindow::default(),
        }
    }

    /// Sets the parent window, so the portal dialog opens correctly.
    pub fn with_parent(parent: ParentWindow) -> Self {
        Self {
            session: Mutex::new(None),
            parent,
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

    async fn supported_sources(&self) -> Vec<SourceType> {
        let Ok(proxy) = Screencast::new().await else {
            return Vec::new();
        };
        let Ok(types) = proxy.available_source_types().await else {
            // An old portal without the property: monitor is the guaranteed minimum.
            return vec![SourceType::Monitor];
        };
        let mut out = Vec::new();
        if types.contains(PortalSourceType::Monitor) {
            out.push(SourceType::Monitor);
        }
        if types.contains(PortalSourceType::Window) {
            out.push(SourceType::Window);
        }
        if types.contains(PortalSourceType::Virtual) {
            out.push(SourceType::Virtual);
        }
        out
    }

    async fn start(&self, source_type: SourceType) -> Result<CaptureSource> {
        let proxy = Screencast::new().await.map_err(cap_err)?;

        // Asking for a type the portal does not support gets the session
        // closed with no explanation; checking first gives the user a useful
        // message.
        let supported = self.supported_sources().await;
        if !supported.contains(&source_type) {
            return Err(NdError::Unsupported(format!(
                "this desktop's portal does not offer {source_type:?} capture \
                 (available: {supported:?})"
            )));
        }

        let portal_type = match source_type {
            SourceType::Monitor => PortalSourceType::Monitor,
            SourceType::Window => PortalSourceType::Window,
            SourceType::Virtual => PortalSourceType::Virtual,
        };

        // Close any previous session before opening another: overwriting the
        // field left the old session hanging in the compositor until the app
        // died.
        self.stop().await?;

        let session = proxy
            .create_session(Default::default())
            .await
            .map_err(cap_err)?;

        // The cursor has to come embedded in the video; if the portal does
        // not support that, carry on without it rather than failing the whole
        // session.
        let cursor_mode = match proxy.available_cursor_modes().await {
            Ok(modes) if modes.contains(CursorMode::Embedded) => CursorMode::Embedded,
            Ok(modes) if modes.contains(CursorMode::Metadata) => CursorMode::Hidden,
            _ => CursorMode::Hidden,
        };

        let stored_token = load_restore_token();
        let mut options = SelectSourcesOptions::default()
            .set_cursor_mode(cursor_mode)
            .set_sources(enumflags2::BitFlags::from(portal_type))
            .set_multiple(false)
            // Authorise once, reuse afterwards: this removes the dialog that
            // came up on every cast.
            .set_persist_mode(PersistMode::ExplicitlyRevoked);
        if let Some(token) = stored_token.as_deref() {
            options = options.set_restore_token(token);
        }

        proxy
            .select_sources(&session, options)
            .await
            .map_err(cap_err)?
            .response()
            .map_err(cap_err)?;

        let identifier = self.parent.identifier();
        let response = proxy
            .start(&session, identifier.as_ref(), StartCastOptions::default())
            .await
            .map_err(cap_err)?;

        let streams = match response.response() {
            Ok(streams) => streams,
            Err(err) => {
                let _ = session.close().await;
                // Expired/revoked token: clear it so the next attempt asks for
                // permission again instead of failing forever.
                if stored_token.is_some() {
                    tracing::info!("restore token rejected; discarding it");
                    clear_restore_token();
                }
                return Err(cap_err(err));
            }
        };

        if let Some(token) = streams.restore_token() {
            store_restore_token(token);
        }

        let stream = streams
            .streams()
            .first()
            .ok_or_else(|| NdError::Capture("the portal returned no capture stream".into()))?;
        let node_id = stream.pipe_wire_node_id();
        let size = stream.size().map(|(w, h)| (w as u32, h as u32));

        let pipewire_fd = proxy
            .open_pipe_wire_remote(&session, Default::default())
            .await
            .map_err(cap_err)?;

        // The session has to outlive the capture; kept for stop().
        *self.session.lock().await = Some(session);

        tracing::info!(node_id, ?size, ?cursor_mode, "captura via portal iniciada");
        Ok(CaptureSource {
            pipewire_fd: Some(pipewire_fd),
            node_id,
            source_type,
            size,
        })
    }

    async fn stop(&self) -> Result<()> {
        if let Some(session) = self.session.lock().await.take() {
            if let Err(err) = session.close().await {
                // It may already have been closed by the compositor:
                // informational, not fatal — teardown must not fail over it.
                tracing::debug!(%err, "the portal session was already closed");
            }
        }
        Ok(())
    }
}

/// A backend that tries Mutter and falls back to the portal when it fails.
///
/// `org.gnome.Mutter.ScreenCast` is a **private GNOME API**: it carries no
/// stability guarantee across versions and may start refusing external calls.
/// Being "available" on the bus does not mean `start()` will work — which is
/// why the fall back to the portal happens at the point of failure, not only
/// in the availability check.
struct MutterWithPortalFallback {
    mutter: MutterBackend,
    portal: PortalBackend,
    /// `true` once Mutter has failed: do not keep trying.
    fell_back: std::sync::atomic::AtomicBool,
}

impl MutterWithPortalFallback {
    fn new() -> Self {
        Self {
            mutter: MutterBackend::new(),
            portal: PortalBackend::new(),
            fell_back: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn using_portal(&self) -> bool {
        self.fell_back.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[async_trait]
impl CaptureBackend for MutterWithPortalFallback {
    fn id(&self) -> &'static str {
        if self.using_portal() {
            "portal"
        } else {
            "mutter"
        }
    }

    async fn is_available(&self) -> bool {
        self.mutter.is_available().await || self.portal.is_available().await
    }

    async fn supported_sources(&self) -> Vec<SourceType> {
        if self.using_portal() {
            self.portal.supported_sources().await
        } else {
            self.mutter.supported_sources().await
        }
    }

    async fn start(&self, source_type: SourceType) -> Result<CaptureSource> {
        if !self.using_portal() {
            match self.mutter.start(source_type).await {
                Ok(source) => return Ok(source),
                Err(err) => {
                    tracing::warn!(
                        %err,
                        "Mutter capture failed; falling back to the portal \
                         (Mutter's API is private and may have changed)"
                    );
                    self.fell_back
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
        self.portal.start(source_type).await
    }

    async fn stop(&self) -> Result<()> {
        // Stopping both is safe: `stop` without `start` is a no-op on either side.
        let _ = self.mutter.stop().await;
        self.portal.stop().await
    }
}

/// Selects the capture backend that suits the environment and source type.
///
/// The portal is the default: it works on every desktop, it is mandatory under
/// Flatpak, and on GNOME it uses Mutter underneath anyway — the latency
/// difference between the two paths is nil. Talking to Mutter directly is
/// reserved for the **virtual monitor** (the one concrete win: a screen at
/// exactly the resolution the sink negotiated, with no scaling and no change
/// to the physical layout) and for whoever asks explicitly with
/// `BIGNETSCREEN_CAPTURE=mutter`.
///
/// On KDE, Sway, Cosmic and the like Mutter simply is not on the bus and this
/// path is never exercised.
pub async fn select_backend_for(source_type: SourceType) -> Box<dyn CaptureBackend> {
    let forced = std::env::var("BIGNETSCREEN_CAPTURE").ok();
    match forced.as_deref() {
        Some("portal") => return Box::new(PortalBackend::new()),
        Some("mutter") => {
            let backend = MutterWithPortalFallback::new();
            if backend.mutter.is_available().await {
                tracing::info!("capturing through Mutter (forced by BIGNETSCREEN_CAPTURE)");
                return Box::new(backend);
            }
            tracing::warn!(
                "BIGNETSCREEN_CAPTURE=mutter, but org.gnome.Mutter.ScreenCast \
                 is not on the bus; using the portal"
            );
        }
        Some(other) => {
            tracing::warn!(%other, "BIGNETSCREEN_CAPTURE desconhecido; usando o portal");
        }
        None => {}
    }

    if source_type == SourceType::Virtual {
        let backend = MutterWithPortalFallback::new();
        if backend.mutter.is_available().await {
            tracing::info!("virtual monitor: trying Mutter directly (portal as fallback)");
            return Box::new(backend);
        }
    }

    Box::new(PortalBackend::new())
}

/// A shortcut for the common case (monitor capture).
pub async fn select_backend() -> Box<dyn CaptureBackend> {
    select_backend_for(SourceType::Monitor).await
}

/// Are we running inside a Flatpak sandbox?
///
/// Inside the sandbox NetworkManager/firewalld do not exist: the app has to
/// degrade to "Chromecast only" and say so to the user.
pub fn is_sandboxed() -> bool {
    std::path::Path::new("/.flatpak-info").exists() || std::env::var_os("FLATPAK_ID").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_token_path_follows_xdg() {
        // Writes nothing; it only checks the path's shape.
        if let Some(path) = restore_token_path() {
            assert!(path.ends_with("bignetscreen/restore-token"), "{path:?}");
        }
    }

    #[test]
    fn sandbox_detection_does_not_panic() {
        let _ = is_sandboxed();
    }
}
