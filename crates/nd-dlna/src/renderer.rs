//! A renderer driven as a player: handed a file, it plays it itself.
//!
//! [`crate::session`] sends a renderer a live stream of our own making; this
//! is the other way, for what a television plays as it is (music, first of
//! all). It needs three of the renderer's services: `AVTransport` to load and
//! drive the item ([`crate::avtransport`]), `ConnectionManager` to ask which
//! formats it accepts, and `RenderingControl` for its volume.

use nd_core::sink::UpnpRenderer;
use nd_core::{NdError, Result};

use crate::upnp::{self, Endpoint};

const CONNECTION_MANAGER: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";
const RENDERING_CONTROL: &str = "urn:schemas-upnp-org:service:RenderingControl:1";

pub struct Renderer {
    pub av_transport: Endpoint,
    rendering_control: Option<Endpoint>,
    connection_manager: Option<Endpoint>,
}

impl Renderer {
    pub fn new(renderer: &UpnpRenderer) -> Result<Self> {
        let parse = |url: &str| Endpoint::parse(url);
        Ok(Self {
            av_transport: parse(&renderer.av_transport).ok_or_else(|| {
                NdError::Protocol("the renderer has no usable AVTransport".into())
            })?,
            rendering_control: renderer.rendering_control.as_deref().and_then(parse),
            connection_manager: renderer.connection_manager.as_deref().and_then(parse),
        })
    }

    /// The content types the renderer lists as ones it plays, lower case.
    /// Empty when it does not say: that is no answer, not "nothing".
    pub async fn accepted_types(&self) -> Result<Vec<String>> {
        let Some(control) = &self.connection_manager else {
            return Ok(Vec::new());
        };
        let response = upnp::soap(control, CONNECTION_MANAGER, "GetProtocolInfo", "").await?;
        Ok(parse_sink_types(
            upnp::tag_text(&response, "Sink").unwrap_or(""),
        ))
    }

    /// 0 to 1, and whether it is muted, when the renderer reports them.
    pub async fn volume(&self) -> Option<(f64, bool)> {
        let control = self.rendering_control.as_ref()?;
        let arguments = "<InstanceID>0</InstanceID><Channel>Master</Channel>";
        let volume = upnp::soap(control, RENDERING_CONTROL, "GetVolume", arguments)
            .await
            .ok()?;
        let level = upnp::tag_text(&volume, "CurrentVolume")?
            .parse::<f64>()
            .ok()?;
        let mute = upnp::soap(control, RENDERING_CONTROL, "GetMute", arguments)
            .await
            .ok()
            .and_then(|mute| upnp::tag_text(&mute, "CurrentMute").map(|m| m == "1"))
            .unwrap_or(false);
        Some(((level / 100.0).clamp(0.0, 1.0), mute))
    }

    pub async fn set_volume(&self, level: f64) -> Result<()> {
        let control = self.rendering_control()?;
        let arguments = format!(
            "<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredVolume>{}</DesiredVolume>",
            (level.clamp(0.0, 1.0) * 100.0).round() as u32
        );
        upnp::soap(control, RENDERING_CONTROL, "SetVolume", &arguments).await?;
        Ok(())
    }

    pub async fn set_mute(&self, muted: bool) -> Result<()> {
        let control = self.rendering_control()?;
        let arguments = format!(
            "<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredMute>{}</DesiredMute>",
            u8::from(muted)
        );
        upnp::soap(control, RENDERING_CONTROL, "SetMute", &arguments).await?;
        Ok(())
    }

    fn rendering_control(&self) -> Result<&Endpoint> {
        self.rendering_control
            .as_ref()
            .ok_or_else(|| NdError::Unsupported("this TV has no volume control".into()))
    }
}

/// Whether `content_type` is among what the renderer accepts. `audio/wav` and
/// its two other spellings are one format to every renderer that lists any.
pub fn accepts(accepted: &[String], content_type: &str) -> bool {
    const WAV: [&str; 3] = ["audio/wav", "audio/x-wav", "audio/wave"];
    let wanted = content_type.to_ascii_lowercase();
    accepted.iter().any(|accepted| {
        *accepted == wanted || (WAV.contains(&accepted.as_str()) && WAV.contains(&wanted.as_str()))
    })
}

/// The third field of each `protocolInfo` in a `Sink` list.
fn parse_sink_types(sink: &str) -> Vec<String> {
    let mut types: Vec<String> = sink
        .split(',')
        .filter_map(|info| info.trim().split(':').nth(2))
        .map(str::to_ascii_lowercase)
        .filter(|content_type| content_type.contains('/'))
        .collect();
    types.sort();
    types.dedup();
    types
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sink_list_yields_its_content_types() {
        // Abridged from a Panasonic TX-75GX880's answer.
        let sink = "http-get:*:audio/mpeg:DLNA.ORG_PN=MP3,http-get:*:audio/L16;rate=44100;channels=2:DLNA.ORG_PN=LPCM,\
                    http-get:*:audio/x-wav:*, http-get:*:video/mpeg:*,http-get:*:audio/flac:*,junk";
        let types = parse_sink_types(sink);
        assert_eq!(
            types,
            [
                "audio/flac",
                "audio/l16;rate=44100;channels=2",
                "audio/mpeg",
                "audio/x-wav",
                "video/mpeg"
            ]
        );
        assert!(accepts(&types, "audio/mpeg"));
        assert!(accepts(&types, "audio/wav"), "any spelling of wav");
        assert!(!accepts(&types, "audio/ogg"));
        assert!(!accepts(&[], "audio/mpeg"));
    }
}
