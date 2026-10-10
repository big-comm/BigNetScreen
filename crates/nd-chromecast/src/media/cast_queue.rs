//! A queue the Cast receiver plays itself: each item is LOADed by URL into
//! the Default Media Receiver, which fetches it from the [`FileServer`] or
//! from the web, and reports position and state in `MEDIA_STATUS`.

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use nd_core::media::{MediaCommand, PlaybackStart, PlaybackState, seek_target};
use nd_core::{NdError, Result};

use super::{MediaItem, PHOTO_SECONDS, Queue, Shared, next_command};
use crate::cast::{CastChannel, LaunchedApp, NS_MEDIA};
use crate::file_server::{FileServer, MediaKind};

/// How long the receiver may go without reporting on the current item.
const SILENCE_TIMEOUT: Duration = Duration::from_secs(60);

#[expect(
    clippy::too_many_arguments,
    reason = "the session's channel, server and queue, passed through once"
)]
pub(super) async fn play_queue(
    channel: &CastChannel,
    app: &LaunchedApp,
    server: Option<&FileServer>,
    shared: &Shared,
    sender_name: &str,
    files: Vec<MediaItem>,
    mut commands: mpsc::Receiver<MediaCommand>,
    mut start: Option<PlaybackStart>,
) -> Result<()> {
    let mut queue = Queue::new(files);
    while let Some((index, file)) = queue.pending.front().cloned() {
        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }
        queue.publish(shared, true);
        let url = match &file {
            MediaItem::Url { uri, .. } => uri.clone(),
            MediaItem::File(_) => server
                .ok_or_else(|| NdError::Protocol("file server unavailable".into()))?
                .url(index),
        };
        let loaded = channel
            .load_item(app, &url, &file, sender_name, start)
            .await
            .map_err(|e| {
                NdError::Protocol(format!("the receiver refused {}: {e}", file.title()))
            })?;
        let item = PlayingItem {
            channel,
            app,
            session_id: loaded
                .pointer("/status/0/mediaSessionId")
                .and_then(Value::as_i64)
                .ok_or_else(|| NdError::Protocol("LOAD returned no media session".into()))?,
            url,
            photo: file.kind() == MediaKind::Photo,
        };
        if let Some(entry) = item.entry(&loaded) {
            shared.update(|status| update_playback(&mut status.playback, entry, item.photo));
        }
        let mut poll = tokio::time::interval(Duration::from_secs(1));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_status = tokio::time::Instant::now();
        let photo_end = last_status + Duration::from_secs(PHOTO_SECONDS);
        loop {
            let payload = tokio::select! {
                command = next_command(&mut commands, shared) => {
                    let Some(command) = command else { return Ok(()); };
                    if queue.edit(&command, shared) { break; }
                    match item.control(&command, shared).await {
                        Some(reply) => reply,
                        None => continue,
                    }
                }
                _ = tokio::time::sleep_until(photo_end), if item.photo => { queue.advance(); break; }
                _ = poll.tick() => item.request(json!({"type": "GET_STATUS"})).await?,
                event = channel.next_event() => {
                    let event = event.ok_or_else(|| NdError::Protocol("media channel closed".into()))?;
                    if event.namespace != NS_MEDIA { continue; }
                    event.payload
                }
                _ = tokio::time::sleep_until(last_status + SILENCE_TIMEOUT) => {
                    return Err(NdError::Protocol("media receiver stopped reporting status".into()));
                }
            };
            let Some(entry) = item.entry(&payload) else {
                continue;
            };
            last_status = tokio::time::Instant::now();
            shared.update(|status| update_playback(&mut status.playback, entry, item.photo));
            if !item.photo && has_ended(entry) {
                if entry.get("idleReason").and_then(Value::as_str) != Some("FINISHED") {
                    return Err(NdError::Protocol(
                        "receiver cancelled or failed to play the item".into(),
                    ));
                }
                queue.advance();
                break;
            }
        }
        shared.next_start(&mut start);
    }
    Ok(())
}

/// The item the receiver is playing: where its commands go, and which status
/// entries are its own.
struct PlayingItem<'a> {
    channel: &'a CastChannel,
    app: &'a LaunchedApp,
    url: String,
    session_id: i64,
    photo: bool,
}

impl PlayingItem<'_> {
    /// This item's entry in a `MEDIA_STATUS`, never an earlier item's.
    fn entry<'v>(&self, payload: &'v Value) -> Option<&'v Value> {
        active_status(payload, self.session_id, &self.url)
    }

    /// A media request for this item; `mediaSessionId` is filled in.
    async fn request(&self, mut payload: Value) -> Result<Value> {
        payload["mediaSessionId"] = json!(self.session_id);
        self.channel
            .request(NS_MEDIA, &self.app.transport_id, payload)
            .await
    }

    /// Carries out a player command, publishing why when it cannot. The
    /// receiver's confirming status, when it gave one.
    async fn control(&self, command: &MediaCommand, shared: &Shared) -> Option<Value> {
        if matches!(command, MediaCommand::Remove(_)) {
            return None;
        }
        let playback = shared
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .playback
            .clone();
        let Some(request) = control_request(command, self.session_id, &playback) else {
            shared.update(|status| {
                status.control_error =
                    Some("playback control is not available for this item".into())
            });
            return None;
        };
        let error = match self.request(request).await {
            Ok(reply) if self.entry(&reply).is_some() => {
                shared.update(|status| status.control_error = None);
                return Some(reply);
            }
            Ok(_) => "receiver rejected the playback control".to_string(),
            Err(err) => err.to_string(),
        };
        shared.update(|status| status.control_error = Some(error));
        None
    }
}

fn update_playback(playback: &mut PlaybackState, entry: &Value, photo: bool) {
    if let Some(seconds) = entry
        .get("currentTime")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v >= 0.0)
    {
        playback.seconds = seconds;
    }
    if let Some(duration) = entry
        .pointer("/media/duration")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v > 0.0)
    {
        playback.duration = Some(duration);
    }
    if let Some(state) = entry.get("playerState").and_then(Value::as_str) {
        playback.paused = state == "PAUSED";
    }
    if let Some(flags) = entry.get("supportedMediaCommands").and_then(Value::as_u64) {
        playback.can_pause = !photo && flags & 1 != 0;
        playback.can_seek = !photo && flags & 2 != 0;
        playback.volume = if !photo && flags & 4 != 0 {
            entry
                .pointer("/volume/level")
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
                .or(playback.volume)
        } else {
            None
        };
        playback.muted = if !photo && flags & 8 != 0 {
            entry
                .pointer("/volume/muted")
                .and_then(Value::as_bool)
                .or(playback.muted)
        } else {
            None
        };
    } else {
        if playback.volume.is_some() {
            playback.volume = entry
                .pointer("/volume/level")
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
                .or(playback.volume);
        }
        if playback.muted.is_some() {
            playback.muted = entry
                .pointer("/volume/muted")
                .and_then(Value::as_bool)
                .or(playback.muted);
        }
    }
}

fn control_request(
    command: &MediaCommand,
    session_id: i64,
    state: &PlaybackState,
) -> Option<Value> {
    match command {
        MediaCommand::TogglePause if state.can_pause => Some(
            json!({"type": if state.paused {"PLAY"} else {"PAUSE"}, "mediaSessionId": session_id}),
        ),
        MediaCommand::SetPaused(paused) if state.can_pause => {
            Some(json!({"type": if *paused {"PAUSE"} else {"PLAY"}, "mediaSessionId": session_id}))
        }
        MediaCommand::SeekTo(seconds) if state.can_seek => {
            Some(json!({"type": "SEEK", "mediaSessionId": session_id,
            "currentTime": seek_target(0.0, *seconds, state.duration)?}))
        }
        MediaCommand::SetVolume(level)
            if state.volume.is_some() && level.is_finite() && (0.0..=1.0).contains(level) =>
        {
            Some(
                json!({"type": "SET_VOLUME", "mediaSessionId": session_id, "volume": {"level": level}}),
            )
        }
        MediaCommand::SetMute(muted) if state.muted.is_some() => Some(
            json!({"type": "SET_VOLUME", "mediaSessionId": session_id, "volume": {"muted": muted}}),
        ),
        MediaCommand::SeekRelative(offset) if state.can_seek => {
            Some(json!({"type": "SEEK", "mediaSessionId": session_id,
            "currentTime": seek_target(state.seconds, *offset, state.duration)?}))
        }
        _ => None,
    }
}

pub(crate) fn active_status<'a>(
    payload: &'a Value,
    session_id: i64,
    url: &str,
) -> Option<&'a Value> {
    if payload.get("type").and_then(Value::as_str) != Some("MEDIA_STATUS") {
        return None;
    }
    payload.get("status")?.as_array()?.iter().find(|entry| {
        entry.get("mediaSessionId").and_then(Value::as_i64) == Some(session_id)
            && !entry
                .pointer("/media/contentId")
                .and_then(Value::as_str)
                .is_some_and(|id| id != url)
    })
}

/// Does this status entry say the item has ended?
///
/// The receiver reports the end as `playerState: IDLE` with an `idleReason`.
/// `IDLE` on its own is also what it reports before the first item starts,
/// and reading that as the end would skip the file the person asked for.
/// `FINISHED` is the end; `ERROR` and `CANCELLED` mean it will not play, and
/// waiting longer will not change that.
fn has_ended(entry: &Value) -> bool {
    entry.get("playerState").and_then(Value::as_str) == Some("IDLE")
        && matches!(
            entry.get("idleReason").and_then(Value::as_str),
            Some("FINISHED" | "ERROR" | "CANCELLED")
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receiver_updates_preserve_duration_and_use_capabilities() {
        let mut playback = PlaybackState::default();
        update_playback(
            &mut playback,
            &json!({"currentTime": 8.0, "media": {"duration": 50.0}, "playerState": "PLAYING", "supportedMediaCommands": 3}),
            false,
        );
        assert!(playback.can_seek && playback.can_pause);
        update_playback(
            &mut playback,
            &json!({"currentTime": 9.0, "playerState": "PAUSED"}),
            false,
        );
        assert_eq!(playback.duration, Some(50.0));
        assert!(playback.paused);
        let request = control_request(&MediaCommand::TogglePause, 7, &playback).unwrap();
        assert_eq!(request, json!({"type": "PLAY", "mediaSessionId": 7}));
        let request = control_request(&MediaCommand::SeekRelative(-10.0), 7, &playback).unwrap();
        assert_eq!(request["currentTime"], 0.0);
        assert!(
            request.get("resumeState").is_none(),
            "seeking preserves pause"
        );
        update_playback(&mut playback, &json!({"supportedMediaCommands": 0}), false);
        assert!(control_request(&MediaCommand::TogglePause, 7, &playback).is_none());
    }

    #[test]
    fn explicit_controls_target_only_the_owned_media_and_validate_levels() {
        let mut state = PlaybackState::default();
        update_playback(
            &mut state,
            &json!({"supportedMediaCommands": 15,
            "volume": {"level": 0.5, "muted": false}, "media": {"duration": 60.0}}),
            false,
        );
        for (command, expected) in [
            (
                MediaCommand::SetPaused(true),
                json!({"type":"PAUSE", "mediaSessionId":7}),
            ),
            (
                MediaCommand::SetPaused(false),
                json!({"type":"PLAY", "mediaSessionId":7}),
            ),
            (
                MediaCommand::SeekTo(90.0),
                json!({"type":"SEEK", "mediaSessionId":7, "currentTime":60.0}),
            ),
            (
                MediaCommand::SetVolume(0.37),
                json!({"type":"SET_VOLUME", "mediaSessionId":7, "volume":{"level":0.37}}),
            ),
            (
                MediaCommand::SetMute(true),
                json!({"type":"SET_VOLUME", "mediaSessionId":7, "volume":{"muted":true}}),
            ),
        ] {
            assert_eq!(control_request(&command, 7, &state), Some(expected));
        }
        for level in [f64::NAN, f64::INFINITY, -0.1, 1.01] {
            assert!(control_request(&MediaCommand::SetVolume(level), 7, &state).is_none());
        }
        update_playback(
            &mut state,
            &json!({"volume":{"level":0.83, "muted":true}}),
            false,
        );
        assert_eq!(state.volume, Some(0.83));
        assert_eq!(state.muted, Some(true));
        update_playback(&mut state, &json!({"supportedMediaCommands":0}), false);
        assert!(control_request(&MediaCommand::SetVolume(0.5), 7, &state).is_none());
        assert!(control_request(&MediaCommand::SetMute(false), 7, &state).is_none());
    }

    #[test]
    fn stale_status_cannot_finish_the_current_item() {
        let status = json!({"type": "MEDIA_STATUS", "status": [{"mediaSessionId": 5, "media": {"contentId": "file-one"}, "playerState": "IDLE", "idleReason": "FINISHED"}]});
        assert!(active_status(&status, 6, "file-two").is_none());
        assert!(active_status(&status, 5, "file-two").is_none());
        assert!(active_status(&status, 5, "file-one").is_some());
        assert!(active_status(&json!({"type": "PING"}), 5, "file-one").is_none());
    }

    #[test]
    fn the_end_of_an_item_is_idle_with_a_reason() {
        for reason in ["FINISHED", "ERROR", "CANCELLED"] {
            assert!(has_ended(
                &json!({"playerState": "IDLE", "idleReason": reason})
            ));
        }
    }

    #[test]
    fn idle_before_the_first_item_is_not_the_end() {
        // The receiver is IDLE when the app has just started.
        assert!(!has_ended(&json!({"playerState": "IDLE"})));
    }

    #[test]
    fn playing_and_buffering_are_not_the_end() {
        for state in ["PLAYING", "BUFFERING", "PAUSED"] {
            assert!(
                !has_ended(&json!({"playerState": state})),
                "{state} must not advance the queue"
            );
        }
        assert!(!has_ended(&Value::Null));
    }
}
