//! Starting a cast or a file session, and clearing one that has finished.

use std::sync::Arc;

use tokio::sync::watch;

use nd_chromecast::media::{MediaItem, MediaSession};
use nd_core::capture::SourceType;
use nd_core::settings;
use nd_core::sink::{Sink, SinkKind};

use super::{Engine, Event, LINK_PROBE_INTERVAL};
use crate::wire::{Status, StatusKind, refusal};

impl Engine {
    pub(super) fn begin_cast(
        &mut self,
        id: String,
        source: SourceType,
        screen: Option<String>,
        page_text: Vec<(String, String)>,
    ) -> Result<(), String> {
        if self.active_cast.is_some() || self.media_session.is_some() {
            return Err(refusal::STREAM_RUNNING.into());
        }
        self.refused.take_if(|(refused, _)| *refused == id);

        self.media_owner = None;
        // From the file, every time: the window writes it, and the copy this
        // process read at start-up holds whatever was configured when the
        // service happened to be activated.
        self.settings = settings::reload();

        let sink: Arc<dyn Sink> = if id == nd_ndi::ID {
            if !nd_ndi::available() {
                return Err(refusal::NDI_UNAVAILABLE.into());
            }
            Arc::new(nd_ndi::NdiPublisher::new(self.settings.display_name()))
        } else if id == nd_webrtc::ID {
            if !nd_webrtc::available() {
                return Err(refusal::WEBRTC_UNAVAILABLE.into());
            }
            let name = self.settings.display_name();
            Arc::new(nd_webrtc::WebRtcPublisher::new(
                name.clone(),
                nd_core::pipeline::GpuDriver::detect(),
                crate::web_page_text(name, &page_text),
            ))
        } else if let Some(sink) = self.registry.get(&id).cloned() {
            sink
        } else {
            return Err(refusal::UNKNOWN_RECEIVER.into());
        };

        let sink_info = sink.info();
        if !sink_info.kind.is_castable() {
            return Err(refusal::NOT_CASTABLE.into());
        }

        tracing::info!(name = %sink_info.display_name, ?source, ?screen, "starting the stream");
        self.status = Status::about(StatusKind::Connecting, sink_info.display_name.clone());
        self.operation_generation += 1;
        self.probing = false;
        self.measured = None;
        self.cast_source = source;
        self.active_cast = Some(id.clone());
        self.active_sink = Some(sink.clone());
        let (cancel, cancelled) = watch::channel(false);
        self.cast_cancel = Some(cancel);

        let events = self.events.clone();
        tokio::spawn(async move {
            let outcome = crate::cast::run(sink, id.clone(), source, screen, cancelled).await;
            let _ = events
                .send(Event::CastFinished {
                    id,
                    error: outcome.error,
                    ndi_runtime_unavailable: outcome.ndi_runtime_unavailable,
                })
                .await;
        });
        Ok(())
    }

    /// Starts sending files to the receiver a client chose.
    ///
    /// The two protocols do genuinely different things here: a Chromecast is
    /// given a URL and plays the file itself, so this computer only serves
    /// bytes; a Miracast receiver is a screen and nothing else, so the file
    /// is decoded here and streamed as the picture.
    pub(super) fn send_media(
        &mut self,
        target: String,
        files: Vec<MediaItem>,
        start: Option<nd_core::media::PlaybackStart>,
    ) -> Result<(), String> {
        self.settings = settings::reload();
        let Some(sink) = self.registry.get(&target).cloned() else {
            return Err(refusal::UNKNOWN_RECEIVER.into());
        };
        // Sending files and mirroring the screen are two things the receiver
        // cannot do at once.
        if self.active_cast.is_some() || self.media_session.is_some() {
            return Err(refusal::STREAM_RUNNING.into());
        }
        self.refused.take_if(|(refused, _)| *refused == target);

        let sink_info = sink.info();
        if start.is_some() && !matches!(sink_info.kind, SinkKind::Chromecast | SinkKind::Dlna) {
            return Err("player controls require a Chromecast or DLNA receiver".into());
        }
        tracing::info!(name = %sink_info.display_name, count = files.len(), "sending files");
        self.media_owner = None;

        // A Chromecast fetches its own formats directly; anything else is
        // decoded here and streamed, the way DLNA always is.
        let fetchable = files.iter().all(|item| match item {
            MediaItem::File(file) => file.plays_natively(),
            MediaItem::Url { .. } => true,
        });
        if sink_info.kind == SinkKind::Chromecast && fetchable {
            let endpoint = sink.control_endpoint().ok_or(refusal::UNKNOWN_RECEIVER)?;
            let session = MediaSession::start(
                endpoint,
                sink_info.id.clone(),
                files,
                self.settings.port,
                self.settings.display_name(),
                start,
            )
            .map_err(|err| err.to_string())?;
            self.media_session = Some(session);
            self.media_target = Some(target);
            self.status = Status::of(StatusKind::Sending);
            self.refresh_status();
            return Ok(());
        }

        self.operation_generation += 1;
        self.probing = false;
        self.measured = None;
        // A DLNA television is asked first whether it plays the files itself.
        let session = match sink.upnp_renderer() {
            Some(renderer) => {
                MediaSession::start_upnp(sink.clone(), renderer, files, self.settings.port, start)
            }
            None => MediaSession::start_mirroring(sink.clone(), files, start),
        }
        .map_err(|err| err.to_string())?;
        self.media_target = Some(target.clone());
        self.active_cast = Some(target);
        self.active_sink = Some(sink);
        self.media_session = Some(session);
        self.status = Status::of(StatusKind::Sending);
        self.refresh_status();
        Ok(())
    }

    /// A finished queue clears itself: leaving "playing 3 of 3" published after
    /// the last file ended would be untrue within a second.
    pub(super) fn reap_finished_media(&mut self) {
        let Some(status) = self.media_session.as_ref().map(|s| s.status()) else {
            return;
        };
        if !status.finished {
            return;
        }
        self.media_session = None;
        self.media_owner = None;
        let target = self.media_target.take();
        self.active_cast = None;
        self.active_sink = None;
        self.cast_cancel = None;
        self.measured = None;
        self.probing = false;
        match status.error {
            Some(error) => {
                if error.ends_with(nd_chromecast::identity::IDENTITY_CHANGED)
                    && let Some(target) = target
                {
                    self.refused = Some((target, error.clone()));
                }
                self.status = Status::error(error);
            }
            None => {
                self.status = Status::of(StatusKind::Idle);
                self.refresh_status();
            }
        }
    }

    /// Measures the link, at most one probe at a time.
    ///
    /// Only while something is streaming, and only when the protocol has said
    /// where the receiver is: a Miracast receiver is announced by MAC and has
    /// no address at all until the Wi-Fi Direct group exists.
    pub(super) fn measure_link(&mut self) {
        if self.probing {
            return;
        }
        let Some(endpoint) = self
            .active_sink
            .as_ref()
            .and_then(|sink| sink.link())
            .and_then(|link| link.endpoint)
        else {
            return;
        };
        self.probing = true;
        let generation = self.operation_generation;
        let events = self.events.clone();
        tokio::spawn(async move {
            // Spaced out rather than run on every poll: the poll keeps the list
            // fresh, and opening a connection to the receiver two and a half
            // times a second would be rude to its firmware.
            tokio::time::sleep(LINK_PROBE_INTERVAL).await;
            let _ = events
                .send(Event::LinkMeasured(
                    generation,
                    nd_net::probe::round_trip(endpoint).await,
                ))
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use nd_chromecast::file_server::MediaFile;
    use nd_core::media::MediaCommand;

    use crate::engine::MediaOwner;
    use crate::engine::tests::engine;

    #[tokio::test]
    async fn cancelled_playback_waits_for_cleanup() {
        let mut engine = engine();
        let file = MediaFile {
            path: "/nonexistent/cancellation-test.png".into(),
            kind: nd_core::media::MediaKind::Photo,
            content_type: "image/png",
            size: 0,
        };
        engine.media_session = Some(
            MediaSession::start(
                "127.0.0.1:9".parse().unwrap(),
                "test".into(),
                vec![MediaItem::File(file)],
                0,
                "test".into(),
                None,
            )
            .unwrap(),
        );
        engine.media_owner = Some(MediaOwner {
            id: "current-session".into(),
            sender: ":1.10".into(),
            receiver: "test".into(),
        });
        for (sender, id) in [(":1.11", "current-session"), (":1.10", "previous-session")] {
            assert!(engine.control_player(sender, id, None).is_err());
            assert!(
                engine
                    .control_player(sender, id, Some(MediaCommand::SetPaused(true)))
                    .is_err()
            );
        }
        assert!(!engine.status.is(StatusKind::Stopping));
        engine
            .control_player(":1.10", "current-session", None)
            .unwrap();
        engine.refresh_status();
        assert!(engine.media_session.is_some());
        assert!(engine.status.is(StatusKind::Stopping));
        tokio::time::timeout(Duration::from_secs(2), async {
            while engine.media_session.is_some() {
                tokio::task::yield_now().await;
                engine.reap_finished_media();
            }
        })
        .await
        .expect("cancelled file session must finish cleanup");
        assert!(!engine.status.is(StatusKind::Stopping));
    }
}
