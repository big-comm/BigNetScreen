//! Receiver identity on the Cast channel: the TLS certificate check and the
//! DeviceAuth challenge that follows the handshake.
//!
//! [`CastCertVerifier`] accepts a receiver's self-issued chain;
//! [`authenticate_device`] obtains the device key that [`crate::identity`]
//! remembers on first use. Together they recognise a receiver seen before;
//! they do not prove the first one was genuine (see `SECURITY.md`).

use std::sync::Arc;

use prost::Message as _;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

use nd_core::{NdError, Result};

use super::wire::{CastMessage, read_raw_message};
use super::{PLATFORM_DEST, SOURCE_ID, WRITE_TIMEOUT, net_err, proto_err};

pub(super) const NS_DEVICE_AUTH: &str = "urn:x-cast:com.google.cast.tp.deviceauth";
/// How long a receiver gets to answer the identity challenge. A Google
/// receiver answers within milliseconds; one that never does is treated as
/// unauthenticated rather than waited on.
const AUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

// The messages below are `cast_channel.proto`'s DeviceAuth. Its enums stay
// plain integers: only one value of each is ever sent or accepted.

/// `SignatureAlgorithm.RSASSA_PKCS1v15`, the protocol's default.
pub(super) const RSASSA_PKCS1V15: i32 = 1;
/// `HashAlgorithm.SHA256`; the default, SHA-1, is not accepted.
pub(super) const SHA256: i32 = 1;

#[derive(Clone, PartialEq, ::prost::Message)]
pub(super) struct AuthChallenge {
    #[prost(int32, optional, tag = "1")]
    pub(super) signature_algorithm: Option<i32>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub(super) sender_nonce: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "3")]
    pub(super) hash_algorithm: Option<i32>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub(super) struct AuthResponse {
    #[prost(bytes = "vec", required, tag = "1")]
    pub(super) signature: Vec<u8>,
    #[prost(bytes = "vec", required, tag = "2")]
    pub(super) client_auth_certificate: Vec<u8>,
    #[prost(bytes = "vec", repeated, tag = "3")]
    pub(super) intermediate_certificate: Vec<Vec<u8>>,
    #[prost(int32, optional, tag = "4")]
    pub(super) signature_algorithm: Option<i32>,
    #[prost(bytes = "vec", optional, tag = "5")]
    pub(super) sender_nonce: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "6")]
    pub(super) hash_algorithm: Option<i32>,
    #[prost(bytes = "vec", optional, tag = "7")]
    pub(super) crl: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub(super) struct AuthError {
    #[prost(int32, required, tag = "1")]
    pub(super) error_type: i32,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub(super) struct DeviceAuthMessage {
    #[prost(message, optional, tag = "1")]
    pub(super) challenge: Option<AuthChallenge>,
    #[prost(message, optional, tag = "2")]
    pub(super) response: Option<AuthResponse>,
    #[prost(message, optional, tag = "3")]
    pub(super) error: Option<AuthError>,
}

/// Asks the receiver to prove which device it is, before anything else is
/// said on the channel.
///
/// The TLS certificate cannot identify a receiver: it is self-signed and a
/// Chromecast rotates it every two days. What stays is the device
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
pub(super) async fn authenticate_device(
    stream: &mut TlsStream<TcpStream>,
) -> Result<Option<Vec<u8>>> {
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
    let frame = CastMessage::binary(
        SOURCE_ID,
        PLATFORM_DEST,
        NS_DEVICE_AUTH,
        challenge.encode_to_vec(),
    )
    .to_frame()?;
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

/// Certificate verifier for Cast receivers.
///
/// A Chromecast presents a chain issued by a Google CA that is in no system
/// trust store, for a name that is not the IP connected to. The presented
/// chain's root becomes an ad-hoc anchor and webpki's validation runs on top
/// of it: every signature, the validity period and the key usage are still
/// checked; only the anchor's provenance and the host name are waived.
///
/// This checks a certificate, not a receiver. [`authenticate_device`] and
/// [`crate::identity`] hold a receiver to the device key it proved on first
/// use. Authenticating that first device takes what Chromium's sender does
/// (`cast/channel/cast_auth_util.cc`, `cast/certificate/`): chaining the
/// DeviceAuth certificate to the embedded Cast root CA under Cast's own
/// policy, not `KeyUsage::server_auth()`, and checking revocation against
/// Cast's signed CRL (`cast_crl.cc`). Until the root CA check exists the nonce
/// signature proves nothing about a first connection, since any key can sign
/// it; a mistake in that check refuses every genuine receiver, so it needs
/// validation against real devices.
#[derive(Debug)]
pub(super) struct CastCertVerifier(pub(super) Arc<CryptoProvider>);

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
}
