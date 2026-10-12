//! The write half of the control channel, shared by requests, the heartbeat
//! task and the reader's PONGs.
//!
//! A frame goes out whole or the writer closes for good: `write_all` is not
//! cancellation safe, and a STOP or CLOSE appended to half a message would be
//! read by the receiver as the rest of that message.

use std::sync::atomic::{AtomicBool, Ordering};

use tokio::io::{AsyncWrite, AsyncWriteExt, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_rustls::client::TlsStream;

use nd_core::{NdError, Result};

use super::wire::CastMessage;
use super::{CLOSE_PAYLOAD, NS_CONNECTION, NS_HEARTBEAT, PLATFORM_DEST, SOURCE_ID, WRITE_TIMEOUT};

pub(super) struct ControlWriter<W = WriteHalf<TlsStream<TcpStream>>> {
    stream: Mutex<W>,
    /// Nothing more may be written: the platform CLOSE went out, a frame was
    /// left half written, or the reader saw the connection end.
    closed: AtomicBool,
    /// The platform CLOSE is on its way; heartbeats stop, requests do not.
    closing: AtomicBool,
}

/// Marks the writer closed unless the frame it guards completed.
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

impl<W: AsyncWrite + Unpin> ControlWriter<W> {
    pub(super) fn new(stream: W) -> Self {
        Self {
            stream: Mutex::new(stream),
            closed: AtomicBool::new(false),
            closing: AtomicBool::new(false),
        }
    }

    pub(super) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(super) fn mark_closed(&self) {
        self.closed.store(true, Ordering::Release);
    }

    /// Sends one JSON message. Heartbeats are dropped silently once the
    /// platform CLOSE has begun.
    pub(super) async fn send(
        &self,
        namespace: &str,
        destination: &str,
        payload: &str,
    ) -> Result<()> {
        tracing::trace!(%namespace, %destination, bytes = payload.len(), ">>> Cast sent");
        let frame = CastMessage::text(SOURCE_ID, destination, namespace, payload).to_frame()?;
        self.write(&frame, namespace == NS_HEARTBEAT, false).await
    }

    /// Sends the platform CLOSE as the last frame. Idempotent: a closed writer
    /// sends nothing.
    pub(super) async fn send_platform_close(&self) {
        self.closing.store(true, Ordering::Release);
        if !self.is_closed() {
            let frame = CastMessage::text(SOURCE_ID, PLATFORM_DEST, NS_CONNECTION, CLOSE_PAYLOAD)
                .to_frame();
            if let Ok(frame) = frame {
                let _ = self.write(&frame, false, true).await;
            }
        }
        self.mark_closed();
    }

    /// Ends the TLS stream, bounded for a receiver that stopped reading.
    pub(super) async fn shutdown(&self) {
        let _ = tokio::time::timeout(WRITE_TIMEOUT, async {
            self.stream.lock().await.shutdown().await
        })
        .await;
    }

    async fn write(&self, frame: &[u8], heartbeat: bool, last: bool) -> Result<()> {
        let deadline = tokio::time::Instant::now() + WRITE_TIMEOUT;
        let mut stream = tokio::time::timeout_at(deadline, self.stream.lock())
            .await
            .map_err(|_| NdError::Network("Cast writer lock timed out".into()))?;
        if self.is_closed() {
            return Err(NdError::Network("Cast channel is closed".into()));
        }
        if heartbeat && self.closing.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut transaction = FrameWrite {
            closed: &self.closed,
            complete: false,
        };
        tokio::time::timeout_at(deadline, async {
            stream.write_all(frame).await?;
            stream.flush().await
        })
        .await
        .map_err(|_| NdError::Network("Cast frame write timed out; channel closed".into()))?
        .map_err(NdError::network)?;
        transaction.complete = true;
        if last {
            // Still holding the lock: no request can follow the CLOSE.
            self.mark_closed();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn interrupted_partial_write_permanently_closes_writer() {
        let (stream, mut peer) = tokio::io::duplex(4);
        let writer = ControlWriter::new(stream);
        // The peer never reads until the timeout: four bytes fit, the rest block.
        let result = tokio::time::timeout(
            Duration::from_millis(20),
            writer.write(b"partial message", false, false),
        )
        .await;
        assert!(result.is_err());
        assert!(writer.is_closed());
        let mut prefix = [0; 4];
        peer.read_exact(&mut prefix).await.unwrap();
        assert_eq!(&prefix, b"part");
        assert!(writer.write(b"STOP", false, false).await.is_err());
    }

    #[tokio::test]
    async fn heartbeat_runs_until_terminal_close() {
        for _ in 0..20 {
            let (stream, mut peer) = tokio::io::duplex(128);
            let writer = ControlWriter::new(stream);
            for (bytes, heartbeat) in [
                (b"STOP".as_slice(), false),
                (b"PING", true),
                (b"PONG", true),
            ] {
                writer.write(bytes, heartbeat, false).await.unwrap();
            }
            let mut received = [0; 12];
            peer.read_exact(&mut received).await.unwrap();
            assert_eq!(&received, b"STOPPINGPONG");
            writer.closing.store(true, Ordering::Release);
            writer.write(b"CLOSE", false, true).await.unwrap();
            assert!(writer.is_closed());
        }
    }

    #[tokio::test]
    async fn terminal_close_suppresses_following_writes() {
        let (stream, mut peer) = tokio::io::duplex(64);
        let writer = ControlWriter::new(stream);
        writer.closing.store(true, Ordering::Release);
        writer.write(b"PING", true, false).await.unwrap();
        writer.write(b"CLOSE", false, true).await.unwrap();
        let mut received = [0; 5];
        peer.read_exact(&mut received).await.unwrap();
        assert_eq!(&received, b"CLOSE"); // PING was not written.
        assert!(writer.write(b"LOAD", false, false).await.is_err());
    }
}
