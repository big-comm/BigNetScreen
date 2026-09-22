//! The HTTP server that delivers the stream to a receiver that fetches it.
//!
//! Two protocols work this way and share every byte of this module. Neither
//! gets media over its control channel: we hand the receiver a **URL** — Cast's
//! `LOAD`, DLNA's `SetAVTransportURI` — and it opens a `GET` back to us. This
//! is that other side. The only difference between them is what the response
//! says the body is, which is [`MediaType`].
//!
//! ## Why hand-written HTTP instead of `hyper`
//!
//! The pipeline ends in `multisocketsink`, which writes **straight into the
//! receiver's socket** — no intermediate copy, and no bytes passing through
//! the async runtime. That requires handing the socket descriptor to GStreamer
//! after the headers have been written, and an HTTP framework will not give
//! back the raw connection halfway through a response. It is the same design
//! as the reference C project (`src/cc/cc-http-server.c`, libsoup +
//! `wrote-headers` → `add`).
//!
//! The HTTP surface is tiny (one `GET`, one EOF-terminated response), which
//! makes a hand-written parser a reasonable trade for the latency gain.
//!
//! ## Protections
//!
//! The stream is the user's screen: anyone on the LAN who can reach it is
//! watching their desktop. Two barriers, as in the C code:
//!
//! 1. **A random 128-bit token in the path.** A portscan will not find the
//!    stream; the path has to be guessed. A wrong path gives `404` (not
//!    `403`), without disclosing whether a particular protected path exists.
//! 2. **An allowlist for the receiver's IP.** Even someone who learned the
//!    token is only served if they come from the Chromecast the session was
//!    opened with.
//!
//! The socket also listens **only on the IP of the interface that reaches the
//! receiver**, never on `0.0.0.0`.

use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use gio::prelude::*;
use gstreamer as gst;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::{NdError, Result};

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
/// sn-increasing (bits 27 and 26) — DLNA's signature for content with no fixed
/// start and no end — plus streaming transfer mode and DLNA 1.5.
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

/// Cap on a request's header size.
const MAX_REQUEST_BYTES: usize = 8 * 1024;
/// How long the client gets to finish sending the request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

fn net_err<E: std::fmt::Display>(e: E) -> NdError {
    NdError::Network(e.to_string())
}

/// Generates a random 128-bit token in hexadecimal.
///
/// Reads from `/dev/urandom` so as not to drag in another dependency for the
/// sake of 16 bytes.
pub fn random_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| NdError::Network(format!("could not generate the session token: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Finds the local IP of the interface that reaches `peer`.
///
/// The classic trick: a "connected" UDP socket sends nothing, but it makes the
/// kernel pick the route — and with it the source address. Far better than
/// guessing the first interface: on a machine with a VPN (Tailscale, say) the
/// wrong IP leaves the Chromecast unable to connect back.
pub fn local_ip_towards(peer: IpAddr) -> Result<IpAddr> {
    let bind: SocketAddr = if peer.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let socket = std::net::UdpSocket::bind(bind).map_err(net_err)?;
    socket.connect((peer, 9)).map_err(net_err)?;
    Ok(socket.local_addr().map_err(net_err)?.ip())
}

/// The outcome of triaging a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    /// Serve o stream.
    Stream,
    /// Headers only (the receiver sometimes probes with `HEAD`).
    HeadOnly,
    /// Unknown path: `404`, without disclosing a protected resource.
    NotFound,
    /// Correct token, but from another origin: `403`.
    Forbidden,
    /// Unsupported method.
    MethodNotAllowed,
    /// Malformed or oversized request.
    BadRequest,
}

impl Verdict {
    fn status_line(self) -> &'static str {
        match self {
            Verdict::Stream | Verdict::HeadOnly => "200 OK",
            Verdict::NotFound => "404 Not Found",
            Verdict::Forbidden => "403 Forbidden",
            Verdict::MethodNotAllowed => "405 Method Not Allowed",
            Verdict::BadRequest => "400 Bad Request",
        }
    }
}

/// Decides what to do with an already-read request.
///
/// Kept apart from the I/O so it can be tested without a network.
fn triage(request: &str, expected_path: &str, allowed: IpAddr, from: IpAddr) -> Verdict {
    let Some((method, path)) = request_line(request) else {
        return Verdict::BadRequest;
    };

    // The query string is not part of the secret.
    let path = path.split('?').next().unwrap_or(path);

    // Do not reveal whether an incorrect path names a protected resource.
    // The live listener independently rejects a foreign IP before reading.
    if path != expected_path {
        return Verdict::NotFound;
    }

    if from != allowed {
        return Verdict::Forbidden;
    }

    match method {
        "GET" => Verdict::Stream,
        "HEAD" => Verdict::HeadOnly,
        _ => Verdict::MethodNotAllowed,
    }
}

/// Builds the response's header block.
///
/// No `Content-Length` and no `Transfer-Encoding`: the body ends when the
/// connection closes (the equivalent of the C code's `SOUP_ENCODING_EOF`).
/// That is what allows streaming indefinitely without knowing the size up
/// front.
fn response_headers(verdict: Verdict, media: MediaType) -> String {
    let status = verdict.status_line();
    match verdict {
        Verdict::Stream | Verdict::HeadOnly => format!(
            "HTTP/1.1 {status}\r\n\
             Content-Type: {content_type}\r\n\
             {extra}\
             Cache-Control: no-cache, no-store, must-revalidate\r\n\
             Pragma: no-cache\r\n\
             Connection: close\r\n\
             Server: BigNetScreen\r\n\
             \r\n",
            content_type = media.content_type,
            extra = media.extra_headers,
        ),
        _ => format!(
            "HTTP/1.1 {status}\r\n\
             Content-Length: 0\r\n\
             Connection: close\r\n\
             Server: BigNetScreen\r\n\
             \r\n"
        ),
    }
}

/// Minimal origin-form HTTP/1.x requests, no body or ambiguous framing.
/// Kept shared with file serving so both token-protected paths reject the same
/// malformed syntax. These endpoints intentionally do not implement uploads.
pub fn request_line(request: &str) -> Option<(&str, &str)> {
    if !request.is_ascii() || request.len() > MAX_REQUEST_BYTES {
        return None;
    }
    let mut lines = request.strip_suffix("\r\n\r\n")?.split("\r\n");
    let mut parts = lines.next()?.split(' ');
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some()
        || method.is_empty()
        || !method.bytes().all(token_char)
        || !target.starts_with('/')
        || target.bytes().any(|b| b <= 32 || b == 127)
        || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
    {
        return None;
    }
    let mut host = false;
    let mut length = false;
    let mut range = false;
    for line in lines {
        let (name, value) = line.split_once(':')?;
        if name.is_empty()
            || !name.bytes().all(token_char)
            || value.bytes().any(|b| (b < 32 && b != b'\t') || b == 127)
        {
            return None;
        }
        let value = value.trim();
        if name.eq_ignore_ascii_case("host") {
            if host || value.is_empty() {
                return None;
            }
            host = true;
        } else if name.eq_ignore_ascii_case("content-length") {
            if length || value != "0" {
                return None;
            }
            length = true;
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            return None;
        } else if name.eq_ignore_ascii_case("range") {
            if range {
                return None;
            }
            range = true;
        }
    }
    (version == "HTTP/1.0" || host).then_some((method, target))
}

fn token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// The deadline covers the WHOLE request, not a fresh timeout per byte/chunk.
pub async fn read_request<R: AsyncRead + Unpin>(stream: &mut R) -> Result<String> {
    read_request_with_timeout(stream, REQUEST_TIMEOUT).await
}

async fn read_request_with_timeout<R: AsyncRead + Unpin>(
    stream: &mut R,
    timeout: Duration,
) -> Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 512];
    loop {
        let read = tokio::time::timeout_at(deadline, stream.read(&mut chunk))
            .await
            .map_err(|_| NdError::Network("the client did not finish the request in time".into()))?
            .map_err(net_err)?;
        if read == 0 {
            return Err(NdError::Network("incomplete HTTP headers".into()));
        }
        buffer.extend_from_slice(&chunk[..read]);
        // Check BEFORE accepting a terminating delimiter in an oversized last chunk.
        if buffer.len() > MAX_REQUEST_BYTES {
            return Err(NdError::Network("HTTP request too large".into()));
        }
        if buffer.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let request = String::from_utf8(buffer)
        .map_err(|_| NdError::Protocol("invalid HTTP header encoding".into()))?;
    if request_line(&request).is_none() {
        return Err(NdError::Protocol("malformed HTTP request".into()));
    }
    Ok(request)
}

/// Cancelled writes are followed by dropping the connection, never a retry on
/// the same partially written response. File bodies also have a per-chunk limit.
pub async fn write_bounded(stream: &mut TcpStream, bytes: &[u8]) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), stream.write_all(bytes))
        .await
        .map_err(|_| NdError::Network("HTTP peer stopped reading".into()))?
        .map_err(net_err)
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
}

impl StreamServer {
    /// Brings the server up on an ephemeral port of the IP that reaches `receiver`.
    pub async fn bind(receiver: IpAddr, media: MediaType) -> Result<Self> {
        let local_ip = local_ip_towards(receiver)?;
        // Port 0 = the kernel picks. Listening on this IP alone keeps the
        // stream off the other interfaces (VPN, docker0, loopback…).
        let listener = TcpListener::bind((local_ip, crate::settings::current().port))
            .await
            .map_err(net_err)?;
        let local_addr = listener.local_addr().map_err(net_err)?;
        let path = format!("/{}", random_token()?);

        tracing::info!(%local_addr, %receiver, "stream server listening");
        Ok(Self {
            listener,
            path,
            allowed: receiver,
            local_addr,
            media,
        })
    }

    /// The URL to hand the receiver.
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
    /// `on_first_client` is called when the first client is accepted — the
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
        // Closing the descriptor is this side's job once GStreamer gives it
        // back — it was this side that handed it over. Wired here rather than
        // by each caller so a second protocol cannot forget it and leak a
        // socket per disconnect.
        sink.connect("client-socket-removed", false, |values| {
            if let Ok(socket) = values[1].get::<gio::Socket>() {
                let _ = socket.close();
            }
            tracing::info!("the receiver disconnected from the stream");
            None
        });

        let mut cancel = cancel;
        let mut served_any = false;

        loop {
            if *cancel.borrow() {
                return Ok(());
            }
            let accepted = tokio::select! {
                result = self.listener.accept() => result,
                _ = cancel.changed() => {
                    tracing::debug!("stream server stopped on request");
                    return Ok(());
                }
            };

            let (mut stream, peer) = match accepted {
                Ok(pair) => pair,
                Err(err) => {
                    tracing::warn!(%err, "failed to accept a connection");
                    continue;
                }
            };

            // Reject another IP before allowing it to occupy the serial reader.
            if peer.ip() != self.allowed {
                continue;
            }
            let read = tokio::select! {
                result = read_request(&mut stream) => result,
                _ = cancel.changed() => return Ok(()),
            };
            let request = match read {
                Ok(request) => request,
                Err(err) => {
                    tracing::debug!(%peer, %err, "request discarded");
                    let _ = write_bounded(
                        &mut stream,
                        response_headers(Verdict::BadRequest, self.media).as_bytes(),
                    )
                    .await;
                    continue;
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
                let _ = write_bounded(
                    &mut stream,
                    response_headers(verdict, self.media).as_bytes(),
                )
                .await;
                // Dropping TcpStream closes it; no unbounded shutdown wait.
                continue;
            }

            // Headers first; only then does GStreamer take the socket over.
            let headers = response_headers(Verdict::Stream, self.media);
            let written = tokio::select! {
                result = write_bounded(&mut stream, headers.as_bytes()) => result,
                _ = cancel.changed() => return Ok(()),
            };
            if let Err(err) = written {
                tracing::warn!(%peer, %err, "failed to write the headers");
                continue;
            }
            if *cancel.borrow() {
                return Ok(());
            }

            match hand_socket_to_sink(stream, &sink) {
                Ok(()) => tracing::info!(%peer, "receiver connected to the stream"),
                Err(err) => {
                    tracing::error!(%peer, %err, "could not hand the socket to the pipeline");
                    continue;
                }
            }

            if !served_any {
                served_any = true;
                on_first_client()?;
            }
        }
    }
}

/// Transfers ownership of the TCP socket to `multisocketsink`.
///
/// A detail that only shows up in practice: the descriptor coming from tokio
/// is in **non-blocking** mode, but `multisocketsink` writes from its own
/// streaming thread and expects a blocking socket. Without this GStreamer
/// would see `EAGAIN` and treat it as a client error.
///
/// Ownership of the descriptor passes to the `gio::Socket` (and from there to
/// GStreamer); if construction fails, the `OwnedFd` is consumed and closed
/// right there.
fn hand_socket_to_sink(stream: TcpStream, sink: &gst::Element) -> Result<()> {
    let std_stream = stream.into_std().map_err(net_err)?;
    std_stream.set_nonblocking(false).map_err(net_err)?;

    let socket = gio::Socket::from_fd(std::os::fd::OwnedFd::from(std_stream))
        .map_err(|e| NdError::Network(format!("gio::Socket: {e}")))?;

    sink.emit_by_name::<()>("add", &[&socket]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    const CAST: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 0, 50));
    const INTRUSO: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 0, 99));

    fn get(path: &str) -> String {
        format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n")
    }

    #[test]
    fn token_is_128_bits_of_hex() {
        let token = random_token().expect("/dev/urandom");
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        // Two tokens in a row must not coincide.
        assert_ne!(token, random_token().unwrap());
    }

    #[test]
    fn serves_the_receiver_on_the_right_path() {
        assert_eq!(
            triage(&get("/segredo"), "/segredo", CAST, CAST),
            Verdict::Stream
        );
    }

    #[test]
    fn wrong_path_is_404_not_403() {
        // Do not distinguish protected resources from unknown paths.
        assert_eq!(
            triage(&get("/chute"), "/segredo", CAST, CAST),
            Verdict::NotFound
        );
        // Including for those already on the allowlist.
        assert_eq!(triage(&get("/"), "/segredo", CAST, CAST), Verdict::NotFound);
    }

    #[test]
    fn right_path_from_another_host_is_rejected() {
        // Defence in depth: even with the token leaked, only the session's
        // receiver is served.
        assert_eq!(
            triage(&get("/segredo"), "/segredo", CAST, INTRUSO),
            Verdict::Forbidden
        );
    }

    #[test]
    fn query_string_is_not_part_of_the_secret() {
        assert_eq!(
            triage(&get("/segredo?x=1"), "/segredo", CAST, CAST),
            Verdict::Stream
        );
    }

    #[test]
    fn head_probe_is_answered_without_a_body() {
        let request = "HEAD /segredo HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(triage(request, "/segredo", CAST, CAST), Verdict::HeadOnly);
    }

    #[test]
    fn other_methods_are_refused() {
        let request = "POST /segredo HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(
            triage(request, "/segredo", CAST, CAST),
            Verdict::MethodNotAllowed
        );
    }

    #[test]
    fn malformed_requests_do_not_panic() {
        for raw in ["", "\r\n", "LIXO", "GET", "GET  \r\n"] {
            let verdict = triage(raw, "/segredo", CAST, CAST);
            assert_ne!(verdict, Verdict::Stream, "aceitou {raw:?}");
        }
    }

    #[test]
    fn stream_response_has_no_length_so_it_can_run_forever() {
        let headers = response_headers(Verdict::Stream, CAST_MEDIA);
        assert!(headers.contains("200 OK"), "{headers}");
        assert!(headers.contains(CAST_MEDIA.content_type), "{headers}");
        assert!(!headers.contains("Content-Length"), "{headers}");
        assert!(!headers.contains("Transfer-Encoding"), "{headers}");
        assert!(headers.contains("Connection: close"), "{headers}");
        assert!(headers.ends_with("\r\n\r\n"), "{headers}");
    }

    #[test]
    fn a_dlna_renderer_is_told_the_stream_is_live() {
        // A renderer that gets no `transferMode` treats the body as a file to
        // download, and one that gets no flags has no way to know the content
        // never ends. Both were verified against a Panasonic VIErA, which
        // probes with `HEAD` and `getcontentFeatures.dlna.org: 1` before it
        // fetches a single byte.
        let headers = response_headers(Verdict::Stream, DLNA_MEDIA);
        assert!(headers.contains("Content-Type: video/mpeg"), "{headers}");
        assert!(
            headers.contains("transferMode.dlna.org: Streaming"),
            "{headers}"
        );
        assert!(
            headers.contains("DLNA.ORG_FLAGS=8d100000"),
            "sender-paced, s0- and sn-increasing: {headers}"
        );
        // Still no length, or the receiver waits for an end that never comes.
        assert!(!headers.contains("Content-Length"), "{headers}");
        assert!(headers.ends_with("\r\n\r\n"), "{headers}");
    }

    #[test]
    fn error_responses_close_cleanly() {
        for verdict in [
            Verdict::NotFound,
            Verdict::Forbidden,
            Verdict::MethodNotAllowed,
            Verdict::BadRequest,
        ] {
            let headers = response_headers(verdict, CAST_MEDIA);
            assert!(headers.contains("Content-Length: 0"), "{headers}");
        }
    }

    #[test]
    fn local_ip_towards_picks_a_routable_address() {
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
    async fn refuses_a_request_on_the_wrong_path() {
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
    #[test]
    fn rejects_ambiguous_or_incomplete_http_framing() {
        for raw in [
            "GET /secret\r\n\r\n",
            "GET /secret HTTP/1.1\r\n\r\n",
            "GET /secret HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n",
            "GET /secret HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n",
            "GET /secret HTTP/1.1\r\nHost: a\r\nContent-Length: 1\r\n\r\n",
            "GET /secret HTTP/1.1\r\nHost: a\r\n folded: bad\r\n\r\n",
            "GET /secret HTTP/1.1\r\nHost: a\r\nX: \0\r\n\r\n",
        ] {
            assert!(request_line(raw).is_none(), "accepted {raw:?}");
        }
        assert_eq!(
            request_line("GET /secret HTTP/1.0\r\n\r\n"),
            Some(("GET", "/secret"))
        );
    }

    #[tokio::test]
    async fn header_reader_rejects_eof_invalid_utf8_and_oversized_final_chunk() {
        for bytes in [
            b"GET /secret HTTP/1.1\r\nHost: a\r\n".to_vec(),
            b"GET /secret HTTP/1.1\r\nHost: \xff\r\n\r\n".to_vec(),
            format!(
                "GET /secret HTTP/1.1\r\nHost: a\r\nX: {}\r\n\r\n",
                "x".repeat(MAX_REQUEST_BYTES)
            )
            .into_bytes(),
        ] {
            assert!(read_request(&mut bytes.as_slice()).await.is_err());
        }
    }

    #[tokio::test]
    async fn a_partial_header_cannot_hold_the_reader_indefinitely() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer
            .write_all(b"GET /secret HTTP/1.1\r\nHost: a")
            .await
            .unwrap();
        assert!(
            read_request_with_timeout(&mut reader, Duration::from_millis(20))
                .await
                .is_err()
        );
    }
}
