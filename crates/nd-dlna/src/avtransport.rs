//! Driving a renderer through its `AVTransport` service.
//!
//! Four actions carry a whole session: `Stop` to clear whatever was there,
//! `SetAVTransportURI` to hand over the stream's address, `Play` to start, and
//! `GetTransportInfo` to notice when the television was turned off or the
//! viewer pressed stop on its own remote.
//!
//! There is deliberately nothing here about buffering. The renderer's entire
//! UPnP surface was read off a real television — every action and every state
//! variable of `AVTransport`, `RenderingControl` and `ConnectionManager` — and
//! it holds no control over playback delay, prebuffer or latency of any kind.
//! Unlike Cast, which negotiates a `targetDelay` we set to zero, DLNA gives the
//! sender no say: the receiver buffers what it decides to buffer. Measured at
//! roughly 1.5 s on a Panasonic VIErA over Ethernet, once the stream's bitrate
//! was high enough to fill its byte-counted buffer quickly. That is the floor,
//! and it is the reason this protocol is offered for watching rather than for
//! working on the big screen.

use nd_core::Result;

use crate::upnp::{self, Endpoint};

pub const SERVICE_TYPE: &str = "urn:schemas-upnp-org:service:AVTransport:1";

/// Where the renderer says it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportState {
    Playing,
    /// Loading, or waiting on the first bytes.
    Transitioning,
    /// Stopped, paused, or holding nothing. All mean the session is over as
    /// far as we are concerned: nobody is watching our screen any more.
    Idle,
}

impl TransportState {
    fn parse(raw: &str) -> Self {
        match raw {
            "PLAYING" => TransportState::Playing,
            "TRANSITIONING" => TransportState::Transitioning,
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
pub async fn set_uri(control: &Endpoint, url: &str, title: &str, size: (u32, u32)) -> Result<()> {
    let metadata = didl_lite(url, title, size);
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
fn didl_lite(url: &str, title: &str, size: (u32, u32)) -> String {
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
         <item id=\"0\" parentID=\"-1\" restricted=\"1\">\
         <dc:title>{title}</dc:title>\
         <upnp:class>object.item.videoItem</upnp:class>\
         <res protocolInfo=\"http-get:*:{content_type}:DLNA.ORG_OP=00;\
         DLNA.ORG_FLAGS=8d100000000000000000000000000000\" \
         resolution=\"{width}x{height}\">{url}</res>\
         </item></DIDL-Lite>",
        title = upnp::escape(title),
        url = upnp::escape(url),
        content_type = nd_core::stream_server::DLNA_MEDIA.content_type,
        width = size.0,
        height = size.1,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_item_describes_a_live_stream_with_no_seeking() {
        let didl = didl_lite("http://192.168.1.41:7236/abc", "BigNetScreen", (1920, 1080));
        // Declared, not left to be inferred: the renderer scales to whatever
        // geometry it believes the item has, and the one thing we know for
        // certain is what the encoder was told to produce.
        assert!(didl.contains(r#"resolution="1920x1080""#), "{didl}");
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
            (1920, 1080),
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
    fn only_playing_counts_as_playing() {
        assert_eq!(TransportState::parse("PLAYING"), TransportState::Playing);
        assert_eq!(
            TransportState::parse("TRANSITIONING"),
            TransportState::Transitioning
        );
        // Everything else ends the session, including values this crate has
        // never seen: guessing that an unknown state means "still fine" is how
        // a session outlives the television being switched off.
        for raw in ["STOPPED", "PAUSED_PLAYBACK", "NO_MEDIA_PRESENT", "", "WAT"] {
            assert_eq!(TransportState::parse(raw), TransportState::Idle, "{raw}");
        }
    }
}
