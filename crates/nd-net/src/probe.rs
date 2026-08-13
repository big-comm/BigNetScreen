//! Measuring the link to a receiver, instead of guessing at it.
//!
//! The design this interface came from shows five green bars and "Ping: 8 ms".
//! Nothing in this application knew either number, and inventing them would be
//! a claim about someone's Wi-Fi that we had not earned. This module earns one:
//! the time to open a TCP connection to the port the receiver is already
//! listening on.
//!
//! ## Why a TCP handshake and not a ping
//!
//! - **ICMP is not available.** A raw socket needs a capability this application
//!   does not have and should not ask for.
//! - **It measures the path that matters.** The handshake is a round trip over
//!   the same radio, to the same device, answered by the same firmware that
//!   carries the video. A receiver whose CPU is saturated is slow to accept,
//!   and that shows here — which is a truer picture of "how is this going" than
//!   an ICMP echo answered in the kernel.
//! - **It is cheap and it is polite.** One connection, opened and immediately
//!   closed, every few seconds. The Cast control port and the WFD RTSP port
//!   both accept connections while a session runs.
//!
//! What it is **not** is a bandwidth measurement. A link can answer in 8 ms and
//! still not carry 10 Mbit/s, so the number is labelled as what it is — a round
//! trip — and never as "quality of the picture".

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::net::TcpStream;

/// How long to wait before calling the receiver unreachable.
///
/// Generous: a busy receiver on a congested channel can take a while, and
/// reporting "no answer" for a link that is merely slow would be wrong.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How the link looks, in words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    /// Answers immediately; a pointer will feel attached to the hand.
    Excellent,
    /// Perfectly usable; video is smooth, the pointer has a little weight.
    Good,
    /// Answers late enough to be felt. Expect stutter under movement.
    Weak,
    /// Did not answer at all within the timeout.
    Unreachable,
}

impl Quality {
    /// Classifies a round trip.
    ///
    /// The thresholds come from what the numbers mean for a person watching,
    /// not from a network textbook. On a quiet 5 GHz link to a receiver in the
    /// same room the handshake lands in single-digit milliseconds; once it is
    /// past ~30 ms the radio is contended or the receiver is struggling, and
    /// past ~80 ms it is visible in the picture.
    pub fn of(round_trip: Option<Duration>) -> Self {
        match round_trip {
            None => Quality::Unreachable,
            Some(rtt) if rtt <= Duration::from_millis(30) => Quality::Excellent,
            Some(rtt) if rtt <= Duration::from_millis(80) => Quality::Good,
            Some(_) => Quality::Weak,
        }
    }

    /// How many bars out of four to draw.
    pub fn bars(self) -> usize {
        match self {
            Quality::Excellent => 4,
            Quality::Good => 3,
            Quality::Weak => 2,
            Quality::Unreachable => 0,
        }
    }
}

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

    #[test]
    fn a_link_that_does_not_answer_is_not_called_weak() {
        // "Weak" invites waiting for it to improve; "unreachable" tells the
        // person to look at the device. Collapsing the two would send them to
        // the wrong place.
        assert_eq!(Quality::of(None), Quality::Unreachable);
        assert_eq!(Quality::of(None).bars(), 0);
    }

    #[test]
    fn the_thresholds_are_ordered_and_reach_every_verdict() {
        assert_eq!(
            Quality::of(Some(Duration::from_millis(5))),
            Quality::Excellent
        );
        assert_eq!(
            Quality::of(Some(Duration::from_millis(30))),
            Quality::Excellent
        );
        assert_eq!(Quality::of(Some(Duration::from_millis(31))), Quality::Good);
        assert_eq!(Quality::of(Some(Duration::from_millis(80))), Quality::Good);
        assert_eq!(Quality::of(Some(Duration::from_millis(81))), Quality::Weak);
        assert_eq!(Quality::of(Some(Duration::from_secs(5))), Quality::Weak);
    }

    #[test]
    fn better_links_never_draw_fewer_bars() {
        let ladder = [
            Quality::Unreachable,
            Quality::Weak,
            Quality::Good,
            Quality::Excellent,
        ];
        for pair in ladder.windows(2) {
            assert!(
                pair[0].bars() < pair[1].bars(),
                "{:?} must not draw as many bars as {:?}",
                pair[0],
                pair[1]
            );
        }
    }

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
