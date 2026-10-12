//! Serves files from disk to a receiver over HTTP.
//!
//! A Cast receiver or DLNA renderer is given a URL and fetches the file
//! itself ([`crate::media`] hands it over). Unlike the live stream of
//! `nd_core::stream_server`, a file:
//!
//! - must support `Range`: receivers seek by byte range, and some firmware
//!   opens the file with a range request before playing at all;
//! - is finite: the response carries a `Content-Length` and ends;
//! - is served more than once: the receiver reconnects when it seeks, so the
//!   server stays up for the whole session.
//!
//! Anything reachable here is the person's own files. The path carries a
//! random token, only the receiver's IP is answered, and the URL names an
//! index into the list the session was started with, never a path: there is
//! no filename to normalise, so directory traversal cannot be expressed.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::net::{TcpListener, TcpStream};

use nd_core::{NdError, Result};

use nd_core::stream_server::{
    local_ip_towards, random_token, read_request, request_line, write_bounded,
};

/// Copy buffer. Large enough that a film is not sent in tiny writes, small
/// enough not to hold a megabyte per connection.
const CHUNK: usize = 64 * 1024;

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
    /// The check is by extension, which has a limit: the container
    /// says nothing about the codecs inside, and a Chromecast rejects `.mp4`
    /// carrying HEVC as it rejects `.mkv`. It catches a file type the Default
    /// Media Receiver never plays, so the interface can say so instead of the
    /// receiver going black.
    pub fn inspect(path: &Path) -> std::result::Result<Self, String> {
        let extension = extension(path);
        let format = native_format(&extension).ok_or_else(|| unplayable(&extension))?;
        Self::describe(path, format)
    }

    /// Describes a file that a receiver cannot fetch as it is but that the
    /// decoding path (the one DLNA and screen mirroring use) can play: a
    /// player hands over whatever it is showing, and `.mkv` or HEVC is common.
    /// Formats the receiver plays natively are described as by [`Self::inspect`].
    pub fn inspect_decodable(path: &Path) -> std::result::Result<Self, String> {
        let extension = extension(path);
        let format = native_format(&extension)
            .or_else(|| decodable_format(&extension))
            .ok_or_else(|| unplayable(&extension))?;
        Self::describe(path, format)
    }

    fn describe(
        path: &Path,
        (kind, content_type): (MediaKind, &'static str),
    ) -> std::result::Result<Self, String> {
        let metadata = std::fs::metadata(path).map_err(|e| e.to_string())?;
        if !metadata.is_file() {
            return Err("not a regular file".into());
        }
        Ok(Self {
            path: path.to_path_buf(),
            kind,
            content_type,
            size: metadata.len(),
        })
    }

    /// Whether a Chromecast can fetch this file as it is.
    pub fn plays_natively(&self) -> bool {
        native_format(&extension(&self.path)).is_some()
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

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// The Default Media Receiver's supported formats. Anything outside them
/// fails on the device, so it is refused where it can be explained.
fn native_format(extension: &str) -> Option<(MediaKind, &'static str)> {
    Some(match extension {
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
        _ => return None,
    })
}

/// Containers only the decoding path plays.
fn decodable_format(extension: &str) -> Option<(MediaKind, &'static str)> {
    Some(match extension {
        "mkv" | "avi" | "mov" | "ts" | "m2ts" | "mts" | "wmv" | "flv" | "mpg" | "mpeg" | "3gp"
        | "ogv" | "vob" => (MediaKind::Video, "video/x-matroska"),
        "mka" | "wma" | "ape" | "aiff" | "aif" | "wv" | "ac3" | "dts" | "mp2" | "m4b" => {
            (MediaKind::Music, "audio/x-matroska")
        }
        _ => return None,
    })
}

fn unplayable(extension: &str) -> String {
    if extension.is_empty() {
        "a file with no extension".to_string()
    } else {
        format!(".{extension} is not a format this receiver plays")
    }
}

/// An HTTP server for a fixed list of files, running until it is dropped.
pub struct FileServer {
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
        tracing::info!(%address, count = files.len(), "serving media files");
        let files = Arc::new(files);

        let task = tokio::spawn({
            let token = token.clone();
            async move {
                let mut clients = tokio::task::JoinSet::new();
                loop {
                    let accepted = tokio::select! {
                        result = listener.accept(), if clients.len() < 16 => result,
                        _ = clients.join_next(), if !clients.is_empty() => continue,
                    };
                    let Ok((stream, peer)) = accepted else {
                        break;
                    };
                    // Only the receiver this session serves.
                    if peer.ip() != receiver {
                        tracing::warn!(%peer, "a request from outside the session was refused");
                        continue;
                    }
                    let files = files.clone();
                    let token = token.clone();
                    clients.spawn(async move {
                        if let Err(err) = tokio::time::timeout(
                            Duration::from_secs(3600),
                            serve_one(stream, &files, &token),
                        )
                        .await
                        {
                            tracing::debug!(%err, "file connection expired");
                        }
                    });
                }
            }
        });

        Ok(Self {
            token,
            address,
            task,
        })
    }

    /// The URL of the file at `index`, for the receiver to fetch.
    pub fn url(&self, index: usize) -> String {
        format!("http://{}/{}/{index}", self.address, self.token,)
    }
}

/// What a DLNA renderer is told about a whole file: it may seek by byte
/// range and stall the connection to pause. A Chromecast ignores them.
const DLNA_HEADERS: &str = "transferMode.dlna.org: Streaming\r\n\
     contentFeatures.dlna.org: DLNA.ORG_OP=01;DLNA.ORG_CI=0;\
     DLNA.ORG_FLAGS=01700000000000000000000000000000\r\n";

const NOT_FOUND: &[u8] =
    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// Reads one request and answers it.
async fn serve_one(mut stream: TcpStream, files: &[MediaFile], token: &str) -> Result<()> {
    let request = read_request(&mut stream).await?;
    let (method, target) =
        request_line(&request).ok_or_else(|| NdError::Protocol("malformed HTTP request".into()))?;

    // A wrong token and an unknown index both get 404, so a protected file
    // cannot be told from a missing one.
    let Some(file) = parse_target(target, token).and_then(|index| files.get(index)) else {
        let _ = write_bounded(&mut stream, NOT_FOUND).await;
        return Ok(());
    };
    if method != "GET" && method != "HEAD" {
        write_bounded(&mut stream, b"HTTP/1.1 405 Method Not Allowed\r\nAllow: GET, HEAD\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
        return Ok(());
    }
    let mut handle = tokio::fs::File::open(&file.path)
        .await
        .map_err(|e| NdError::Network(format!("could not open file: {e}")))?;
    let metadata = handle
        .metadata()
        .await
        .map_err(|e| NdError::Network(e.to_string()))?;
    if !metadata.is_file() {
        return Err(NdError::Network("not a regular file".into()));
    }
    let total = metadata.len();
    let requested = request
        .lines()
        .skip(1)
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("range").then_some(value.trim())
        })
        .filter(|_| method == "GET");
    let range = requested.and_then(|value| parse_range(value, total));
    if requested.is_some() && range.is_none() {
        let header = format!(
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        write_bounded(&mut stream, header.as_bytes()).await?;
        return Ok(());
    }
    write_bounded(&mut stream, response_head(file, range, total).as_bytes()).await?;
    // A HEAD asks what the file is, not for the file. Some firmware sends one
    // before deciding whether it can play the item.
    if method == "HEAD" {
        return Ok(());
    }
    let start = range.map_or(0, |(start, _)| start);
    let length = range.map_or(total, |(start, end)| end - start + 1);
    send_bytes(&mut handle, &mut stream, start, length).await
}

/// The status line and headers for the whole file or one range of it.
fn response_head(file: &MediaFile, range: Option<(u64, u64)>, total: u64) -> String {
    let (status, length, content_range) = match range {
        Some((start, end)) => (
            "206 Partial Content",
            end - start + 1,
            format!("Content-Range: bytes {start}-{end}/{total}\r\n"),
        ),
        None => ("200 OK", total, String::new()),
    };
    format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {length}\r\n\
         {content_range}\
         Accept-Ranges: bytes\r\n\
         {DLNA_HEADERS}\
         Connection: close\r\n\r\n",
        file.content_type
    )
}

/// Copies `length` bytes from `start`. A receiver that stops watching closes
/// the socket; that ends the transfer and is not a fault.
async fn send_bytes(
    handle: &mut tokio::fs::File,
    stream: &mut TcpStream,
    start: u64,
    length: u64,
) -> Result<()> {
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
            return Err(NdError::Network(
                "file truncated during HTTP transfer".into(),
            ));
        }
        if write_bounded(stream, &buffer[..read]).await.is_err() {
            break;
        }
        remaining -= read as u64;
    }
    Ok(())
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
    // Plain comparison: the token is 128 bits of one-shot randomness, not a
    // password to be attacked over many attempts.
    if given != token {
        return None;
    }
    index.parse().ok()
}

/// Parse a single explicit, open-ended or suffix byte range.
fn parse_range(value: &str, size: u64) -> Option<(u64, u64)> {
    let spec = value.trim().strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    if size == 0 {
        return None;
    }
    if start.trim().is_empty() {
        let suffix = end.trim().parse::<u64>().ok()?;
        return (suffix > 0).then_some((size.saturating_sub(suffix), size - 1));
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
    use tokio::io::AsyncWriteExt;

    fn inspect_fixture(path: &Path) -> std::result::Result<MediaFile, String> {
        let dir = std::env::temp_dir().join(format!("nd-file-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(path.file_name().unwrap());
        std::fs::write(&path, []).unwrap();
        let result = MediaFile::inspect(&path);
        std::fs::remove_file(&path).unwrap();
        result
    }

    async fn fixture(bytes: &[u8], label: &str) -> (FileServer, String, String, PathBuf) {
        let path = std::env::temp_dir().join(format!("nd-http-{}-{label}.mp4", std::process::id()));
        std::fs::write(&path, bytes)
            .unwrap_or_else(|err| panic!("could not write fixture {}: {err}", path.display()));
        let server = FileServer::start(
            "127.0.0.1".parse().unwrap(),
            0,
            vec![MediaFile::inspect(&path).unwrap()],
        )
        .await
        .unwrap();
        let url = server.url(0);
        let (authority, target) = url
            .strip_prefix("http://")
            .unwrap()
            .split_once('/')
            .unwrap();
        (server, authority.into(), target.into(), path)
    }

    #[tokio::test]
    async fn empty_file_has_zero_content_length() {
        let (_server, authority, target, path) = fixture(b"", "empty").await;
        let mut client = TcpStream::connect(authority).await.unwrap();
        client
            .write_all(format!("GET /{target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut response = String::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_to_string(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.contains("Content-Length: 0\r\n"), "{response}");
        assert!(response.ends_with("\r\n\r\n"));
        std::fs::remove_file(path).unwrap();
    }

    async fn fetch(authority: &str, request: &str) -> String {
        let mut client = TcpStream::connect(authority).await.unwrap();
        client.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_to_string(&mut response))
            .await
            .unwrap()
            .unwrap();
        response
    }

    #[tokio::test]
    async fn range_and_head_requests_answered() {
        let (_server, authority, target, path) = fixture(b"0123456789", "range").await;
        let range = fetch(
            &authority,
            &format!("GET /{target} HTTP/1.1\r\nHost: localhost\r\nRange: bytes=2-5\r\n\r\n"),
        )
        .await;
        assert!(
            range.starts_with("HTTP/1.1 206 Partial Content\r\n"),
            "{range}"
        );
        assert!(range.contains("Content-Length: 4\r\n"), "{range}");
        assert!(range.contains("Content-Range: bytes 2-5/10\r\n"), "{range}");
        assert!(range.ends_with("\r\n\r\n2345"), "{range}");

        let head = fetch(
            &authority,
            &format!("HEAD /{target} HTTP/1.1\r\nHost: localhost\r\n\r\n"),
        )
        .await;
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
        assert!(head.contains("Content-Length: 10\r\n"), "{head}");
        assert!(head.ends_with("\r\n\r\n"), "{head}");

        let beyond = fetch(
            &authority,
            &format!("GET /{target} HTTP/1.1\r\nHost: localhost\r\nRange: bytes=20-\r\n\r\n"),
        )
        .await;
        assert!(beyond.starts_with("HTTP/1.1 416 "), "{beyond}");
        assert!(beyond.contains("Content-Range: bytes */10\r\n"), "{beyond}");
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn accepted_connections_close_with_server() {
        let (server, authority, target, path) = fixture(b"PRIVATE", "drop").await;
        let mut client = TcpStream::connect(authority).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(server);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = client
            .write_all(format!("GET /{target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .await;
        let mut response = String::new();
        let _ = tokio::time::timeout(Duration::from_secs(2), client.read_to_string(&mut response))
            .await
            .expect("connection closes");
        assert!(!response.contains("PRIVATE"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn decoder_only_files_are_handed_over() {
        let dir = std::env::temp_dir().join(format!("nd-decodable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let film = dir.join("film.mkv");
        let clip = dir.join("clip.mp4");
        let notes = dir.join("notes.txt");
        for path in [&film, &clip, &notes] {
            std::fs::write(path, []).unwrap();
        }
        assert!(MediaFile::inspect(&film).is_err());
        let decodable = MediaFile::inspect_decodable(&film).unwrap();
        assert_eq!(decodable.kind, MediaKind::Video);
        assert!(!decodable.plays_natively());
        assert!(
            MediaFile::inspect_decodable(&clip)
                .unwrap()
                .plays_natively()
        );
        assert!(MediaFile::inspect_decodable(&notes).is_err());
        assert!(MediaFile::inspect_decodable(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn wrong_token_gets_no_file() {
        assert_eq!(parse_target("/abc123/2", "abc123"), Some(2));
        assert_eq!(parse_target("/wrong/2", "abc123"), None);
        assert_eq!(parse_target("/abc123", "abc123"), None);
        assert_eq!(parse_target("/", "abc123"), None);
    }

    #[test]
    fn only_indexes_are_served() {
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
    fn query_string_keeps_the_index() {
        // Receivers append cache-busting parameters.
        assert_eq!(parse_target("/abc123/0?t=1699", "abc123"), Some(0));
    }

    #[test]
    fn seek_requests_the_file_rest() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=500-", 1000), Some((500, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=-2000", 1000), Some((0, 999)));
        assert_eq!(parse_range("bytes=-0", 1000), None);
        // Beyond the end, backwards, or against an unknown size: no range,
        // which makes the server answer 416 rather than a nonsensical 206.
        assert_eq!(parse_range("bytes=2000-3000", 1000), None);
        assert_eq!(parse_range("bytes=900-100", 1000), None);
        assert_eq!(parse_range("bytes=0-10", 0), None);
        assert_eq!(parse_range("items=0-10", 1000), None);
        // Clamped to the file rather than refused: a receiver asking for more
        // than there is should get what there is.
        assert_eq!(parse_range("bytes=0-9999", 1000), Some((0, 999)));
    }

    #[test]
    fn files_classified_by_receiver_support() {
        assert_eq!(
            inspect_fixture(Path::new("/tmp/holiday.JPG")).unwrap().kind,
            MediaKind::Photo,
            "the extension's case must not matter"
        );
        assert_eq!(
            inspect_fixture(Path::new("/tmp/film.mp4")).unwrap().kind,
            MediaKind::Video
        );
        assert_eq!(
            inspect_fixture(Path::new("/tmp/song.flac")).unwrap().kind,
            MediaKind::Music
        );
        // Refused here so the interface can say why, instead of the receiver
        // going black with no explanation.
        assert!(inspect_fixture(Path::new("/tmp/film.mkv")).is_err());
        assert!(inspect_fixture(Path::new("/tmp/notes")).is_err());
    }

    #[test]
    fn title_drops_the_extension() {
        let file = inspect_fixture(Path::new("/tmp/Sunset at the lake.jpg")).unwrap();
        assert_eq!(file.title(), "Sunset at the lake");
    }
}
