//! The Google Cast (CASTV2) control channel over TLS (port 8009).
//!
//! Every message is a protobuf `CastMessage` behind a 4-byte length
//! (`wire`); `payload_utf8` carries JSON organised by namespace:
//! `connection` (CONNECT/CLOSE), `heartbeat` (PING/PONG), `receiver`
//! (LAUNCH/STOP/GET_STATUS) and `media` (LOAD/PLAY).
//!
//! A background task owns the read half: it answers PINGs whether or not
//! anyone is reading events, correlates replies by `requestId`, and queues
//! the rest as [`CastEvent`]s. The channel works through `&self`, so a request
//! can be in flight while another task waits for events.
//!
//! Teardown is ordered: a STOP addressed to our own session, confirmation
//! that it is gone, CLOSE of the app's virtual connection, then the platform
//! CLOSE, with heartbeat alive until that last one ([`CastChannel::finish_app`]).

mod device_auth;
mod media_receiver;
mod wire;
mod writer;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::pki_types::ServerName;
use serde_json::{Value, json};
use tokio::io::ReadHalf;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use nd_core::{NdError, Result};

use device_auth::{CastCertVerifier, authenticate_device};
use wire::read_message;
use writer::ControlWriter;

/// The control port. Public because it is also the address the interface
/// measures the link against.
pub const PORT: u16 = 8009;
const SOURCE_ID: &str = "sender-0";
const PLATFORM_DEST: &str = "receiver-0";

/// `CONNECT`, with the fields Chromium's own sender puts in it.
///
/// A bare `{"type":"CONNECT"}` opens the virtual connection but does not say
/// who is on the other end, and a receiver that never learned it had a local
/// sender does not always release the session when that sender leaves: the
/// device then refuses new connections until reset from its own settings.
///
/// `connType` 1 is `CONNECTION_TYPE_LOCAL`; `sdkType` 2 and the platform
/// numbering (6 for Linux) are Chromium's, from
/// `components/media_router/common/providers/cast/channel/cast_message_util.cc`.
fn connect_payload() -> String {
    json!({
        "type": "CONNECT",
        "connType": 1,
        "origin": {},
        "userAgent": concat!("BigNetScreen/", env!("CARGO_PKG_VERSION")),
        "senderInfo": {
            "sdkType": 2,
            "version": env!("CARGO_PKG_VERSION"),
            "platform": 6,
            "connectionType": 1,
        },
    })
    .to_string()
}

/// `CLOSE`, with the reason that says this was deliberate.
///
/// `reasonCode` 5 is Chromium's `kVirtualConnectionClosedByPeer`: "gracefully
/// closed by the sender". Without it the receiver cannot tell a sender that
/// left from one whose network died, and it keeps the connection open waiting
/// for the sender that is never coming back.
const CLOSE_PAYLOAD: &str = r#"{"type":"CLOSE","reasonCode":5}"#;
const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
/// The media namespace (LOAD/PLAY/STOP on the receiver app).
pub const NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";

/// The Default Media Receiver's app ID (plays media from an HTTP URL).
pub const DEFAULT_MEDIA_RECEIVER: &str = "CC1AD845";

/// How long to allow for establishing TCP+TLS.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// How long to wait for an ordinary request's reply.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one frame, including waiting for the writer, may take.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
/// How long to allow the receiver to start an application.
///
/// Far more generous than an ordinary request: a freshly switched-on device
/// takes longer than [`REQUEST_TIMEOUT`] to bring the Default Media Receiver up.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(45);
/// Silence that ends the channel: several heartbeat intervals with not even a
/// PING or PONG from the receiver.
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(40);
/// The interval between the PINGs we send.
const PING_INTERVAL: Duration = Duration::from_secs(5);

fn net_err<E: std::fmt::Display>(e: E) -> NdError {
    NdError::Network(e.to_string())
}
fn proto_err<E: std::fmt::Display>(e: E) -> NdError {
    NdError::Protocol(e.to_string())
}

/// A spontaneous message from the receiver (not a reply to anything we asked).
#[derive(Clone, Debug)]
pub struct CastEvent {
    pub source_id: String,
    pub namespace: String,
    pub payload: Value,
}

impl CastEvent {
    pub fn closes(&self, app: &LaunchedApp) -> bool {
        self.namespace == NS_CONNECTION
            && (self.source_id == PLATFORM_DEST || self.source_id == app.transport_id)
            && self.payload.get("type").and_then(Value::as_str) == Some("CLOSE")
    }
}

struct PendingReply {
    namespace: String,
    source_id: String,
    sender: oneshot::Sender<Value>,
}
type Pending = Arc<Mutex<HashMap<i32, PendingReply>>>;

/// A control connection to a Cast receiver.
pub struct CastChannel {
    writer: Arc<ControlWriter>,
    events: tokio::sync::Mutex<mpsc::Receiver<CastEvent>>,
    pending: Pending,
    request_id: AtomicI32,
    reader_task: tokio::task::JoinHandle<()>,
    ping_task: tokio::task::JoinHandle<()>,
}

impl Drop for CastChannel {
    fn drop(&mut self) {
        self.reader_task.abort();
        self.ping_task.abort();
    }
}

struct PendingRequest {
    pending: Pending,
    id: i32,
}
impl Drop for PendingRequest {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
    }
}

impl CastChannel {
    /// Connects (TCP+TLS) to the receiver and sends the initial platform CONNECT.
    pub async fn connect(ip: IpAddr, receiver: &str) -> Result<Self> {
        Self::connect_to(ip, PORT, receiver).await
    }

    /// Connects on the port the receiver announced.
    ///
    /// Nearly every Cast device answers on [`PORT`], but the port is part of
    /// the mDNS record because a receiver may choose another one.
    ///
    /// `receiver` is the identity the person chose (the mDNS instance), under
    /// which the device's key is remembered; see [`crate::identity`].
    pub async fn connect_to(ip: IpAddr, port: u16, receiver: &str) -> Result<Self> {
        let mut stream = open_tls(ip, port).await?;
        let device_key = authenticate_device(&mut stream).await?;
        crate::identity::check_and_remember(receiver, device_key.as_deref())?;

        let (read_half, write_half) = tokio::io::split(stream);
        let writer = Arc::new(ControlWriter::new(write_half));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, event_rx) = mpsc::channel(128);

        let reader_task = tokio::spawn(reader_loop(
            read_half,
            writer.clone(),
            pending.clone(),
            event_tx,
        ));

        let ping_task = tokio::spawn(heartbeat(writer.clone()));

        let channel = Self {
            writer,
            events: tokio::sync::Mutex::new(event_rx),
            pending,
            request_id: AtomicI32::new(1),
            reader_task,
            ping_task,
        };

        channel
            .send(NS_CONNECTION, PLATFORM_DEST, &connect_payload())
            .await?;
        tracing::debug!(%ip, "Cast channel established");
        Ok(channel)
    }

    async fn send(&self, namespace: &str, destination: &str, payload: &str) -> Result<()> {
        self.writer.send(namespace, destination, payload).await
    }

    /// Sends JSON without waiting for a correlated reply.
    ///
    /// Not every protocol over the Cast channel uses `requestId`: the
    /// mirroring negotiation (`urn:x-cast:com.google.cast.webrtc`) matches
    /// messages by `seqNum`, and the reply arrives as a spontaneous event.
    pub async fn send_json(
        &self,
        namespace: &str,
        destination: &str,
        payload: &Value,
    ) -> Result<()> {
        self.send(namespace, destination, &payload.to_string())
            .await
    }

    fn next_request_id(&self) -> i32 {
        self.request_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Sends a request and waits for the reply carrying the same `requestId`
    /// from the same namespace and destination. A refusal is a reply too:
    /// callers check its `type`.
    pub async fn request(
        &self,
        namespace: &str,
        destination: &str,
        payload: Value,
    ) -> Result<Value> {
        self.request_with_timeout(namespace, destination, payload, REQUEST_TIMEOUT)
            .await
    }

    /// Like [`Self::request`], but with an explicit deadline.
    pub async fn request_with_timeout(
        &self,
        namespace: &str,
        destination: &str,
        mut payload: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next_request_id();
        payload["requestId"] = json!(id);

        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                id,
                PendingReply {
                    namespace: namespace.to_string(),
                    source_id: destination.to_string(),
                    sender: tx,
                },
            );
        let _request = PendingRequest {
            pending: self.pending.clone(),
            id,
        };

        let deadline = tokio::time::Instant::now() + timeout;
        tokio::time::timeout_at(deadline, async {
            self.send(namespace, destination, &payload.to_string())
                .await?;
            rx.await
                .map_err(|_| NdError::Protocol("the Cast channel closed before the reply".into()))
        })
        .await
        .map_err(|_| {
            NdError::Protocol(format!(
                "the receiver did not reply within {}s",
                timeout.as_secs()
            ))
        })?
    }

    /// Asks for the receiver's status (running apps, volume, and so on).
    pub async fn status(&self) -> Result<Value> {
        self.request(NS_RECEIVER, PLATFORM_DEST, json!({"type": "GET_STATUS"}))
            .await
    }

    /// Starts a receiver application by `app_id` and returns the session
    /// created.
    ///
    /// A `LAUNCH_ERROR`, or an app missing from the reply, is
    /// [`NdError::Unsupported`]: the receiver does not have that app.
    pub async fn launch(&self, app_id: &str) -> Result<LaunchedApp> {
        let response = self
            .request_with_timeout(
                NS_RECEIVER,
                PLATFORM_DEST,
                json!({"type": "LAUNCH", "appId": app_id}),
                LAUNCH_TIMEOUT,
            )
            .await?;

        let app = LaunchedApp::from_launch_reply(&response, app_id)?;
        // If the app launched but its CONNECT fails, still attempt STOP for
        // exactly that session before dropping the platform connection.
        if let Err(err) = self
            .send(NS_CONNECTION, &app.transport_id, &connect_payload())
            .await
        {
            let _ = self.stop_app(&app).await;
            self.close().await;
            return Err(err);
        }
        tracing::info!(%app_id, transport_id = %app.transport_id, "receiver app started");
        Ok(app)
    }

    /// STOPs only the session we launched, then CLOSEs its virtual connection.
    /// Succeeds only when a receiver status shows that session gone.
    pub async fn stop_app(&self, app: &LaunchedApp) -> Result<()> {
        // Keep PING/PONG alive while STOP is awaiting confirmation. Only the
        // terminal platform CLOSE stops heartbeat traffic under the writer lock.
        let response = self
            .request_with_timeout(
                NS_RECEIVER,
                PLATFORM_DEST,
                json!({"type": "STOP", "sessionId": app.session_id}),
                STOP_TIMEOUT,
            )
            .await;
        let mut stopped = response
            .as_ref()
            .is_ok_and(|reply| session_absent(reply, &app.session_id));
        if !stopped && !self.writer.is_closed() {
            // STOP may race a receiver-initiated shutdown. GET_STATUS can
            // confirm that specific session is already absent without ever
            // stopping another sender's application.
            stopped = self
                .request_with_timeout(
                    NS_RECEIVER,
                    PLATFORM_DEST,
                    json!({"type": "GET_STATUS"}),
                    STOP_TIMEOUT,
                )
                .await
                .is_ok_and(|reply| session_absent(&reply, &app.session_id));
        }
        self.close_to(&app.transport_id).await;
        if stopped {
            Ok(())
        } else {
            Err(NdError::Protocol(
                "receiver did not confirm that the Cast session stopped".into(),
            ))
        }
    }

    /// Sends the platform CLOSE and ends the channel. Idempotent; nothing is
    /// sent after it, and the TLS shutdown is bounded for a receiver that
    /// stopped reading.
    pub async fn close(&self) {
        self.writer.send_platform_close().await;
        self.ping_task.abort();
        self.reader_task.abort();
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.writer.shutdown().await;
    }

    async fn close_to(&self, destination: &str) {
        if let Err(err) = self.send(NS_CONNECTION, destination, CLOSE_PAYLOAD).await {
            tracing::debug!(%destination, %err, "the receiver did not take the CLOSE");
        }
    }

    /// Tears the app down in order (STOP of our session, its confirmation,
    /// app CLOSE, platform CLOSE) and returns the session's outcome. The
    /// original failure wins and an unconfirmed STOP is not a success; the
    /// channel must stay alive until this returns.
    pub async fn finish_app(&self, app: &LaunchedApp, result: Result<()>) -> Result<()> {
        let stopped = self.stop_app(app).await;
        self.close().await;
        if let Err(err) = &stopped {
            tracing::warn!(%err, "Cast teardown was not confirmed");
        }
        match (result, stopped) {
            (Ok(()), stopped) => stopped,
            // HTTP fallback must not replace an app whose STOP failed.
            (Err(NdError::Unsupported(_)), Err(stop_error)) => Err(stop_error),
            (Err(original), _) => Err(original),
        }
    }

    /// The receiver's next spontaneous message (status, media, …).
    ///
    /// PINGs are already answered by the background task and do not show up
    /// here.
    pub async fn next_event(&self) -> Option<CastEvent> {
        self.events.lock().await.recv().await
    }
}

impl LaunchedApp {
    /// Reads our app's session out of the LAUNCH reply. A `LAUNCH_ERROR`, or
    /// the app missing from the status, is [`NdError::Unsupported`]: the
    /// receiver does not have that app.
    fn from_launch_reply(reply: &Value, app_id: &str) -> Result<Self> {
        if reply.get("type").and_then(Value::as_str) == Some("LAUNCH_ERROR") {
            let reason = reply
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("no reason given");
            return Err(NdError::Unsupported(format!(
                "the receiver refused to start app {app_id}: {reason}"
            )));
        }

        let app = reply
            .get("status")
            .and_then(|s| s.get("applications"))
            .and_then(Value::as_array)
            .and_then(|apps| {
                apps.iter()
                    .find(|a| a.get("appId").and_then(Value::as_str) == Some(app_id))
            })
            .ok_or_else(|| {
                NdError::Unsupported(format!(
                    "app {app_id} did not appear in the receiver's status"
                ))
            })?;

        let transport_id = app
            .get("transportId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| NdError::Protocol("reply without a transportId".into()))?
            .to_string();
        let session_id = app
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| NdError::Protocol("reply without a sessionId".into()))?
            .to_string();

        Ok(LaunchedApp {
            transport_id,
            session_id,
        })
    }
}

/// TCP and TLS to the receiver, each under [`CONNECT_TIMEOUT`].
async fn open_tls(ip: IpAddr, port: u16) -> Result<TlsStream<TcpStream>> {
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((ip, port)))
        .await
        // Silence (asleep, casting switched off) reads differently from a
        // refusal by something that is listening.
        .map_err(|_| {
            NdError::Network(format!(
                "{ip}:{port} did not answer — the receiver may be asleep, or its \
                 casting service switched off"
            ))
        })?
        .map_err(net_err)?;
    // No Nagle delay: control messages are small and their latency shows
    // up directly in the time until the picture appears.
    let _ = tcp.set_nodelay(true);

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(proto_err)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(CastCertVerifier(provider)))
        .with_no_client_auth();

    let connector = TlsConnector::from(Arc::new(config));
    let server_name = ServerName::IpAddress(ip.into());
    tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(server_name, tcp))
        .await
        .map_err(|_| NdError::Network("the TLS handshake timed out".into()))?
        .map_err(net_err)
}

/// Our side of the heartbeat; the receiver's PINGs are answered by
/// [`reader_loop`]. Ends when the writer refuses a frame.
async fn heartbeat(writer: Arc<ControlWriter>) {
    let mut ticker = tokio::time::interval(PING_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        if writer
            .send(NS_HEARTBEAT, PLATFORM_DEST, r#"{"type":"PING"}"#)
            .await
            .is_err()
        {
            break;
        }
    }
}

/// A running receiver app.
#[derive(Clone, Debug)]
pub struct LaunchedApp {
    /// Where messages addressed to the app go.
    pub transport_id: String,
    pub session_id: String,
}

fn session_absent(response: &Value, session_id: &str) -> bool {
    if response.get("type").and_then(Value::as_str) != Some("RECEIVER_STATUS") {
        return false;
    }
    let Some(status) = response.get("status").and_then(Value::as_object) else {
        return false;
    };
    match status.get("applications") {
        None | Some(Value::Null) => true,
        Some(Value::Array(apps)) => apps.iter().all(|app| {
            app.get("sessionId")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty() && id != session_id)
        }),
        _ => false,
    }
}

/// The read task: answers PINGs, resolves pending requests and forwards the
/// rest as events.
async fn reader_loop(
    mut reader: ReadHalf<TlsStream<TcpStream>>,
    writer: Arc<ControlWriter>,
    pending: Pending,
    events: mpsc::Sender<CastEvent>,
) {
    loop {
        let event = match tokio::time::timeout(RECEIVE_TIMEOUT, read_message(&mut reader)).await {
            Ok(Ok(event)) => event,
            Ok(Err(err)) => {
                tracing::debug!(%err, "Cast channel closed");
                break;
            }
            Err(_) => {
                tracing::warn!("Cast receiver/control frame timed out");
                break;
            }
        };
        if event.namespace == NS_HEARTBEAT {
            if event.payload.get("type").and_then(Value::as_str) == Some("PING")
                && writer
                    .send(NS_HEARTBEAT, &event.source_id, r#"{"type":"PONG"}"#)
                    .await
                    .is_err()
            {
                break;
            }
            continue;
        }
        let Some(event) = deliver_reply(&pending, event) else {
            continue;
        };
        if let Err(err) = events.try_send(event) {
            // Do not silently discard a CLOSE or ANSWER when the consumer is
            // behind. Terminating is bounded and releases all pending requests.
            tracing::warn!(%err, "Cast event queue unavailable; closing channel");
            break;
        }
    }
    writer.mark_closed();
    pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

/// Hands a reply to the request waiting for it, or gives the event back.
///
/// `requestId`, namespace and source must all match: another app or
/// namespace must not satisfy a STOP or LAUNCH waiter by reusing its id.
fn deliver_reply(pending: &Pending, event: CastEvent) -> Option<CastEvent> {
    let Some(id) = event
        .payload
        .get("requestId")
        .and_then(Value::as_i64)
        .and_then(|id| i32::try_from(id).ok())
    else {
        return Some(event);
    };
    let waiting = {
        let mut requests = pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let matches = requests.get(&id).is_some_and(|reply| {
            reply.namespace == event.namespace && reply.source_id == event.source_id
        });
        if matches { requests.remove(&id) } else { None }
    };
    match waiting {
        Some(reply) => {
            let _ = reply.sender.send(event.payload);
            None
        }
        None => Some(event),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handshake_says_who_we_are_and_the_goodbye_says_it_was_deliberate() {
        // Against Chromium's own sender. A receiver that never learned it had
        // a local sender kept the session after that sender left, and the
        // device had to be reset from its own casting settings before it would
        // take a new connection.
        let connect: Value = serde_json::from_str(&connect_payload()).unwrap();
        assert_eq!(connect["type"], "CONNECT");
        assert_eq!(connect["connType"], 1, "CONNECTION_TYPE_LOCAL");
        assert!(connect["origin"].is_object());
        assert_eq!(connect["senderInfo"]["sdkType"], 2);
        assert_eq!(connect["senderInfo"]["connectionType"], 1);
        assert_eq!(connect["senderInfo"]["platform"], 6, "Linux");
        assert!(
            connect["userAgent"]
                .as_str()
                .unwrap()
                .contains("BigNetScreen")
        );

        let close: Value = serde_json::from_str(CLOSE_PAYLOAD).unwrap();
        assert_eq!(close["type"], "CLOSE");
        assert_eq!(close["reasonCode"], 5, "closed by peer, on purpose");
    }

    #[test]
    fn stop_confirmation_is_scoped_and_structurally_valid() {
        let stopped = json!({"type":"RECEIVER_STATUS", "status":{"applications":[]}});
        assert!(session_absent(&stopped, "ours"));
        let other =
            json!({"type":"RECEIVER_STATUS", "status":{"applications":[{"sessionId":"other"}]}});
        assert!(session_absent(&other, "ours"));
        assert!(!session_absent(&other, "other"));
        for malformed in [
            json!({"type":"INVALID_REQUEST"}),
            json!({"type":"RECEIVER_STATUS", "status":{"applications":[{}]}}),
        ] {
            assert!(!session_absent(&malformed, "ours"));
        }
    }
}

#[cfg(test)]
mod lifecycle_tests;
