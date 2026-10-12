//! The window's link to the session service: one D-Bus connection that reads
//! every published property, follows their changes, renews the window's lease
//! and reconnects after the service goes away.
//!
//! [`ServiceState`] holds what the service last published and converts it into
//! what the pages draw. Nothing here is translated: the service publishes
//! shapes, and [`super::wording`] turns them into sentences.

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use relm4::prelude::*;

use nd_chromecast::media::MediaStatus;
use nd_core::media::PlaybackState;
use nd_service::dbus::ServiceProxy;
use nd_service::wire::{self, StatusKind};

use super::{AppCmd, AppModel};
use crate::pages::{DeviceEntry, SessionInfo};

/// What the service last told us. Everything drawn comes from here.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ServiceState {
    pub(super) receivers: Vec<wire::Receiver>,
    pub(super) session: wire::Session,
    pub(super) link_probe: String,
    pub(super) status: wire::Status,
    pub(super) issues: Vec<wire::Issue>,
    pub(super) media: wire::Media,
    pub(super) virtual_available: bool,
}

impl Default for ServiceState {
    fn default() -> Self {
        Self {
            receivers: Vec::new(),
            session: wire::Session::idle(),
            link_probe: "pending".into(),
            status: wire::Status::of(StatusKind::Searching),
            issues: Vec::new(),
            media: wire::Media::idle(),
            virtual_available: false,
        }
    }
}

/// A proxy whose `Debug` does not print its property cache: relm4 logs every
/// command output at `info`, and `ServiceProxy`'s own `Debug` is the whole
/// cache, including session URLs and PINs.
pub struct Connection(pub(super) ServiceProxy<'static>);

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Connection")
    }
}

/// Subscribes before reading, then applies only the properties that changed.
pub(super) fn follow_the_service(sender: &ComponentSender<AppModel>) {
    sender.command(|out, shutdown| {
        shutdown.register(async move {
            let mut retry = Duration::from_secs(2);
            loop {
                // The session only ends in an error: the reason to show.
                let result: Result<std::convert::Infallible, String> = async {
                    let connection = zbus::Connection::session().await.map_err(|e| e.to_string())?;
                    let service = ServiceProxy::builder(&connection)
                        .cache_properties(zbus::proxy::CacheProperties::No)
                        .build().await.map_err(|e| e.to_string())?;
                    let properties = zbus::fdo::PropertiesProxy::builder(&connection)
                        .destination(nd_service::BUS_NAME).map_err(|e| e.to_string())?
                        .path(nd_service::OBJECT_PATH).map_err(|e| e.to_string())?
                        .build().await.map_err(|e| e.to_string())?;
                    let mut changes = properties.receive_properties_changed().await.map_err(|e| e.to_string())?;
                    let bus = zbus::fdo::DBusProxy::new(&connection).await.map_err(|e| e.to_string())?;
                    let mut owners = bus.receive_name_owner_changed_with_args(&[(0, nd_service::BUS_NAME)])
                        .await.map_err(|e| e.to_string())?;
                    let mut state = read_state(&properties).await?;
                    retry = Duration::from_secs(2);
                    out.send(AppCmd::Connected(Box::new(Connection(service.clone())))).map_err(|_| "window closed")?;
                    out.send(AppCmd::State(Box::new(state.clone()))).map_err(|_| "window closed")?;
                    let mut heartbeat = tokio::time::interval(Duration::from_secs(60));
                    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    loop {
                        tokio::select! {
                            change = changes.next() => {
                                let change = change.ok_or("service subscription ended")?;
                                let args = change.args().map_err(|e| e.to_string())?;
                                if args.interface_name().as_str() != nd_service::dbus::INTERFACE { continue; }
                                if args.invalidated_properties().is_empty() {
                                    for (name, value) in args.changed_properties() {
                                        state.apply_property(name, value.try_to_owned().map_err(|e| e.to_string())?)?;
                                    }
                                } else {
                                    state = read_state(&properties).await?;
                                }
                                out.send(AppCmd::State(Box::new(state.clone()))).map_err(|_| "window closed")?;
                            }
                            owner = owners.next() => {
                                let owner = owner.ok_or("session bus subscription ended")?;
                                let args = owner.args().map_err(|e| e.to_string())?;
                                if args.new_owner().is_none() || args.old_owner().is_some() {
                                    return Err("sharing service disconnected".into());
                                }
                            }
                            _ = heartbeat.tick() => {
                                tokio::time::timeout(Duration::from_secs(5), service.keep_alive()).await
                                    .map_err(|_| "sharing service did not answer")?
                                    .map_err(|e| e.to_string())?;
                            }
                        }
                    }
                }.await;
                let Err(reason) = result;
                if out.send(AppCmd::Unreachable(reason)).is_err() { return; }
                tokio::time::sleep(retry).await;
                retry = (retry * 2).min(Duration::from_secs(30));
            }
        }).drop_on_shutdown()
    });
}

impl ServiceState {
    fn apply_property(
        &mut self,
        name: &str,
        value: zbus::zvariant::OwnedValue,
    ) -> Result<(), String> {
        use zbus::zvariant::Error;
        match name {
            "Receivers" => self.receivers = value.try_into().map_err(|e: Error| e.to_string())?,
            "Session" => self.session = value.try_into().map_err(|e: Error| e.to_string())?,
            "LinkProbe" => self.link_probe = value.try_into().map_err(|e: Error| e.to_string())?,
            "Status" => self.status = value.try_into().map_err(|e: Error| e.to_string())?,
            "Issues" => self.issues = value.try_into().map_err(|e: Error| e.to_string())?,
            "Media" => self.media = value.try_into().map_err(|e: Error| e.to_string())?,
            "VirtualAvailable" => {
                self.virtual_available = value.try_into().map_err(|e: Error| e.to_string())?
            }
            _ => {}
        }
        Ok(())
    }

    /// The receivers, as the pages want them.
    pub(super) fn entries(&self) -> Vec<DeviceEntry> {
        self.receivers.iter().map(DeviceEntry::from_wire).collect()
    }

    /// What is streaming right now.
    pub(super) fn session_info(&self) -> Option<SessionInfo> {
        let session = &self.session;
        if session.is_idle() {
            return None;
        }
        let kind = wire::kind_from_name(&session.kind);
        let round_trip = (self.link_probe == "available").then_some(session.round_trip_ms);
        Some(SessionInfo {
            id: session.id.clone(),
            name: session.display_name.clone(),
            kind,
            protocol: kind
                .map(crate::pages::protocol_label)
                .unwrap_or_else(|| session.kind.clone()),
            address: session.address.clone(),
            // The mode the two ends negotiated, empty until they have: the
            // preference would misstate a link that settled lower.
            mode: crate::pages::describe_mode(
                session.width,
                session.height,
                session.fps,
                session.receivers,
            ),
            state: wire::state_from_name(&session.state)
                .unwrap_or(nd_core::sink::SinkState::Disconnected),
            measurable: session.measurable,
            probe_failed: self.link_probe == "unreachable",
            round_trip_ms: round_trip,
            access: (!session.url.is_empty()).then(|| nd_core::sink::SinkAccess {
                url: session.url.clone(),
                pin: session.pin.clone(),
            }),
        })
    }

    /// The receivers that can be sent a file, for the person to choose from.
    /// None is preselected: a file playing on a device is visible to whoever
    /// stands in front of it, so it takes an explicit choice every time.
    pub(super) fn media_receivers(&self) -> Vec<(String, String)> {
        self.receivers
            .iter()
            .filter(|receiver| wire::can_receive_files(receiver))
            .map(|receiver| (receiver.id.clone(), receiver.display_name.clone()))
            .collect()
    }

    /// The playback the media page draws, from the service's [`wire::Media`].
    pub(super) fn media_status(&self) -> MediaStatus {
        let media = &self.media;
        MediaStatus {
            position: media.index as usize,
            total: media.total as usize,
            title: media.title.clone(),
            queue: media.queue.iter().map(PathBuf::from).collect(),
            playback: PlaybackState {
                paused: media.paused,
                seconds: media.position_ms as f64 / 1000.0,
                duration: (media.duration_ms > 0).then(|| media.duration_ms as f64 / 1000.0),
                can_pause: media.can_pause,
                can_seek: media.can_seek,
                ..Default::default()
            },
            finished: media.finished,
            error: (!media.detail.is_empty()).then(|| media.detail.clone()),
            control_error: (!media.control_detail.is_empty()).then(|| media.control_detail.clone()),
        }
    }
}

/// Reads every published property at once. A missing property fails the
/// connection instead of drawing its default as if the service had said so.
async fn read_state(properties: &zbus::fdo::PropertiesProxy<'_>) -> Result<ServiceState, String> {
    let values = tokio::time::timeout(
        Duration::from_secs(5),
        properties.get_all(
            nd_service::dbus::INTERFACE
                .try_into()
                .map_err(|e: zbus::names::Error| e.to_string())?,
        ),
    )
    .await
    .map_err(|_| "sharing service did not answer")?
    .map_err(|e| e.to_string())?;
    let mut state = ServiceState::default();
    for required in [
        "Receivers",
        "Session",
        "Status",
        "Issues",
        "Media",
        "VirtualAvailable",
    ] {
        if !values.contains_key(required) {
            return Err(format!("service is missing {required}"));
        }
    }
    for (name, value) in values {
        state.apply_property(&name, value)?;
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_reaches_page_complete() {
        let media = wire::Media {
            active: true,
            title: "Song".into(),
            queue: vec!["/music/a.mp3".into(), "/music/b.mp3".into()],
            index: 1,
            total: 2,
            position_ms: 30_500,
            duration_ms: 240_000,
            paused: true,
            can_pause: true,
            can_seek: false,
            finished: false,
            detail: String::new(),
            control_detail: "seek refused".into(),
        };
        let state = ServiceState {
            media,
            ..ServiceState::default()
        };
        let status = state.media_status();
        assert_eq!(status.queue.len(), 2);
        assert_eq!(status.playback.seconds, 30.5);
        assert_eq!(status.playback.duration, Some(240.0));
        assert!(status.playback.can_pause && !status.playback.can_seek);
        assert_eq!(status.error, None);
        assert_eq!(status.control_error.as_deref(), Some("seek refused"));
    }
}
