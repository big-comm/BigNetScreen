//! The receiver's side of a mirroring session: RTCP arriving on the shared
//! UDP socket, routed to the track it names.
//!
//! Acknowledgements move each track's [`AckWindow`], retransmission requests
//! go to that track's [`super::worker`], and picture-loss requests ask the
//! video thread for a key frame. Every accepted packet is also the receiver's
//! sign of life.

use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};

use nd_core::{NdError, Result};

use super::worker::SessionSignals;
use crate::flow::AckWindow;
use crate::mirror::Negotiated;
use crate::rtcp::{Nack, parse_cast_feedbacks, picture_loss_for};

/// NACK batches a track's inbox holds; the receiver repeats what is dropped.
const NACK_INBOX: usize = 64;

struct Track {
    window: Arc<Mutex<AckWindow>>,
    nacks: SyncSender<Vec<Nack>>,
}

/// Where each piece of feedback goes, keyed by our stream's SSRC.
pub(super) struct FeedbackRoutes {
    /// Our SSRC to the receiver's, from the ANSWER. Feedback must name both.
    receiver_ssrcs: HashMap<u32, u32>,
    tracks: HashMap<u32, Track>,
    video_ssrc: Option<u32>,
}

impl FeedbackRoutes {
    pub(super) fn new(session: &Negotiated) -> Self {
        let receiver_ssrcs = session
            .answer
            .send_indexes
            .iter()
            .zip(&session.answer.ssrcs)
            .filter_map(|(index, receiver_ssrc)| {
                session
                    .offer
                    .streams
                    .iter()
                    .find(|stream| stream.index == *index)
                    .map(|stream| (stream.ssrc, *receiver_ssrc))
            })
            .collect();
        Self {
            receiver_ssrcs,
            tracks: HashMap::new(),
            video_ssrc: session.video().map(|stream| stream.ssrc),
        }
    }

    /// Routes feedback for the track sent as `ssrc` to `window`, and returns
    /// the inbox its retransmission requests arrive in.
    pub(super) fn add_track(
        &mut self,
        ssrc: u32,
        window: Arc<Mutex<AckWindow>>,
    ) -> Receiver<Vec<Nack>> {
        let (nacks, inbox) = std::sync::mpsc::sync_channel(NACK_INBOX);
        self.tracks.insert(ssrc, Track { window, nacks });
        inbox
    }

    /// Applies one datagram. Malformed or foreign packets change nothing.
    fn route(&self, packet: &[u8], signals: &SessionSignals) {
        if let Some(sender_ssrc) = self.video_ssrc
            && let Some(receiver_ssrc) = self.receiver_ssrcs.get(&sender_ssrc)
            && picture_loss_for(packet, *receiver_ssrc, sender_ssrc)
        {
            signals.key_frame_wanted.store(true, Ordering::Relaxed);
            signals.receiver_alive.fetch_add(1, Ordering::Relaxed);
        }
        for feedback in parse_cast_feedbacks(packet) {
            if self.receiver_ssrcs.get(&feedback.sender_ssrc) != Some(&feedback.receiver_ssrc) {
                continue;
            }
            let Some(track) = self.tracks.get(&feedback.sender_ssrc) else {
                continue;
            };
            // Stale or reordered feedback must not move the window, nor have
            // its repairs served.
            if !track
                .window
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .acknowledge(feedback.checkpoint_frame_id, feedback.reference_time)
            {
                continue;
            }
            signals.receiver_alive.fetch_add(1, Ordering::Relaxed);
            if !feedback.nacks.is_empty()
                && let Err(TrySendError::Full(_)) = track.nacks.try_send(feedback.nacks)
            {
                tracing::trace!("NACK inbox full; the receiver will ask again");
            }
        }
    }
}

/// Reads the socket until the session stops. The socket's read timeout
/// bounds how long that takes to notice.
pub(super) fn spawn_reader(
    socket: Arc<UdpSocket>,
    routes: FeedbackRoutes,
    signals: SessionSignals,
) -> Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("cast-rtcp-rx".into())
        .spawn(move || {
            let mut buf = [0u8; 65_536];
            while !signals.stop.load(Ordering::Acquire) {
                if let Ok(size) = socket.recv(&mut buf) {
                    routes.route(&buf[..size], &signals);
                }
            }
        })
        .map_err(|e| NdError::Gst(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mirror::{Answer, MirrorConfig, build_offer};

    /// The video and audio pair the receiver answered with.
    const VIDEO_RECEIVER: u32 = 100_002;
    const AUDIO_RECEIVER: u32 = 100_004;

    fn routes() -> (FeedbackRoutes, u32, u32) {
        let (_, offer) = build_offer(&MirrorConfig::default(), 1).unwrap();
        let session = Negotiated {
            receiver_ip: "192.168.0.2".parse().unwrap(),
            answer: Answer {
                udp_port: 51810,
                send_indexes: vec![0, 1],
                ssrcs: vec![VIDEO_RECEIVER, AUDIO_RECEIVER],
                video_limits: None,
                audio_limits: None,
            },
            offer,
        };
        let video = session.video().unwrap().ssrc;
        let audio = session.audio().unwrap().ssrc;
        (FeedbackRoutes::new(&session), video, audio)
    }

    fn feedback(receiver: u32, sender: u32, checkpoint: u8, lost_frame: Option<u8>) -> Vec<u8> {
        let losses = usize::from(lost_frame.is_some());
        let mut block = vec![0x8F, 206];
        block.extend_from_slice(&(4 + losses as u16).to_be_bytes());
        block.extend_from_slice(&receiver.to_be_bytes());
        block.extend_from_slice(&sender.to_be_bytes());
        block.extend_from_slice(b"CAST");
        block.extend_from_slice(&[checkpoint, losses as u8, 0, 150]);
        if let Some(frame) = lost_frame {
            block.extend_from_slice(&[frame, 0xFF, 0xFF, 0]);
        }
        block
    }

    fn signals() -> SessionSignals {
        SessionSignals::new(Arc::default(), 2).0
    }

    #[test]
    fn feedback_reaches_only_the_track_it_names() {
        let (mut routes, video, audio) = routes();
        let video_window = Arc::new(Mutex::new(AckWindow::default()));
        let audio_window = Arc::new(Mutex::new(AckWindow::default()));
        let video_nacks = routes.add_track(video, video_window.clone());
        let audio_nacks = routes.add_track(audio, audio_window.clone());
        for id in 0..3 {
            video_window.lock().unwrap().sent(id);
            audio_window.lock().unwrap().sent(id);
        }
        let signals = signals();

        routes.route(&feedback(VIDEO_RECEIVER, video, 0, Some(1)), &signals);
        assert_eq!(video_window.lock().unwrap().checkpoint(), 0);
        assert_eq!(audio_window.lock().unwrap().checkpoint(), -1);
        assert_eq!(video_nacks.try_recv().unwrap()[0].frame_id, 1);
        assert!(audio_nacks.try_recv().is_err());
        assert_eq!(signals.receiver_alive.load(Ordering::Relaxed), 1);

        // The audio pair's receiver SSRC on the video track: someone else's.
        routes.route(&feedback(AUDIO_RECEIVER, video, 2, Some(2)), &signals);
        assert_eq!(video_window.lock().unwrap().checkpoint(), 0);
        assert!(video_nacks.try_recv().is_err());
        assert_eq!(signals.receiver_alive.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_picture_loss_for_the_video_pair_asks_for_a_key_frame() {
        let (routes, video, audio) = routes();
        let signals = signals();
        let pli = |receiver: u32, sender: u32| {
            let mut packet = vec![0x81, 206, 0, 2];
            packet.extend_from_slice(&receiver.to_be_bytes());
            packet.extend_from_slice(&sender.to_be_bytes());
            packet
        };
        routes.route(&pli(AUDIO_RECEIVER, audio), &signals);
        assert!(!signals.key_frame_wanted.load(Ordering::Relaxed));
        routes.route(&pli(VIDEO_RECEIVER, video), &signals);
        assert!(signals.key_frame_wanted.load(Ordering::Relaxed));
    }
}
