//! Measuring the link to a receiver: the time to open a TCP connection to the
//! port it already listens on (the Cast control port, the WFD RTSP port).
//!
//! A handshake rather than an ICMP echo: raw sockets need a capability this
//! application does not ask for, and the handshake crosses the same radio and
//! is answered by the same firmware that carries the video, so a saturated
//! receiver shows up as slow. It is a round trip, not a bandwidth figure.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::net::TcpStream;

/// How long to wait before calling the receiver unreachable.
///
/// Generous: a busy receiver on a congested channel can take a while, and
/// reporting "no answer" for a link that is merely slow would be wrong.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Times one TCP handshake to `endpoint`.
///
/// `None` means no answer within [`PROBE_TIMEOUT`], or a refused connection.
/// Both are reported the same way on purpose: from the point of view of someone
/// watching a screen, "the receiver is not answering" is one situation.
pub async fn round_trip(endpoint: SocketAddr) -> Option<Duration> {
    let started = Instant::now();
    let connected = tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect(endpoint)).await;
    match connected {
        Ok(Ok(stream)) => {
            let elapsed = started.elapsed();
            // Closed at once. Holding it open would keep a socket on the
            // receiver for no reason, and some firmware limits how many it
            // will accept.
            drop(stream);
            Some(elapsed)
        }
        Ok(Err(err)) => {
            tracing::debug!(%endpoint, %err, "the receiver refused the probe");
            None
        }
        Err(_) => {
            tracing::debug!(%endpoint, "the receiver did not answer the probe");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_closed_port_answers_quickly_and_negatively() {
        // Port 1 on the loopback: nothing listens, and the refusal is
        // immediate. This is the "refused" branch, which must not be reported
        // as a fast round trip.
        let endpoint: SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert_eq!(round_trip(endpoint).await, None);
    }

    #[tokio::test]
    async fn a_listening_port_is_measured() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Accepts one connection so the handshake completes.
            let _ = listener.accept().await;
        });
        let measured = round_trip(endpoint).await;
        assert!(measured.is_some(), "a listening port must be reachable");
        assert!(
            measured.unwrap() < PROBE_TIMEOUT,
            "loopback cannot take longer than the timeout"
        );
    }
}
