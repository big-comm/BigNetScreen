//! Commands for the Default Media Receiver: the live MPEG-TS stream of the
//! HTTP path ([`crate::session`]) and files from this computer
//! ([`crate::media`]).

use serde_json::{Value, json};

use nd_core::{NdError, Result};

use super::{CastChannel, LaunchedApp, NS_MEDIA};

impl CastChannel {
    /// Tells the receiver app to load a media URL (our HTTP stream).
    pub async fn load_media(
        &self,
        app: &LaunchedApp,
        url: &str,
        content_type: &str,
    ) -> Result<Value> {
        let response = self
            .request(
                NS_MEDIA,
                &app.transport_id,
                json!({
                    "type": "LOAD",
                    "sessionId": app.session_id,
                    "autoplay": true,
                    "currentTime": 0,
                    "media": {
                        "contentId": url,
                        "contentType": content_type,
                        // Without this the receiver buffers the screen as if it
                        // were an on-demand video.
                        "streamType": "LIVE",
                    },
                }),
            )
            .await?;
        if response.get("type").and_then(Value::as_str) != Some("MEDIA_STATUS") {
            return Err(NdError::Protocol(
                "receiver did not acknowledge the live media LOAD".into(),
            ));
        }
        Ok(response)
    }

    /// Tells the receiver app to play a file from this computer.
    ///
    /// [`Self::load_media`] describes the live stream instead. Two
    /// differences decide whether the file plays properly:
    ///
    /// - `streamType: BUFFERED`. A file has a beginning and an end, so the
    ///   receiver may buffer ahead and offer a position bar. Declared `LIVE`,
    ///   as mirroring is, the receiver refuses to seek and shows no duration;
    /// - metadata. Without it the receiver shows a bare URL (the token and
    ///   an index). `metadataType` 0 is the
    ///   generic one, understood by every receiver.
    pub async fn load_item(
        &self,
        app: &LaunchedApp,
        url: &str,
        file: &crate::media::MediaItem,
        sender_name: &str,
        start: Option<nd_core::media::PlaybackStart>,
    ) -> Result<Value> {
        let response = self
            .request(
                NS_MEDIA,
                &app.transport_id,
                json!({
                    "type": "LOAD",
                    "sessionId": app.session_id,
                    "autoplay": start.is_none(),
                    "currentTime": start.map(|s| s.seconds).unwrap_or(0.0),
                    "media": {
                        "contentId": url,
                        "contentType": file.content_type(),
                        "streamType": "BUFFERED",
                        "metadata": {
                            "metadataType": 0,
                            "title": file.title(),
                            "subtitle": sender_name,
                        },
                    },
                }),
            )
            .await?;

        // A refusal comes back as a message, not as a transport error: without
        // this check the queue would move on believing the item was playing.
        if response.get("type").and_then(Value::as_str) == Some("LOAD_FAILED") {
            let reason = response
                .get("detailedErrorCode")
                .map(|c| c.to_string())
                .unwrap_or_else(|| "no reason given".to_string());
            return Err(NdError::Unsupported(format!(
                "the receiver could not play this file (error {reason})"
            )));
        }
        if response.get("type").and_then(Value::as_str) != Some("MEDIA_STATUS") {
            return Err(NdError::Protocol(
                "receiver did not acknowledge the media LOAD".into(),
            ));
        }
        match start {
            Some(start) => self.apply_start(app, url, file, &response, start).await,
            None => Ok(response),
        }
    }

    /// Applies the requested start to an item loaded paused: volume and mute
    /// before the first sound, then PLAY unless it starts paused. Each step
    /// must be confirmed by the receiver's status for this item.
    async fn apply_start(
        &self,
        app: &LaunchedApp,
        url: &str,
        file: &crate::media::MediaItem,
        loaded: &Value,
        start: nd_core::media::PlaybackStart,
    ) -> Result<Value> {
        let mut response = loaded.clone();
        let session = loaded
            .pointer("/status/0/mediaSessionId")
            .and_then(Value::as_i64)
            .ok_or_else(|| NdError::Protocol("LOAD returned no media session".into()))?;
        if file.kind() != nd_core::media::MediaKind::Photo {
            response = self
                .request(
                    NS_MEDIA,
                    &app.transport_id,
                    json!({
                        "type": "SET_VOLUME", "mediaSessionId": session,
                        "volume": {"level": start.volume, "muted": start.muted},
                    }),
                )
                .await?;
            let entry = crate::media::active_status(&response, session, url).ok_or_else(|| {
                NdError::Protocol("receiver did not confirm initial volume".into())
            })?;
            if entry.pointer("/volume/muted").and_then(Value::as_bool) != Some(start.muted)
                || !entry
                    .pointer("/volume/level")
                    .and_then(Value::as_f64)
                    .is_some_and(|level| (level - start.volume).abs() < 0.001)
            {
                return Err(NdError::Unsupported(
                    "receiver did not apply initial volume and mute".into(),
                ));
            }
        }
        if !start.paused {
            response = self
                .request(
                    NS_MEDIA,
                    &app.transport_id,
                    json!({"type":"PLAY", "mediaSessionId": session}),
                )
                .await?;
            if crate::media::active_status(&response, session, url).is_none() {
                return Err(NdError::Protocol(
                    "receiver did not acknowledge playback".into(),
                ));
            }
        }
        Ok(response)
    }
}
