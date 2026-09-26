//! Driving a renderer through its `AVTransport` service.
//!
//! Four actions carry a whole session: `Stop` to clear whatever was there,
//! `SetAVTransportURI` to hand over the stream's address, `Play` to start, and
//! `GetTransportInfo` to notice when the television was turned off or the
//! viewer pressed stop on its own remote. A file transmission adds `Pause`.
//!
//! A file the renderer plays itself — sent as it is, not as our stream — adds
//! [`set_file`], [`seek`] and [`position`]: the renderer fetches it, seeks in
//! it and knows where it is, so those questions go to it.
//!
//! These AVTransport:1 actions provide no playback-buffer setting. Sender
//! queues, encoding and transport remain tunable; receiver buffering must be
//! measured per model. Optional AVTransport:3 CLOCKSYNC controls and vendor
//! extensions are not a portable way to tune an HTTP player's prebuffer.

use nd_core::stream_server::MediaType;
use nd_core::Result;

use crate::upnp::{self, Endpoint};

pub const SERVICE_TYPE: &str = "urn:schemas-upnp-org:service:AVTransport:1";

/// Where the renderer says it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportState {
    Playing,
    /// Loading, or waiting on the first bytes.
    Transitioning,
    /// Paused. Ours when we asked for it; otherwise as over as [`Self::Idle`].
    Paused,
    /// Stopped, or holding nothing. The session is over as far as we are
    /// concerned: nobody is watching our screen any more.
    Idle,
}

impl TransportState {
    fn parse(raw: &str) -> Self {
        match raw {
            "PLAYING" => TransportState::Playing,
            "TRANSITIONING" => TransportState::Transitioning,
            "PAUSED_PLAYBACK" => TransportState::Paused,
            _ => TransportState::Idle,
        }
    }
}

/// Clears whatever the renderer was doing.
///
/// Sent before `SetAVTransportURI`, not only at the end: a renderer already
/// playing something may refuse a new URI, and this is cheaper than finding
/// out from a SOAP fault. A failure here is not worth aborting for — there may
/// have been nothing to stop.
pub async fn stop(control: &Endpoint) -> Result<()> {
    upnp::soap(control, SERVICE_TYPE, "Stop", "<InstanceID>0</InstanceID>").await?;
    Ok(())
}

/// Hands the renderer the stream's address.
///
/// The metadata is not decoration. A renderer reads `protocolInfo` from it to
/// decide how to treat the body, and the same DLNA flags that go in the HTTP
/// response belong here too — one says what the response is, the other says
/// what the item is, and a renderer that sees them disagree believes the
/// metadata.
pub async fn set_uri(
    control: &Endpoint,
    url: &str,
    title: &str,
    size: Option<(u32, u32)>,
    media: MediaType,
) -> Result<()> {
    let metadata = didl_lite(url, title, size, media);
    let arguments = format!(
        "<InstanceID>0</InstanceID>\
         <CurrentURI>{url}</CurrentURI>\
         <CurrentURIMetaData>{metadata}</CurrentURIMetaData>",
        url = upnp::escape(url),
        metadata = upnp::escape(&metadata),
    );
    upnp::soap(control, SERVICE_TYPE, "SetAVTransportURI", &arguments).await?;
    Ok(())
}

/// Starts playback of the URI already set.
pub async fn play(control: &Endpoint) -> Result<()> {
    upnp::soap(
        control,
        SERVICE_TYPE,
        "Play",
        "<InstanceID>0</InstanceID><Speed>1</Speed>",
    )
    .await?;
    Ok(())
}

/// Holds the renderer on the current frame.
///
/// A paused file pipeline sends nothing, and a renderer left waiting on a
/// silent stream gives up on it within seconds; one told to pause keeps it.
pub async fn pause(control: &Endpoint) -> Result<()> {
    upnp::soap(control, SERVICE_TYPE, "Pause", "<InstanceID>0</InstanceID>").await?;
    Ok(())
}

/// Flags for a whole file served with byte ranges: streaming transfer,
/// background transfer, connection stalling (so a pause may simply stop
/// reading) and DLNA 1.5. The same as two open media servers use.
pub const FILE_FLAGS: &str = "01700000000000000000000000000000";

/// Hands the renderer a file it fetches and plays itself.
///
/// `class` is the UPnP class (`object.item.audioItem.musicTrack`), and
/// `content_type` must be one the renderer's `GetProtocolInfo` lists: it
/// decides from the metadata whether it can play the item at all.
pub async fn set_file(
    control: &Endpoint,
    url: &str,
    title: &str,
    class: &str,
    content_type: &str,
    size: u64,
    duration: Option<f64>,
) -> Result<()> {
    // Without a duration a Panasonic plays the item but reports it as
    // 0:00:00 long and 0:00:00 in, for as long as it plays.
    let mut attributes = String::new();
    if size > 0 {
        attributes.push_str(&format!(" size=\"{size}\""));
    }
    if let Some(duration) = duration {
        attributes.push_str(&format!(" duration=\"{}.000\"", clock(duration)));
    }
    let metadata = format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
         <item id=\"0\" parentID=\"0\" restricted=\"1\">\
         <dc:title>{title}</dc:title>\
         <upnp:class>{class}</upnp:class>\
         <res protocolInfo=\"http-get:*:{content_type}:DLNA.ORG_OP=01;DLNA.ORG_CI=0;\
         DLNA.ORG_FLAGS={FILE_FLAGS}\"{attributes}>{url}</res>\
         </item></DIDL-Lite>",
        title = upnp::escape(title),
        url = upnp::escape(url),
    );
    let arguments = format!(
        "<InstanceID>0</InstanceID>\
         <CurrentURI>{url}</CurrentURI>\
         <CurrentURIMetaData>{metadata}</CurrentURIMetaData>",
        url = upnp::escape(url),
        metadata = upnp::escape(&metadata),
    );
    upnp::soap(control, SERVICE_TYPE, "SetAVTransportURI", &arguments).await?;
    Ok(())
}

/// Moves the renderer's own playback to `seconds` into the item.
pub async fn seek(control: &Endpoint, seconds: f64) -> Result<()> {
    let arguments = format!(
        "<InstanceID>0</InstanceID><Unit>REL_TIME</Unit><Target>{}</Target>",
        clock(seconds)
    );
    upnp::soap(control, SERVICE_TYPE, "Seek", &arguments).await?;
    Ok(())
}

/// Where the renderer is in the item, and how long the item is when it
/// knows.
pub async fn position(control: &Endpoint) -> Result<(f64, Option<f64>)> {
    let response = upnp::soap(
        control,
        SERVICE_TYPE,
        "GetPositionInfo",
        "<InstanceID>0</InstanceID>",
    )
    .await?;
    let seconds = upnp::tag_text(&response, "RelTime")
        .and_then(parse_clock)
        .unwrap_or(0.0);
    let duration = upnp::tag_text(&response, "TrackDuration")
        .and_then(parse_clock)
        .filter(|duration| *duration > 0.0);
    Ok((seconds, duration))
}

/// `H:MM:SS`, the form `Seek` takes.
fn clock(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    format!("{}:{:02}:{:02}", total / 3600, total / 60 % 60, total % 60)
}

/// `H:MM:SS` or `H:MM:SS.fff`; `NOT_IMPLEMENTED` and the like are `None`.
fn parse_clock(text: &str) -> Option<f64> {
    let mut parts = text.trim().split(':');
    let (hours, minutes, seconds) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let value = hours.parse::<f64>().ok()? * 3600.0
        + minutes.parse::<f64>().ok()? * 60.0
        + seconds.parse::<f64>().ok()?;
    value.is_finite().then_some(value)
}

/// Asks the renderer where it is.
pub async fn transport_state(control: &Endpoint) -> Result<TransportState> {
    let response = upnp::soap(
        control,
        SERVICE_TYPE,
        "GetTransportInfo",
        "<InstanceID>0</InstanceID>",
    )
    .await?;
    Ok(upnp::tag_text(&response, "CurrentTransportState")
        .map(TransportState::parse)
        .unwrap_or(TransportState::Idle))
}

/// The item description a renderer expects alongside a URI.
///
/// Returned unescaped: it travels inside a SOAP string argument, so the caller
/// escapes it once as a whole. Escaping here as well would double it, and a
/// renderer shown `&amp;lt;` finds no item at all.
fn didl_lite(url: &str, title: &str, size: Option<(u32, u32)>, media: MediaType) -> String {
    // Resolution is optional in DIDL-Lite. It is not known until the TV's GET
    // starts capture; advertising logical portal coordinates would be false.
    let resolution = size
        .map(|(width, height)| format!(" resolution=\"{width}x{height}\""))
        .unwrap_or_default();
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
         <item id=\"0\" parentID=\"-1\" restricted=\"1\">\
         <dc:title>{title}</dc:title>\
         <upnp:class>object.item.videoItem</upnp:class>\
         <res protocolInfo=\"http-get:*:{content_type}:{features}\"{resolution}>{url}</res>\
         </item></DIDL-Lite>",
        title = upnp::escape(title),
        url = upnp::escape(url),
        content_type = media.content_type,
        features = media.dlna_features().unwrap_or("*"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use nd_core::stream_server::{DLNA_FILE_MEDIA, DLNA_MEDIA};

    #[test]
    fn a_file_stream_says_the_renderer_may_pause_it() {
        let didl = didl_lite("http://x/y", "BigNetScreen", None, DLNA_FILE_MEDIA);
        assert!(
            didl.contains("DLNA.ORG_OP=00;DLNA.ORG_FLAGS=8d300000"),
            "{didl}"
        );
        assert!(DLNA_FILE_MEDIA
            .extra_headers
            .contains("DLNA.ORG_FLAGS=8d300000"));
    }

    #[test]
    fn the_item_describes_a_live_stream_with_no_seeking() {
        let didl = didl_lite(
            "http://192.168.1.41:7236/abc",
            "BigNetScreen",
            None,
            DLNA_MEDIA,
        );
        assert!(!didl.contains("resolution="), "{didl}");
        let known = didl_lite("http://x/y", "BigNetScreen", Some((1920, 1080)), DLNA_MEDIA);
        assert!(known.contains(r#"resolution="1920x1080""#), "{known}");
        assert!(didl.contains("object.item.videoItem"), "{didl}");
        assert!(didl.contains("http-get:*:video/mpeg:"), "{didl}");
        assert!(didl.contains("DLNA.ORG_OP=00"), "{didl}");
        // The same flags the HTTP response carries: a renderer that sees the
        // two disagree believes this one.
        assert!(didl.contains("DLNA.ORG_FLAGS=8d100000"), "{didl}");
        assert!(didl.contains("<res "), "{didl}");
    }

    #[test]
    fn a_hostile_screen_name_cannot_forge_an_item() {
        // The title reaches here from the user's settings, and the URL from
        // our own server, but neither is a reason to skip escaping: one
        // unescaped `<` ends the element early and the rest is anyone's.
        let didl = didl_lite(
            "http://x/y",
            "</dc:title><upnp:class>object.item</upnp:class>",
            None,
            DLNA_MEDIA,
        );
        assert!(
            !didl.contains("</dc:title><upnp:class>object.item<"),
            "{didl}"
        );
        assert!(didl.contains("&lt;/dc:title&gt;"), "{didl}");
        // Exactly one real class element survives.
        assert_eq!(didl.matches("<upnp:class>").count(), 1, "{didl}");
    }

    #[test]
    fn clocks_go_both_ways() {
        assert_eq!(clock(3723.4), "1:02:03");
        assert_eq!(clock(-5.0), "0:00:00");
        assert_eq!(parse_clock("1:02:03"), Some(3723.0));
        assert_eq!(parse_clock("0:00:07.500"), Some(7.5));
        assert_eq!(parse_clock("NOT_IMPLEMENTED"), None);
        assert_eq!(parse_clock(""), None);
    }

    #[test]
    fn only_playing_counts_as_playing() {
        assert_eq!(TransportState::parse("PLAYING"), TransportState::Playing);
        assert_eq!(
            TransportState::parse("TRANSITIONING"),
            TransportState::Transitioning
        );
        assert_eq!(
            TransportState::parse("PAUSED_PLAYBACK"),
            TransportState::Paused
        );
        // Everything else ends the session, including values this crate has
        // never seen: guessing that an unknown state means "still fine" is how
        // a session outlives the television being switched off.
        for raw in ["STOPPED", "NO_MEDIA_PRESENT", "", "WAT"] {
            assert_eq!(TransportState::parse(raw), TransportState::Idle, "{raw}");
        }
    }
}
