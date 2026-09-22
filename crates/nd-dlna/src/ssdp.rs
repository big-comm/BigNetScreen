//! Finding DLNA renderers with SSDP.
//!
//! SSDP is UPnP's discovery half: an `M-SEARCH` to the multicast group
//! `239.255.255.250:1900`, and every device that matches answers by unicast
//! with a `LOCATION` pointing at its description.
//!
//! This is **not** mDNS, which is what the Chromecast and AirPlay providers
//! use. `avahi-browse` cannot see a DLNA television at all, and a network that
//! looks empty to it may be full of renderers — the two protocols share
//! nothing but the idea of multicast.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::stream::BoxStream;

use nd_core::provider::{DiscoveryEvent, Provider};
use nd_core::sink::Sink;
use nd_core::{NdError, Result};

use crate::upnp::Endpoint;
use crate::DlnaSink;

/// The device type worth answering: something that can be played *to*.
///
/// A television usually also announces `MediaServer`, which is it offering its
/// own content — the opposite direction, and not ours.
const MEDIA_RENDERER: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";

/// How long a device may wait before answering, in seconds. It staggers
/// replies so a large network does not answer all at once.
const MX: u32 = 3;
/// How long to keep reading replies. Longer than `MX`, since a device is
/// allowed to use the whole window and the reply still has to arrive.
const LISTEN: Duration = Duration::from_secs(5);
/// Gap between sweeps.
const SWEEP_INTERVAL: Duration = Duration::from_secs(20);
/// Sweeps a device may miss before it is reported gone.
///
/// Two, not one: a single lost UDP datagram is ordinary, and dropping a
/// television off the list because of it makes the list flicker.
const MISSES_BEFORE_REMOVAL: u32 = 2;
/// A reply is a few hundred bytes; anything larger is not an SSDP reply.
const MAX_REPLY_BYTES: usize = 2048;

/// What one device said about itself in its reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Announcement {
    /// `USN`: stable across the device changing address, unlike `LOCATION`.
    pub usn: String,
    pub location: String,
}

/// Pulls `USN` and `LOCATION` out of a reply, if it is one worth having.
///
/// Header names are matched case-insensitively because devices genuinely
/// disagree on the spelling.
pub fn parse_reply(reply: &str) -> Option<Announcement> {
    let mut lines = reply.split("\r\n");
    if !lines.next()?.starts_with("HTTP/1.1 200") {
        return None;
    }
    let (mut usn, mut location) = (None, None);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("usn") {
            usn = Some(value);
        } else if name.eq_ignore_ascii_case("location") {
            location = Some(value);
        }
    }
    let (usn, location) = (usn?, location?);
    // An empty or absurd value is a malformed reply, not a device.
    if usn.is_empty() || Endpoint::parse(location).is_none() {
        return None;
    }
    Some(Announcement {
        usn: usn.to_string(),
        location: location.to_string(),
    })
}

/// Sends one `M-SEARCH` and collects the replies.
///
/// The search names `MediaRenderer` rather than `ssdp:all`. Both were measured
/// against a real television and both work; the narrow one is preferred
/// because only renderers answer it, where `ssdp:all` also brings in every
/// router, printer and set-top box on the network for us to discard.
pub async fn search() -> Result<Vec<Announcement>> {
    // `0.0.0.0` on purpose, unlike the stream server: discovery has no
    // receiver to route towards yet, and binding one interface would hide the
    // televisions on the others.
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| NdError::Network(format!("SSDP socket: {e}")))?;
    // The default of 1 does not leave the machine on some configurations.
    let _ = socket.set_multicast_ttl_v4(4);

    let probe = format!(
        "M-SEARCH * HTTP/1.1\r\n\
         HOST: 239.255.255.250:1900\r\n\
         MAN: \"ssdp:discover\"\r\n\
         MX: {MX}\r\n\
         ST: {MEDIA_RENDERER}\r\n\r\n"
    );

    // Twice: the probe is UDP and a lost one costs a whole sweep.
    for _ in 0..2 {
        socket
            .send_to(probe.as_bytes(), "239.255.255.250:1900")
            .await
            .map_err(|e| NdError::Network(format!("SSDP M-SEARCH: {e}")))?;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let mut found: Vec<Announcement> = Vec::new();
    let deadline = Instant::now() + LISTEN;
    let mut buffer = [0u8; MAX_REPLY_BYTES];
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        let Ok(Ok((read, _from))) =
            tokio::time::timeout(remaining, socket.recv_from(&mut buffer)).await
        else {
            break;
        };
        let Some(announcement) = std::str::from_utf8(&buffer[..read])
            .ok()
            .and_then(parse_reply)
        else {
            continue;
        };
        // A device answers both probes; keep the first.
        if !found.iter().any(|a| a.usn == announcement.usn) {
            found.push(announcement);
        }
    }
    Ok(found)
}

/// Discovery of DLNA renderers.
pub struct DlnaProvider;

#[async_trait]
impl Provider for DlnaProvider {
    fn id(&self) -> &'static str {
        "dlna"
    }

    fn display_name(&self) -> &'static str {
        "DLNA"
    }

    async fn discover(&self) -> Result<BoxStream<'static, DiscoveryEvent>> {
        let (events, receiver) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move {
            let _ = events
                .send(DiscoveryEvent::ProviderReady { provider: "dlna" })
                .await;
            // How many sweeps in a row each known device has been missing.
            let mut known: HashMap<String, (Arc<DlnaSink>, u32)> = HashMap::new();
            loop {
                match search().await {
                    Ok(found) => {
                        for announcement in &found {
                            match known.get_mut(&announcement.usn) {
                                Some((sink, misses)) => {
                                    *misses = 0;
                                    // Re-announcing the same device is normal
                                    // and says nothing new; only report it
                                    // when the address actually moved.
                                    if sink.location() != announcement.location {
                                        let refreshed = match DlnaSink::describe(announcement).await
                                        {
                                            Ok(sink) => Arc::new(sink),
                                            Err(err) => {
                                                tracing::debug!(%err, "renderer did not describe itself");
                                                continue;
                                            }
                                        };
                                        *sink = refreshed.clone();
                                        let _ =
                                            events.send(DiscoveryEvent::Updated(refreshed)).await;
                                    }
                                }
                                None => match DlnaSink::describe(announcement).await {
                                    Ok(sink) => {
                                        let sink = Arc::new(sink);
                                        known.insert(announcement.usn.clone(), (sink.clone(), 0));
                                        if events.send(DiscoveryEvent::Added(sink)).await.is_err() {
                                            return;
                                        }
                                    }
                                    // A device that answers SSDP but has no
                                    // AVTransport cannot be played to, so it
                                    // is not offered. Logged, not shown: the
                                    // user did not ask about it.
                                    Err(err) => tracing::debug!(
                                        usn = %announcement.usn, %err,
                                        "ignoring a renderer we cannot drive"
                                    ),
                                },
                            }
                        }
                        let mut gone = Vec::new();
                        for (usn, (sink, misses)) in known.iter_mut() {
                            if found.iter().any(|a| &a.usn == usn) {
                                continue;
                            }
                            *misses += 1;
                            if *misses >= MISSES_BEFORE_REMOVAL {
                                gone.push((usn.clone(), sink.info().id));
                            }
                        }
                        for (usn, id) in gone {
                            known.remove(&usn);
                            let _ = events.send(DiscoveryEvent::Removed(id)).await;
                        }
                    }
                    Err(err) => {
                        let _ = events
                            .send(DiscoveryEvent::ProviderUnavailable {
                                provider: "dlna",
                                reason: err.to_string(),
                            })
                            .await;
                    }
                }
                tokio::time::sleep(SWEEP_INTERVAL).await;
            }
        });
        Ok(Box::pin(tokio_stream(receiver)))
    }
}

fn tokio_stream(
    mut receiver: tokio::sync::mpsc::Receiver<DiscoveryEvent>,
) -> impl futures::Stream<Item = DiscoveryEvent> {
    futures::stream::poll_fn(move |cx| receiver.poll_recv(cx))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real reply from a Panasonic 75GX880.
    const REPLY: &str = "HTTP/1.1 200 OK\r\n\
        CACHE-CONTROL: max-age=1800\r\n\
        EXT:\r\n\
        LOCATION: http://192.168.1.59:55000/dmr/ddd.xml\r\n\
        SERVER: Linux/4.0 UPnP/1.0 Panasonic-MIL-DLNA-SV/1.0\r\n\
        ST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\
        USN: uuid:4D454930-0100-1000-8001-80C755DF58D9::\
        urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";

    #[test]
    fn a_real_reply_yields_its_identity_and_description_url() {
        let a = parse_reply(REPLY).expect("a well-formed reply");
        assert_eq!(a.location, "http://192.168.1.59:55000/dmr/ddd.xml");
        assert!(a.usn.starts_with("uuid:4D454930-0100"), "{}", a.usn);
    }

    #[test]
    fn header_case_does_not_matter_because_devices_disagree_on_it() {
        let reply = "HTTP/1.1 200 OK\r\nlocation: http://10.0.0.5:80/d.xml\r\nUsn: uuid:x\r\n\r\n";
        let a = parse_reply(reply).expect("lowercase headers are still headers");
        assert_eq!(a.location, "http://10.0.0.5:80/d.xml");
        assert_eq!(a.usn, "uuid:x");
    }

    #[test]
    fn replies_that_cannot_be_acted_on_are_discarded() {
        for reply in [
            // Not a reply at all — an unsolicited NOTIFY on the same socket.
            "NOTIFY * HTTP/1.1\r\nLOCATION: http://10.0.0.5/d.xml\r\nUSN: uuid:x\r\n\r\n",
            // No LOCATION: nothing to fetch.
            "HTTP/1.1 200 OK\r\nUSN: uuid:x\r\n\r\n",
            // No USN: nothing stable to key on.
            "HTTP/1.1 200 OK\r\nLOCATION: http://10.0.0.5/d.xml\r\n\r\n",
            // A LOCATION we would refuse to fetch anyway.
            "HTTP/1.1 200 OK\r\nLOCATION: https://10.0.0.5/d.xml\r\nUSN: uuid:x\r\n\r\n",
            "HTTP/1.1 200 OK\r\nLOCATION: file:///etc/passwd\r\nUSN: uuid:x\r\n\r\n",
            "",
        ] {
            assert!(parse_reply(reply).is_none(), "accepted {reply:?}");
        }
    }
}
