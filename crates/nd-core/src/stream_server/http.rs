//! The HTTP side of the stream server: reading a request head within bounds,
//! deciding what it may have, and the response headers for that.

use std::net::IpAddr;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::MediaType;
use crate::{NdError, Result};

/// Cap on a request's header size.
const MAX_REQUEST_BYTES: usize = 8 * 1024;
/// How long the client gets to finish sending the request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The outcome of triaging a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Serve the stream.
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
pub(super) fn triage(request: &str, expected_path: &str, allowed: IpAddr, from: IpAddr) -> Verdict {
    let Some((method, path)) = request_line(request) else {
        return Verdict::BadRequest;
    };

    // The query string is not part of the secret.
    let path = path.split('?').next().unwrap_or(path);

    // Do not reveal whether an incorrect path names a protected resource.
    // The live listener independently rejects a foreign IP before reading.
    if !crate::secret::same_secret(path, expected_path) {
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
pub(super) fn response_headers(verdict: Verdict, media: MediaType) -> String {
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

/// The method and target of a complete origin-form HTTP/1.x request head, or
/// `None` for anything with a body, ambiguous framing or malformed syntax.
///
/// Public so file serving rejects exactly what this server rejects.
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

/// Reads one request head, at most `MAX_REQUEST_BYTES`, within one deadline
/// for the whole request so a client trickling bytes cannot hold the reader.
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
            .map_err(NdError::network)?;
        if read == 0 {
            return Err(NdError::Network("incomplete HTTP headers".into()));
        }
        buffer.extend_from_slice(&chunk[..read]);
        // Checked before the delimiter, so an oversized last chunk is refused.
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

/// Writes to a peer that must keep reading: five seconds, then an error.
///
/// After a timeout the connection is dropped, never retried on a partially
/// written response.
pub async fn write_bounded(stream: &mut TcpStream, bytes: &[u8]) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), stream.write_all(bytes))
        .await
        .map_err(|_| NdError::Network("HTTP peer stopped reading".into()))?
        .map_err(NdError::network)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream_server::*;
    use std::net::Ipv4Addr;

    const CAST: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 0, 50));
    const STRANGER: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 0, 99));

    fn get(path: &str) -> String {
        format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n")
    }

    #[test]
    fn serves_the_right_path() {
        assert_eq!(
            triage(&get("/secret"), "/secret", CAST, CAST),
            Verdict::Stream
        );
    }

    #[test]
    fn wrong_path_is_404() {
        // Do not distinguish protected resources from unknown paths.
        assert_eq!(
            triage(&get("/guess"), "/secret", CAST, CAST),
            Verdict::NotFound
        );
        // Including for those already on the allowlist.
        assert_eq!(triage(&get("/"), "/secret", CAST, CAST), Verdict::NotFound);
    }

    #[test]
    fn foreign_host_is_rejected() {
        // Defence in depth: even with the token leaked, only the session's
        // receiver is served.
        assert_eq!(
            triage(&get("/secret"), "/secret", CAST, STRANGER),
            Verdict::Forbidden
        );
    }

    #[test]
    fn secret_ignores_query_string() {
        assert_eq!(
            triage(&get("/secret?x=1"), "/secret", CAST, CAST),
            Verdict::Stream
        );
    }

    #[test]
    fn head_probe_has_no_body() {
        let request = "HEAD /secret HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(triage(request, "/secret", CAST, CAST), Verdict::HeadOnly);
    }

    #[test]
    fn other_methods_are_refused() {
        let request = "POST /secret HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(
            triage(request, "/secret", CAST, CAST),
            Verdict::MethodNotAllowed
        );
    }

    #[test]
    fn malformed_requests_are_refused() {
        for raw in ["", "\r\n", "JUNK", "GET", "GET  \r\n"] {
            let verdict = triage(raw, "/secret", CAST, CAST);
            assert_ne!(verdict, Verdict::Stream, "accepted {raw:?}");
        }
    }

    #[test]
    fn stream_response_has_no_length() {
        let headers = response_headers(Verdict::Stream, CAST_MEDIA);
        assert!(headers.contains("200 OK"), "{headers}");
        assert!(headers.contains(CAST_MEDIA.content_type), "{headers}");
        assert!(!headers.contains("Content-Length"), "{headers}");
        assert!(!headers.contains("Transfer-Encoding"), "{headers}");
        assert!(headers.contains("Connection: close"), "{headers}");
        assert!(headers.ends_with("\r\n\r\n"), "{headers}");
    }

    #[test]
    fn dlna_renderer_sees_live_stream() {
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
    async fn header_reader_rejects_bad_input() {
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
    async fn partial_header_times_out() {
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
