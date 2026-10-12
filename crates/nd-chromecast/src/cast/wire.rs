//! The CASTV2 frame: a protobuf `CastMessage` behind a 4-byte big-endian
//! length, as defined in Chromium's `cast_channel.proto`.
//!
//! The channel in [`super`] reads and writes only through these functions, so
//! the size cap applies to every message in both directions.

use prost::Message as _;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};

use nd_core::{NdError, Result};

use super::{CastEvent, SOURCE_ID, net_err, proto_err};

/// The message size cap.
///
/// The length prefix is chosen by the other end: without a cap, one header
/// from a hostile or buggy receiver allocates up to 4 GiB. Cast control
/// messages are small.
pub(super) const MAX_MESSAGE_BYTES: usize = 64 * 1024;

#[derive(Clone, PartialEq, ::prost::Message)]
pub(super) struct CastMessage {
    #[prost(enumeration = "ProtocolVersion", required, tag = "1")]
    pub(super) protocol_version: i32,
    #[prost(string, required, tag = "2")]
    pub(super) source_id: String,
    #[prost(string, required, tag = "3")]
    pub(super) destination_id: String,
    #[prost(string, required, tag = "4")]
    pub(super) namespace: String,
    #[prost(enumeration = "PayloadType", required, tag = "5")]
    pub(super) payload_type: i32,
    #[prost(string, optional, tag = "6")]
    pub(super) payload_utf8: Option<String>,
    #[prost(bytes = "vec", optional, tag = "7")]
    pub(super) payload_binary: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ::prost::Enumeration)]
#[repr(i32)]
pub(super) enum ProtocolVersion {
    Castv210 = 0,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ::prost::Enumeration)]
#[repr(i32)]
pub(super) enum PayloadType {
    Str = 0,
    Bin = 1,
}

impl CastMessage {
    /// A JSON message, the form every namespace but DeviceAuth uses.
    pub(super) fn text(source: &str, destination: &str, namespace: &str, payload: &str) -> Self {
        Self {
            protocol_version: ProtocolVersion::Castv210 as i32,
            source_id: source.to_string(),
            destination_id: destination.to_string(),
            namespace: namespace.to_string(),
            payload_type: PayloadType::Str as i32,
            payload_utf8: Some(payload.to_string()),
            payload_binary: None,
        }
    }

    /// A protobuf payload, as DeviceAuth carries.
    pub(super) fn binary(
        source: &str,
        destination: &str,
        namespace: &str,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            protocol_version: ProtocolVersion::Castv210 as i32,
            source_id: source.to_string(),
            destination_id: destination.to_string(),
            namespace: namespace.to_string(),
            payload_type: PayloadType::Bin as i32,
            payload_utf8: None,
            payload_binary: Some(payload),
        }
    }

    /// The length-prefixed bytes that go on the TLS stream.
    pub(super) fn to_frame(&self) -> Result<Vec<u8>> {
        let body = self.encode_to_vec();
        let length = u32::try_from(body.len())
            .ok()
            .filter(|_| body.len() <= MAX_MESSAGE_BYTES)
            .ok_or_else(|| NdError::Protocol("Cast message too large".into()))?;
        let mut frame = Vec::with_capacity(4 + body.len());
        frame.extend_from_slice(&length.to_be_bytes());
        frame.extend_from_slice(&body);
        Ok(frame)
    }
}

/// Reads one message and parses its JSON payload. A binary payload (only
/// DeviceAuth uses one) becomes `Value::Null`.
pub(super) async fn read_message<R: AsyncRead + Unpin>(reader: &mut R) -> Result<CastEvent> {
    let msg = read_raw_message(reader).await?;
    let payload = if msg.payload_type == PayloadType::Str as i32 {
        let text = msg
            .payload_utf8
            .as_deref()
            .ok_or_else(|| NdError::Protocol("Cast UTF-8 payload missing".into()))?;
        serde_json::from_str(text).map_err(proto_err)?
    } else if msg.payload_type == PayloadType::Bin as i32 {
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
pub(super) async fn read_raw_message<R: AsyncRead + Unpin>(reader: &mut R) -> Result<CastMessage> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await.map_err(net_err)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MESSAGE_BYTES {
        return Err(NdError::Protocol(format!(
            "Cast message of {len} bytes exceeds the {MAX_MESSAGE_BYTES} limit"
        )));
    }

    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await.map_err(net_err)?;

    let msg = CastMessage::decode(&buf[..]).map_err(proto_err)?;
    // The raw dialogue is what interoperability with a real receiver is
    // debugged from: `RUST_LOG=nd_chromecast=trace`.
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

#[cfg(test)]
mod tests {
    use super::super::{NS_RECEIVER, PLATFORM_DEST};
    use super::*;

    #[tokio::test]
    async fn oversized_prefix_rejected_before_allocation() {
        // Only the prefix arrives: the claim alone has to be refused, before
        // a buffer of that size exists.
        let prefix = u32::MAX.to_be_bytes();
        let err = read_message(&mut prefix.as_slice()).await.unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");
    }

    #[tokio::test]
    async fn parser_rejects_bad_envelopes_and_json() {
        for (destination, text) in [("wrong-sender", "{}"), (SOURCE_ID, "{")] {
            let frame = CastMessage::text(PLATFORM_DEST, destination, NS_RECEIVER, text)
                .to_frame()
                .unwrap();
            assert!(read_message(&mut frame.as_slice()).await.is_err());
        }
    }

    #[test]
    fn an_oversized_message_is_never_framed() {
        let huge = "x".repeat(MAX_MESSAGE_BYTES);
        assert!(
            CastMessage::text(SOURCE_ID, PLATFORM_DEST, NS_RECEIVER, &huge)
                .to_frame()
                .is_err()
        );
    }
}
