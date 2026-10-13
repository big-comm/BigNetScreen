//! The front door: the page, the PIN check, and the WHEP proxy.
//!
//! The WHEP element listens on the loopback interface only. Receivers never
//! reach it directly; they talk to this server, which lets them through once
//! the PIN has been accepted:
//!
//! 1. `GET /`: the page (no secret needed; it is only a form).
//! 2. `POST /pin`: the four digits. Right: `200` and the session token.
//!    Wrong: `403`. Too many wrong ones: `429` for a while, because four
//!    digits are only a lock if they cannot be tried ten thousand times.
//! 3. `POST /whep?token=…`, `PATCH`/`DELETE /whep/resource/<id>?token=…`:
//!    forwarded to the element. A missing or wrong token is a `404`, so the
//!    server does not even confirm what lives here.
//!
//! No TLS: the page stays on the local network, and a certificate a TV would
//! trust cannot be had for a private address. The token in the query string
//! is the same barrier the Chromecast path uses.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};

use nd_core::Result;
use nd_core::secret::same_secret;

/// Cap on any request body (an SDP offer is a few kilobytes).
const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_CLIENTS: usize = 32;
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(60);
/// Wrong PINs tolerated from one address before it is locked out.
const PIN_ATTEMPTS: u32 = 5;
/// Wrong PINs tolerated from all addresses together before the door locks
/// for everyone: per-address lockouts alone let many addresses guess in
/// parallel, and one address cannot reach this by itself.
const PIN_ATTEMPTS_ALL: u32 = 4 * PIN_ATTEMPTS;
/// Addresses whose attempts are remembered; past this, addresses that are
/// not locked out are forgotten, and the shared limit still applies.
const MAX_TRACKED_PEERS: usize = 256;
/// How long the door stays locked the first time; each later lockout in the
/// same publication lasts twice the one before, up to [`PIN_LOCKOUT_MAX`].
const PIN_LOCKOUT: Duration = Duration::from_secs(30);
const PIN_LOCKOUT_MAX: Duration = Duration::from_secs(15 * 60);
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
    /// The length of the last lockout, kept through a right PIN so that
    /// someone else joining does not hand short lockouts back to a guesser.
    last_lockout: Option<Duration>,
}

impl PinGuard {
    fn locked(&self, now: Instant) -> bool {
        self.locked_until.is_some_and(|until| now < until)
    }

    /// Counts a wrong PIN; `true` when it locks this guard.
    fn fail(&mut self, limit: u32, now: Instant) -> bool {
        self.failures += 1;
        if self.failures < limit {
            return false;
        }
        let lockout = self
            .last_lockout
            .map_or(PIN_LOCKOUT, |last| (last * 2).min(PIN_LOCKOUT_MAX));
        self.failures = 0;
        self.last_lockout = Some(lockout);
        self.locked_until = Some(now + lockout);
        true
    }
}

/// Wrong-PIN accounting per address, plus one shared limit.
#[derive(Debug, Default)]
struct PinGuards {
    all: PinGuard,
    peers: HashMap<IpAddr, PinGuard>,
}

impl PinGuards {
    fn locked(&self, peer: IpAddr, now: Instant) -> bool {
        self.all.locked(now) || self.peers.get(&peer).is_some_and(|guard| guard.locked(now))
    }
}

struct Shared {
    config: FrontDoorConfig,
    guards: Mutex<PinGuards>,
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
        guards: Mutex::new(PinGuards::default()),
    });
    tokio::spawn(async move {
        // Dropping the front door must close existing HTTP sessions too, not
        // leave token-bearing keep-alive tasks alive after publication ends.
        let mut clients = tokio::task::JoinSet::new();
        loop {
            if *stopped.borrow() {
                break;
            }
            let accepted = tokio::select! {
                biased;
                _ = stopped.changed() => break,
                _ = clients.join_next(), if !clients.is_empty() => continue,
                accepted = listener.accept() => accepted,
            };
            let (stream, peer) = match accepted {
                Ok(pair) => pair,
                Err(err) => {
                    tracing::debug!(%err, "front door accept failed");
                    // Out of descriptors, accept fails again at once.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            if clients.len() >= MAX_CLIENTS {
                drop(stream);
                continue;
            }
            let shared = shared.clone();
            clients.spawn(async move {
                let service = service_fn(move |request| handle(shared.clone(), peer, request));
                let mut http = hyper::server::conn::http1::Builder::new();
                http.keep_alive(false)
                    .max_buf_size(16 * 1024)
                    .timer(TokioTimer::new())
                    .header_read_timeout(REQUEST_READ_TIMEOUT);
                let result = tokio::time::timeout(
                    CONNECTION_TIMEOUT,
                    http.serve_connection(TokioIo::new(stream), service),
                )
                .await;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => tracing::debug!(%peer, %err, "front door connection failed"),
                    Err(_) => tracing::debug!(%peer, "front door connection timed out"),
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

/// Whether the request's `token` query parameter is the session token.
fn carries_token(request: &Request<Incoming>, token: &str) -> bool {
    request
        .uri()
        .query()
        .and_then(|query| {
            query
                .split('&')
                .find_map(|pair| pair.strip_prefix("token="))
        })
        .is_some_and(|guess| same_secret(guess, token))
}

async fn read_body(request: Request<Incoming>) -> Option<Bytes> {
    let body = request.into_body();
    let limited = http_body_util::Limited::new(body, MAX_BODY_BYTES);
    tokio::time::timeout(REQUEST_READ_TIMEOUT, limited.collect())
        .await
        .ok()?
        .ok()
        .map(|c| c.to_bytes())
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
            if !carries_token(&request, &shared.config.token) {
                not_found()
            } else {
                forward(&shared, request, "/whep/endpoint").await
            }
        }
        (Method::PATCH | Method::DELETE, path) if path.starts_with("/whep/resource/") => {
            if !carries_token(&request, &shared.config.token) {
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
    let locked = shared
        .guards
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .locked(peer.ip(), Instant::now());
    if locked {
        return reply(StatusCode::TOO_MANY_REQUESTS, "");
    }
    let Some(body) = read_body(request).await else {
        return reply(StatusCode::BAD_REQUEST, "");
    };
    // Only surrounding whitespace is ignored. Do not transform arbitrary
    // attacker-controlled text into a valid PIN by discarding its characters.
    let attempt = std::str::from_utf8(&body).unwrap_or_default().trim();
    let mut guards = shared
        .guards
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Checked again after the body read, under the lock that records the
    // attempt, so parallel slow POSTs cannot slip past a lockout.
    let status = record_pin_attempt(
        &mut guards,
        peer.ip(),
        attempt,
        &shared.config.pin,
        Instant::now(),
    );
    if status == StatusCode::OK {
        tracing::info!(%peer, "receiver accepted with the PIN");
        let mut response = reply(status, shared.config.token.clone());
        response.headers_mut().insert(
            http::header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        );
        return response;
    }
    tracing::debug!(%peer, %status, "PIN request rejected");
    reply(status, "")
}

fn record_pin_attempt(
    guards: &mut PinGuards,
    peer: IpAddr,
    attempt: &str,
    expected: &str,
    now: Instant,
) -> StatusCode {
    if guards.locked(peer, now) {
        return StatusCode::TOO_MANY_REQUESTS;
    }
    if same_secret(attempt, expected) {
        if let Some(guard) = guards.peers.get_mut(&peer) {
            guard.failures = 0;
        }
        return StatusCode::OK;
    }
    if guards.peers.len() >= MAX_TRACKED_PEERS && !guards.peers.contains_key(&peer) {
        guards.peers.retain(|_, guard| guard.locked(now));
    }
    let peer_locked = guards
        .peers
        .entry(peer)
        .or_default()
        .fail(PIN_ATTEMPTS, now);
    let all_locked = guards.all.fail(PIN_ATTEMPTS_ALL, now);
    if peer_locked || all_locked {
        StatusCode::TOO_MANY_REQUESTS
    } else {
        StatusCode::FORBIDDEN
    }
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
        let body = http_body_util::Limited::new(response.into_body(), MAX_BODY_BYTES)
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
    async fn only_the_page_is_served() {
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
    async fn wrong_pins_lock_the_door() {
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
    async fn forwarded_location_carries_token() {
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
    async fn absent_element_is_bad_gateway() {
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
    const PEER: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 20));

    fn peer(last: u8) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, last))
    }

    #[test]
    fn parallel_attempt_respects_lockout() {
        let now = Instant::now();
        let mut guards = PinGuards::default();
        // Bodies whose reads started before the lockout.
        for _ in 0..PIN_ATTEMPTS {
            record_pin_attempt(&mut guards, PEER, "0000", "1234", now);
        }
        for _ in 0..100 {
            assert_eq!(
                record_pin_attempt(&mut guards, PEER, "1234", "1234", now),
                StatusCode::TOO_MANY_REQUESTS
            );
        }
        assert_eq!(
            record_pin_attempt(&mut guards, PEER, "1234", "1234", now + PIN_LOCKOUT),
            StatusCode::OK
        );
    }

    #[test]
    fn guesser_locks_only_itself() {
        let now = Instant::now();
        let mut guards = PinGuards::default();
        for _ in 0..PIN_ATTEMPTS {
            record_pin_attempt(&mut guards, peer(66), "0000", "1234", now);
        }
        assert_eq!(
            record_pin_attempt(&mut guards, peer(66), "1234", "1234", now),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            record_pin_attempt(&mut guards, PEER, "1234", "1234", now),
            StatusCode::OK
        );
    }

    #[test]
    fn many_guessers_lock_everyone() {
        let now = Instant::now();
        let mut guards = PinGuards::default();
        for attempt in 0..PIN_ATTEMPTS_ALL {
            let from = peer(100 + (attempt / (PIN_ATTEMPTS - 1)) as u8);
            record_pin_attempt(&mut guards, from, "0000", "1234", now);
        }
        assert_eq!(
            record_pin_attempt(&mut guards, PEER, "1234", "1234", now),
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[test]
    fn lockout_doubles_up_to_ceiling() {
        let mut now = Instant::now();
        let mut guards = PinGuards::default();
        let lock = |guards: &mut PinGuards, now: Instant| {
            for _ in 0..PIN_ATTEMPTS {
                record_pin_attempt(guards, PEER, "0000", "1234", now);
            }
            guards.peers[&PEER].locked_until.unwrap() - now
        };
        assert_eq!(lock(&mut guards, now), PIN_LOCKOUT);
        now += PIN_LOCKOUT;
        assert_eq!(lock(&mut guards, now), PIN_LOCKOUT * 2);
        now += PIN_LOCKOUT * 2;
        // Someone else joining does not hand an attacker short lockouts again.
        assert_eq!(
            record_pin_attempt(&mut guards, PEER, "1234", "1234", now),
            StatusCode::OK
        );
        assert_eq!(lock(&mut guards, now), PIN_LOCKOUT * 4);
        for _ in 0..20 {
            now += PIN_LOCKOUT_MAX;
            lock(&mut guards, now);
        }
        assert_eq!(lock(&mut guards, now + PIN_LOCKOUT_MAX), PIN_LOCKOUT_MAX);
    }

    #[tokio::test]
    async fn pin_keeps_embedded_letters() {
        let door = door("1234", 1).await;
        let (status, _, _) =
            request(door.local_addr(), &post("/pin", "text/plain", "a1b2c3d4")).await;
        assert_eq!(status, 403);
    }

    #[tokio::test]
    async fn dropping_door_closes_partial_request() {
        let door = door("1234", 1).await;
        let mut stream = TcpStream::connect(door.local_addr()).await.unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(door);
        let mut response = Vec::new();
        let ended =
            tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut response)).await;
        assert!(
            ended.is_ok(),
            "a request survived the publication's shutdown"
        );
    }
}
