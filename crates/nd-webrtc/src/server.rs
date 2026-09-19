//! The front door: the page, the PIN check, and the WHEP proxy.
//!
//! The WHEP element listens on the loopback interface only. Receivers never
//! reach it directly; they talk to this server, which lets them through once
//! the PIN has been accepted:
//!
//! 1. `GET /` — the page (no secret needed; it is only a form).
//! 2. `POST /pin` — the four digits. Right: `200` and the session token.
//!    Wrong: `403`. Too many wrong ones: `429` for a while, because four
//!    digits are only a lock if they cannot be tried ten thousand times.
//! 3. `POST /whep?token=…`, `PATCH`/`DELETE /whep/resource/<id>?token=…` —
//!    forwarded to the element. A missing or wrong token is a `404`, so the
//!    server does not even confirm what lives here.
//!
//! No TLS: the page stays on the local network, and a certificate a TV would
//! trust cannot be had for a private address. The token in the query string
//! is the same barrier the Chromecast path uses.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};

use nd_core::Result;

/// Cap on any request body (an SDP offer is a few kilobytes).
const MAX_BODY_BYTES: usize = 64 * 1024;
/// Wrong PINs tolerated before the door is locked.
const PIN_ATTEMPTS: u32 = 5;
/// How long the door stays locked after that.
const PIN_LOCKOUT: Duration = Duration::from_secs(30);
/// Time given to the element behind the door to answer.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(15);
/// How long to keep knocking while the element is still starting up.
///
/// `webrtcsink` only opens its WHEP server once it has seen the first frames
/// and settled on a codec, a moment after the pipeline starts. A receiver
/// that arrives in that window should wait, not be turned away.
const UPSTREAM_STARTUP: Duration = Duration::from_secs(8);
const UPSTREAM_RETRY: Duration = Duration::from_millis(200);

/// What a session hands to its front door.
#[derive(Clone, Debug)]
pub struct FrontDoorConfig {
    pub pin: String,
    pub token: String,
    /// Loopback port the WHEP element listens on.
    pub whep_port: u16,
    /// The rendered page.
    pub page: String,
}

#[derive(Debug, Default)]
struct PinGuard {
    failures: u32,
    locked_until: Option<Instant>,
}

struct Shared {
    config: FrontDoorConfig,
    guard: Mutex<PinGuard>,
}

/// A running front door. Dropping it stops accepting connections.
pub struct FrontDoor {
    local_addr: SocketAddr,
    stop: tokio::sync::watch::Sender<bool>,
}

impl FrontDoor {
    /// Where receivers connect.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl Drop for FrontDoor {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

/// Serves on an already bound listener until the returned handle is dropped.
pub fn serve(listener: TcpListener, config: FrontDoorConfig) -> Result<FrontDoor> {
    let local_addr = listener
        .local_addr()
        .map_err(|e| nd_core::NdError::Network(e.to_string()))?;
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    let shared = Arc::new(Shared {
        config,
        guard: Mutex::new(PinGuard::default()),
    });
    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                _ = stopped.changed() => break,
            };
            let (stream, peer) = match accepted {
                Ok(pair) => pair,
                Err(err) => {
                    tracing::debug!(%err, "front door accept failed");
                    continue;
                }
            };
            let shared = shared.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request| handle(shared.clone(), peer, request));
                if let Err(err) = hyper::server::conn::http1::Builder::new()
                    .keep_alive(true)
                    .serve_connection(TokioIo::new(stream), service)
                    .await
                {
                    tracing::debug!(%peer, %err, "front door connection ended with an error");
                }
            });
        }
        tracing::debug!("front door closed");
    });
    Ok(FrontDoor { local_addr, stop })
}

type Reply = Response<Full<Bytes>>;

fn reply(status: StatusCode, body: impl Into<Bytes>) -> Reply {
    Response::builder()
        .status(status)
        .body(Full::new(body.into()))
        .expect("a static response builds")
}

fn not_found() -> Reply {
    reply(StatusCode::NOT_FOUND, "")
}

/// The `token` query parameter, if any.
fn token_of(request: &Request<Incoming>) -> Option<&str> {
    request
        .uri()
        .query()?
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))
}

async fn read_body(request: Request<Incoming>) -> Option<Bytes> {
    let body = request.into_body();
    let limited = http_body_util::Limited::new(body, MAX_BODY_BYTES);
    limited.collect().await.ok().map(|c| c.to_bytes())
}

async fn handle(
    shared: Arc<Shared>,
    peer: SocketAddr,
    request: Request<Incoming>,
) -> std::result::Result<Reply, std::convert::Infallible> {
    let path = request.uri().path().to_string();
    let method = request.method().clone();
    tracing::trace!(%peer, %method, %path, "front door request");

    let response = match (method, path.as_str()) {
        (Method::GET, "/") | (Method::GET, "/index.html") => {
            let mut response = reply(StatusCode::OK, shared.config.page.clone());
            response.headers_mut().insert(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            response.headers_mut().insert(
                http::header::CACHE_CONTROL,
                HeaderValue::from_static("no-store"),
            );
            response
        }
        // Browsers ask for one on their own; an empty answer keeps their
        // console clean.
        (Method::GET, "/favicon.ico") => reply(StatusCode::NO_CONTENT, ""),
        (Method::POST, "/pin") => check_pin(&shared, peer, request).await,
        (Method::POST, "/whep") => {
            if token_of(&request) != Some(shared.config.token.as_str()) {
                not_found()
            } else {
                forward(&shared, request, "/whep/endpoint").await
            }
        }
        (Method::PATCH | Method::DELETE, path) if path.starts_with("/whep/resource/") => {
            if token_of(&request) != Some(shared.config.token.as_str()) {
                not_found()
            } else {
                let upstream_path = path.to_string();
                forward(&shared, request, &upstream_path).await
            }
        }
        _ => not_found(),
    };
    Ok(response)
}

async fn check_pin(shared: &Shared, peer: SocketAddr, request: Request<Incoming>) -> Reply {
    {
        let guard = shared
            .guard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(until) = guard.locked_until {
            if Instant::now() < until {
                return reply(StatusCode::TOO_MANY_REQUESTS, "");
            }
        }
    }
    let Some(body) = read_body(request).await else {
        return reply(StatusCode::BAD_REQUEST, "");
    };
    let attempt: String = String::from_utf8_lossy(&body)
        .chars()
        .filter(|c| c.is_ascii_digit())
        .take(8)
        .collect();
    let mut guard = shared
        .guard
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if attempt == shared.config.pin {
        *guard = PinGuard::default();
        tracing::info!(%peer, "receiver accepted with the PIN");
        return reply(StatusCode::OK, shared.config.token.clone());
    }
    guard.failures += 1;
    tracing::info!(%peer, failures = guard.failures, "wrong PIN");
    if guard.failures >= PIN_ATTEMPTS {
        guard.failures = 0;
        guard.locked_until = Some(Instant::now() + PIN_LOCKOUT);
        tracing::warn!(%peer, "too many wrong PINs; the door is locked for a while");
        return reply(StatusCode::TOO_MANY_REQUESTS, "");
    }
    reply(StatusCode::FORBIDDEN, "")
}

/// Forwards one request to the WHEP element on the loopback interface.
///
/// Only the headers that matter for WHEP travel: content type, `If-Match` for
/// trickle ICE, and back the content type and the resource `Location`, which
/// is rewritten to carry the token so the browser can `PATCH`/`DELETE` it.
async fn forward(shared: &Shared, request: Request<Incoming>, upstream_path: &str) -> Reply {
    let method = request.method().clone();
    let content_type = request.headers().get(http::header::CONTENT_TYPE).cloned();
    let if_match = request.headers().get(http::header::IF_MATCH).cloned();
    let Some(body) = read_body(request).await else {
        return reply(StatusCode::BAD_REQUEST, "");
    };

    let upstream = async {
        let stream = connect_upstream(shared.config.whep_port).await?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| format!("handshake: {e}"))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let mut builder = Request::builder()
            .method(method)
            .uri(upstream_path)
            .header(http::header::HOST, "127.0.0.1");
        if let Some(value) = content_type {
            builder = builder.header(http::header::CONTENT_TYPE, value);
        }
        if let Some(value) = if_match {
            builder = builder.header(http::header::IF_MATCH, value);
        }
        let request = builder
            .body(Full::new(body))
            .map_err(|e| format!("request: {e}"))?;
        let response = sender
            .send_request(request)
            .await
            .map_err(|e| format!("send: {e}"))?;
        let status = response.status();
        let content_type = response.headers().get(http::header::CONTENT_TYPE).cloned();
        let location = response.headers().get(http::header::LOCATION).cloned();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| format!("body: {e}"))?
            .to_bytes();
        Ok::<_, String>((status, content_type, location, body))
    };

    match tokio::time::timeout(UPSTREAM_TIMEOUT, upstream).await {
        Ok(Ok((status, content_type, location, body))) => {
            let mut response = reply(status, body);
            if let Some(value) = content_type {
                response
                    .headers_mut()
                    .insert(http::header::CONTENT_TYPE, value);
            }
            if let Some(value) = location {
                // `/whep/resource/<id>` → `/whep/resource/<id>?token=…`
                let with_token = format!(
                    "{}?token={}",
                    value.to_str().unwrap_or_default(),
                    shared.config.token
                );
                if let Ok(value) = HeaderValue::from_str(&with_token) {
                    response.headers_mut().insert(http::header::LOCATION, value);
                }
            }
            response
        }
        Ok(Err(err)) => {
            tracing::warn!(%err, "the WHEP element did not answer");
            reply(StatusCode::BAD_GATEWAY, "")
        }
        Err(_) => {
            tracing::warn!("the WHEP element timed out");
            reply(StatusCode::GATEWAY_TIMEOUT, "")
        }
    }
}

/// Connects to the element, retrying while it is still coming up.
async fn connect_upstream(port: u16) -> std::result::Result<TcpStream, String> {
    let deadline = Instant::now() + UPSTREAM_STARTUP;
    loop {
        match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(stream) => return Ok(stream),
            Err(err) if Instant::now() < deadline => {
                tracing::trace!(%err, "WHEP element not up yet; retrying");
                tokio::time::sleep(UPSTREAM_RETRY).await;
            }
            Err(err) => return Err(format!("connect: {err}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn door(pin: &str, whep_port: u16) -> FrontDoor {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        serve(
            listener,
            FrontDoorConfig {
                pin: pin.into(),
                token: "feedface".into(),
                whep_port,
                page: "<html>page</html>".into(),
            },
        )
        .unwrap()
    }

    async fn request(addr: SocketAddr, raw: &str) -> (u16, String, String) {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(raw.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf).to_string();
        let status: u16 = text
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let (headers, body) = text.split_once("\r\n\r\n").unwrap_or(("", ""));
        (status, headers.to_string(), body.to_string())
    }

    fn post(path: &str, content_type: &str, body: &str) -> String {
        format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn the_page_is_served_and_the_rest_is_hidden() {
        let door = door("1234", 1).await;
        let (status, headers, body) = request(
            door.local_addr(),
            "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(status, 200);
        assert!(headers.contains("text/html"));
        assert_eq!(body, "<html>page</html>");
        for path in [
            "/secret",
            "/whep",
            "/whep?token=wrong",
            "/whep/resource/abc",
        ] {
            let (status, _, _) = request(
                door.local_addr(),
                &format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
            )
            .await;
            assert_eq!(status, 404, "{path}");
        }
        let (status, _, _) =
            request(door.local_addr(), &post("/whep", "application/sdp", "v=0")).await;
        assert_eq!(
            status, 404,
            "media without a token must look like nothing is here"
        );
    }

    #[tokio::test]
    async fn the_pin_opens_the_door_and_wrong_ones_lock_it() {
        let door = door("4321", 1).await;
        let (status, _, _) = request(door.local_addr(), &post("/pin", "text/plain", "1111")).await;
        assert_eq!(status, 403);
        let (status, _, body) =
            request(door.local_addr(), &post("/pin", "text/plain", " 4321\n")).await;
        assert_eq!(status, 200);
        assert_eq!(body, "feedface");
        for _ in 0..PIN_ATTEMPTS {
            request(door.local_addr(), &post("/pin", "text/plain", "0000")).await;
        }
        let (status, _, _) = request(door.local_addr(), &post("/pin", "text/plain", "4321")).await;
        assert_eq!(
            status, 429,
            "even the right PIN waits while the door is locked"
        );
    }

    #[tokio::test]
    async fn media_requests_are_forwarded_with_the_token_added_to_location() {
        // A stand-in for the WHEP element: answers any POST with 201 + Location.
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let whep_port = upstream.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = upstream.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = stream.read(&mut buf).await.unwrap();
                    let seen = String::from_utf8_lossy(&buf[..n]).to_string();
                    assert!(seen.starts_with("POST /whep/endpoint HTTP/1.1"), "{seen}");
                    assert!(seen.contains("content-type: application/sdp"), "{seen}");
                    assert!(seen.ends_with("v=0 offer"), "{seen}");
                    let body = "v=0 answer";
                    let reply = format!(
                        "HTTP/1.1 201 Created\r\nContent-Type: application/sdp\r\nLocation: /whep/resource/abc\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(reply.as_bytes()).await.unwrap();
                });
            }
        });
        let door = door("1234", whep_port).await;
        let (status, headers, body) = request(
            door.local_addr(),
            &post("/whep?token=feedface", "application/sdp", "v=0 offer"),
        )
        .await;
        assert_eq!(status, 201);
        assert!(
            headers.contains("location: /whep/resource/abc?token=feedface"),
            "{headers}"
        );
        assert!(
            headers.contains("content-type: application/sdp"),
            "{headers}"
        );
        assert_eq!(body, "v=0 answer");
    }

    #[tokio::test]
    async fn an_absent_element_is_a_bad_gateway_not_a_hang() {
        let door = door("1234", free_port()).await;
        let (status, _, _) = request(
            door.local_addr(),
            &post("/whep?token=feedface", "application/sdp", "v=0"),
        )
        .await;
        assert_eq!(status, 502);
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }
}
