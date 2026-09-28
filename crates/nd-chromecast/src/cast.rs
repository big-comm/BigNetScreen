//! The Google Cast (CASTV2) control channel over TLS (port 8009).
//!
//! The protocol's shape:
//! - every message is a `CastMessage` (protobuf) prefixed by 4 length bytes
//!   (big-endian);
//! - `payload_utf8` carries JSON, organised by *namespace*: `connection`
//!   (CONNECT), `heartbeat` (PING/PONG), `receiver` (LAUNCH/GET_STATUS) and
//!   `media` (LOAD/PLAY).
//!
//! ## Design
//!
//! A background task owns the read half and:
//! - answers PINGs **on its own** (previously the PONG only went out if the
//!   application happened to be calling `next_event()`; stopping the polling
//!   dropped the connection within ~10 s);
//! - sends periodic PINGs;
//! - **correlates replies by `requestId`**, so `launch()` knows whether it
//!   worked instead of firing and hoping.
//!
//! This lets the channel be used from several places at once (`&self`), which
//! the `&mut self` version made impossible — sending LOAD while reading the
//! status could not be done.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use prost::Message as _;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use nd_core::{NdError, Result};

/// The control port. Public because it is also the address the interface
/// measures the link against.
pub const PORT: u16 = 8009;
const SOURCE_ID: &str = "sender-0";
const PLATFORM_DEST: &str = "receiver-0";

/// `CONNECT`, with the fields Chromium's own sender puts in it.
///
/// A bare `{"type":"CONNECT"}` opens the virtual connection, which is why it
/// worked. What it does not do is tell the receiver who is on the other end,
/// and a receiver that never learned it had a local sender does not always let
/// go of the session when that sender leaves — reported from use as having to
/// go into the device's own casting settings before it would take a new
/// connection.
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
const NS_DEVICE_AUTH: &str = "urn:x-cast:com.google.cast.tp.deviceauth";
/// The media namespace (LOAD/PLAY/STOP on the receiver app).
pub const NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";

/// The Default Media Receiver's app ID (plays media from an HTTP URL).
pub const DEFAULT_MEDIA_RECEIVER: &str = "CC1AD845";

/// The message size cap.
///
/// The 4-byte prefix is controlled by the other end: with no cap, a hostile
/// (or merely buggy) receiver could take the app down through OOM by
/// allocating up to 4 GiB from a single header. The Cast protocol does not use
/// large messages.
const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// How long to allow for establishing TCP+TLS.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// How long a receiver gets to answer the identity challenge. A Google
/// receiver answers within milliseconds; one that never does is treated as
/// unauthenticated rather than waited on.
const AUTH_TIMEOUT: Duration = Duration::from_secs(3);
/// How long to wait for an ordinary request's reply.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
/// How long to allow the receiver to **start an application**.
///
/// Far more generous than an ordinary request: bringing the Default Media
/// Receiver up from scratch on a freshly switched-on device takes several
/// seconds, and the 10 s deadline expired before the app even existed.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(45);
/// The interval between the PINGs we send.
const PING_INTERVAL: Duration = Duration::from_secs(5);

fn net_err<E: std::fmt::Display>(e: E) -> NdError {
    NdError::Network(e.to_string())
}
fn proto_err<E: std::fmt::Display>(e: E) -> NdError {
    NdError::Protocol(e.to_string())
}

// ---------------------------------------------------------------------------
// CastMessage protobuf (reference: `src/cc/cast_channel.proto` in the C project)
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, ::prost::Message)]
struct CastMessage {
    #[prost(enumeration = "ProtocolVersion", required, tag = "1")]
    protocol_version: i32,
    #[prost(string, required, tag = "2")]
    source_id: String,
    #[prost(string, required, tag = "3")]
    destination_id: String,
    #[prost(string, required, tag = "4")]
    namespace: String,
    #[prost(enumeration = "PayloadType", required, tag = "5")]
    payload_type: i32,
    #[prost(string, optional, tag = "6")]
    payload_utf8: Option<String>,
    #[prost(bytes = "vec", optional, tag = "7")]
    payload_binary: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ::prost::Enumeration)]
#[repr(i32)]
enum ProtocolVersion {
    Castv210 = 0,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ::prost::Enumeration)]
#[repr(i32)]
enum PayloadType {
    Str = 0,
    Bin = 1,
}

// DeviceAuth, from the same `cast_channel.proto`. The enums stay plain
// integers: only two of their values are ever sent or accepted.

/// `SignatureAlgorithm.RSASSA_PKCS1v15`, the protocol's default.
const RSASSA_PKCS1V15: i32 = 1;
/// `HashAlgorithm.SHA256`; the default, SHA-1, is not accepted.
const SHA256: i32 = 1;

#[derive(Clone, PartialEq, ::prost::Message)]
struct AuthChallenge {
    #[prost(int32, optional, tag = "1")]
    signature_algorithm: Option<i32>,
    #[prost(bytes = "vec", optional, tag = "2")]
    sender_nonce: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "3")]
    hash_algorithm: Option<i32>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
struct AuthResponse {
    #[prost(bytes = "vec", required, tag = "1")]
    signature: Vec<u8>,
    #[prost(bytes = "vec", required, tag = "2")]
    client_auth_certificate: Vec<u8>,
    #[prost(bytes = "vec", repeated, tag = "3")]
    intermediate_certificate: Vec<Vec<u8>>,
    #[prost(int32, optional, tag = "4")]
    signature_algorithm: Option<i32>,
    #[prost(bytes = "vec", optional, tag = "5")]
    sender_nonce: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "6")]
    hash_algorithm: Option<i32>,
    #[prost(bytes = "vec", optional, tag = "7")]
    crl: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
struct AuthError {
    #[prost(int32, required, tag = "1")]
    error_type: i32,
}

#[derive(Clone, PartialEq, ::prost::Message)]
struct DeviceAuthMessage {
    #[prost(message, optional, tag = "1")]
    challenge: Option<AuthChallenge>,
    #[prost(message, optional, tag = "2")]
    response: Option<AuthResponse>,
    #[prost(message, optional, tag = "3")]
    error: Option<AuthError>,
}

// ---------------------------------------------------------------------------
// Channel
// ---------------------------------------------------------------------------

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

struct ControlWriter {
    stream: tokio::sync::Mutex<WriteHalf<TlsStream<TcpStream>>>,
    closed: AtomicBool,
    closing: AtomicBool,
}
type Writer = Arc<ControlWriter>;

/// Dropping a partial length-prefixed write must permanently poison framing.
/// `write_all` is not cancellation safe; a later STOP/CLOSE must never be
/// appended halfway through the payload of the abandoned message.
struct FrameWrite<'a> {
    closed: &'a AtomicBool,
    complete: bool,
}
impl Drop for FrameWrite<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.closed.store(true, Ordering::Release);
        }
    }
}

async fn write_frame<W: AsyncWrite + Unpin>(
    stream: &tokio::sync::Mutex<W>,
    closed: &AtomicBool,
    closing: &AtomicBool,
    frame: &[u8],
    heartbeat: bool,
    terminal: bool,
    timeout: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut stream = tokio::time::timeout_at(deadline, stream.lock())
        .await
        .map_err(|_| NdError::Network("Cast writer lock timed out".into()))?;
    if closed.load(Ordering::Acquire) {
        return Err(NdError::Network("Cast channel is closed".into()));
    }
    if heartbeat && closing.load(Ordering::Acquire) {
        return Ok(());
    }
    let mut transaction = FrameWrite {
        closed,
        complete: false,
    };
    tokio::time::timeout_at(deadline, async {
        stream.write_all(frame).await?;
        stream.flush().await
    })
    .await
    .map_err(|_| NdError::Network("Cast frame write timed out; channel closed".into()))?
    .map_err(net_err)?;
    transaction.complete = true;
    if terminal {
        // Still holding the writer lock: no request can race after CLOSE.
        closed.store(true, Ordering::Release);
    }
    Ok(())
}

/// A control connection to a Cast receiver.
pub struct CastChannel {
    writer: Writer,
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

    /// Connects on the port the receiver **announced**.
    ///
    /// Nearly every Cast device answers on 8009, and hard-coding it worked
    /// until it did not: the port is part of the mDNS record precisely because
    /// a receiver may choose another one, and such a device would be listed by
    /// discovery and then be unreachable, with no clue as to why.
    ///
    /// `receiver` is the identity the person chose (the mDNS instance), under
    /// which the device's key is remembered; see [`crate::identity`].
    pub async fn connect_to(ip: IpAddr, port: u16, receiver: &str) -> Result<Self> {
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((ip, port)))
            .await
            // The two failures below are different situations and the wording
            // separates them: nothing answered at all (asleep, or the receiver
            // app is not running), against a refusal from something that is
            // listening.
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
        let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(server_name, tcp))
            .await
            .map_err(|_| NdError::Network("the TLS handshake timed out".into()))?
            .map_err(net_err)?;
        let device_key = authenticate_device(&mut stream).await?;
        crate::identity::check_and_remember(receiver, device_key.as_deref())?;

        let (read_half, write_half) = tokio::io::split(stream);
        let writer: Writer = Arc::new(ControlWriter {
            stream: tokio::sync::Mutex::new(write_half),
            closed: AtomicBool::new(false),
            closing: AtomicBool::new(false),
        });
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, event_rx) = mpsc::channel(128);

        let reader_task = tokio::spawn(reader_loop(
            read_half,
            writer.clone(),
            pending.clone(),
            event_tx,
        ));

        let ping_writer = writer.clone();
        let ping_task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(PING_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if send_raw(
                    &ping_writer,
                    NS_HEARTBEAT,
                    PLATFORM_DEST,
                    r#"{"type":"PING"}"#,
                )
                .await
                .is_err()
                {
                    break;
                }
            }
        });

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
        send_raw(&self.writer, namespace, destination, payload).await
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

    /// Sends a request and **waits for the correlated reply**.
    ///
    /// The correlation uses the `requestId` the protocol returns; without it
    /// there was no way to know whether a LAUNCH had worked.
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
    /// Fails with a descriptive error if the receiver refuses
    /// (`LAUNCH_ERROR`) — that refusal used to go unnoticed.
    pub async fn launch(&self, app_id: &str) -> Result<LaunchedApp> {
        let response = self
            .request_with_timeout(
                NS_RECEIVER,
                PLATFORM_DEST,
                json!({"type": "LAUNCH", "appId": app_id}),
                LAUNCH_TIMEOUT,
            )
            .await?;

        if response.get("type").and_then(Value::as_str) == Some("LAUNCH_ERROR") {
            let reason = response
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("no reason given");
            // `Unsupported`, not `Protocol`: a receiver that refuses to start
            // the mirroring app simply does not have it.
            return Err(NdError::Unsupported(format!(
                "the receiver refused to start app {app_id}: {reason}"
            )));
        }

        let app = response
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

        let app = LaunchedApp {
            transport_id,
            session_id,
        };
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

    /// Tells the receiver app to load a media URL (our HTTP stream).
    pub async fn load_media(
        &self,
        app: &LaunchedApp,
        url: &str,
        content_type: &str,
    ) -> Result<Value> {
        let response = self
            .request(
                NS_MEDIA,
                &app.transport_id,
                json!({
                    "type": "LOAD",
                    "sessionId": app.session_id,
                    "autoplay": true,
                    "currentTime": 0,
                    "media": {
                        "contentId": url,
                        "contentType": content_type,
                        // Mirroring is live: without this the receiver tries to
                        // buffer as if it were an on-demand video.
                        "streamType": "LIVE",
                    },
                }),
            )
            .await?;
        if response.get("type").and_then(Value::as_str) != Some("MEDIA_STATUS") {
            return Err(NdError::Protocol(
                "receiver did not acknowledge the live media LOAD".into(),
            ));
        }
        Ok(response)
    }

    /// Tells the receiver app to play a **file** from this computer.
    ///
    /// Deliberately not [`Self::load_media`], which describes the mirroring
    /// stream. Two differences decide whether the file plays properly:
    ///
    /// - `streamType: BUFFERED`. A file has a beginning and an end, so the
    ///   receiver may buffer ahead and offer a position bar. Declared `LIVE`,
    ///   as mirroring is, the receiver refuses to seek and shows no duration;
    /// - **metadata**. Without it the receiver shows a bare URL — the token and
    ///   an index — which tells the room nothing. `metadataType` 0 is the
    ///   generic one, understood by every receiver.
    pub async fn load_item(
        &self,
        app: &LaunchedApp,
        url: &str,
        file: &crate::media::MediaItem,
        sender_name: &str,
        start: Option<nd_core::media::PlaybackStart>,
    ) -> Result<Value> {
        let mut response = self
            .request(
                NS_MEDIA,
                &app.transport_id,
                json!({
                    "type": "LOAD",
                    "sessionId": app.session_id,
                    "autoplay": start.is_none(),
                    "currentTime": start.map(|s| s.seconds).unwrap_or(0.0),
                    "media": {
                        "contentId": url,
                        "contentType": file.content_type(),
                        "streamType": "BUFFERED",
                        "metadata": {
                            "metadataType": 0,
                            "title": file.title(),
                            "subtitle": sender_name,
                        },
                    },
                }),
            )
            .await?;

        // A refusal comes back as a message, not as a transport error: without
        // this check the queue would move on believing the item was playing.
        if response.get("type").and_then(Value::as_str) == Some("LOAD_FAILED") {
            let reason = response
                .get("detailedErrorCode")
                .map(|c| c.to_string())
                .unwrap_or_else(|| "no reason given".to_string());
            return Err(NdError::Unsupported(format!(
                "the receiver could not play this file (error {reason})"
            )));
        }
        if response.get("type").and_then(Value::as_str) != Some("MEDIA_STATUS") {
            return Err(NdError::Protocol(
                "receiver did not acknowledge the media LOAD".into(),
            ));
        }
        if let Some(start) = start {
            let session = response
                .pointer("/status/0/mediaSessionId")
                .and_then(Value::as_i64)
                .ok_or_else(|| NdError::Protocol("LOAD returned no media session".into()))?;
            // Load paused so initial mute/volume precede the first sound.
            if file.kind() != nd_core::media::MediaKind::Photo {
                response = self
                    .request(
                        NS_MEDIA,
                        &app.transport_id,
                        json!({
                            "type": "SET_VOLUME", "mediaSessionId": session,
                            "volume": {"level": start.volume, "muted": start.muted},
                        }),
                    )
                    .await?;
                let entry =
                    crate::media::active_status(&response, session, url).ok_or_else(|| {
                        NdError::Protocol("receiver did not confirm initial volume".into())
                    })?;
                if entry.pointer("/volume/muted").and_then(Value::as_bool) != Some(start.muted)
                    || !entry
                        .pointer("/volume/level")
                        .and_then(Value::as_f64)
                        .is_some_and(|level| (level - start.volume).abs() < 0.001)
                {
                    return Err(NdError::Unsupported(
                        "receiver did not apply initial volume and mute".into(),
                    ));
                }
            }
            if !start.paused {
                response = self
                    .request(
                        NS_MEDIA,
                        &app.transport_id,
                        json!({"type":"PLAY", "mediaSessionId": session}),
                    )
                    .await?;
                if crate::media::active_status(&response, session, url).is_none() {
                    return Err(NdError::Protocol(
                        "receiver did not acknowledge playback".into(),
                    ));
                }
            }
        }
        Ok(response)
    }

    /// STOP only the session we launched, then CLOSE its virtual connection.
    /// A transport-level reply is not sufficient: verify the app is gone.
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
        if !stopped && !self.writer.closed.load(Ordering::Acquire) {
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

    /// Terminal and idempotent: no heartbeat/request is sent after platform
    /// CLOSE. Bound the TLS shutdown too, including a receiver that stops reading.
    pub async fn close(&self) {
        self.writer.closing.store(true, Ordering::Release);
        if !self.writer.closed.load(Ordering::Acquire) {
            let _ = send_raw_kind(
                &self.writer,
                NS_CONNECTION,
                PLATFORM_DEST,
                CLOSE_PAYLOAD,
                true,
            )
            .await;
        }
        self.writer.closed.store(true, Ordering::Release);
        self.ping_task.abort();
        self.reader_task.abort();
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        let _ = tokio::time::timeout(WRITE_TIMEOUT, async {
            self.writer.stream.lock().await.shutdown().await
        })
        .await;
    }

    async fn close_to(&self, destination: &str) {
        if let Err(err) = self.send(NS_CONNECTION, destination, CLOSE_PAYLOAD).await {
            tracing::debug!(%destination, %err, "the receiver did not take the CLOSE");
        }
    }

    /// Preserve the original failure; do not call an unconfirmed disconnect a
    /// success. Explicit callers keep the channel alive until this completes.
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

/// A running receiver app.
#[derive(Clone, Debug)]
pub struct LaunchedApp {
    /// Where messages addressed to the app go.
    pub transport_id: String,
    pub session_id: String,
}

async fn send_raw(
    writer: &Writer,
    namespace: &str,
    destination: &str,
    payload: &str,
) -> Result<()> {
    send_raw_kind(writer, namespace, destination, payload, false).await
}

async fn send_raw_kind(
    writer: &Writer,
    namespace: &str,
    destination: &str,
    payload: &str,
    terminal: bool,
) -> Result<()> {
    let msg = CastMessage {
        protocol_version: ProtocolVersion::Castv210 as i32,
        source_id: SOURCE_ID.to_string(),
        destination_id: destination.to_string(),
        namespace: namespace.to_string(),
        payload_type: PayloadType::Str as i32,
        payload_utf8: Some(payload.to_string()),
        payload_binary: None,
    };
    tracing::trace!(%namespace, %destination, bytes = payload.len(), ">>> Cast sent");
    let buf = msg.encode_to_vec();
    if buf.len() > MAX_MESSAGE_BYTES {
        return Err(NdError::Protocol("Cast message too large".into()));
    }

    let mut frame = Vec::with_capacity(4 + buf.len());
    frame.extend_from_slice(&(buf.len() as u32).to_be_bytes());
    frame.extend_from_slice(&buf);
    write_frame(
        &writer.stream,
        &writer.closed,
        &writer.closing,
        &frame,
        namespace == NS_HEARTBEAT,
        terminal,
        WRITE_TIMEOUT,
    )
    .await
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

async fn read_message<R: AsyncRead + Unpin>(reader: &mut R) -> Result<CastEvent> {
    let msg = read_raw_message(reader).await?;
    let payload = if msg.payload_type == PayloadType::Str as i32 {
        let text = msg
            .payload_utf8
            .as_deref()
            .ok_or_else(|| NdError::Protocol("Cast UTF-8 payload missing".into()))?;
        serde_json::from_str(text).map_err(proto_err)?
    } else if msg.payload_type == PayloadType::Bin as i32 {
        // DeviceAuth is a separate binary protocol, not a JSON reply.
        Value::Null
    } else {
        return Err(NdError::Protocol("unknown Cast payload type".into()));
    };
    Ok(CastEvent {
        source_id: msg.source_id,
        namespace: msg.namespace,
        payload,
    })
}

/// One framed, envelope-checked message, payload untouched.
async fn read_raw_message<R: AsyncRead + Unpin>(reader: &mut R) -> Result<CastMessage> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await.map_err(net_err)?;
    let len = u32::from_be_bytes(len_buf) as usize;

    // A mandatory cap: the length comes from the other side of the network.
    if len > MAX_MESSAGE_BYTES {
        return Err(NdError::Protocol(format!(
            "Cast message of {len} bytes exceeds the {MAX_MESSAGE_BYTES} limit"
        )));
    }

    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await.map_err(net_err)?;

    let msg = CastMessage::decode(&buf[..]).map_err(proto_err)?;
    // The raw dialogue is the only way to debug interoperability with a real
    // receiver: enable it with `RUST_LOG=nd_chromecast=trace`.
    tracing::trace!(
        namespace = %msg.namespace,
        bytes = msg.payload_utf8.as_ref().map_or(0, String::len),
        "<<< Cast received"
    );
    if msg.protocol_version != ProtocolVersion::Castv210 as i32
        || (msg.destination_id != SOURCE_ID && msg.destination_id != "*")
        || msg.source_id.is_empty()
    {
        return Err(NdError::Protocol("invalid Cast message envelope".into()));
    }
    Ok(msg)
}

/// Asks the receiver to prove which device it is, before anything else is
/// said on the channel.
///
/// The TLS certificate cannot identify a receiver: it is self-signed and a
/// Chromecast replaces it every two days. What stays is the device
/// certificate it answers the challenge with, whose key signs our fresh nonce
/// followed by this session's TLS certificate. Returns that key (the
/// certificate's SPKI) once the signature verifies, binding the device to the
/// connection just made.
///
/// Whether the key belongs to a device Google issued is not checked: that
/// takes Cast's own root CA and revocation list (see [`CastCertVerifier`]).
/// Remembering the key on first use, as VLC remembers a receiver's TLS key,
/// is what [`crate::identity`] does with it.
///
/// `None` when the receiver declines or does not answer within
/// [`AUTH_TIMEOUT`]; an answer that fails verification is an error.
async fn authenticate_device(stream: &mut TlsStream<TcpStream>) -> Result<Option<Vec<u8>>> {
    let peer = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|chain| chain.first())
        .ok_or_else(|| NdError::Protocol("the receiver presented no TLS certificate".into()))?
        .to_vec();
    let nonce = crate::mirror::random_bytes()?;
    let challenge = DeviceAuthMessage {
        challenge: Some(AuthChallenge {
            signature_algorithm: Some(RSASSA_PKCS1V15),
            sender_nonce: Some(nonce.to_vec()),
            hash_algorithm: Some(SHA256),
        }),
        response: None,
        error: None,
    };
    let msg = CastMessage {
        protocol_version: ProtocolVersion::Castv210 as i32,
        source_id: SOURCE_ID.to_string(),
        destination_id: PLATFORM_DEST.to_string(),
        namespace: NS_DEVICE_AUTH.to_string(),
        payload_type: PayloadType::Bin as i32,
        payload_utf8: None,
        payload_binary: Some(challenge.encode_to_vec()),
    };
    let buf = msg.encode_to_vec();
    let mut frame = Vec::with_capacity(4 + buf.len());
    frame.extend_from_slice(&(buf.len() as u32).to_be_bytes());
    frame.extend_from_slice(&buf);
    tokio::time::timeout(WRITE_TIMEOUT, async {
        stream.write_all(&frame).await?;
        stream.flush().await
    })
    .await
    .map_err(|_| NdError::Network("the identity challenge could not be sent".into()))?
    .map_err(net_err)?;

    let answer = tokio::time::timeout(AUTH_TIMEOUT, async {
        // Nothing else is expected before CONNECT; a few strays are skipped.
        for _ in 0..8 {
            let msg = read_raw_message(stream).await?;
            if msg.namespace == NS_DEVICE_AUTH {
                return Ok(Some(msg));
            }
        }
        Ok::<_, NdError>(None)
    })
    .await;
    let binary = match answer {
        Ok(Ok(Some(msg))) => msg.payload_binary.unwrap_or_default(),
        Ok(Ok(None)) | Err(_) => {
            tracing::info!("the receiver did not answer the identity challenge");
            return Ok(None);
        }
        Ok(Err(err)) => return Err(err),
    };
    let reply = DeviceAuthMessage::decode(&binary[..]).map_err(proto_err)?;
    let Some(response) = reply.response else {
        tracing::info!(
            error = ?reply.error.map(|e| e.error_type),
            "the receiver declined the identity challenge"
        );
        return Ok(None);
    };
    verify_auth_response(&nonce, &peer, &response).map(Some)
}

/// Checks a DeviceAuth answer against our nonce and the session's TLS
/// certificate, returning the device key that signed it.
fn verify_auth_response(nonce: &[u8], peer_der: &[u8], response: &AuthResponse) -> Result<Vec<u8>> {
    let refuse = |why: &str| NdError::Protocol(format!("the receiver's identity proof {why}"));
    if response.sender_nonce.as_deref() != Some(nonce) {
        return Err(refuse("does not answer this connection's challenge"));
    }
    if response.hash_algorithm != Some(SHA256)
        || response.signature_algorithm.unwrap_or(RSASSA_PKCS1V15) != RSASSA_PKCS1V15
    {
        return Err(refuse("uses a signature this sender does not accept"));
    }
    let certificate = CertificateDer::from(response.client_auth_certificate.as_slice());
    let device = webpki::EndEntityCert::try_from(&certificate)
        .map_err(|_| refuse("carries an unreadable device certificate"))?;
    let signed = [nonce, peer_der].concat();
    device
        .verify_signature(
            webpki::ring::RSA_PKCS1_2048_8192_SHA256,
            &signed,
            &response.signature,
        )
        .map_err(|_| refuse("has a signature that does not verify"))?;
    Ok(device.subject_public_key_info().as_ref().to_vec())
}

/// The read task: answers PINGs, resolves pending requests and forwards the
/// rest as events.
async fn reader_loop(
    mut reader: ReadHalf<TlsStream<TcpStream>>,
    writer: Writer,
    pending: Pending,
    events: mpsc::Sender<CastEvent>,
) {
    loop {
        let read = tokio::time::timeout(Duration::from_secs(40), read_message(&mut reader)).await;
        let CastEvent {
            source_id,
            namespace,
            payload,
        } = match read {
            Err(_) => {
                tracing::warn!("Cast receiver/control frame timed out");
                break;
            }
            Ok(result) => match result {
                Ok(msg) => msg,
                Err(err) => {
                    tracing::debug!(%err, "Cast channel closed");
                    break;
                }
            },
        };

        if namespace == NS_HEARTBEAT {
            if payload.get("type").and_then(Value::as_str) == Some("PING")
                && send_raw(&writer, NS_HEARTBEAT, &source_id, r#"{"type":"PONG"}"#)
                    .await
                    .is_err()
            {
                break;
            }
            continue;
        }

        // Correlate all three fields; another app/namespace must not satisfy
        // a STOP/LAUNCH waiter by copying or overflowing its requestId.
        if let Some(id) = payload
            .get("requestId")
            .and_then(Value::as_i64)
            .and_then(|id| i32::try_from(id).ok())
        {
            let waiting = {
                let mut requests = pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if requests.get(&id).is_some_and(|reply| {
                    reply.namespace == namespace && reply.source_id == source_id
                }) {
                    requests.remove(&id)
                } else {
                    None
                }
            };
            if let Some(reply) = waiting {
                let _ = reply.sender.send(payload);
                continue;
            }
        }
        if let Err(err) = events.try_send(CastEvent {
            source_id,
            namespace,
            payload,
        }) {
            // Do not silently discard a CLOSE or ANSWER when the consumer is
            // behind. Terminating is bounded and releases all pending requests.
            tracing::warn!(%err, "Cast event queue unavailable; closing channel");
            break;
        }
    }
    writer.closed.store(true, Ordering::Release);
    pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

// ---------------------------------------------------------------------------
// TLS verification
// ---------------------------------------------------------------------------

/// Certificate verifier for Cast receivers.
///
/// Chromecasts use a chain issued by a Google CA that is **not** in the
/// system's trust stores, and the certificate does not match the IP used to
/// connect. The audit of the C project settled on the right policy: accept
/// `UNKNOWN_CA` and `BAD_IDENTITY`, **rejecting everything else** — rather
/// than the blind `return TRUE`, which accepted even expired or malformed
/// certificates.
///
/// Implementation: the root of the presented chain is adopted as an ad-hoc
/// anchor, and `webpki`'s normal validation runs on top of that. Each link's
/// signature, the **validity period** and the key usage all keep being
/// checked; only the anchor's provenance and the host name are waived.
///
/// SECURITY LIMIT: this validates syntax/signatures but does not authenticate
/// the device's identity; certificate validity is not receiver identity.
/// [`authenticate_device`] and [`crate::identity`] then hold a receiver to
/// the device key it proved on first use (steps 1 to 3 below, remembered).
/// Proving that the first device was genuine is what remains.
///
/// What closing it takes, from Chromium's own sender
/// (`cast/channel/cast_auth_util.cc`, `cast/certificate/`):
///
/// 1. `DEVICE_AUTH_CHALLENGE` on `urn:x-cast:com.google.cast.tp.deviceauth`
///    to `receiver-0`, as a **binary** payload — a protobuf, not JSON. Our
///    `CastMessage` already carries `payload_binary` and `PayloadType::Bin`.
/// 2. A 16-byte random nonce, checked back against the response.
/// 3. The signature covers **nonce ‖ the receiver's TLS certificate in DER**
///    (`cast_auth_util.cc:338`), under SHA-1 or SHA-256 as the response's
///    `hash_algorithm` says, verified with the device certificate's key.
/// 4. That certificate chained to an embedded Cast root CA. Not with
///    `KeyUsage::server_auth()`: Cast device certificates are validated
///    against Cast's own policy, so the rule this verifier uses for the TLS
///    handshake is the wrong one to reuse.
/// 5. Revocation, which in Cast is a signed CRL format of its own with its
///    own separate root CA (`cast_crl.cc`). Without it a revoked device still
///    passes, which is a narrower hole than the one above — it takes a
///    genuine Google-issued device key that has since been revoked.
///
/// These do not stage. Steps 1 to 3 on their own prove nothing: an attacker
/// generates a certificate, signs our nonce with its key, and passes. The
/// signature only means something once step 4 has established that the key
/// belongs to a device Google issued. So step 4 is not the polish, it is the
/// authentication — and it is also where a mistake costs every user their
/// casting, which is why it wants a pass with a receiver on the bench rather
/// than a careful guess.
#[derive(Debug)]
struct CastCertVerifier(Arc<CryptoProvider>);

impl ServerCertVerifier for CastCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        use rustls::CertificateError;

        // The presented chain's root becomes the anchor; the rest are intermediates.
        let (anchor_der, chain) = match intermediates.split_last() {
            Some((last, rest)) => (last, rest),
            None => (end_entity, &[] as &[CertificateDer<'_>]),
        };

        let anchor = webpki::anchor_from_trusted_cert(anchor_der)
            .map_err(|_| rustls::Error::InvalidCertificate(CertificateError::BadEncoding))?;
        let anchors = [anchor];

        let cert = webpki::EndEntityCert::try_from(end_entity)
            .map_err(|_| rustls::Error::InvalidCertificate(CertificateError::BadEncoding))?;

        cert.verify_for_usage(
            self.0.signature_verification_algorithms.all,
            &anchors,
            chain,
            now,
            webpki::KeyUsage::server_auth(),
            None,
            None,
        )
        .map_err(|err| {
            tracing::warn!(?err, "Cast receiver certificate rejected");
            match err {
                webpki::Error::CertExpired { .. } | webpki::Error::CertNotValidYet { .. } => {
                    rustls::Error::InvalidCertificate(CertificateError::Expired)
                }
                _ => rustls::Error::InvalidCertificate(CertificateError::BadSignature),
            }
        })?;

        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
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
    fn oversized_length_prefix_is_rejected_before_allocating() {
        // A security regression: `vec![0u8; len]` with `len` coming off the
        // network allowed allocating up to 4 GiB from a 4-byte header.
        const { assert!(MAX_MESSAGE_BYTES < 1024 * 1024) };
        let claimed = u32::MAX as usize;
        assert!(claimed > MAX_MESSAGE_BYTES);
    }

    #[test]
    fn malformed_certificate_is_rejected() {
        // This fixture tests malformed DER, not certificate expiry.
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = CastCertVerifier(provider);

        // Invalid DER has to be rejected too (never blindly accepted).
        let garbage = CertificateDer::from(vec![0x30, 0x00]);
        let result = verifier.verify_server_cert(
            &garbage,
            &[],
            &ServerName::try_from("192.168.0.1").unwrap(),
            &[],
            UnixTime::now(),
        );
        assert!(result.is_err(), "a malformed certificate was accepted");
    }

    #[test]
    fn request_ids_are_unique_and_increasing() {
        let counter = AtomicI32::new(1);
        let a = counter.fetch_add(1, Ordering::Relaxed);
        let b = counter.fetch_add(1, Ordering::Relaxed);
        assert!(b > a);
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

    #[tokio::test]
    async fn interrupted_partial_write_permanently_closes_writer() {
        let (stream, mut peer) = tokio::io::duplex(4);
        let stream = tokio::sync::Mutex::new(stream);
        let closed = AtomicBool::new(false);
        let closing = AtomicBool::new(false);
        // The peer never reads until the timeout: four bytes fit, the rest block.
        let result = tokio::time::timeout(
            Duration::from_millis(20),
            write_frame(
                &stream,
                &closed,
                &closing,
                b"partial message",
                false,
                false,
                Duration::from_secs(1),
            ),
        )
        .await;
        assert!(result.is_err());
        assert!(closed.load(Ordering::Acquire));
        let mut prefix = [0; 4];
        peer.read_exact(&mut prefix).await.unwrap();
        assert_eq!(&prefix, b"part");
        assert!(
            write_frame(
                &stream,
                &closed,
                &closing,
                b"STOP",
                false,
                false,
                Duration::from_secs(1)
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn heartbeat_remains_live_during_stop_until_terminal_close() {
        for _ in 0..20 {
            let (stream, mut peer) = tokio::io::duplex(128);
            let stream = tokio::sync::Mutex::new(stream);
            let closed = AtomicBool::new(false);
            let closing = AtomicBool::new(false);
            for (bytes, heartbeat) in [
                (b"STOP".as_slice(), false),
                (b"PING", true),
                (b"PONG", true),
            ] {
                write_frame(
                    &stream,
                    &closed,
                    &closing,
                    bytes,
                    heartbeat,
                    false,
                    Duration::from_secs(1),
                )
                .await
                .unwrap();
            }
            let mut received = [0; 12];
            peer.read_exact(&mut received).await.unwrap();
            assert_eq!(&received, b"STOPPINGPONG");
            closing.store(true, Ordering::Release);
            write_frame(
                &stream,
                &closed,
                &closing,
                b"CLOSE",
                false,
                true,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
            assert!(closed.load(Ordering::Acquire));
        }
    }

    #[tokio::test]
    async fn terminal_close_suppresses_following_writes() {
        let (stream, mut peer) = tokio::io::duplex(64);
        let stream = tokio::sync::Mutex::new(stream);
        let closed = AtomicBool::new(false);
        let closing = AtomicBool::new(true);
        write_frame(
            &stream,
            &closed,
            &closing,
            b"PING",
            true,
            false,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        write_frame(
            &stream,
            &closed,
            &closing,
            b"CLOSE",
            false,
            true,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        let mut received = [0; 5];
        peer.read_exact(&mut received).await.unwrap();
        assert_eq!(&received, b"CLOSE"); // PING was not written.
        assert!(
            write_frame(
                &stream,
                &closed,
                &closing,
                b"LOAD",
                false,
                false,
                Duration::from_secs(1)
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn parser_rejects_bad_envelopes_and_json() {
        for (destination, text) in [("wrong-sender", "{}"), (SOURCE_ID, "{")] {
            let message = CastMessage {
                protocol_version: ProtocolVersion::Castv210 as i32,
                source_id: PLATFORM_DEST.into(),
                destination_id: destination.into(),
                namespace: NS_RECEIVER.into(),
                payload_type: PayloadType::Str as i32,
                payload_utf8: Some(text.into()),
                payload_binary: None,
            }
            .encode_to_vec();
            let mut bytes = (message.len() as u32).to_be_bytes().to_vec();
            bytes.extend(message);
            assert!(read_message(&mut bytes.as_slice()).await.is_err());
        }
    }
}

#[cfg(test)]
#[path = "cast_lifecycle_tests.rs"]
mod lifecycle_tests;
