//! The D-Bus face of the service, and the proxy its clients use.
//!
//! D-Bus and not a socket of our own: it is what a panel applet already
//! speaks, from QML or JavaScript, with no library from us; it gives us
//! start-on-demand for free, so nothing runs while nothing is sharing; and
//! `zbus` is already in this tree for the portal and NetworkManager.
//!
//! Preferences stay where they were — a file `nd_core::settings` owns. A client
//! that changes them writes the file and calls [`ServiceProxy::reload_settings`],
//! which is one method instead of a second copy of every field.

use futures::StreamExt;

use nd_chromecast::file_server::MediaFile;
use nd_core::media::MediaCommand;

use crate::engine::{self, Command};
use crate::wire::{Issue, Media, Receiver, Session, Status};
use crate::{BUS_NAME, OBJECT_PATH};

/// The interface name. The trailing `1` is the version, as D-Bus convention
/// has it: a second incompatible shape becomes `…BigNetScreen2` and both can
/// be served at once while clients catch up.
pub const INTERFACE: &str = "br.com.biglinux.BigNetScreen1";

struct Service {
    engine: engine::Handle,
}

#[zbus::interface(name = "br.com.biglinux.BigNetScreen1")]
impl Service {
    /// Starts sharing to a receiver.
    ///
    /// `source` is `monitor`, `window` or `virtual`. `page_text` is for the
    /// browser page of a web share and may be empty; see
    /// [`crate::web_page_text`].
    async fn cast(
        &self,
        id: &str,
        source: &str,
        page_text: Vec<(String, String)>,
    ) -> zbus::fdo::Result<()> {
        let source = engine::source_from_name(source)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("unknown source `{source}`")))?;
        self.engine
            .cast(id.to_string(), source, page_text)
            .await
            .map_err(zbus::fdo::Error::Failed)
    }

    /// Ends whatever is running: the stream, the file sending, or both.
    async fn stop(&self) -> zbus::fdo::Result<()> {
        self.engine
            .send(Command::Stop)
            .await
            .map_err(zbus::fdo::Error::Failed)
    }

    async fn rescan(&self) -> zbus::fdo::Result<()> {
        self.engine
            .send(Command::Rescan)
            .await
            .map_err(zbus::fdo::Error::Failed)
    }

    async fn set_auto_discovery(&self, on: bool) -> zbus::fdo::Result<()> {
        self.engine
            .send(Command::SetAutoDiscovery(on))
            .await
            .map_err(zbus::fdo::Error::Failed)
    }

    /// Re-reads the preferences file and applies what changed.
    async fn reload_settings(&self) -> zbus::fdo::Result<()> {
        self.engine
            .send(Command::ApplySettings(
                Box::new(nd_core::settings::reload()),
            ))
            .await
            .map_err(zbus::fdo::Error::Failed)
    }

    /// Sends files to a receiver. Paths are read here, so a caller does not
    /// have to know which of them this receiver can play.
    async fn send_media(&self, target: &str, paths: Vec<String>) -> zbus::fdo::Result<()> {
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            let file = MediaFile::inspect(std::path::Path::new(&path))
                .map_err(|err| zbus::fdo::Error::InvalidArgs(format!("{path}: {err}")))?;
            files.push(file);
        }
        self.engine
            .send_media(target.to_string(), files)
            .await
            .map_err(zbus::fdo::Error::Failed)
    }

    /// `toggle-pause`, `next`, `seek` (with `argument` in seconds, signed) or
    /// `remove` (with `path`).
    async fn control_media(
        &self,
        command: &str,
        argument: f64,
        path: &str,
    ) -> zbus::fdo::Result<()> {
        let command = match command {
            "toggle-pause" => MediaCommand::TogglePause,
            "next" => MediaCommand::Next,
            "seek" => MediaCommand::SeekRelative(argument),
            "remove" => MediaCommand::Remove(path.into()),
            other => {
                return Err(zbus::fdo::Error::InvalidArgs(format!(
                    "unknown media command `{other}`"
                )))
            }
        };
        self.engine
            .send(Command::ControlMedia(command))
            .await
            .map_err(zbus::fdo::Error::Failed)
    }

    #[zbus(property)]
    async fn receivers(&self) -> Vec<Receiver> {
        self.engine.snapshot().receivers
    }

    #[zbus(property)]
    async fn session(&self) -> Session {
        self.engine.snapshot().session
    }

    #[zbus(property)]
    async fn status(&self) -> Status {
        self.engine.snapshot().status
    }

    #[zbus(property)]
    async fn issues(&self) -> Vec<Issue> {
        self.engine.snapshot().issues
    }

    #[zbus(property)]
    async fn media(&self) -> Media {
        self.engine.snapshot().media
    }

    /// Can this desktop make a screen that exists only for the cast?
    #[zbus(property)]
    async fn virtual_available(&self) -> bool {
        self.engine.snapshot().virtual_available
    }
}

/// The client side. Generated from the same interface, so the two cannot drift.
#[zbus::proxy(
    interface = "br.com.biglinux.BigNetScreen1",
    default_service = "br.com.biglinux.BigNetScreen.Service",
    default_path = "/br/com/biglinux/BigNetScreen"
)]
pub trait Service {
    fn cast(&self, id: &str, source: &str, page_text: &[(String, String)]) -> zbus::Result<()>;
    fn stop(&self) -> zbus::Result<()>;
    fn rescan(&self) -> zbus::Result<()>;
    fn set_auto_discovery(&self, on: bool) -> zbus::Result<()>;
    fn reload_settings(&self) -> zbus::Result<()>;
    fn send_media(&self, target: &str, paths: &[String]) -> zbus::Result<()>;
    fn control_media(&self, command: &str, argument: f64, path: &str) -> zbus::Result<()>;

    #[zbus(property)]
    fn receivers(&self) -> zbus::Result<Vec<Receiver>>;
    #[zbus(property)]
    fn session(&self) -> zbus::Result<Session>;
    #[zbus(property)]
    fn status(&self) -> zbus::Result<Status>;
    #[zbus(property)]
    fn issues(&self) -> zbus::Result<Vec<Issue>>;
    #[zbus(property)]
    fn media(&self) -> zbus::Result<Media>;
    #[zbus(property)]
    fn virtual_available(&self) -> zbus::Result<bool>;
}

/// Serves the interface on the session bus until the process ends.
///
/// Claiming the name is what makes D-Bus activation work: a client calls the
/// name, the bus starts this, and the call lands on the object below.
pub async fn serve(engine: engine::Handle) -> zbus::Result<zbus::Connection> {
    let connection = zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, Service { engine })?
        .build()
        .await?;

    // A property is only useful to an applet if it says when it changed.
    // Everything published comes from one snapshot, so one task watching it
    // emits every notification there is.
    let emitter = connection
        .object_server()
        .interface::<_, Service>(OBJECT_PATH)
        .await?;
    let mut snapshots = emitter.get().await.engine.subscribe();
    tokio::spawn(async move {
        let mut previous = snapshots.borrow().clone();
        while snapshots.changed().await.is_ok() {
            let current = snapshots.borrow().clone();
            let interface = emitter.get().await;
            let context = emitter.signal_emitter();
            if current.receivers != previous.receivers {
                let _ = interface.receivers_changed(context).await;
            }
            if current.session != previous.session {
                let _ = interface.session_changed(context).await;
            }
            if current.status != previous.status {
                let _ = interface.status_changed(context).await;
            }
            if current.issues != previous.issues {
                let _ = interface.issues_changed(context).await;
            }
            if current.media != previous.media {
                let _ = interface.media_changed(context).await;
            }
            if current.virtual_available != previous.virtual_available {
                let _ = interface.virtual_available_changed(context).await;
            }
            previous = current;
        }
    });
    Ok(connection)
}

/// Returns once the service has had nothing to do for `grace`.
///
/// This is what keeps "start on demand" from meaning "start once and stay
/// forever". A cast or a file transfer holds the process open; an idle window
/// does not, because an idle window is exactly the case this exists to stop
/// paying for — the bus starts another process the moment it is wanted again.
///
/// The deadline is fixed when the service falls idle and only a **busy**
/// snapshot moves it. Restarting it on every change looked equivalent and was
/// not: receivers come and go while nothing is being shared, and a service that
/// reset its timer on each of those never left at all. Measured, after this
/// shipped claiming otherwise.
pub async fn idle_after(
    mut snapshots: tokio::sync::watch::Receiver<engine::Snapshot>,
    grace: std::time::Duration,
) {
    fn idle(snapshot: &engine::Snapshot) -> bool {
        snapshot.session.is_idle() && !snapshot.media.active
    }

    loop {
        while !idle(&snapshots.borrow().clone()) {
            if snapshots.changed().await.is_err() {
                return;
            }
        }
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    if idle(&snapshots.borrow().clone()) {
                        return;
                    }
                    break;
                }
                result = snapshots.changed() => {
                    if result.is_err() {
                        return;
                    }
                    if !idle(&snapshots.borrow().clone()) {
                        break;
                    }
                }
            }
        }
    }
}

/// Follows the service's state, for a client that wants to react rather than poll.
pub async fn watch_status(
    proxy: &ServiceProxy<'_>,
    mut on_change: impl FnMut(Status),
) -> zbus::Result<()> {
    let mut changes = proxy.receive_status_changed().await;
    while let Some(change) = changes.next().await {
        if let Ok(status) = change.get().await {
            on_change(status);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::watch;

    fn idle_snapshot() -> engine::Snapshot {
        engine::Snapshot::initial(nd_core::settings::Settings::default())
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_service_leaves_even_while_receivers_come_and_go() {
        // The bug this exists for: the deadline used to restart on every
        // change, and receivers appear and vanish the whole time a machine is
        // idle. The service never left, which is the entire cost this design
        // was meant to avoid.
        let (tx, rx) = watch::channel(idle_snapshot());
        let churn = tokio::spawn(async move {
            for n in 0..50u32 {
                tokio::time::sleep(Duration::from_millis(400)).await;
                let mut snapshot = idle_snapshot();
                snapshot.status = crate::wire::Status::found(n % 3);
                if tx.send(snapshot).is_err() {
                    return;
                }
            }
        });
        tokio::time::timeout(
            Duration::from_secs(30),
            idle_after(rx, Duration::from_secs(10)),
        )
        .await
        .expect("an idle service must leave while discovery churns");
        churn.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_running_session_keeps_the_service_alive() {
        let (tx, rx) = watch::channel(idle_snapshot());
        let mut busy = idle_snapshot();
        busy.session.id = "receiver".into();
        tx.send(busy).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            idle_after(rx, Duration::from_secs(10)),
        )
        .await;
        assert!(result.is_err(), "a live session must hold the service open");
    }
}
