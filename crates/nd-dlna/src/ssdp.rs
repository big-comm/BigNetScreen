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
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};

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
    search_announcements(None).await
}

async fn search_announcements(
    deliver: Option<tokio::sync::mpsc::Sender<Announcement>>,
) -> Result<Vec<Announcement>> {
    let mut addresses =
        tokio::time::timeout(Duration::from_secs(2), nd_net::p2p::local_ipv4_addresses())
            .await
            .ok()
            .and_then(std::result::Result::ok)
            .unwrap_or_default();
    // NetworkManager is optional for LAN sharing, including under Flatpak.
    if addresses.is_empty() {
        addresses.push(Ipv4Addr::UNSPECIFIED);
    }
    search_on(
        &addresses,
        SocketAddr::from(([239, 255, 255, 250], 1900)),
        deliver,
    )
    .await
}

async fn search_on(
    addresses: &[Ipv4Addr],
    target: SocketAddr,
    deliver: Option<tokio::sync::mpsc::Sender<Announcement>>,
) -> Result<Vec<Announcement>> {
    let mut sockets = Vec::new();
    for address in addresses {
        if let Ok(socket) = tokio::net::UdpSocket::bind((*address, 0)).await {
            // Linux uses the bound local source to choose the multicast interface.
            let _ = socket.set_multicast_ttl_v4(4);
            sockets.push(socket);
        }
    }

    let probe = format!(
        "M-SEARCH * HTTP/1.1\r\n\
         HOST: 239.255.255.250:1900\r\n\
         MAN: \"ssdp:discover\"\r\n\
         MX: {MX}\r\n\
         ST: {MEDIA_RENDERER}\r\n\r\n"
    );

    // Twice: the probe is UDP and a lost one costs a whole sweep.
    for _ in 0..2 {
        let mut working = Vec::new();
        for socket in sockets {
            if socket.send_to(probe.as_bytes(), target).await.is_ok() {
                working.push(socket);
            }
        }
        sockets = working;
        if sockets.is_empty() {
            return Err(NdError::Network(
                "SSDP has no reachable network interface".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let mut found: Vec<Announcement> = Vec::new();
    let deadline = Instant::now() + LISTEN;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        if sockets.is_empty() {
            break;
        }
        let receives = sockets.iter().map(|socket| {
            Box::pin(async move {
                let mut buffer = [0u8; MAX_REPLY_BYTES];
                let (read, _) = socket.recv_from(&mut buffer).await?;
                Ok::<_, std::io::Error>(
                    std::str::from_utf8(&buffer[..read])
                        .ok()
                        .and_then(parse_reply),
                )
            })
        });
        let Ok((received, index, pending)) =
            tokio::time::timeout(remaining, futures::future::select_all(receives)).await
        else {
            break;
        };
        drop(pending);
        let announcement = match received {
            Ok(Some(announcement)) => announcement,
            Ok(None) => continue,
            Err(_) => {
                sockets.swap_remove(index);
                continue;
            }
        };
        // A device answers both probes; keep the first.
        if found.len() < nd_core::provider::MAX_RECEIVERS
            && !found.iter().any(|a| a.usn == announcement.usn)
        {
            if let Some(deliver) = &deliver {
                if deliver.try_send(announcement.clone()).is_err() {
                    break;
                }
            }
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
            let scan = async {
                let mut known: HashMap<String, (Arc<DlnaSink>, u32)> = HashMap::new();
                loop {
                    let locations: HashMap<_, _> = known
                        .iter()
                        .map(|(usn, (sink, _))| (usn.clone(), sink.location().to_string()))
                        .collect();
                    let (deliver, mut announcements) =
                        tokio::sync::mpsc::channel(nd_core::provider::MAX_RECEIVERS);
                    let describe = async {
                        let mut descriptions =
                            stream::poll_fn(move |cx| announcements.poll_recv(cx))
                                .filter(|announcement: &Announcement| {
                                    futures::future::ready(
                                        locations.get(&announcement.usn)
                                            != Some(&announcement.location),
                                    )
                                })
                                .map(|announcement| async move {
                                    let result = DlnaSink::describe(&announcement).await;
                                    (announcement.usn, result)
                                })
                                .buffer_unordered(4);
                        while let Some((usn, result)) = descriptions.next().await {
                            match result {
                                Ok(sink) => {
                                    let updated = known.contains_key(&usn);
                                    if !updated && known.len() >= nd_core::provider::MAX_RECEIVERS {
                                        continue;
                                    }
                                    let sink = Arc::new(sink);
                                    known.insert(usn, (sink.clone(), 0));
                                    let event = if updated {
                                        DiscoveryEvent::Updated(sink)
                                    } else {
                                        DiscoveryEvent::Added(sink)
                                    };
                                    if events.send(event).await.is_err() {
                                        return;
                                    }
                                }
                                Err(err) => {
                                    tracing::debug!(%err, "renderer did not describe itself")
                                }
                            }
                        }
                    };
                    let (found, ()) = tokio::join!(search_announcements(Some(deliver)), describe);
                    match found {
                        Ok(found) => {
                            if events
                                .send(DiscoveryEvent::ProviderReady { provider: "dlna" })
                                .await
                                .is_err()
                            {
                                return;
                            }
                            let mut gone = Vec::new();
                            for (usn, (sink, misses)) in &mut known {
                                if found.iter().any(|a| &a.usn == usn) {
                                    *misses = 0;
                                    continue;
                                }
                                *misses += 1;
                                if *misses >= MISSES_BEFORE_REMOVAL {
                                    gone.push((usn.clone(), sink.info().id));
                                }
                            }
                            for (usn, id) in gone {
                                known.remove(&usn);
                                if events.send(DiscoveryEvent::Removed(id)).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Err(err) => {
                            if events
                                .send(DiscoveryEvent::ProviderUnavailable {
                                    provider: "dlna",
                                    reason: err.to_string(),
                                })
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                    tokio::time::sleep(SWEEP_INTERVAL).await;
                }
            };
            tokio::select! {
                _ = events.closed() => {},
                _ = scan => {},
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

    #[tokio::test]
    async fn searches_each_source_and_delivers_before_the_sweep_ends() {
        let receiver = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target = receiver.local_addr().unwrap();
        let replies = tokio::spawn(async move {
            let mut sources = std::collections::HashSet::new();
            let mut buffer = [0; 2048];
            for _ in 0..4 {
                let (_, source) =
                    tokio::time::timeout(Duration::from_secs(2), receiver.recv_from(&mut buffer))
                        .await
                        .unwrap()
                        .unwrap();
                sources.insert(source.ip());
                receiver.send_to(REPLY.as_bytes(), source).await.unwrap();
            }
            sources
        });
        let (deliver, mut events) = tokio::sync::mpsc::channel(8);
        let search = tokio::spawn(async move {
            search_on(
                &[Ipv4Addr::new(127, 0, 0, 1), Ipv4Addr::new(127, 0, 0, 2)],
                target,
                Some(deliver),
            )
            .await
        });
        let first = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, parse_reply(REPLY).unwrap());
        assert!(
            !search.is_finished(),
            "receivers must appear before the reply window ends"
        );
        assert_eq!(replies.await.unwrap().len(), 2);
        assert_eq!(search.await.unwrap().unwrap(), vec![first]);
        assert!(
            events.recv().await.is_none(),
            "one receiver on two networks is still one receiver"
        );
    }

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
