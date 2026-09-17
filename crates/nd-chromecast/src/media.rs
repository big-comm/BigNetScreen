//! Sending photos, films and music to a receiver.
//!
//! This is the other half of what a Chromecast does, and it is nothing like
//! mirroring. Mirroring encodes the screen in real time and fights for every
//! millisecond; here the receiver is handed a URL and plays the file *itself*,
//! decoding with its own hardware. Consequences worth stating plainly:
//!
//! - **quality is the file's**, not a re-encode of it. A film sent this way
//!   looks better than the same film mirrored, at a fraction of the CPU;
//! - **the computer is only a file server.** Closing the lid stops the
//!   playback, but nothing is being encoded while it plays;
//! - **buffering is the receiver's**, so the delay that matters for mirroring
//!   is irrelevant here. There is nothing to keep in step with.
//!
//! The queue advances on evidence rather than on a guess: for a film or a
//! track the receiver reports `IDLE`/`FINISHED` when it reaches the end, and
//! that is what moves to the next item. A photo has no end to report, so it —
//! and only it — is given a fixed time on screen.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use nd_core::{NdError, Result};

use crate::cast::{CastChannel, LaunchedApp, DEFAULT_MEDIA_RECEIVER, NS_MEDIA};
use crate::file_server::{FileServer, MediaFile, MediaKind};

/// How long a photo stays on screen before the queue moves on.
///
/// Long enough to look at, short enough that a folder of holiday pictures does
/// not need a remote control to get through.
pub const PHOTO_SECONDS: u64 = 8;

/// Maximum silence from the active media session before reporting failure.
const SILENCE_TIMEOUT: Duration = Duration::from_secs(60);

/// What the interface needs to show about the session.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaStatus {
    /// The item being played, counted from 1. `0` before the first one starts.
    pub position: usize,
    pub total: usize,
    pub title: String,
    /// The queue has run out, or playback was stopped.
    pub finished: bool,
    pub error: Option<String>,
}

#[derive(Debug)]
struct Shared {
    position: AtomicUsize,
    total: usize,
    title: Mutex<String>,
    finished: AtomicBool,
    error: Mutex<Option<String>>,
    stopping: AtomicBool,
}

/// A running "send media" session.
///
/// Dropping it stops playback and takes the file server down with it: the files
/// are only reachable for as long as the person is actually sending them.
pub struct MediaSession {
    shared: Arc<Shared>,
    _task: tokio::task::JoinHandle<()>,
    cancel: tokio::sync::watch::Sender<bool>,
}

impl Drop for MediaSession {
    fn drop(&mut self) {
        self.stop();
    }
}

impl MediaSession {
    /// Own startup immediately; playback and cleanup run cooperatively in the task.
    pub fn start(
        receiver: SocketAddr,
        files: Vec<MediaFile>,
        port: u16,
        sender_name: String,
    ) -> Result<Self> {
        if files.is_empty() {
            return Err(NdError::Protocol("no files to send".into()));
        }
        let shared = Arc::new(Shared {
            position: AtomicUsize::new(0),
            total: files.len(),
            title: Mutex::new(String::new()),
            finished: AtomicBool::new(false),
            error: Mutex::new(None),
            stopping: AtomicBool::new(false),
        });
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let task = tokio::spawn({
            let shared = shared.clone();
            async move {
                let outcome = async {
                    let server = FileServer::start(receiver.ip(), port, files).await?;
                    let channel = tokio::select! {
                        result = CastChannel::connect_to(receiver.ip(), receiver.port()) => result?,
                        _ = cancelled.changed() => return Ok(()),
                    };
                    // Finish LAUNCH so a cancellation can explicitly STOP its result.
                    let app = channel.launch(DEFAULT_MEDIA_RECEIVER).await?;
                    let outcome = if shared.stopping.load(Ordering::SeqCst) { Ok(()) } else {
                        tokio::select! {
                            result = play_queue(&channel, &app, &server, &shared, &sender_name) => result,
                            _ = cancelled.changed() => Ok(()),
                        }
                    };
                    drop(server);
                    let _ = tokio::time::timeout(Duration::from_secs(5), async {
                        let _ = channel.stop_app(&app).await;
                        channel.close().await;
                    }).await;
                    outcome
                }.await;
                if let Err(err) = outcome {
                    if !shared.stopping.load(Ordering::SeqCst) {
                        *shared.error.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(err.to_string());
                    }
                }
                shared.finished.store(true, Ordering::SeqCst);
            }
        });
        Ok(Self {
            shared,
            _task: task,
            cancel,
        })
    }

    pub fn stop(&self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        self.cancel.send_replace(true);
    }

    /// What to show about the session right now.
    pub fn status(&self) -> MediaStatus {
        MediaStatus {
            position: self.shared.position.load(Ordering::SeqCst),
            total: self.shared.total,
            title: self
                .shared
                .title
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            finished: self.shared.finished.load(Ordering::SeqCst),
            error: self
                .shared
                .error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }

    /// Has the queue run out (or been stopped)?
    pub fn is_finished(&self) -> bool {
        self.shared.finished.load(Ordering::SeqCst)
    }
}

/// Plays each file in turn, waiting for the receiver to finish with it.
async fn play_queue(
    channel: &CastChannel,
    app: &LaunchedApp,
    server: &FileServer,
    shared: &Arc<Shared>,
    sender_name: &str,
) -> Result<()> {
    for (index, file) in server.files().iter().enumerate() {
        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }
        shared.position.store(index + 1, Ordering::SeqCst);
        *shared.title.lock().unwrap_or_else(|e| e.into_inner()) = file.title();

        let url = server.url(index);
        tracing::info!(title = %file.title(), "sending an item to the receiver");
        let loaded = channel
            .load_file(app, &url, file, sender_name)
            .await
            .map_err(|e| {
                NdError::Protocol(format!("the receiver refused {}: {e}", file.title()))
            })?;

        match file.kind {
            // A photo never ends, so nothing will ever report that it did.
            MediaKind::Photo => tokio::time::sleep(Duration::from_secs(PHOTO_SECONDS)).await,
            MediaKind::Video | MediaKind::Music => {
                let session_id = loaded
                    .get("status")
                    .and_then(Value::as_array)
                    .and_then(|entries| entries.first())
                    .and_then(|entry| entry.get("mediaSessionId"))
                    .and_then(Value::as_i64)
                    .ok_or_else(|| NdError::Protocol("LOAD returned no media session".into()))?;
                wait_until_finished(channel, app, session_id, &url).await?;
            }
        }
    }
    Ok(())
}

/// Poll the active item; only its confirmed FINISHED status advances the queue.
async fn wait_until_finished(
    channel: &CastChannel,
    app: &LaunchedApp,
    session_id: i64,
    url: &str,
) -> Result<()> {
    let mut poll = tokio::time::interval(Duration::from_secs(5));
    let mut last_status = tokio::time::Instant::now();
    loop {
        let payload = tokio::select! {
            _ = poll.tick() => channel.request(NS_MEDIA, &app.transport_id,
                json!({"type": "GET_STATUS", "mediaSessionId": session_id})).await?,
            event = channel.next_event() => {
                let event = event.ok_or_else(|| NdError::Protocol("media channel closed".into()))?;
                if event.namespace != NS_MEDIA { continue; }
                event.payload
            },
            _ = tokio::time::sleep_until(last_status + SILENCE_TIMEOUT) => {
                return Err(NdError::Protocol("media receiver stopped reporting status".into()));
            }
        };
        let Some(entry) = active_status(&payload, session_id, url) else {
            continue;
        };
        last_status = tokio::time::Instant::now();
        let current = json!({"type": "MEDIA_STATUS", "status": [entry]});
        if item_has_ended(&current) {
            return if entry.get("idleReason").and_then(Value::as_str) == Some("FINISHED") {
                Ok(())
            } else {
                Err(NdError::Protocol(
                    "receiver cancelled or failed to play the item".into(),
                ))
            };
        }
    }
}

fn active_status<'a>(payload: &'a Value, session_id: i64, url: &str) -> Option<&'a Value> {
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

/// Does this `MEDIA_STATUS` say the item has ended?
///
/// The receiver reports the end as `playerState: IDLE` with an `idleReason`.
/// `IDLE` on its own is not enough — it is also what is reported *before* the
/// first item starts, and treating that as "finished" would skip straight past
/// the file the person asked for.
fn item_has_ended(payload: &Value) -> bool {
    if payload.get("type").and_then(Value::as_str) != Some("MEDIA_STATUS") {
        return false;
    }
    let Some(entries) = payload.get("status").and_then(Value::as_array) else {
        return false;
    };
    entries.iter().any(|entry| {
        let idle = entry.get("playerState").and_then(Value::as_str) == Some("IDLE");
        let reason = entry.get("idleReason").and_then(Value::as_str);
        // `FINISHED`: played to the end. `ERROR`/`CANCELLED`: it will not play,
        // and waiting longer will not change that.
        idle && matches!(reason, Some("FINISHED") | Some("ERROR") | Some("CANCELLED"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        assert!(item_has_ended(&json!({
            "type": "MEDIA_STATUS",
            "status": [{"playerState": "IDLE", "idleReason": "FINISHED"}]
        })));
    }

    #[test]
    fn idle_before_the_first_item_is_not_the_end() {
        // The receiver is IDLE when the app has just started. Reading that as
        // "finished" skipped the file the person asked for.
        assert!(!item_has_ended(&json!({
            "type": "MEDIA_STATUS",
            "status": [{"playerState": "IDLE"}]
        })));
    }

    #[test]
    fn playing_and_buffering_are_not_the_end() {
        for state in ["PLAYING", "BUFFERING", "PAUSED"] {
            assert!(
                !item_has_ended(&json!({
                    "type": "MEDIA_STATUS",
                    "status": [{"playerState": state}]
                })),
                "{state} must not advance the queue"
            );
        }
    }

    #[test]
    fn an_item_that_cannot_be_played_does_not_stall_the_queue() {
        // A file the receiver refuses mid-play reports ERROR. Waiting for a
        // FINISHED that will never come would hang the rest of the queue.
        assert!(item_has_ended(&json!({
            "type": "MEDIA_STATUS",
            "status": [{"playerState": "IDLE", "idleReason": "ERROR"}]
        })));
    }

    #[test]
    fn other_messages_are_ignored() {
        assert!(!item_has_ended(&json!({"type": "PING"})));
        assert!(!item_has_ended(&json!({"type": "MEDIA_STATUS"})));
        assert!(!item_has_ended(&Value::Null));
    }
}
