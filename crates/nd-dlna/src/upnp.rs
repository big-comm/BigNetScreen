//! Talking to a UPnP device: fetching its description and invoking SOAP.
//!
//! ## Why a hand-written HTTP client
//!
//! The whole conversation with a renderer is six requests: one `GET` for the
//! device description, one for the service list, and four SOAP `POST`s of
//! roughly 600 bytes each. Bodies are small and framed by `Content-Length`. A
//! general HTTP client would be the largest dependency in the workspace for
//! that, and the crate already hand-writes the server side of HTTP for the
//! same reason.
//!
//! ## The XML is untrusted
//!
//! Everything parsed here arrives from a device on the LAN, and the friendly
//! name ends up in the interface. Sizes are capped before reading, the name
//! goes through [`nd_core::sink::sanitize_name`], and the extraction is a
//! deliberate tag scan rather than a parser that could be led somewhere.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use nd_core::{NdError, Result};

/// A device answers a description or a SOAP call in well under this.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
/// Descriptions run to a few kilobytes; SOAP replies to a few hundred bytes.
/// `GetProtocolInfo` is the outlier and still under 64 KiB on every set seen.
const MAX_RESPONSE_BYTES: usize = 512 * 1024;

fn net_err<E: std::fmt::Display>(e: E) -> NdError {
    NdError::Network(e.to_string())
}

/// An absolute `http://host:port/path` split into the parts a request needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// Verbatim `host:port`, for the `Host` header.
    pub authority: String,
    pub addr: SocketAddr,
    pub path: String,
}

impl Endpoint {
    /// Parses an absolute URL. `https` is refused rather than silently
    /// downgraded: no renderer uses it, and quietly speaking plaintext to
    /// something that asked for TLS is the wrong way to find that out.
    pub fn parse(url: &str) -> Option<Self> {
        let rest = url.strip_prefix("http://")?;
        let (authority, path) = match rest.find('/') {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, "/"),
        };
        // An SSDP `LOCATION` always carries a literal address, never a name to
        // resolve, so this parses rather than does DNS.
        let addr: SocketAddr = if authority.contains(':') {
            authority.parse().ok()?
        } else {
            format!("{authority}:80").parse().ok()?
        };
        Some(Self {
            authority: authority.to_string(),
            addr,
            path: path.to_string(),
        })
    }

    /// Resolves a `controlURL` from a description against this endpoint.
    ///
    /// The value may be absolute, root-relative, or relative — all three occur
    /// in the wild, and a Panasonic uses the root-relative form.
    pub fn resolve(&self, target: &str) -> Option<Self> {
        if target.starts_with("http://") {
            return Self::parse(target);
        }
        let path = if let Some(absolute) = target.strip_prefix('/') {
            format!("/{absolute}")
        } else {
            let base = self.path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
            format!("{base}/{target}")
        };
        Some(Self {
            authority: self.authority.clone(),
            addr: self.addr,
            path,
        })
    }

    pub fn url(&self) -> String {
        format!("http://{}{}", self.authority, self.path)
    }
}

/// Performs one request and returns the body.
///
/// `extra` holds any header lines beyond the standard block, each ending in
/// CRLF. An empty `body` makes this a `GET`.
async fn request(endpoint: &Endpoint, extra: &str, body: &str) -> Result<String> {
    let (method, framing) = if body.is_empty() {
        ("GET", String::new())
    } else {
        ("POST", format!("Content-Length: {}\r\n", body.len()))
    };
    let head = format!(
        "{method} {path} HTTP/1.1\r\n\
         Host: {authority}\r\n\
         User-Agent: Linux/1.0 UPnP/1.0 BigNetScreen/1.0\r\n\
         Connection: close\r\n\
         {framing}{extra}\r\n",
        path = endpoint.path,
        authority = endpoint.authority,
    );

    let exchange = async {
        let mut stream = TcpStream::connect(endpoint.addr).await.map_err(net_err)?;
        stream.write_all(head.as_bytes()).await.map_err(net_err)?;
        stream.write_all(body.as_bytes()).await.map_err(net_err)?;
        read_response(&mut stream).await
    };

    let raw = tokio::time::timeout(REQUEST_TIMEOUT, exchange)
        .await
        .map_err(|_| NdError::Network(format!("{} did not answer in time", endpoint.url())))??;

    let (head, body) = raw
        .split_once("\r\n\r\n")
        .ok_or_else(|| NdError::Protocol("the device sent no complete HTTP headers".into()))?;

    let status = head.split_whitespace().nth(1).unwrap_or("");
    if status != "200" {
        // A SOAP fault arrives as 500 with the reason in the body; the caller
        // gets the code, and the detail goes to the log where it belongs.
        tracing::debug!(%status, url = %endpoint.url(), body = %truncated(body), "the device refused");
        return Err(NdError::Protocol(format!(
            "the device answered {} to {}",
            if status.is_empty() { "nothing" } else { status },
            endpoint.path
        )));
    }
    Ok(body.to_string())
}

fn truncated(text: &str) -> String {
    text.chars().take(400).collect()
}

/// Reads one response, framed the way the device actually framed it.
///
/// **`Content-Length` first, EOF only as the fallback.** We ask for
/// `Connection: close`, and a Panasonic answers with a length and then holds
/// the connection open anyway. Reading to EOF against that device blocks for
/// the whole request timeout and every SOAP call fails — measured, after the
/// first field test refused to start a session while the same request made
/// with `curl` succeeded, because `curl` honours the length.
async fn read_response<R: AsyncRead + Unpin>(stream: &mut R) -> Result<String> {
    let mut raw = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];

    // Headers first: the body cannot be framed before they are known.
    let head_end = loop {
        if let Some(at) = find_blank_line(&raw) {
            break at;
        }
        let read = stream.read(&mut chunk).await.map_err(net_err)?;
        if read == 0 {
            return Err(NdError::Protocol(
                "the device closed the connection mid-header".into(),
            ));
        }
        raw.extend_from_slice(&chunk[..read]);
        if raw.len() > MAX_RESPONSE_BYTES {
            return Err(NdError::Protocol(
                "the device sent oversized headers".into(),
            ));
        }
    };

    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let want = content_length(&head);

    loop {
        let have = raw.len() - (head_end + 4);
        match want {
            Some(length) if have >= length => break,
            // No length: the device really does mean to frame by EOF.
            None if raw.len() > MAX_RESPONSE_BYTES => break,
            _ => {}
        }
        let read = stream.read(&mut chunk).await.map_err(net_err)?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
        if raw.len() > MAX_RESPONSE_BYTES {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

fn find_blank_line(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|w| w == b"\r\n\r\n")
}

/// The declared body length, if the device declared one.
fn content_length(head: &str) -> Option<usize> {
    head.split("\r\n").find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse().ok())
            .flatten()
    })
}

/// Fetches a device description.
pub async fn describe(location: &Endpoint) -> Result<String> {
    request(location, "", "").await
}

/// Invokes one SOAP action and returns the response body.
///
/// `arguments` is the already-built XML of the action's children — the callers
/// each have a fixed, small set, so building it by hand beats a generic
/// serialiser that would need escaping rules anyway.
pub async fn soap(
    control: &Endpoint,
    service_type: &str,
    action: &str,
    arguments: &str,
) -> Result<String> {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><u:{action} xmlns:u=\"{service_type}\">{arguments}</u:{action}></s:Body>\
         </s:Envelope>"
    );
    let extra = format!(
        "Content-Type: text/xml; charset=\"utf-8\"\r\n\
         SOAPAction: \"{service_type}#{action}\"\r\n"
    );
    request(control, &extra, &body).await
}

/// The text of the first `<tag>` in `xml`, trimmed.
///
/// Namespace prefixes are ignored on purpose: devices send `<friendlyName>`,
/// `<dlna:friendlyName>` and worse, and matching the suffix is what makes the
/// same scan work across them.
pub fn tag_text<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let mut cursor = 0usize;
    let mut body_start = None;
    while let Some(offset) = xml[cursor..].find('<') {
        let open = cursor + offset;
        let close = open + xml[open..].find('>')?;
        let inner = &xml[open + 1..close];
        let closing = inner.starts_with('/');
        // Stop at whatever ends the name: attributes, or a self-closing slash.
        let name = inner
            .trim_start_matches('/')
            .split([' ', '\t', '\r', '\n', '/'])
            .next()
            .unwrap_or("");
        // `<dlna:friendlyName>` is the same element as `<friendlyName>`, and
        // `<myFriendlyName>` is not. Comparing the part after the colon, whole,
        // is what tells those two apart — a substring search cannot.
        if name.rsplit(':').next() == Some(tag) {
            match (closing, body_start) {
                (false, None) => body_start = Some(close + 1),
                (true, Some(start)) => return Some(xml[start..open].trim()),
                _ => {}
            }
        }
        cursor = close + 1;
    }
    None
}

/// The `controlURL` of the service whose `serviceType` ends with `wanted`.
///
/// A renderer's description lists several services and only their order tells
/// them apart, so this walks the `<service>` blocks rather than searching the
/// whole document — which would happily return RenderingControl's URL for an
/// AVTransport request.
pub fn control_url(description: &str, wanted: &str) -> Option<String> {
    for block in description.split("<service>").skip(1) {
        let block = block.split("</service>").next()?;
        if tag_text(block, "serviceType").is_some_and(|t| t.ends_with(wanted)) {
            return tag_text(block, "controlURL").map(str::to_string);
        }
    }
    None
}

/// Escapes text for an XML attribute or element body.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_reply_is_framed_by_its_length_even_if_the_device_keeps_the_socket_open() {
        // This is the bug that cost one field run: we ask for
        // `Connection: close`, the Panasonic answers with a length and holds
        // the socket open anyway, and reading to EOF hangs until the request
        // times out. The writer here is never dropped, so the read only
        // returns if the length was honoured.
        let (mut device, mut ours) = tokio::io::duplex(4096);
        let body = "<ok/>";
        tokio::io::AsyncWriteExt::write_all(
            &mut device,
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();

        let response = tokio::time::timeout(Duration::from_secs(2), read_response(&mut ours))
            .await
            .expect("must not wait for an EOF that never comes")
            .expect("a well-formed reply");
        assert!(response.ends_with(body), "{response}");
    }

    #[tokio::test]
    async fn a_device_that_sends_no_length_is_still_read_to_the_end() {
        let (device, mut ours) = tokio::io::duplex(4096);
        let mut device = device;
        tokio::io::AsyncWriteExt::write_all(&mut device, b"HTTP/1.1 200 OK\r\n\r\n<ok/>")
            .await
            .unwrap();
        drop(device);
        let response = read_response(&mut ours).await.unwrap();
        assert!(response.ends_with("<ok/>"), "{response}");
    }

    #[test]
    fn the_length_header_is_read_whatever_case_the_device_spelled_it_in() {
        assert_eq!(
            content_length("HTTP/1.1 200 OK\r\ncontent-length: 42"),
            Some(42)
        );
        assert_eq!(
            content_length("HTTP/1.1 200 OK\r\nContent-Length:  7 "),
            Some(7)
        );
        assert_eq!(
            content_length("HTTP/1.1 200 OK\r\nContent-Length: wat"),
            None
        );
        assert_eq!(content_length("HTTP/1.1 200 OK"), None);
    }

    /// Trimmed from the real Panasonic 75GX880 at `/dmr/ddd.xml`.
    const PANASONIC: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0"><device>
<friendlyName>75GX880_Series</friendlyName>
<manufacturer>Panasonic</manufacturer>
<serviceList>
<service><serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
<controlURL>/dmr/control_0</controlURL></service>
<service><serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
<controlURL>/dmr/control_1</controlURL></service>
<service><serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
<controlURL>/dmr/control_2</controlURL></service>
</serviceList></device></root>"#;

    #[test]
    fn the_right_service_url_is_picked_out_of_several() {
        // The bug this guards against is real and silent: searching the whole
        // document for `controlURL` returns RenderingControl's, and every
        // later Play lands on the volume service.
        assert_eq!(
            control_url(PANASONIC, "AVTransport:1").as_deref(),
            Some("/dmr/control_2")
        );
        assert_eq!(
            control_url(PANASONIC, "RenderingControl:1").as_deref(),
            Some("/dmr/control_0")
        );
        assert_eq!(control_url(PANASONIC, "ContentDirectory:1"), None);
    }

    #[test]
    fn the_friendly_name_is_read_back() {
        assert_eq!(tag_text(PANASONIC, "friendlyName"), Some("75GX880_Series"));
    }

    #[test]
    fn a_namespace_prefix_does_not_hide_a_tag_and_a_longer_name_does_not_match() {
        assert_eq!(
            tag_text("<dlna:friendlyName>X</dlna:friendlyName>", "friendlyName"),
            Some("X")
        );
        // `<myControlURL>` must not answer a request for `<controlURL>`.
        assert_eq!(
            tag_text("<myfriendlyName>X</myfriendlyName>", "friendlyName"),
            None
        );
    }

    #[test]
    fn a_truncated_document_returns_nothing_rather_than_garbage() {
        assert_eq!(
            tag_text("<friendlyName>no closing tag", "friendlyName"),
            None
        );
        assert_eq!(tag_text("", "friendlyName"), None);
        assert_eq!(
            control_url("<service><serviceType>x", "AVTransport:1"),
            None
        );
    }

    #[test]
    fn urls_are_split_into_what_a_request_needs() {
        let e = Endpoint::parse("http://192.168.1.59:55000/dmr/ddd.xml").unwrap();
        assert_eq!(e.authority, "192.168.1.59:55000");
        assert_eq!(e.path, "/dmr/ddd.xml");
        assert_eq!(e.addr.port(), 55000);
        // No port means the HTTP default.
        assert_eq!(
            Endpoint::parse("http://10.0.0.1/x").unwrap().addr.port(),
            80
        );
        // Not silently downgraded.
        assert!(Endpoint::parse("https://10.0.0.1/x").is_none());
        assert!(Endpoint::parse("192.168.1.59:55000/x").is_none());
    }

    #[test]
    fn control_urls_resolve_in_all_three_forms_devices_use() {
        let base = Endpoint::parse("http://192.168.1.59:55000/dmr/ddd.xml").unwrap();
        assert_eq!(
            base.resolve("/dmr/control_2").unwrap().path,
            "/dmr/control_2"
        );
        assert_eq!(base.resolve("control_2").unwrap().path, "/dmr/control_2");
        let absolute = base.resolve("http://10.0.0.9:80/ctl").unwrap();
        assert_eq!(absolute.authority, "10.0.0.9:80");
        assert_eq!(absolute.path, "/ctl");
    }

    #[test]
    fn metadata_from_a_device_cannot_break_out_of_the_xml_we_build() {
        assert_eq!(
            escape(r#"<a href="x">&'"#),
            "&lt;a href=&quot;x&quot;&gt;&amp;&apos;"
        );
    }
}
