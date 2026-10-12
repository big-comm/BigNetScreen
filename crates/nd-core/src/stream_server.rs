//! The HTTP server that delivers the stream to a receiver that fetches it.
//!
//! Cast (`LOAD`) and DLNA (`SetAVTransportURI`) both hand the receiver a URL,
//! and the receiver opens a `GET` back to us; this module is that other side,
//! shared by both. They differ only in how the response describes the body,
//! which is [`MediaType`].
//!
//! HTTP is hand-written because the pipeline's `multisocketsink` writes
//! straight into the receiver's socket, which means handing GStreamer the
//! descriptor once the headers are out; an HTTP framework does not give the
//! raw connection back halfway through a response. The surface is one `GET`
//! and one response that ends when the connection closes.
//!
//! The stream is the person's screen, so it is guarded three ways: a random
//! 128-bit token in the path (a wrong path gets `404`, never revealing that a
//! protected one exists), an allowlist of the one receiver address the session
//! was opened with, and a listener bound only to the interface that reaches
//! that receiver.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use gio::prelude::*;
use gst::prelude::*;
use gstreamer as gst;
use tokio::net::{TcpListener, TcpStream};

use crate::{NdError, Result};

mod http;

use http::{Verdict, response_headers, triage};
pub use http::{read_request, request_line, write_bounded};

/// What the response says about the body it is about to stream.
///
/// The body itself is the same H.264 + AAC transport stream in both cases; the
/// receivers just need to be told about it in their own vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaType {
    pub content_type: &'static str,
    /// Protocol-specific header lines, each already ending in CRLF. Empty for
    /// a receiver that needs nothing beyond the standard block.
    pub extra_headers: &'static str,
}

impl MediaType {
    /// The `DLNA.ORG_OP=…;DLNA.ORG_FLAGS=…` this response declares, for the
    /// item metadata to say the same: a renderer that sees the two disagree
    /// believes the metadata.
    pub fn dlna_features(&self) -> Option<&'static str> {
        let start = self.extra_headers.find("DLNA.ORG_OP=")?;
        let rest = &self.extra_headers[start..];
        Some(rest.split("\r\n").next().unwrap_or(rest))
    }
}

/// The Cast HTTP fallback: H.264 + AAC in MPEG-TS, nothing else to declare.
pub const CAST_MEDIA: MediaType = MediaType {
    content_type: "video/mp2t",
    extra_headers: "",
};

/// The same stream, described the way a DLNA renderer expects.
///
/// `transferMode.dlna.org: Streaming` is what the renderer asks for in its
/// `GET`; answering it is what separates a live stream from a file download.
/// The flags are the ones a Panasonic VIErA advertised for its own AVC_TS
/// profiles, and they decode to sender-paced (bit 31) plus s0-increasing and
/// sn-increasing (bits 27 and 26), DLNA's signature for content with no fixed
/// start and no end, plus streaming transfer mode and DLNA 1.5.
///
/// `DLNA.ORG_OP=00` says neither seek mode is available, which is the truth
/// for a live screen and stops a renderer from probing for byte ranges.
///
/// No `DLNA.ORG_PN`: the renderer's own `GetProtocolInfo` lists `video/mpeg:*`
/// and naming an exact profile only narrows what it will accept. Tested
/// against the explicit `AVC_TS_HD_60_AC3_T` profile with timestamped
/// 192-byte packets, which the same set also advertises: it played, and the
/// delay was identical. The simpler stream wins on a tie.
pub const DLNA_MEDIA: MediaType = MediaType {
    content_type: "video/mpeg",
    extra_headers: "transferMode.dlna.org: Streaming\r\n\
                    contentFeatures.dlna.org: DLNA.ORG_OP=00;\
                    DLNA.ORG_FLAGS=8d100000000000000000000000000000\r\n",
};

/// The same stream when it carries a file: [`DLNA_MEDIA`] plus the
/// connection-stalling flag (bit 21), which lets the renderer pause by no
/// longer reading. Measured on a Panasonic TX-75GX880: without it the set
/// lists only `Stop,Play` and answers `Pause` with 500; with it, it lists and
/// accepts `Pause` and keeps the connection through it. A screen keeps the
/// plain flags: paused from the remote, it would go on producing into a
/// connection nobody reads until the queue limit ended the session.
pub const DLNA_FILE_MEDIA: MediaType = MediaType {
    content_type: "video/mpeg",
    extra_headers: "transferMode.dlna.org: Streaming\r\n\
                    contentFeatures.dlna.org: DLNA.ORG_OP=00;\
                    DLNA.ORG_FLAGS=8d300000000000000000000000000000\r\n",
};

/// Finds the local IP of the interface that reaches `peer`.
///
/// A "connected" UDP socket sends nothing, but it makes the kernel pick the
/// route and with it the source address. The first interface is not enough:
/// on a machine with a VPN (Tailscale, say) the wrong IP leaves the Chromecast
/// unable to connect back.
pub fn local_ip_towards(peer: IpAddr) -> Result<IpAddr> {
    let bind = if peer.is_ipv4() {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
    };
    let socket = std::net::UdpSocket::bind(bind).map_err(NdError::network)?;
    socket.connect((peer, 9)).map_err(NdError::network)?;
    Ok(socket.local_addr().map_err(NdError::network)?.ip())
}

/// The stream server for one cast session.
pub struct StreamServer {
    listener: TcpListener,
    /// The secret path, leading slash included.
    path: String,
    /// The only IP allowed to fetch the stream.
    allowed: IpAddr,
    local_addr: SocketAddr,
    media: MediaType,
    attendance: Arc<Mutex<Attendance>>,
}

/// How long receivers stayed: the longest finished connection, when each
/// open one began, by socket descriptor, and when the last one left.
#[derive(Default)]
struct Attendance {
    longest: Duration,
    open: HashMap<RawFd, Instant>,
    last_left: Option<Instant>,
}

/// The attendance, whatever a panicking holder left behind: it is only
/// timestamps.
fn lock(attendance: &Mutex<Attendance>) -> MutexGuard<'_, Attendance> {
    attendance
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

impl StreamServer {
    /// Brings the server up on the address that reaches `receiver`, at the
    /// port set in the preferences or, when that is `0`, one the kernel picks.
    pub async fn bind(receiver: IpAddr, media: MediaType) -> Result<Self> {
        let local_ip = local_ip_towards(receiver)?;
        // Listening on this address alone keeps the stream off the other
        // interfaces (VPN, docker0, loopback).
        let listener = TcpListener::bind((local_ip, crate::settings::current().port))
            .await
            .map_err(NdError::network)?;
        let local_addr = listener.local_addr().map_err(NdError::network)?;
        let path = format!("/{}", crate::secret::random_token()?);

        tracing::info!(%local_addr, %receiver, "stream server listening");
        Ok(Self {
            listener,
            path,
            allowed: receiver,
            local_addr,
            media,
            attendance: Arc::default(),
        })
    }

    /// The longest any receiver has stayed connected, counting open ones.
    ///
    /// A television that probes and gives up never holds a connection for
    /// more than a couple of seconds; one that shows the picture does.
    pub fn longest_receiver(&self) -> Duration {
        let attendance = lock(&self.attendance);
        let now = Instant::now();
        attendance
            .open
            .values()
            .map(|since| now - *since)
            .fold(attendance.longest, Duration::max)
    }

    /// How long the stream has gone without a receiver since the last one
    /// left; `None` while one is connected or before any came.
    pub fn abandoned_for(&self) -> Option<Duration> {
        let attendance = lock(&self.attendance);
        if attendance.open.is_empty() {
            attendance.last_left.map(|left| left.elapsed())
        } else {
            None
        }
    }

    /// How the responses describe the stream.
    pub fn media(&self) -> MediaType {
        self.media
    }

    /// The URL to hand the receiver; it carries the secret path.
    pub fn url(&self) -> String {
        let host = match self.local_addr.ip() {
            IpAddr::V4(ip) => ip.to_string(),
            IpAddr::V6(ip) => format!("[{ip}]"),
        };
        format!("http://{host}:{}{}", self.local_addr.port(), self.path)
    }

    /// The address the server listens on.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serves connections until `cancel` fires.
    ///
    /// On every valid `GET` the socket is handed to the pipeline's
    /// `multisocketsink`: from then on the bytes go from GStreamer straight to
    /// the receiver, never passing through here.
    ///
    /// `on_first_client` is called when the first client is accepted: the
    /// hook for putting the pipeline into `Playing` (before that there is
    /// nowhere to write).
    pub async fn serve<F>(
        &self,
        sink: gst::Element,
        mut on_first_client: F,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()>
    where
        F: FnMut() -> Result<()> + Send,
    {
        let mut slow_client = watch_clients(&sink, self.attendance.clone());
        if self.media != CAST_MEDIA {
            mark_join_points(&sink);
        }
        let mut cancel = cancel;
        let mut served_any = false;

        loop {
            if *cancel.borrow() {
                return Ok(());
            }
            let accepted = tokio::select! {
                result = self.listener.accept() => result,
                _ = slow_client.changed() => {
                    return Err(NdError::Network(
                        "the receiver stopped reading fast enough; the stream queue limit was reached".into(),
                    ));
                }
                _ = cancel.changed() => {
                    tracing::debug!("stream server stopped on request");
                    return Ok(());
                }
            };

            let (stream, peer) = match accepted {
                Ok(pair) => pair,
                Err(err) => {
                    tracing::warn!(%err, "failed to accept a connection");
                    // EMFILE fails again at once; do not spin on it.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };

            // Reject another IP before allowing it to occupy the serial reader.
            if peer.ip() != self.allowed {
                continue;
            }
            let admitted = tokio::select! {
                admitted = self.admit(stream, peer) => admitted,
                _ = cancel.changed() => return Ok(()),
            };
            let Some(stream) = admitted else {
                continue;
            };
            if *cancel.borrow() {
                return Ok(());
            }

            let fd = stream.as_raw_fd();
            match hand_socket_to_sink(stream, &sink) {
                Ok(()) => {
                    lock(&self.attendance).open.insert(fd, Instant::now());
                    tracing::info!(%peer, "receiver connected to the stream");
                }
                Err(err) => {
                    tracing::error!(%peer, %err, "could not hand the socket to the pipeline");
                    continue;
                }
            }

            if !served_any {
                served_any = true;
                on_first_client()?;
            } else if self.media == CAST_MEDIA {
                // A receiver joining a running stream cannot decode until the
                // next key frame. A DLNA reader starts at the last one
                // instead (`mark_join_points`).
                request_key_frame(&sink);
            }
        }
    }

    /// Reads one request and answers it, unless it is a valid `GET`: then the
    /// stream's headers are written and the socket is returned for the sink.
    async fn admit(&self, mut stream: TcpStream, peer: SocketAddr) -> Option<TcpStream> {
        let request = match read_request(&mut stream).await {
            Ok(request) => request,
            Err(err) => {
                tracing::debug!(%peer, %err, "request discarded");
                let _ = write_bounded(
                    &mut stream,
                    response_headers(Verdict::BadRequest, self.media).as_bytes(),
                )
                .await;
                return None;
            }
        };
        let verdict = triage(&request, &self.path, self.allowed, peer.ip());
        if verdict != Verdict::Stream {
            if verdict == Verdict::Forbidden {
                tracing::warn!(
                    %peer, expected = %self.allowed,
                    "refusing a stream request from another address"
                );
            } else {
                tracing::debug!(%peer, ?verdict, "request refused");
            }
            // Dropping the stream closes it; there is no shutdown wait.
            let _ = write_bounded(
                &mut stream,
                response_headers(verdict, self.media).as_bytes(),
            )
            .await;
            return None;
        }
        // Headers first; only then does GStreamer take the socket over.
        let headers = response_headers(Verdict::Stream, self.media);
        if let Err(err) = write_bounded(&mut stream, headers.as_bytes()).await {
            tracing::warn!(%peer, %err, "failed to write the headers");
            return None;
        }
        Some(stream)
    }
}

/// Makes every key frame of a DLNA stream a place a reader can start, for
/// `multisocketsink`'s `sync-method=latest-keyframe`.
///
/// A television's first connections last a few tens of milliseconds and are
/// how it recognises the stream: one that starts mid-picture, or before any
/// video, makes it give up. The muxer pushes each table or frame as a list of
/// 188-byte packets; each list becomes one buffer (`multisocketsink` walks a
/// slow reader's whole queue for every buffer, so tens of thousands of packets
/// would stall the pipeline). A buffer that opens a key frame gets the PAT
/// and PMT last seen in front of it and is the only kind without
/// `DELTA_UNIT`. The muxer's own `HEADER` flags come off: `multisocketsink`
/// would drop those buffers instead of sending them.
fn mark_join_points(sink: &gst::Element) {
    let Some(pad) = sink.static_pad("sink") else {
        return;
    };
    let tables = Mutex::new(Tables::default());
    pad.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        move |_, info| {
            let joined = match &info.data {
                Some(gst::PadProbeData::Buffer(buffer)) => {
                    join_buffers(std::iter::once(buffer.as_ref()))
                }
                Some(gst::PadProbeData::BufferList(list)) => join_buffers(list.iter()),
                _ => return gst::PadProbeReturn::Ok,
            };
            let buffer = lock_tables(&tables).mark(joined);
            info.data = Some(match &info.data {
                Some(gst::PadProbeData::BufferList(_)) => {
                    let mut single = gst::BufferList::new_sized(1);
                    single
                        .get_mut()
                        .expect("a new list is writable")
                        .add(buffer);
                    gst::PadProbeData::BufferList(single)
                }
                _ => gst::PadProbeData::Buffer(buffer),
            });
            gst::PadProbeReturn::Ok
        },
    );
}

/// The packets of `buffers` as one buffer, timed like the first.
fn join_buffers<'a>(buffers: impl Iterator<Item = &'a gst::BufferRef>) -> gst::Buffer {
    let mut data = Vec::new();
    let mut timing = None;
    for buffer in buffers {
        timing.get_or_insert((buffer.pts(), buffer.dts()));
        if let Ok(map) = buffer.map_readable() {
            data.extend_from_slice(&map);
        }
    }
    let mut joined = gst::Buffer::from_mut_slice(data);
    if let Some((pts, dts)) = timing {
        let writable = joined.get_mut().expect("a new buffer is writable");
        writable.set_pts(pts);
        writable.set_dts(dts);
    }
    joined
}

/// The last PAT and PMT packets, put in front of each key frame.
#[derive(Default)]
struct Tables {
    pat: Option<Vec<u8>>,
    pmt: Option<(u16, Vec<u8>)>,
}

impl Tables {
    /// Remembers the tables in `buffer`, and makes a buffer that opens a key
    /// frame a join point: tables of its own, or the last ones put in front.
    /// Every other buffer becomes `DELTA_UNIT`.
    fn mark(&mut self, buffer: gst::Buffer) -> gst::Buffer {
        let mut key_frame = None;
        if let Ok(map) = buffer.map_readable() {
            let packets: Vec<&[u8]> = map
                .as_chunks::<TS_PACKET>()
                .0
                .iter()
                .map(|packet| packet.as_slice())
                .take_while(|packet| packet[0] == 0x47)
                .collect();
            let mut opens_with_tables = false;
            for (index, packet) in packets.iter().enumerate() {
                let pid = ts_pid(packet);
                if pid == 0 {
                    if let Some(pmt_pid) = pmt_pid(packet) {
                        self.pmt.take_if(|(pid, _)| *pid != pmt_pid);
                        self.pmt.get_or_insert((pmt_pid, Vec::new()));
                    }
                    self.pat = Some(packet.to_vec());
                    opens_with_tables |= index == 0;
                } else if let Some((_, pmt)) =
                    self.pmt.as_mut().filter(|(pmt_pid, _)| *pmt_pid == pid)
                {
                    *pmt = packet.to_vec();
                } else if random_access(packet) {
                    key_frame = if opens_with_tables {
                        Some(map.to_vec())
                    } else if index == 0
                        && let (Some(pat), Some((_, pmt))) = (&self.pat, &self.pmt)
                        && !pmt.is_empty()
                    {
                        Some([pat.as_slice(), pmt.as_slice(), &map].concat())
                    } else {
                        None
                    };
                    break;
                }
            }
        }
        if let Some(data) = key_frame {
            let mut joined = gst::Buffer::from_mut_slice(data);
            let writable = joined.get_mut().expect("a new buffer is writable");
            writable.set_pts(buffer.pts());
            writable.set_dts(buffer.dts());
            return joined;
        }
        let mut marked = buffer;
        marked.make_mut().set_flags(gst::BufferFlags::DELTA_UNIT);
        marked
    }
}

const TS_PACKET: usize = 188;

fn ts_pid(packet: &[u8]) -> u16 {
    (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2])
}

/// The PMT's PID, from a PAT packet that starts its section.
fn pmt_pid(pat: &[u8]) -> Option<u16> {
    let section = 5 + usize::from(*pat.get(4)?);
    let entry = pat.get(section + 8..section + 12)?;
    Some((u16::from(entry[2] & 0x1f) << 8) | u16::from(entry[3]))
}

/// Whether a packet's adaptation field marks a random access point, which
/// the muxer sets on a video key frame only.
fn random_access(packet: &[u8]) -> bool {
    packet[3] & 0x20 != 0 && packet[4] > 0 && packet[5] & 0x40 != 0
}

fn lock_tables(tables: &Mutex<Tables>) -> MutexGuard<'_, Tables> {
    tables.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Asks the encoder feeding `sink` for an IDR with its parameter sets.
fn request_key_frame(sink: &gst::Element) {
    let encoder = sink
        .parent()
        .and_then(|parent| parent.downcast::<gst::Bin>().ok())
        .and_then(|bin| bin.by_name(crate::pipeline::ENCODER_NAME));
    match encoder {
        Some(encoder) => {
            encoder.send_event(crate::pipeline::force_key_unit());
        }
        None => tracing::debug!("no encoder to ask for a key frame"),
    }
}

/// Wires the sink's client signals: a removed client's socket is closed, and
/// the returned receiver changes when a client is dropped for reading too
/// slowly.
fn watch_clients(
    sink: &gst::Element,
    attendance: Arc<Mutex<Attendance>>,
) -> tokio::sync::watch::Receiver<bool> {
    let (slow_client, slow_client_rx) = tokio::sync::watch::channel(false);
    sink.connect("client-removed", false, move |values| {
        // A panic would unwind into C, so even the element's own signature
        // is read without indexing.
        let status = values
            .get(2)
            .and_then(gst::glib::EnumValue::from_value)
            .map(|(_, status)| status.nick());
        if status == Some("slow") {
            slow_client.send_replace(true);
        }
        // Still answerable here, before the client is gone: whether a receiver
        // that left got any of the stream tells a refused stream from a starved one.
        if let (Some(Ok(sink)), Some(Ok(socket))) = (
            values.first().map(|value| value.get::<gst::Element>()),
            values.get(1).map(|value| value.get::<gio::Socket>()),
        ) {
            let mut attendance = lock(&attendance);
            if let Some(since) = attendance.open.remove(&socket.as_raw_fd()) {
                attendance.longest = attendance.longest.max(since.elapsed());
            }
            attendance.last_left = Some(Instant::now());
            drop(attendance);
            let stats = sink.emit_by_name::<gst::Structure>("get-stats", &[&socket]);
            tracing::info!(
                bytes_sent = stats.get::<u64>("bytes-sent").ok(),
                connected_ms = stats
                    .get::<u64>("connect-duration")
                    .ok()
                    .map(|ns| ns / 1_000_000),
                status,
                "a receiver left the stream"
            );
        }
        None
    });
    // `multisocketsink` hands a removed client's socket back without
    // closing it. Closing it here rather than in each caller means no
    // protocol can leak a descriptor per disconnect.
    sink.connect("client-socket-removed", false, |values| {
        if let Some(Ok(socket)) = values.get(1).map(|value| value.get::<gio::Socket>()) {
            let _ = socket.close();
        }
        tracing::info!("the receiver disconnected from the stream");
        None
    });
    slow_client_rx
}

/// The kernel's share of a slow reader's backlog.
///
/// Left to autotuning, the send buffer grows toward `net.ipv4.tcp_wmem`'s
/// maximum (32 MiB on BigLinux), where a stopped reader's backlog piles up
/// out of `multisocketsink`'s sight and its queue limit fires tens of seconds
/// late. Fixed, the sink's own limit is the bound. 256 KiB, which the kernel
/// doubles, covers a 21 Mbit/s stream over a 150 ms round trip.
const SEND_BUFFER_BYTES: i32 = 256 * 1024;

/// Transfers ownership of the TCP socket to `multisocketsink`.
///
/// tokio's descriptor is non-blocking, but `multisocketsink` writes from its
/// own streaming thread and treats `EAGAIN` as a client error, so the socket
/// is made blocking first. The `gio::Socket` takes ownership of the
/// descriptor; if building it fails, the descriptor is closed there.
fn hand_socket_to_sink(stream: TcpStream, sink: &gst::Element) -> Result<()> {
    let std_stream = stream.into_std().map_err(NdError::network)?;
    std_stream
        .set_nonblocking(false)
        .map_err(NdError::network)?;

    let socket = gio::Socket::from_fd(std::os::fd::OwnedFd::from(std_stream))
        .map_err(|e| NdError::Network(format!("gio::Socket: {e}")))?;
    // Linux's SOL_SOCKET and SO_SNDBUF. Without it the queue limit is not ours.
    const SOL_SOCKET: i32 = 1;
    const SO_SNDBUF: i32 = 7;
    if let Err(err) = socket.set_option(SOL_SOCKET, SO_SNDBUF, SEND_BUFFER_BYTES) {
        tracing::warn!(%err, "the stream socket keeps the kernel's send buffer");
    }

    sink.emit_by_name::<()>("add", &[&socket]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn local_ip_is_routable() {
        // It does not depend on the real network: only on a route existing at all.
        let Ok(ip) = local_ip_towards(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))) else {
            return; // a machine with no external route (isolated CI)
        };
        assert!(!ip.is_unspecified(), "{ip}");
    }

    #[tokio::test]
    async fn url_carries_host_port_and_token() {
        let Ok(server) = StreamServer::bind(IpAddr::V4(Ipv4Addr::LOCALHOST), CAST_MEDIA).await
        else {
            return;
        };
        let url = server.url();
        assert!(url.starts_with("http://"), "{url}");
        assert!(
            url.contains(&server.local_addr().port().to_string()),
            "{url}"
        );
        // 32 hex characters after the last slash.
        let token = url.rsplit('/').next().unwrap();
        assert_eq!(token.len(), 32, "{url}");
    }

    #[tokio::test]
    async fn server_refuses_wrong_path() {
        let Ok(server) = StreamServer::bind(IpAddr::V4(Ipv4Addr::LOCALHOST), CAST_MEDIA).await
        else {
            return;
        };
        let addr = server.local_addr();

        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(b"GET /nothing HTTP/1.1\r\nHost: x\r\n\r\n")
                .await
                .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            response
        });

        // A manual accept, mirroring the triage `serve` performs.
        let (mut stream, peer) = server.listener.accept().await.unwrap();
        let request = read_request(&mut stream).await.unwrap();
        let verdict = triage(&request, &server.path, server.allowed, peer.ip());
        stream
            .write_all(response_headers(verdict, CAST_MEDIA).as_bytes())
            .await
            .unwrap();
        drop(stream);

        let response = client.await.unwrap();
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    }
    #[tokio::test]
    async fn stalled_dlna_reader_hits_queue_bound() {
        use crate::pipeline::{self, PipelineGuard, StreamConfig, VideoSource, VideoTarget};
        use gst::prelude::*;

        let cfg = StreamConfig {
            width: 320,
            height: 240,
            ..Default::default()
        };
        let description = pipeline::ts_http_pipeline_description(
            &cfg,
            &VideoSource::Test,
            VideoTarget::Exact((320, 240)),
            Some(8_000_000),
        );
        let (built, _events) = pipeline::build_pipeline(&description, cfg.latency_ms()).unwrap();
        let guard = PipelineGuard::new(built);
        let sink = guard
            .pipeline()
            .by_name(pipeline::TS_HTTP_SINK_NAME)
            .unwrap();
        // A small OS receive window makes a stopped TV deterministic on loopback.
        let server = StreamServer {
            listener: TcpListener::bind("127.0.0.1:0").await.unwrap(),
            path: "/test".into(),
            allowed: IpAddr::V4(Ipv4Addr::LOCALHOST),
            local_addr: "127.0.0.1:0".parse().unwrap(),
            media: DLNA_MEDIA,
            attendance: Arc::default(),
        };
        let client = gio::Socket::new(
            gio::SocketFamily::Ipv4,
            gio::SocketType::Stream,
            gio::SocketProtocol::Tcp,
        )
        .unwrap();
        // SOL_SOCKET/SO_RCVBUF on Linux; production socket options stay untouched.
        client.set_option(1, 8, 4096).unwrap();
        let address = server.listener.local_addr().unwrap();
        gio::prelude::SocketExt::connect(
            &client,
            &gio::InetSocketAddress::from_string("127.0.0.1", address.port().into()).unwrap(),
            gio::Cancellable::NONE,
        )
        .unwrap();
        client
            .send(
                b"GET /test HTTP/1.1\r\nHost: localhost\r\n\r\n",
                gio::Cancellable::NONE,
            )
            .unwrap();
        let (_cancel, cancelled) = tokio::sync::watch::channel(false);
        // Eight seconds of the 8 Mbit/s multiplex fill the limit.
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            server.serve(
                sink.clone(),
                || {
                    guard
                        .pipeline()
                        .set_state(gst::State::Playing)
                        .map(|_| ())
                        .map_err(|err| NdError::Gst(err.to_string()))
                },
                cancelled,
            ),
        )
        .await
        .expect("a stalled receiver must be disconnected");
        assert!(result.unwrap_err().to_string().contains("queue limit"));
        // The one reader's backlog: what reached the sink less what it sent.
        let backlog = sink.property::<u64>("bytes-to-serve") - sink.property::<u64>("bytes-served");
        assert!(backlog < 8_100_000, "{backlog} bytes queued");
        client.close().unwrap();
    }

    /// The video elementary stream inside a transport stream: the payloads of
    /// the PID whose PES carries stream id 0xE0, concatenated.
    fn video_payload(ts: &[u8]) -> Vec<u8> {
        let mut pid = None;
        let mut out = Vec::new();
        for packet in ts.as_chunks::<188>().0.iter().filter(|p| p[0] == 0x47) {
            let this = (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2]);
            let start = match (packet[3] >> 4) & 0x3 {
                1 => 4,
                3 => 5 + usize::from(packet[4]),
                _ => continue,
            };
            let Some(payload) = packet.get(start..) else {
                continue;
            };
            if packet[1] & 0x40 != 0 && payload.starts_with(&[0, 0, 1, 0xe0]) {
                pid = Some(this);
            }
            if pid == Some(this) {
                out.extend_from_slice(payload);
            }
        }
        out
    }

    #[tokio::test]
    async fn late_receiver_starts_at_key_frame() {
        use crate::pipeline::{self, PipelineGuard, StreamConfig, VideoSource, VideoTarget};
        use gst::prelude::*;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // A key frame a second, as the DLNA stream has.
        let cfg = StreamConfig {
            width: 320,
            height: 240,
            ..Default::default()
        };
        let description = pipeline::ts_http_pipeline_description(
            &cfg,
            &VideoSource::Test,
            VideoTarget::Exact((320, 240)),
            Some(2_000_000),
        );
        let (built, _events) = pipeline::build_pipeline(&description, cfg.latency_ms()).unwrap();
        let guard = PipelineGuard::new(built);
        let pipeline = guard.pipeline().clone();
        let sink = pipeline.by_name(pipeline::TS_HTTP_SINK_NAME).unwrap();
        let server = StreamServer {
            listener: TcpListener::bind("127.0.0.1:0").await.unwrap(),
            path: "/test".into(),
            allowed: IpAddr::V4(Ipv4Addr::LOCALHOST),
            local_addr: "127.0.0.1:0".parse().unwrap(),
            media: DLNA_MEDIA,
            attendance: Arc::default(),
        };
        let address = server.listener.local_addr().unwrap();
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        let serving = tokio::spawn(async move {
            server
                .serve(
                    sink,
                    || {
                        pipeline
                            .set_state(gst::State::Playing)
                            .map(|_| ())
                            .map_err(|err| NdError::Gst(err.to_string()))
                    },
                    cancelled,
                )
                .await
        });
        let join = || async {
            let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
            client
                .write_all(b"GET /test HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();
            client
        };

        // The first receiver starts the pipeline and keeps it running.
        let mut first = join().await;
        let draining = tokio::spawn(async move {
            let mut buffer = vec![0; 65536];
            while matches!(first.read(&mut buffer).await, Ok(n) if n > 0) {}
        });
        tokio::time::sleep(Duration::from_millis(1500)).await;

        let mut second = join().await;
        let mut received = Vec::new();
        let _ = tokio::time::timeout(Duration::from_millis(1000), async {
            let mut buffer = vec![0; 65536];
            while let Ok(n) = second.read(&mut buffer).await {
                if n == 0 {
                    break;
                }
                received.extend_from_slice(&buffer[..n]);
            }
        })
        .await;
        let body = received
            .split_at(received.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4)
            .1;
        // Its first packet is the PAT, and its first picture a key frame.
        assert_eq!(
            body.get(..3),
            Some(&[0x47, 0x40, 0x00][..]),
            "joined mid-stream"
        );
        let video = video_payload(body);
        let first_slice = video
            .windows(4)
            .find(|w| w[..3] == [0, 0, 1] && matches!(w[3] & 0x1f, 1 | 5))
            .map(|w| w[3] & 0x1f);
        assert_eq!(
            first_slice,
            Some(5),
            "a receiver that joins late starts before a key frame"
        );

        cancel.send_replace(true);
        let _ = serving.await;
        draining.abort();
        drop(guard);
    }
}
