//! A sink standing in for a receiver in the queue tests: it records what it
//! was asked to play and plays until told to stop.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::watch;

use nd_core::Result;
use nd_core::capture::CaptureSource;
use nd_core::media::{MediaSource, PlaybackStart};
use nd_core::sink::Sink;

pub(super) struct RecordingSink {
    pub(super) started: Mutex<Vec<PathBuf>>,
    pub(super) initial: Mutex<Vec<Option<PlaybackStart>>>,
    cancel: watch::Sender<bool>,
    pub(super) ready: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl Sink for RecordingSink {
    fn info(&self) -> nd_core::sink::SinkInfo {
        nd_core::sink::SinkInfo {
            id: "test".into(),
            display_name: "test".into(),
            kind: nd_core::sink::SinkKind::WfdP2p,
            address: None,
        }
    }
    fn state(&self) -> nd_core::sink::SinkState {
        nd_core::sink::SinkState::Streaming
    }
    async fn start_stream(&self, source: CaptureSource) -> Result<()> {
        self.cancel.send_replace(false);
        let mut cancelled = self.cancel.subscribe();
        let media = source.media.unwrap();
        self.initial.lock().unwrap().push(media.start);
        let MediaSource::File(path) = media.source else {
            panic!("expected a local test file");
        };
        self.started.lock().unwrap().push(path);
        self.ready.notify_one();
        cancelled.changed().await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        Ok(())
    }
    async fn stop_stream(&self) -> Result<()> {
        self.cancel.send_replace(true);
        Ok(())
    }
}

impl RecordingSink {
    pub(super) fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            started: Mutex::new(Vec::new()),
            initial: Mutex::new(Vec::new()),
            cancel: watch::channel(false).0,
            ready: tokio::sync::Notify::new(),
        })
    }
}

/// Reads one HTTP request: its head and its body.
pub(super) async fn read_request(socket: &mut tokio::net::TcpStream) -> (String, String) {
    use tokio::io::AsyncReadExt;
    let mut data = Vec::new();
    let mut chunk = [0; 4096];
    let head_end = loop {
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "request ended early");
        data.extend_from_slice(&chunk[..read]);
        if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8_lossy(&data[..head_end]).to_string();
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while data.len() < head_end + length {
        let read = socket.read(&mut chunk).await.unwrap();
        data.extend_from_slice(&chunk[..read]);
    }
    let body = String::from_utf8_lossy(&data[head_end..head_end + length]).to_string();
    (head, body)
}
