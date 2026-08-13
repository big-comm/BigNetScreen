//! Serves files from disk to a receiver over HTTP.
//!
//! A Chromecast never receives media over the control channel: it is given a
//! **URL** and fetches it itself. For mirroring that URL is a live stream
//! ([`crate::http`]); for a photo or a film it is a file on this computer, and
//! the difference is not cosmetic:
//!
//! - **it must support `Range`.** The receiver asks for byte ranges to seek,
//!   and — with some firmware — opens the file with a range request before
//!   playing at all. A server that ignores `Range` and always sends from zero
//!   makes seeking silently do nothing;
//! - **it is finite.** The response carries a `Content-Length` and ends, rather
//!   than being terminated by EOF the way the live stream is;
//! - **it is served more than once.** The receiver reconnects when it seeks, so
//!   the server stays up for the whole session instead of handing its socket to
//!   GStreamer and stepping aside.
//!
//! ## Protections
//!
//! The same two as the live stream, for the same reason — anything reachable
//! here is the person's own files:
//!
//! 1. a random token in the path, so a portscan finds nothing;
//! 2. an allowlist for the receiver's IP.
//!
//! On top of those, this server can only ever serve **the exact paths it was
//! given**: the URL carries an *index into that list*, never a path. There is
//! no filename in the request to normalise, so `../../.ssh/id_rsa` is not a
//! request this server can express, and directory traversal is impossible by
//! construction rather than by filtering.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use nd_core::{NdError, Result};

use crate::http::{local_ip_towards, random_token};

/// Cap on a request's header size.
const MAX_REQUEST_BYTES: usize = 8 * 1024;
/// How long the receiver gets to finish sending its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Copy buffer. Large enough that a film is not sent in tiny writes, small
/// enough not to hold a megabyte per connection.
const CHUNK: usize = 64 * 1024;

// The kind lives in `nd-core`: it decides how a file is played on **either**
// protocol, and a second definition here would be the same idea maintained in
// two places.
pub use nd_core::media::MediaKind;

/// A file that can be sent, with everything the receiver needs to know.
#[derive(Clone, Debug)]
pub struct MediaFile {
    pub path: PathBuf,
    pub kind: MediaKind,
    pub content_type: &'static str,
    /// Size in bytes, `0` when it could not be read.
    pub size: u64,
}

impl MediaFile {
    /// Describes a file, or explains why this receiver cannot play it.
    ///
    /// The check is by extension, and that is the honest limit: identifying a
    /// container tells us nothing about the codecs inside it, and a Chromecast
    /// rejects `.mp4` carrying HEVC just as it rejects `.mkv`. What this does
    /// catch is the common case — a file type the Default Media Receiver has
    /// never supported — and catching it here means saying so in the interface
    /// instead of the receiver going black with no explanation.
    pub fn inspect(path: &Path) -> std::result::Result<Self, String> {
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();

        // The Default Media Receiver's supported list. Anything outside it
        // fails on the device, so it is refused here where it can be explained.
        let (kind, content_type) = match extension.as_str() {
            "jpg" | "jpeg" => (MediaKind::Photo, "image/jpeg"),
            "png" => (MediaKind::Photo, "image/png"),
            "gif" => (MediaKind::Photo, "image/gif"),
            "webp" => (MediaKind::Photo, "image/webp"),
            "bmp" => (MediaKind::Photo, "image/bmp"),
            "mp4" | "m4v" => (MediaKind::Video, "video/mp4"),
            "webm" => (MediaKind::Video, "video/webm"),
            "mp3" => (MediaKind::Music, "audio/mpeg"),
            "m4a" | "aac" => (MediaKind::Music, "audio/mp4"),
            "wav" => (MediaKind::Music, "audio/wav"),
            "ogg" | "opus" => (MediaKind::Music, "audio/ogg"),
            "flac" => (MediaKind::Music, "audio/flac"),
            "" => return Err("a file with no extension".to_string()),
            other => return Err(format!(".{other} is not a format this receiver plays")),
        };

        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path: path.to_path_buf(),
            kind,
            content_type,
            size,
        })
    }

    /// The name to show on the receiver.
    pub fn title(&self) -> String {
        self.path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("File")
            .to_string()
    }
}

/// An HTTP server for a fixed list of files, running until it is dropped.
pub struct FileServer {
    files: Arc<Vec<MediaFile>>,
    token: String,
    address: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FileServer {
    fn drop(&mut self) {
        // The receiver keeps the connection open between items; without this
        // the listener would outlive the session that owns it.
        self.task.abort();
    }
}

impl FileServer {
    /// Binds on the interface that reaches `receiver` and starts serving.
    ///
    /// `port` of `0` lets the system choose. A fixed port exists for people who
    /// open one by hand on a firewall the application cannot manage.
    pub async fn start(receiver: IpAddr, port: u16, files: Vec<MediaFile>) -> Result<Self> {
        let local = local_ip_towards(receiver)?;
        let listener = TcpListener::bind(SocketAddr::new(local, port))
            .await
            .map_err(|e| NdError::Network(format!("could not bind {local}:{port}: {e}")))?;
        let address = listener
            .local_addr()
            .map_err(|e| NdError::Network(e.to_string()))?;
        let token = random_token()?;
        let files = Arc::new(files);

        let task = tokio::spawn({
            let files = files.clone();
            let token = token.clone();
            async move {
                loop {
                    let Ok((stream, peer)) = listener.accept().await else {
                        break;
                    };
                    // Refuse anyone but the receiver this session opened.
                    if peer.ip() != receiver {
                        tracing::warn!(%peer, "a request from outside the session was refused");
                        continue;
                    }
                    let files = files.clone();
                    let token = token.clone();
                    tokio::spawn(async move {
                        if let Err(err) = serve_one(stream, &files, &token).await {
                            tracing::debug!(%err, "the file request ended");
                        }
                    });
                }
            }
        });

        tracing::info!(%address, count = files.len(), "serving media files");
        Ok(Self {
            files,
            token,
            address,
            task,
        })
    }

    /// The URL of the file at `index`, for the receiver to fetch.
    pub fn url(&self, index: usize) -> String {
        format!("http://{}/{}/{index}", self.address, self.token,)
    }

    pub fn files(&self) -> &[MediaFile] {
        &self.files
    }
}

/// Reads one request and answers it.
async fn serve_one(mut stream: TcpStream, files: &[MediaFile], token: &str) -> Result<()> {
    let request = match tokio::time::timeout(REQUEST_TIMEOUT, read_request(&mut stream)).await {
        Ok(request) => request?,
        Err(_) => return Err(NdError::Network("the request timed out".into())),
    };

    let mut lines = request.lines();
    let start_line = lines.next().unwrap_or_default();
    let mut parts = start_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();

    // A wrong token gets 404, not 403: a 403 would confirm that the server is
    // here and that only the token is missing.
    let Some(index) = parse_target(target, token) else {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
            .await;
        return Ok(());
    };
    let Some(file) = files.get(index) else {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
            .await;
        return Ok(());
    };

    let range = lines
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("range")
                .then(|| value.trim().to_string())
        })
        .and_then(|value| parse_range(&value, file.size));

    let mut handle = tokio::fs::File::open(&file.path)
        .await
        .map_err(|e| NdError::Network(format!("could not open {}: {e}", file.path.display())))?;
    let total = handle
        .metadata()
        .await
        .map(|m| m.len())
        .unwrap_or(file.size);

    let (start, end) = range.unwrap_or((0, total.saturating_sub(1)));
    let length = end.saturating_sub(start) + 1;

    let header = if range.is_some() {
        format!(
            "HTTP/1.1 206 Partial Content\r\n\
             Content-Type: {}\r\n\
             Content-Length: {length}\r\n\
             Content-Range: bytes {start}-{end}/{total}\r\n\
             Accept-Ranges: bytes\r\n\
             Connection: close\r\n\r\n",
            file.content_type
        )
    } else {
        format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: {}\r\n\
             Content-Length: {length}\r\n\
             Accept-Ranges: bytes\r\n\
             Connection: close\r\n\r\n",
            file.content_type
        )
    };
    stream
        .write_all(header.as_bytes())
        .await
        .map_err(|e| NdError::Network(e.to_string()))?;

    // A HEAD asks what the file is, not for the file. Some firmware sends one
    // before deciding whether it can play the item.
    if method.eq_ignore_ascii_case("HEAD") {
        return Ok(());
    }

    handle
        .seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(|e| NdError::Network(e.to_string()))?;

    let mut remaining = length;
    let mut buffer = vec![0u8; CHUNK];
    while remaining > 0 {
        let want = remaining.min(CHUNK as u64) as usize;
        let read = handle
            .read(&mut buffer[..want])
            .await
            .map_err(|e| NdError::Network(e.to_string()))?;
        if read == 0 {
            break;
        }
        // A receiver that stops watching closes the socket; that is an ordinary
        // end of transfer, not a fault to report.
        if stream.write_all(&buffer[..read]).await.is_err() {
            break;
        }
        remaining -= read as u64;
    }
    Ok(())
}

/// Reads until the end of the headers.
async fn read_request(stream: &mut TcpStream) -> Result<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|e| NdError::Network(e.to_string()))?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buffer.len() > MAX_REQUEST_BYTES {
            return Err(NdError::Network("request headers too large".into()));
        }
    }
    Ok(String::from_utf8_lossy(&buffer).to_string())
}

/// Extracts the file's index from `/<token>/<index>`.
///
/// Returns `None` for anything else, including a correct token with junk after
/// it. There is no filename in this path by design: the index is looked up in a
/// list fixed when the session started.
fn parse_target(target: &str, token: &str) -> Option<usize> {
    let path = target.split('?').next().unwrap_or(target);
    let rest = path.strip_prefix('/')?;
    let (given, index) = rest.split_once('/')?;
    // Constant-time comparison is not warranted here — the token is 128 bits of
    // one-shot randomness, not a password to be attacked over many attempts.
    if given != token {
        return None;
    }
    index.parse().ok()
}

/// Parses `bytes=start-end`, the only form a receiver sends.
///
/// An open end (`bytes=500-`) means "to the end of the file", which is what a
/// receiver sends when it seeks.
fn parse_range(value: &str, size: u64) -> Option<(u64, u64)> {
    let spec = value.trim().strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    if size == 0 {
        return None;
    }
    let start: u64 = start.trim().parse().ok()?;
    let end = match end.trim() {
        "" => size - 1,
        value => value.parse().ok()?,
    };
    if start > end || start >= size {
        return None;
    }
    Some((start, end.min(size - 1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_right_token_gets_a_file() {
        assert_eq!(parse_target("/abc123/2", "abc123"), Some(2));
        assert_eq!(parse_target("/wrong/2", "abc123"), None);
        assert_eq!(parse_target("/abc123", "abc123"), None);
        assert_eq!(parse_target("/", "abc123"), None);
    }

    #[test]
    fn a_path_cannot_be_asked_for_only_an_index() {
        // The point of indexing: there is no filename to escape from. Each of
        // these is simply not a number, so it addresses nothing.
        for attack in [
            "/abc123/../../etc/passwd",
            "/abc123/%2e%2e%2fetc%2fpasswd",
            "/abc123//etc/passwd",
            "/abc123/~/.ssh/id_rsa",
        ] {
            assert_eq!(
                parse_target(attack, "abc123"),
                None,
                "{attack} must not address anything"
            );
        }
    }

    #[test]
    fn a_query_string_does_not_confuse_the_index() {
        // Receivers append cache-busting parameters.
        assert_eq!(parse_target("/abc123/0?t=1699", "abc123"), Some(0));
    }

    #[test]
    fn seeking_asks_for_the_rest_of_the_file() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=500-", 1000), Some((500, 999)));
        // Beyond the end, backwards, or against an unknown size: no range,
        // which makes the server answer 200 with the whole file rather than
        // a nonsensical 206.
        assert_eq!(parse_range("bytes=2000-3000", 1000), None);
        assert_eq!(parse_range("bytes=900-100", 1000), None);
        assert_eq!(parse_range("bytes=0-10", 0), None);
        assert_eq!(parse_range("items=0-10", 1000), None);
        // Clamped to the file rather than refused: a receiver asking for more
        // than there is should get what there is.
        assert_eq!(parse_range("bytes=0-9999", 1000), Some((0, 999)));
    }

    #[test]
    fn a_file_is_classified_by_what_the_receiver_can_play() {
        assert_eq!(
            MediaFile::inspect(Path::new("/tmp/holiday.JPG"))
                .unwrap()
                .kind,
            MediaKind::Photo,
            "the extension's case must not matter"
        );
        assert_eq!(
            MediaFile::inspect(Path::new("/tmp/film.mp4")).unwrap().kind,
            MediaKind::Video
        );
        assert_eq!(
            MediaFile::inspect(Path::new("/tmp/song.flac"))
                .unwrap()
                .kind,
            MediaKind::Music
        );
        // Refused here so the interface can say why, instead of the receiver
        // going black with no explanation.
        assert!(MediaFile::inspect(Path::new("/tmp/film.mkv")).is_err());
        assert!(MediaFile::inspect(Path::new("/tmp/notes")).is_err());
    }

    #[test]
    fn the_title_is_the_name_without_the_extension() {
        let file = MediaFile::inspect(Path::new("/tmp/Sunset at the lake.jpg")).unwrap();
        assert_eq!(file.title(), "Sunset at the lake");
    }
}
