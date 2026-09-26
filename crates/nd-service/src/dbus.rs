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
use nd_chromecast::media::MediaItem;
use nd_core::media::{MediaCommand, PlaybackStart};

use crate::engine::{self, Command};
use crate::wire::{Issue, Media, PlayerSession, Receiver, Session, Status};
use crate::{BUS_NAME, OBJECT_PATH};

/// The interface name. The trailing `1` is the version, as D-Bus convention
/// has it: a second incompatible shape becomes `…BigNetScreen2` and both can
/// be served at once while clients catch up.
pub const INTERFACE: &str = "br.com.biglinux.BigNetScreen1";

async fn inspect_media(paths: Vec<String>) -> zbus::fdo::Result<Vec<MediaFile>> {
    inspect_with(paths, MediaFile::inspect).await
}

/// A player session may also take files that only the decoding path plays.
async fn inspect_media_for_player(paths: Vec<String>) -> zbus::fdo::Result<Vec<MediaFile>> {
    inspect_with(paths, MediaFile::inspect_decodable).await
}

async fn inspect_with(
    paths: Vec<String>,
    inspect: fn(&std::path::Path) -> Result<MediaFile, String>,
) -> zbus::fdo::Result<Vec<MediaFile>> {
    if paths.is_empty() || paths.len() > nd_core::media::MAX_FILES {
        return Err(zbus::fdo::Error::InvalidArgs(
            "select between 1 and 1000 files".into(),
        ));
    }
    let files = tokio::task::spawn_blocking(move || {
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            let file = inspect(std::path::Path::new(&path))
                .map_err(|err| zbus::fdo::Error::InvalidArgs(format!("{path}: {err}")))?;
            files.push(file);
        }
        Ok::<_, zbus::fdo::Error>(files)
    })
    .await
    .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))??;
    Ok(files)
}

fn media_command(command: &str, argument: f64, path: &str) -> zbus::fdo::Result<MediaCommand> {
    Ok(match command {
        "toggle-pause" => MediaCommand::TogglePause,
        "pause" => MediaCommand::SetPaused(true),
        "play" => MediaCommand::SetPaused(false),
        "seek-to" if argument.is_finite() && argument >= 0.0 => MediaCommand::SeekTo(argument),
        "volume" if argument.is_finite() && (0.0..=1.0).contains(&argument) => {
            MediaCommand::SetVolume(argument)
        }
        "mute" => MediaCommand::SetMute(true),
        "unmute" => MediaCommand::SetMute(false),
        "next" => MediaCommand::Next,
        "seek" if argument.is_finite() => MediaCommand::SeekRelative(argument),
        "remove" => MediaCommand::Remove(path.into()),
        other => {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "unknown media command `{other}`"
            )));
        }
    })
}

struct Service {
    engine: engine::Handle,
}

impl Service {
    async fn start_items(
        &self,
        target: &str,
        files: Vec<MediaItem>,
        start: (f64, bool, f64, bool, u32),
        header: zbus::message::Header<'_>,
        connection: &zbus::Connection,
    ) -> zbus::fdo::Result<String> {
        let start = PlaybackStart {
            seconds: start.0,
            paused: start.1,
            volume: start.2,
            muted: start.3,
            height: start.4,
        }
        .validate()
        .map_err(|err| zbus::fdo::Error::InvalidArgs(err.to_string()))?;
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::AccessDenied("missing sender".into()))?
            .to_string();
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.engine
            .send(Command::StartPlayer {
                sender: sender.clone(),
                target: target.into(),
                files,
                start,
                reply,
            })
            .await
            .map_err(zbus::fdo::Error::Failed)?;
        let id = answer
            .await
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))?
            .map_err(zbus::fdo::Error::Failed)?;
        // The caller may disappear during file inspection, before the engine
        // records ownership. Close that gap after the start acknowledgement.
        let bus = zbus::fdo::DBusProxy::new(connection).await?;
        let name = sender
            .as_str()
            .try_into()
            .map_err(|err: zbus::names::Error| zbus::fdo::Error::Failed(err.to_string()))?;
        if !bus.name_has_owner(name).await? {
            self.engine
                .send(Command::PlayerGone(sender))
                .await
                .map_err(zbus::fdo::Error::Failed)?;
            return Err(zbus::fdo::Error::Failed(
                "player disconnected while starting".into(),
            ));
        }
        Ok(id)
    }
}

#[zbus::interface(name = "br.com.biglinux.BigNetScreen1")]
impl Service {
    /// Keeps discovery available while an interactive client is open.
    async fn keep_alive(&self) -> zbus::fdo::Result<()> {
        self.engine
            .send(Command::KeepAlive)
            .await
            .map_err(zbus::fdo::Error::Failed)
    }
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
        let (reply, result) = tokio::sync::oneshot::channel();
        self.engine
            .send(Command::SetAutoDiscovery(on, reply))
            .await
            .map_err(zbus::fdo::Error::Failed)?;
        result
            .await
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))?
            .map_err(zbus::fdo::Error::Failed)
    }

    /// Re-reads the preferences file and applies what changed.
    async fn reload_settings(&self) -> zbus::fdo::Result<()> {
        let settings = tokio::task::spawn_blocking(nd_core::settings::reload)
            .await
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))?;
        let (reply, result) = tokio::sync::oneshot::channel();
        self.engine
            .send(Command::ApplySettings(Box::new(settings), reply))
            .await
            .map_err(zbus::fdo::Error::Failed)?;
        result
            .await
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))?
            .map_err(zbus::fdo::Error::Failed)
    }

    /// Sends files to a receiver. Paths are read here, so a caller does not
    /// have to know which of them this receiver can play.
    async fn send_media(&self, target: &str, paths: Vec<String>) -> zbus::fdo::Result<()> {
        let files = inspect_media(paths).await?;
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
        let command = media_command(command, argument, path)?;
        self.engine
            .send(Command::ControlMedia(command))
            .await
            .map_err(zbus::fdo::Error::Failed)
    }

    /// Starts only while idle and returns a session id unique across restarts.
    async fn start_player(
        &self,
        target: &str,
        paths: Vec<String>,
        start: (f64, bool, f64, bool, u32),
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::fdo::Result<String> {
        let files = inspect_media_for_player(paths)
            .await?
            .into_iter()
            .map(MediaItem::File)
            .collect();
        self.start_items(target, files, start, header, connection)
            .await
    }

    // zbus injects the header and connection in addition to the public arguments.
    #[allow(clippy::too_many_arguments)]
    async fn start_player_url(
        &self,
        target: &str,
        uri: String,
        content_type: String,
        title: String,
        audio: bool,
        start: (f64, bool, f64, bool, u32),
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::fdo::Result<String> {
        let item = MediaItem::url(uri, content_type, title, audio)
            .map_err(|err| zbus::fdo::Error::InvalidArgs(err.to_string()))?;
        self.start_items(target, vec![item], start, header, connection)
            .await
    }

    /// A stale id or another application's D-Bus connection cannot stop or
    /// control a replacement session. The sender comes from the bus header.
    async fn control_player(
        &self,
        id: &str,
        command: &str,
        argument: f64,
        path: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::AccessDenied("missing sender".into()))?
            .to_string();
        let command = if command == "stop" {
            None
        } else {
            Some(media_command(command, argument, path)?)
        };
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.engine
            .send(Command::ControlPlayer {
                sender,
                id: id.into(),
                command,
                reply,
            })
            .await
            .map_err(zbus::fdo::Error::Failed)?;
        answer
            .await
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))?
            .map_err(zbus::fdo::Error::Failed)
    }

    #[zbus(property)]
    /// 2: the start tuple carries the frame height as a fifth field.
    async fn player_api(&self) -> u32 {
        2
    }

    #[zbus(property)]
    async fn player_session(&self) -> PlayerSession {
        let snapshot = self.engine.snapshot();
        let owner = snapshot.media_owner.as_ref();
        PlayerSession {
            id: owner.map(|owner| owner.id.clone()).unwrap_or_default(),
            owner: owner.map(|owner| owner.sender.clone()).unwrap_or_default(),
            receiver: owner
                .map(|owner| owner.receiver.clone())
                .unwrap_or_default(),
            volume: snapshot.media_volume.unwrap_or(1.0),
            muted: snapshot.media_muted.unwrap_or(false),
            can_volume: snapshot.media_volume.is_some(),
            can_mute: snapshot.media_muted.is_some(),
        }
    }

    #[zbus(property)]
    async fn receivers(&self) -> Vec<Receiver> {
        self.engine.snapshot().receivers.clone()
    }

    #[zbus(property)]
    async fn session(&self) -> Session {
        self.engine.snapshot().session.clone()
    }

    #[zbus(property)]
    async fn link_probe(&self) -> String {
        self.engine.snapshot().link_probe.clone()
    }

    #[zbus(property)]
    async fn status(&self) -> Status {
        self.engine.snapshot().status.clone()
    }

    #[zbus(property)]
    async fn issues(&self) -> Vec<Issue> {
        self.engine.snapshot().issues.clone()
    }

    #[zbus(property)]
    async fn media(&self) -> Media {
        self.engine.snapshot().media.clone()
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
    fn keep_alive(&self) -> zbus::Result<()>;
    fn cast(&self, id: &str, source: &str, page_text: &[(String, String)]) -> zbus::Result<()>;
    fn stop(&self) -> zbus::Result<()>;
    fn rescan(&self) -> zbus::Result<()>;
    fn set_auto_discovery(&self, on: bool) -> zbus::Result<()>;
    fn reload_settings(&self) -> zbus::Result<()>;
    fn send_media(&self, target: &str, paths: &[String]) -> zbus::Result<()>;
    fn start_player(
        &self,
        target: &str,
        paths: &[String],
        start: (f64, bool, f64, bool, u32),
    ) -> zbus::Result<String>;
    fn start_player_url(
        &self,
        target: &str,
        uri: &str,
        content_type: &str,
        title: &str,
        audio: bool,
        start: (f64, bool, f64, bool, u32),
    ) -> zbus::Result<String>;
    fn control_player(
        &self,
        id: &str,
        command: &str,
        argument: f64,
        path: &str,
    ) -> zbus::Result<()>;
    #[zbus(property)]
    fn player_api(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn player_session(&self) -> zbus::Result<PlayerSession>;
    fn control_media(&self, command: &str, argument: f64, path: &str) -> zbus::Result<()>;

    #[zbus(property)]
    fn receivers(&self) -> zbus::Result<Vec<Receiver>>;
    #[zbus(property)]
    fn session(&self) -> zbus::Result<Session>;
    #[zbus(property)]
    fn link_probe(&self) -> zbus::Result<String>;
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
    let owner_engine = engine.clone();
    let connection = zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, Service { engine })?
        .build()
        .await?;

    let bus = zbus::fdo::DBusProxy::new(&connection).await?;
    let mut owners = bus.receive_name_owner_changed().await?;
    tokio::spawn(async move {
        while let Some(change) = owners.next().await {
            let Ok(args) = change.args() else {
                continue;
            };
            if args.new_owner().is_none()
                && args.name().as_str().starts_with(':')
                && owner_engine
                    .send(Command::PlayerGone(args.name().to_string()))
                    .await
                    .is_err()
            {
                break;
            }
        }
    });

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
            if current.link_probe != previous.link_probe {
                let _ = interface.link_probe_changed(context).await;
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
            if current.media_owner != previous.media_owner
                || current.media_volume != previous.media_volume
                || current.media_muted != previous.media_muted
            {
                let _ = interface.player_session_changed(context).await;
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
/// forever". Sessions and explicit client keepalives extend its lifetime.
/// Discovery updates do not: receivers can keep appearing while no client
/// needs the service, and resetting the deadline for each update prevents exit.
pub async fn idle_after(
    mut snapshots: tokio::sync::watch::Receiver<engine::Snapshot>,
    grace: std::time::Duration,
) {
    fn idle(snapshot: &engine::Snapshot) -> bool {
        snapshot.session.is_idle() && !snapshot.media.active
    }

    loop {
        while !idle(&snapshots.borrow()) {
            if snapshots.changed().await.is_err() {
                return;
            }
        }
        let mut deadline = tokio::time::Instant::now() + grace;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    if idle(&snapshots.borrow()) {
                        return;
                    }
                    break;
                }
                result = snapshots.changed() => {
                    if result.is_err() {
                        return;
                    }
                    if !idle(&snapshots.borrow()) {
                        break;
                    }
                    if let Some(activity) = snapshots.borrow().last_client_activity {
                        deadline = deadline.max(activity + grace);
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

    #[tokio::test(start_paused = true)]
    async fn an_open_client_keeps_discovery_alive_until_it_leaves() {
        let (tx, rx) = watch::channel(idle_snapshot());
        let waiting = tokio::spawn(idle_after(rx, Duration::from_secs(10)));
        tokio::task::yield_now().await;
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(8)).await;
            tx.send_modify(|state| state.last_client_activity = Some(tokio::time::Instant::now()));
            tokio::task::yield_now().await;
            assert!(!waiting.is_finished());
        }
        tokio::time::advance(Duration::from_secs(11)).await;
        waiting.await.unwrap();
    }
}
