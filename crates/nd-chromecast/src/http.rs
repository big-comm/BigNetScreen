//! The HTTP server that delivers the stream to a Chromecast.
//!
//! The receiver does not get media over the control channel: we send it a
//! **URL** (`LOAD`) and it opens a `GET` back to us. This module is that other
//! side.
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
//!    `403`), so as not to even confirm that the server exists.
//! 2. **An allowlist for the receiver's IP.** Even someone who learned the
//!    token is only served if they come from the Chromecast the session was
//!    opened with.
//!
//! The socket also listens **only on the IP of the interface that reaches the
//! receiver**, never on `0.0.0.0`.

use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use gst::prelude::*;
use gstreamer as gst;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use nd_core::{NdError, Result};

/// Tipo MIME do container servido (H.264 + AAC em Matroska).
pub const CONTENT_TYPE: &str = "video/x-matroska";

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
pub(crate) fn random_token() -> Result<String> {
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
    /// Unknown path: `404`, without revealing that a server is here.
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
    let Some(line) = request.lines().next() else {
        return Verdict::BadRequest;
    };
    let mut parts = line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return Verdict::BadRequest;
    };

    // The query string is not part of the secret.
    let path = path.split('?').next().unwrap_or(path);

    // The path is checked BEFORE the origin: answering 403 to an invalid path
    // would confirm the server's existence to a mere portscan.
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
fn response_headers(verdict: Verdict) -> String {
    let status = verdict.status_line();
    match verdict {
        Verdict::Stream | Verdict::HeadOnly => format!(
            "HTTP/1.1 {status}\r\n\
             Content-Type: {CONTENT_TYPE}\r\n\
             Cache-Control: no-cache, no-store, must-revalidate\r\n\
             Pragma: no-cache\r\n\
             Connection: close\r\n\
             Server: BigNetScreen\r\n\
             \r\n"
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

/// Reads the request up to the blank line, with a cap and a deadline.
async fn read_request(stream: &mut TcpStream) -> Result<String> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 512];

    loop {
        let read = tokio::time::timeout(REQUEST_TIMEOUT, stream.read(&mut chunk))
            .await
            .map_err(|_| NdError::Network("the client did not finish the request in time".into()))?
            .map_err(net_err)?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);

        if buffer.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        // Cap: the header comes off the network and must not grow unbounded.
        if buffer.len() > MAX_REQUEST_BYTES {
            return Err(NdError::Network("HTTP request too large".into()));
        }
    }

    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

/// The stream server for one cast session.
pub struct StreamServer {
    listener: TcpListener,
    /// The secret path, leading slash included.
    path: String,
    /// The only IP allowed to fetch the stream.
    allowed: IpAddr,
    local_addr: SocketAddr,
}

impl StreamServer {
    /// Brings the server up on an ephemeral port of the IP that reaches `receiver`.
    pub async fn bind(receiver: IpAddr) -> Result<Self> {
        let local_ip = local_ip_towards(receiver)?;
        // Port 0 = the kernel picks. Listening on this IP alone keeps the
        // stream off the other interfaces (VPN, docker0, loopback…).
        let listener = TcpListener::bind((local_ip, 0)).await.map_err(net_err)?;
        let local_addr = listener.local_addr().map_err(net_err)?;
        let path = format!("/{}", random_token()?);

        tracing::info!(%local_addr, %receiver, "stream server listening");
        Ok(Self {
            listener,
            path,
            allowed: receiver,
            local_addr,
        })
    }

    /// URL a enviar no `LOAD` do Chromecast.
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
        let mut cancel = cancel;
        let mut served_any = false;

        loop {
            let accepted = tokio::select! {
                result = self.listener.accept() => result,
                _ = cancel.changed() => {
                    tracing::debug!("servidor do stream encerrado a pedido");
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

            let request = match read_request(&mut stream).await {
                Ok(request) => request,
                Err(err) => {
                    tracing::debug!(%peer, %err, "request discarded");
                    let _ = stream
                        .write_all(response_headers(Verdict::BadRequest).as_bytes())
                        .await;
                    continue;
                }
            };

            let verdict = triage(&request, &self.path, self.allowed, peer.ip());
            if verdict != Verdict::Stream {
                if verdict == Verdict::Forbidden {
                    tracing::warn!(
                        %peer, esperado = %self.allowed,
                        "refusing a stream request from another address"
                    );
                } else {
                    tracing::debug!(%peer, ?verdict, "request refused");
                }
                let _ = stream.write_all(response_headers(verdict).as_bytes()).await;
                let _ = stream.shutdown().await;
                continue;
            }

            // Headers first; only then does GStreamer take the socket over.
            if let Err(err) = stream
                .write_all(response_headers(Verdict::Stream).as_bytes())
                .await
            {
                tracing::warn!(%peer, %err, "failed to write the headers");
                continue;
            }
            if let Err(err) = stream.flush().await {
                tracing::warn!(%peer, %err, "failed to flush the headers");
                continue;
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
        // Answering 403 would confirm the server's existence to a portscan.
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
        let headers = response_headers(Verdict::Stream);
        assert!(headers.contains("200 OK"), "{headers}");
        assert!(headers.contains(CONTENT_TYPE), "{headers}");
        assert!(!headers.contains("Content-Length"), "{headers}");
        assert!(!headers.contains("Transfer-Encoding"), "{headers}");
        assert!(headers.contains("Connection: close"), "{headers}");
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
            let headers = response_headers(verdict);
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
        let Ok(server) = StreamServer::bind(IpAddr::V4(Ipv4Addr::LOCALHOST)).await else {
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
        let Ok(server) = StreamServer::bind(IpAddr::V4(Ipv4Addr::LOCALHOST)).await else {
            return;
        };
        let addr = server.local_addr();

        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(b"GET /nada HTTP/1.1\r\nHost: x\r\n\r\n")
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
            .write_all(response_headers(verdict).as_bytes())
            .await
            .unwrap();
        drop(stream);

        let response = client.await.unwrap();
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    }
}
