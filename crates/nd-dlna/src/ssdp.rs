//! Finding DLNA renderers with SSDP.
//!
//! SSDP is UPnP's discovery half: an `M-SEARCH` to the multicast group
//! `239.255.255.250:1900`, and every device that matches answers by unicast
//! with a `LOCATION` pointing at its description.
//!
//! The Chromecast and AirPlay providers use mDNS instead. `avahi-browse` cannot
//! see a DLNA television, so a network that looks empty to it may be full of
//! renderers.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use nd_core::provider::{DiscoveryEvent, Provider};
use nd_core::sink::Sink;
use nd_core::{NdError, Result};

use crate::DlnaSink;
use crate::upnp::Endpoint;

/// The device type worth answering: something that can be played to.
///
/// A television usually also announces `MediaServer`, which offers its own
/// content in the opposite direction.
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

/// Whether the reply's `LOCATION` is on the host that sent it.
///
/// Discovery fetches every description it hears of, unprompted. A reply
/// naming another address would make any host on the network, or anything
/// able to spoof a datagram, aim that fetch at a third machine or at a
/// loopback service here. A renderer describes itself from the address it
/// answers from.
fn describes_itself(announcement: &Announcement, sender: SocketAddr) -> bool {
    Endpoint::parse(&announcement.location)
        .is_some_and(|location| location.addr.ip() == sender.ip())
}

/// Sends one `M-SEARCH` and collects the replies.
///
/// The search names `MediaRenderer` rather than `ssdp:all`: televisions answer
/// both, and only renderers answer the narrow one, where `ssdp:all` also
/// brings in every router, printer and set-top box on the network.
pub async fn search() -> Result<Vec<Announcement>> {
    search_announcements(None).await
}

async fn search_announcements(
    deliver: Option<mpsc::Sender<Announcement>>,
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

/// Sends the `M-SEARCH` from each local address and returns the sockets that
/// sent it, for the replies to arrive on.
async fn send_search(addresses: &[Ipv4Addr], target: SocketAddr) -> Result<Vec<UdpSocket>> {
    let mut sockets = Vec::new();
    for address in addresses {
        if let Ok(socket) = UdpSocket::bind((*address, 0)).await {
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

    Ok(sockets)
}

/// Searches from `addresses` and collects replies for [`LISTEN`], handing
/// each new renderer to `deliver` as soon as it answers.
async fn search_on(
    addresses: &[Ipv4Addr],
    target: SocketAddr,
    deliver: Option<mpsc::Sender<Announcement>>,
) -> Result<Vec<Announcement>> {
    let mut sockets = send_search(addresses, target).await?;
    let mut found: Vec<Announcement> = Vec::new();
    let deadline = Instant::now() + LISTEN;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        if sockets.is_empty() {
            break;
        }
        let receives = sockets.iter().map(|socket| {
            Box::pin(async move {
                let mut buffer = [0u8; MAX_REPLY_BYTES];
                let (read, sender) = socket.recv_from(&mut buffer).await?;
                Ok::<_, std::io::Error>(
                    std::str::from_utf8(&buffer[..read])
                        .ok()
                        .and_then(parse_reply)
                        .filter(|announcement| describes_itself(announcement, sender)),
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
            if let Some(deliver) = &deliver
                && deliver.try_send(announcement.clone()).is_err()
            {
                break;
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
        let (events, mut receiver) = mpsc::channel(32);
        tokio::spawn(async move {
            let scan = async {
                let mut known = Known::new();
                while sweep(&mut known, &events).await {
                    tokio::time::sleep(SWEEP_INTERVAL).await;
                }
            };
            tokio::select! {
                _ = events.closed() => {},
                () = scan => {},
            }
        });
        Ok(Box::pin(stream::poll_fn(move |cx| receiver.poll_recv(cx))))
    }
}

/// Renderers listed so far, by USN, with the sweeps each has missed in a row.
type Known = HashMap<String, (Arc<DlnaSink>, u32)>;

/// One sweep: search, describe what is new or moved, and report what stayed
/// silent too long as gone. `false` once nobody is listening.
async fn sweep(known: &mut Known, events: &mpsc::Sender<DiscoveryEvent>) -> bool {
    let (deliver, announcements) = mpsc::channel(nd_core::provider::MAX_RECEIVERS);
    let (found, listening) = tokio::join!(
        search_announcements(Some(deliver)),
        describe_announced(announcements, known, events)
    );
    if !listening {
        return false;
    }
    let found = match found {
        Ok(found) => found,
        Err(err) => {
            let unavailable = DiscoveryEvent::ProviderUnavailable {
                provider: "dlna",
                reason: err.to_string(),
            };
            return events.send(unavailable).await.is_ok();
        }
    };
    let ready = DiscoveryEvent::ProviderReady { provider: "dlna" };
    if events.send(ready).await.is_err() {
        return false;
    }
    let mut gone = Vec::new();
    for (usn, (sink, misses)) in known.iter_mut() {
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
            return false;
        }
    }
    true
}

/// Describes each renderer as its reply arrives, skipping those already
/// listed at the same address. `false` once nobody is listening.
async fn describe_announced(
    mut announcements: mpsc::Receiver<Announcement>,
    known: &mut Known,
    events: &mpsc::Sender<DiscoveryEvent>,
) -> bool {
    let locations: HashMap<_, _> = known
        .iter()
        .map(|(usn, (sink, _))| (usn.clone(), sink.location().to_string()))
        .collect();
    let mut descriptions = stream::poll_fn(move |cx| announcements.poll_recv(cx))
        .filter(|announcement: &Announcement| {
            futures::future::ready(locations.get(&announcement.usn) != Some(&announcement.location))
        })
        .map(|announcement| async move {
            let result = DlnaSink::describe(&announcement).await;
            (announcement.usn, result)
        })
        .buffer_unordered(4);
    while let Some((usn, result)) = descriptions.next().await {
        let sink = match result {
            Ok(sink) => Arc::new(sink),
            Err(err) => {
                tracing::debug!(%err, "renderer did not describe itself");
                continue;
            }
        };
        let updated = known.contains_key(&usn);
        if !updated && known.len() >= nd_core::provider::MAX_RECEIVERS {
            continue;
        }
        known.insert(usn, (sink.clone(), 0));
        let event = if updated {
            DiscoveryEvent::Updated(sink)
        } else {
            DiscoveryEvent::Added(sink)
        };
        if events.send(event).await.is_err() {
            return false;
        }
    }
    true
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

    /// [`REPLY`] as the device answers from loopback, its own address.
    fn loopback_reply() -> String {
        REPLY.replace("192.168.1.59", "127.0.0.1")
    }

    #[tokio::test]
    async fn searches_each_source_and_delivers_early() {
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
                receiver
                    .send_to(loopback_reply().as_bytes(), source)
                    .await
                    .unwrap();
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
        assert_eq!(first, parse_reply(&loopback_reply()).unwrap());
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

    #[tokio::test]
    async fn foreign_host_reply_is_ignored() {
        let device = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target = device.local_addr().unwrap();
        let replies = tokio::spawn(async move {
            let mut buffer = [0; 2048];
            for _ in 0..2 {
                let (_, source) = device.recv_from(&mut buffer).await.unwrap();
                // From 127.0.0.1, naming 192.168.1.59.
                device.send_to(REPLY.as_bytes(), source).await.unwrap();
            }
        });
        let found = search_on(&[Ipv4Addr::new(127, 0, 0, 1)], target, None)
            .await
            .unwrap();
        replies.await.unwrap();
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn real_reply_yields_identity_and_url() {
        let a = parse_reply(REPLY).expect("a well-formed reply");
        assert_eq!(a.location, "http://192.168.1.59:55000/dmr/ddd.xml");
        assert!(a.usn.starts_with("uuid:4D454930-0100"), "{}", a.usn);
    }

    #[test]
    fn headers_are_case_insensitive() {
        let reply = "HTTP/1.1 200 OK\r\nlocation: http://10.0.0.5:80/d.xml\r\nUsn: uuid:x\r\n\r\n";
        let a = parse_reply(reply).expect("lowercase headers are still headers");
        assert_eq!(a.location, "http://10.0.0.5:80/d.xml");
        assert_eq!(a.usn, "uuid:x");
    }

    #[test]
    fn unusable_replies_are_discarded() {
        for reply in [
            // An unsolicited NOTIFY on the same socket.
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
