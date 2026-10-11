//! Screen capture backends.
//!
//! - [`PortalBackend`]: `ashpd` / xdg-desktop-portal `ScreenCast`. The only
//!   viable path under Flatpak, and it works on any desktop.
//! - [`MutterBackend`]: `org.gnome.Mutter.ScreenCast` over D-Bus. Used for
//!   the virtual monitor on native GNOME, which the portal cannot size.
//! - [`KWinBackend`]: `zkde_screencast_unstable_v1` over Wayland, on KWin and
//!   big-compositor: existing screens without a portal dialog, and virtual
//!   screens at the chosen size, where the KDE portal makes every one
//!   1920x1080.
//!
//! [`select_backend_for`] chooses at runtime; `nd-service` starts and stops
//! the backend around each cast.
//!
//! Every backend hands over the PipeWire stream as a [`CaptureSource`]. The
//! portal's FD and its `pipewire-serial` travel together, and the serial, when
//! present, is the only target: a malformed one fails the capture rather than
//! falling back to a node id that PipeWire may have reused.
//!
//! A monitor capture asks the portal to persist its authorization and keeps
//! the returned restore token, so later casts start with no dialog. The
//! token lives in `$XDG_DATA_HOME/bignetscreen/restore-token-<kind>`, private
//! to the user.

pub mod display_config;
pub mod kwin;
pub mod mutter;
mod portal_start;

use std::path::PathBuf;

use ashpd::desktop::screencast::{
    CursorMode, Screencast, SelectSourcesOptions, SourceType as PortalSourceType,
};
use ashpd::desktop::{PersistMode, Session};
use async_trait::async_trait;
use tokio::sync::Mutex;

use nd_core::capture::{CaptureBackend, CaptureSource, SourceType};
use nd_core::{NdError, Result};

pub use kwin::{KWinBackend, Screen};
pub use mutter::MutterBackend;

fn cap_err<E: std::fmt::Display>(e: E) -> NdError {
    NdError::Capture(e.to_string())
}

/// Does a selection of this kind keep its authorisation between casts?
///
/// Sharing "the whole screen" means the same screen every time, so asking again
/// on each cast is noise. Sharing "a window" is a choice, and it is usually a
/// different one: restoring it skipped the picker and silently cast the window
/// from the previous run.
fn restores_selection(source_type: SourceType) -> bool {
    source_type != SourceType::Window
}

/// Where the restore token for a given source kind lives.
///
/// The kind is part of the file name on purpose. A token belongs to the
/// selection that produced it, and handing a window's token to a screen
/// request would restore that window instead.
fn restore_token_path(source_type: SourceType) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    let kind = match source_type {
        SourceType::Monitor => "monitor",
        SourceType::Virtual => "virtual",
        SourceType::Window => "window",
    };
    Some(
        base.join("bignetscreen")
            .join(format!("restore-token-{kind}")),
    )
}

fn load_restore_token(source_type: SourceType) -> Option<String> {
    let path = restore_token_path(source_type)?;
    let token = std::fs::read_to_string(path).ok()?;
    let token = token.trim().to_string();
    (!token.is_empty()).then_some(token)
}

fn store_restore_token(source_type: SourceType, token: &str) {
    let Some(path) = restore_token_path(source_type) else {
        return;
    };
    if let Some(dir) = path.parent()
        && let Err(err) = std::fs::create_dir_all(dir)
    {
        tracing::warn!(%err, "could not create the token directory");
        return;
    }
    if let Err(err) = nd_core::persistence::write_private(&path, token.as_bytes()) {
        tracing::warn!(%err, "could not save the restore token");
        return;
    }
    tracing::debug!("restore token saved");
}

/// Discards the token (the portal rejected it, or the user revoked permission).
fn clear_restore_token(source_type: SourceType) {
    if let Some(path) = restore_token_path(source_type) {
        let _ = std::fs::remove_file(path);
    }
}

/// What `SelectSources` asks for: one source of `source_type`, persisted only
/// when [`restores_selection`] says so, restoring `token` when there is one.
fn selection_options(
    source_type: SourceType,
    cursor_mode: CursorMode,
    token: Option<&str>,
) -> SelectSourcesOptions {
    let portal_type = match source_type {
        SourceType::Monitor => PortalSourceType::Monitor,
        SourceType::Window => PortalSourceType::Window,
        SourceType::Virtual => PortalSourceType::Virtual,
    };
    let options = SelectSourcesOptions::default()
        .set_cursor_mode(cursor_mode)
        .set_sources(enumflags2::BitFlags::from(portal_type))
        .set_multiple(false)
        .set_persist_mode(if restores_selection(source_type) {
            PersistMode::ExplicitlyRevoked
        } else {
            PersistMode::DoNot
        });
    match token {
        Some(token) => options.set_restore_token(token),
        None => options,
    }
}

/// Capture backend through xdg-desktop-portal (`ashpd`).
pub struct PortalBackend {
    session: Mutex<Option<std::sync::Arc<Session<Screencast>>>>,
}

impl PortalBackend {
    pub fn new() -> Self {
        Self {
            session: Mutex::new(None),
        }
    }

    /// Opens a session and its stream. The session is recorded before
    /// anything can fail, so [`CaptureBackend::stop`] closes it.
    async fn open(&self, proxy: &Screencast, source_type: SourceType) -> Result<CaptureSource> {
        let session = proxy
            .create_session(Default::default())
            .await
            .map_err(cap_err)?;
        let session = std::sync::Arc::new(session);
        *self.session.lock().await = Some(session.clone());

        // The cursor has to come embedded in the video; if the portal does
        // not support that, carry on without it rather than failing the whole
        // session.
        let cursor_mode = match proxy.available_cursor_modes().await {
            Ok(modes) if modes.contains(CursorMode::Embedded) => CursorMode::Embedded,
            _ => CursorMode::Hidden,
        };

        let restorable = restores_selection(source_type);
        let stored_token = restorable
            .then(|| load_restore_token(source_type))
            .flatten();
        let options = selection_options(source_type, cursor_mode, stored_token.as_deref());
        proxy
            .select_sources(&session, options)
            .await
            .map_err(cap_err)?
            .response()
            .map_err(cap_err)?;

        // ashpd 0.13 drops unknown stream properties during deserialization.
        // Read Start's documented dictionary so pipewire-serial survives.
        let streams = portal_start::start(proxy, &session)
            .await
            .inspect_err(|_| {
                // An expired or revoked token fails every later attempt too:
                // discarding it makes the next one ask for permission again.
                if stored_token.is_some() {
                    tracing::info!("restore token rejected; discarding it");
                    clear_restore_token(source_type);
                }
            })?;
        if restorable && let Some(token) = streams.restore_token.as_deref() {
            store_restore_token(source_type, token);
        }

        let pipewire_fd = proxy
            .open_pipe_wire_remote(&session, Default::default())
            .await
            .map_err(cap_err)?;

        tracing::info!(
            node_id = streams.node_id,
            pipewire_serial = ?streams.serial,
            size = ?streams.size,
            ?cursor_mode,
            "portal capture started"
        );
        Ok(CaptureSource {
            pipewire_fd: Some(pipewire_fd),
            node_id: streams.node_id,
            pipewire_serial: streams.serial,
            source_type,
            size: streams.size,
            // The portal fronts whatever compositor is there, and their
            // advertised buffer ranges are not all in front of us. The element
            // default stands; asking for more than a producer offers fails the
            // allocation rather than being clamped.
            min_buffers: None,
            media: None,
        })
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

        // A session left open stays in the compositor until the app exits.
        self.stop().await?;
        let started = self.open(&proxy, source_type).await;
        if started.is_err() {
            let _ = self.stop().await;
        }
        started
    }

    async fn stop(&self) -> Result<()> {
        if let Some(session) = self.session.lock().await.take()
            && let Err(err) = session.close().await
        {
            // It may already have been closed by the compositor:
            // informational, not fatal — teardown must not fail over it.
            tracing::debug!(%err, "the portal session was already closed");
        }
        Ok(())
    }
}

/// A backend that tries Mutter and falls back to the portal when it fails.
///
/// `org.gnome.Mutter.ScreenCast` is a private GNOME API: it carries no
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
/// Flatpak, and on GNOME it uses Mutter underneath anyway. A compositor that
/// grants `zkde_screencast` (big-compositor, KWin) streams an existing
/// screen directly, `screen` naming its connector, and the person is asked
/// nothing. A virtual monitor goes to the compositor whenever it can, since
/// no portal can size one. `BIGNETSCREEN_CAPTURE=portal` or `=mutter` forces
/// a path. Window capture is always the portal's.
pub async fn select_backend_for(
    source_type: SourceType,
    virtual_limit: Option<(u32, u32)>,
    screen: Option<String>,
) -> Box<dyn CaptureBackend> {
    let forced = std::env::var("BIGNETSCREEN_CAPTURE").ok();
    match forced.as_deref() {
        Some("portal") => return Box::new(PortalBackend::new()),
        Some("mutter") => {
            let backend = MutterWithPortalFallback::new();
            let size = virtual_size(virtual_limit);
            backend.mutter.set_virtual_size(size.0, size.1).await;
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
            tracing::warn!(%other, "unknown BIGNETSCREEN_CAPTURE value; using the portal");
        }
        None => {}
    }

    if source_type == SourceType::Virtual {
        let size = virtual_size(virtual_limit);

        let backend = MutterWithPortalFallback::new();
        backend.mutter.set_virtual_size(size.0, size.1).await;
        if backend.mutter.is_available().await {
            tracing::info!("virtual monitor: trying Mutter directly (portal as fallback)");
            return Box::new(backend);
        }

        let kwin = KWinBackend::new();
        kwin.set_virtual_size(size.0, size.1).await;
        if kwin.is_available().await {
            tracing::info!(
                width = size.0,
                height = size.1,
                "virtual monitor: asking the compositor over zkde_screencast"
            );
            return Box::new(kwin);
        }
    }

    if source_type == SourceType::Monitor {
        let kwin = KWinBackend::new();
        if kwin.is_available().await {
            tracing::info!(?screen, "screen: streaming it over zkde_screencast");
            kwin.set_screen(screen).await;
            return Box::new(kwin);
        }
    }

    Box::new(PortalBackend::new())
}

/// The screens a capture can name, when the compositor streams them directly;
/// empty where the portal asks the person instead.
pub async fn screens() -> Vec<Screen> {
    if is_sandboxed() {
        return Vec::new();
    }
    tokio::task::spawn_blocking(kwin::screens)
        .await
        .ok()
        .and_then(|result| result.ok())
        .unwrap_or_default()
}

/// How big to make a virtual screen.
///
/// The person's preference, held down to what the protocol can actually carry.
/// Miracast tops out at 1920x1200 whatever the sink is, so a larger desktop is
/// not extra detail on the receiver — it is a `videoscale` on every frame and a
/// bigger screen to composite, for a picture that arrives the same size.
fn virtual_size(limit: Option<(u32, u32)>) -> (u32, u32) {
    let (width, height) = nd_core::settings::current().resolution_limit();
    match limit {
        Some((max_width, max_height)) if width > max_width || height > max_height => {
            tracing::info!(
                width,
                height,
                max_width,
                max_height,
                "the protocol cannot carry the chosen size; making the virtual screen smaller"
            );
            (width.min(max_width), height.min(max_height))
        }
        _ => (width, height),
    }
}

/// A shortcut for the common case (monitor capture).
pub async fn select_backend() -> Box<dyn CaptureBackend> {
    select_backend_for(SourceType::Monitor, None, None).await
}

/// Are we running inside a Flatpak sandbox?
///
/// Inside the sandbox NetworkManager/firewalld do not exist: the app has to
/// degrade to "Chromecast only" and say so to the user.
pub fn is_sandboxed() -> bool {
    std::path::Path::new("/.flatpak-info").exists() || std::env::var_os("FLATPAK_ID").is_some()
}

/// Can a capture target this PipeWire object?
///
/// A present serial decides alone, and 0 never names a stream, so a malformed
/// one fails closed instead of falling back to the reusable node id. Without
/// a serial, node 0 (the PipeWire core) and `u32::MAX` (`SPA_ID_INVALID`) name
/// no stream either.
pub(crate) fn is_valid_pipewire_target(node_id: u32, serial: Option<u64>) -> bool {
    match serial {
        Some(serial) => serial != 0,
        None => node_id != 0 && node_id != u32::MAX,
    }
}

/// The GStreamer source fragment for a PipeWire node in the session's own
/// daemon (no descriptor to pass).
pub(crate) fn pipeline_source(node_id: u32) -> String {
    nd_core::pipeline::VideoSource::PipeWire {
        fd: None,
        node_id,
        serial: None,
        size: None,
        min_buffers: None,
    }
    .description()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_screen_is_worth_restoring() {
        assert!(restores_selection(SourceType::Monitor));
        assert!(restores_selection(SourceType::Virtual));
        assert!(!restores_selection(SourceType::Window));
    }

    #[test]
    fn a_token_is_never_offered_to_a_different_kind_of_source() {
        // The bug this pins down: one token file for every source kind. A
        // window session stored its token, and the next screen cast handed that
        // token back to the portal, which restored the window.
        let paths: Vec<_> = [SourceType::Monitor, SourceType::Virtual, SourceType::Window]
            .into_iter()
            .filter_map(restore_token_path)
            .collect();
        assert_eq!(paths.len(), 3, "every source kind needs somewhere to store");
        let unique: std::collections::HashSet<_> = paths.iter().collect();
        assert_eq!(unique.len(), paths.len(), "the paths must not collide");
    }

    #[test]
    fn restore_token_path_follows_xdg() {
        // Writes nothing; it only checks the path's shape.
        if let Some(path) = restore_token_path(SourceType::Monitor) {
            assert!(
                path.ends_with("bignetscreen/restore-token-monitor"),
                "{path:?}"
            );
        }
    }
}
