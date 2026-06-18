//! Canal de controle do protocolo Google Cast (CASTV2) sobre TLS (porta 8009).
//!
//! Estrutura do protocolo:
//! - Cada mensagem é um `CastMessage` (protobuf) prefixado por 4 bytes de
//!   comprimento (big-endian).
//! - O `payload_utf8` carrega JSON, organizado por *namespaces*:
//!   `connection` (CONNECT), `heartbeat` (PING/PONG), `receiver`
//!   (LAUNCH/GET_STATUS) e `media` (LOAD/PLAY — Fase 2 posterior).
//!
//! Para projetar a tela usamos o **Default Media Receiver**: damos LAUNCH nele
//! e, na sequência, um LOAD apontando para o stream HTTP local (próximo passo).

use std::net::IpAddr;
use std::sync::Arc;

use prost::Message as _;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

use nd_core::{NdError, Result};

const PORT: u16 = 8009;
const SOURCE_ID: &str = "sender-0";
const PLATFORM_DEST: &str = "receiver-0";
const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";

/// App ID do Default Media Receiver (reproduz mídia a partir de uma URL HTTP).
pub const DEFAULT_MEDIA_RECEIVER: &str = "CC1AD845";

fn net_err<E: std::fmt::Display>(e: E) -> NdError {
    NdError::Network(e.to_string())
}
fn proto_err<E: std::fmt::Display>(e: E) -> NdError {
    NdError::Protocol(e.to_string())
}

// ---------------------------------------------------------------------------
// protobuf CastMessage (escrito à mão; referência: bkp/src/cc/cast_channel.proto)
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

// ---------------------------------------------------------------------------
// Canal
// ---------------------------------------------------------------------------

/// Conexão de controle com um receptor Cast.
pub struct CastChannel {
    stream: TlsStream<TcpStream>,
    request_id: i32,
}

impl CastChannel {
    /// Conecta (TLS) ao receptor e envia o CONNECT inicial à plataforma.
    pub async fn connect(ip: IpAddr) -> Result<Self> {
        let tcp = TcpStream::connect((ip, PORT)).await.map_err(net_err)?;

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(proto_err)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert(provider)))
            .with_no_client_auth();

        let connector = TlsConnector::from(Arc::new(config));
        let server_name = ServerName::IpAddress(ip.into());
        let stream = connector.connect(server_name, tcp).await.map_err(net_err)?;

        let mut channel = Self {
            stream,
            request_id: 0,
        };
        channel
            .send(NS_CONNECTION, PLATFORM_DEST, r#"{"type":"CONNECT"}"#)
            .await?;
        Ok(channel)
    }

    async fn send(&mut self, namespace: &str, destination: &str, payload: &str) -> Result<()> {
        let msg = CastMessage {
            protocol_version: ProtocolVersion::Castv210 as i32,
            source_id: SOURCE_ID.to_string(),
            destination_id: destination.to_string(),
            namespace: namespace.to_string(),
            payload_type: PayloadType::Str as i32,
            payload_utf8: Some(payload.to_string()),
            payload_binary: None,
        };
        let buf = msg.encode_to_vec();
        self.stream
            .write_all(&(buf.len() as u32).to_be_bytes())
            .await
            .map_err(net_err)?;
        self.stream.write_all(&buf).await.map_err(net_err)?;
        self.stream.flush().await.map_err(net_err)?;
        Ok(())
    }

    async fn read(&mut self) -> Result<(String, Value)> {
        let mut len_buf = [0u8; 4];
        self.stream.read_exact(&mut len_buf).await.map_err(net_err)?;
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        self.stream.read_exact(&mut buf).await.map_err(net_err)?;

        let msg = CastMessage::decode(&buf[..]).map_err(proto_err)?;
        let payload = msg
            .payload_utf8
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(Value::Null);
        Ok((msg.namespace, payload))
    }

    /// Pede o status do receptor (apps em execução, volume, etc.).
    pub async fn request_status(&mut self) -> Result<()> {
        self.request_id += 1;
        let payload =
            serde_json::json!({ "type": "GET_STATUS", "requestId": self.request_id }).to_string();
        self.send(NS_RECEIVER, PLATFORM_DEST, &payload).await
    }

    /// Inicia um aplicativo receptor por `app_id` (ex.: Default Media Receiver).
    pub async fn launch(&mut self, app_id: &str) -> Result<()> {
        self.request_id += 1;
        let payload = serde_json::json!({
            "type": "LAUNCH",
            "requestId": self.request_id,
            "appId": app_id,
        })
        .to_string();
        self.send(NS_RECEIVER, PLATFORM_DEST, &payload).await
    }

    async fn pong(&mut self) -> Result<()> {
        self.send(NS_HEARTBEAT, PLATFORM_DEST, r#"{"type":"PONG"}"#)
            .await
    }

    /// Lê o próximo evento relevante (namespace, payload JSON), respondendo a
    /// PINGs de heartbeat automaticamente.
    pub async fn next_event(&mut self) -> Result<(String, Value)> {
        loop {
            let (namespace, payload) = self.read().await?;
            if namespace == NS_HEARTBEAT {
                if payload.get("type").and_then(Value::as_str) == Some("PING") {
                    self.pong().await?;
                }
                continue;
            }
            return Ok((namespace, payload));
        }
    }
}

// ---------------------------------------------------------------------------
// Verificação TLS
// ---------------------------------------------------------------------------

/// Aceita o certificado do Chromecast (auto-assinado).
///
/// TODO Fase 2: endurecer como manda a auditoria — aceitar apenas
/// `UNKNOWN_CA`/`BAD_IDENTITY`, rejeitando expirado/revogado (o C aceitava tudo).
#[derive(Debug)]
struct AcceptAnyServerCert(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
