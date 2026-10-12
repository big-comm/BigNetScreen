//! RTSP message framing over the sink's TCP connection.
//!
//! Both peers send requests and responses on the same socket, so a message is
//! read whole, bounded, and classified afterwards. Nothing here knows WFD.

use std::collections::HashMap;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::tcp::OwnedWriteHalf;

use nd_core::{NdError, Result};

/// Cap on an RTSP message's body.
///
/// `Content-Length` comes off the network; without a cap, one header could
/// make the reader allocate whatever it names.
const MAX_BODY_BYTES: usize = 64 * 1024;
/// Cap on the start line and headers together.
const MAX_HEAD_BYTES: usize = 16 * 1024;
const MAX_LINE_BYTES: usize = 4096;
const MAX_HEADERS: usize = 64;
/// A sink that stops reading must not hold the session open forever.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

fn proto_err<E: std::fmt::Display>(e: E) -> NdError {
    NdError::Protocol(e.to_string())
}

/// An RTSP message (request or response).
#[derive(Debug)]
pub(super) struct RtspMessage {
    pub(super) start_line: String,
    /// Keyed by the lower-cased header name.
    pub(super) headers: HashMap<String, String>,
    pub(super) body: String,
}

impl RtspMessage {
    pub(super) fn is_response(&self) -> bool {
        self.start_line.starts_with("RTSP/")
    }

    pub(super) fn method(&self) -> Option<&str> {
        self.start_line.split_whitespace().next()
    }

    /// A response's status code (`RTSP/1.0 200 OK` → 200).
    pub(super) fn status(&self) -> Option<u16> {
        self.start_line.split_whitespace().nth(1)?.parse().ok()
    }

    pub(super) fn is_success(&self) -> bool {
        matches!(self.status(), Some(code) if (200..300).contains(&code))
    }

    pub(super) fn cseq(&self) -> u32 {
        self.headers
            .get("cseq")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    }

    pub(super) fn cseq_str(&self) -> &str {
        self.headers.get("cseq").map(String::as_str).unwrap_or("0")
    }
}

async fn bounded_line<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    budget: &mut usize,
) -> Result<String> {
    let mut line = Vec::new();
    loop {
        let bytes = reader.fill_buf().await.map_err(proto_err)?;
        if bytes.is_empty() {
            return Err(NdError::Protocol(
                "RTSP connection closed during headers".into(),
            ));
        }
        let count = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |n| n + 1);
        if count > *budget || line.len() + count > MAX_LINE_BYTES {
            return Err(NdError::Protocol("RTSP headers too large".into()));
        }
        line.extend_from_slice(&bytes[..count]);
        *budget -= count;
        reader.consume(count);
        if line.last() == Some(&b'\n') {
            return String::from_utf8(line).map_err(proto_err);
        }
    }
}

pub(super) async fn read_message<R>(reader: &mut R) -> Result<RtspMessage>
where
    R: AsyncBufReadExt + Unpin,
{
    let mut budget = MAX_HEAD_BYTES;
    let start_line = bounded_line(reader, &mut budget)
        .await?
        .trim_end()
        .to_string();
    if start_line.is_empty() {
        return Err(NdError::Protocol("empty RTSP start line".into()));
    }
    let mut headers = HashMap::new();
    loop {
        let line = bounded_line(reader, &mut budget).await?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if headers.len() >= MAX_HEADERS {
            return Err(NdError::Protocol("too many RTSP headers".into()));
        }
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| NdError::Protocol("malformed RTSP header".into()))?;
        if headers
            .insert(key.trim().to_ascii_lowercase(), value.trim().to_string())
            .is_some()
        {
            return Err(NdError::Protocol("duplicate RTSP header".into()));
        }
    }
    let mut body = String::new();
    if let Some(value) = headers.get("content-length") {
        let len = value.parse::<usize>().map_err(proto_err)?;
        if len > MAX_BODY_BYTES {
            return Err(NdError::Protocol("RTSP body too large".into()));
        }
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).await.map_err(proto_err)?;
        body = String::from_utf8(buf).map_err(proto_err)?;
    }

    let msg = RtspMessage {
        start_line,
        headers,
        body,
    };
    tracing::trace!(
        "\n<<< RECEIVED <<<\n{}\n{}\n{}",
        msg.start_line,
        msg.headers
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n"),
        msg.body.trim_end(),
    );
    Ok(msg)
}

pub(super) async fn send(writer: &mut OwnedWriteHalf, msg: &str) -> Result<()> {
    // The raw RTSP dialogue is the only way to debug interoperability with a
    // real sink: enable it with `RUST_LOG=nd_wfd::rtsp=trace`.
    tracing::trace!("\n>>> SENT >>>\n{}", msg.trim_end());
    write_message(writer, msg.as_bytes(), WRITE_TIMEOUT).await
}

/// Writes one whole message within `deadline`.
///
/// On failure the caller ends the connection: a partial message is never
/// retried.
async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &[u8],
    deadline: Duration,
) -> Result<()> {
    tokio::time::timeout(deadline, async {
        writer.write_all(message).await.map_err(proto_err)?;
        writer.flush().await.map_err(proto_err)
    })
    .await
    .map_err(|_| NdError::Protocol("RTSP receiver stopped reading control messages".into()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn blocked_control_write_has_a_deadline() {
        let (mut writer, _receiver) = tokio::io::duplex(1);
        let result = super::write_message(
            &mut writer,
            b"RTSP/1.0 200 OK\r\n\r\n",
            Duration::from_millis(20),
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn control_write_preserves_complete_message() {
        let (mut writer, mut receiver) = tokio::io::duplex(128);
        let message = b"RTSP/1.0 200 OK\r\n\r\n";
        super::write_message(&mut writer, message, Duration::from_secs(1))
            .await
            .unwrap();
        let mut actual = vec![0; message.len()];
        receiver.read_exact(&mut actual).await.unwrap();
        assert_eq!(actual, message);
    }

    async fn parse(raw: &str) -> Result<RtspMessage> {
        let mut reader = BufReader::new(raw.as_bytes());
        read_message(&mut reader).await
    }

    #[tokio::test]
    async fn read_survives_an_unrelated_timer() {
        let (mut writer, reader) = tokio::io::duplex(4096);
        let mut reader = tokio::io::BufReader::new(reader);
        writer
            .write_all(b"RTSP/1.0 200 OK\r\nCSeq: 42\r\n")
            .await
            .unwrap();
        let read = read_message(&mut reader);
        tokio::pin!(read);
        tokio::select! {
            result = &mut read => panic!("premature read: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(10)) => {},
        }
        writer.write_all(b"\r\n").await.unwrap();
        let message = read.await.unwrap();
        assert_eq!(message.cseq(), 42);
        assert_eq!(message.status(), Some(200));
    }

    #[tokio::test]
    async fn oversized_or_truncated_messages_rejected() {
        let large = format!(
            "OPTIONS * RTSP/1.0\r\nX: {}\r\n\r\n",
            "x".repeat(1024 * 1024)
        );
        assert!(parse(&large).await.is_err());
        assert!(parse("OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n").await.is_err());
        assert!(
            parse("OPTIONS * RTSP/1.0\r\nContent-Length: invalid\r\n\r\n")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn reads_a_request_with_body() {
        let msg = parse(
            "SET_PARAMETER rtsp://x/wfd1.0 RTSP/1.0\r\nCSeq: 4\r\nContent-Length: 11\r\n\r\nhello world",
        )
        .await
        .unwrap();
        assert!(!msg.is_response());
        assert_eq!(msg.method(), Some("SET_PARAMETER"));
        assert_eq!(msg.cseq(), 4);
        assert_eq!(msg.body, "hello world");
    }

    #[tokio::test]
    async fn recognises_error_responses() {
        let msg = parse("RTSP/1.0 400 Bad Request\r\nCSeq: 3\r\n\r\n")
            .await
            .unwrap();
        assert!(msg.is_response());
        assert_eq!(msg.status(), Some(400));
        assert!(!msg.is_success());

        let ok = parse("RTSP/1.0 200 OK\r\nCSeq: 3\r\n\r\n").await.unwrap();
        assert!(ok.is_success());
    }

    #[tokio::test]
    async fn oversized_body_is_rejected() {
        let raw = "PLAY x RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 999999999\r\n\r\n";
        let err = parse(raw).await.unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }

    #[tokio::test]
    async fn headers_are_case_insensitive() {
        let msg = parse("PLAY x RTSP/1.0\r\ncSeQ: 7\r\nTRANSPORT: a;client_port=5000-5001\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(msg.cseq(), 7);
        assert_eq!(
            msg.headers.get("transport").map(String::as_str),
            Some("a;client_port=5000-5001")
        );
    }

    #[tokio::test]
    async fn closed_connection_is_an_error() {
        assert!(parse("").await.is_err());
    }
}
