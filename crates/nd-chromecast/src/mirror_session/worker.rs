//! The thread behind each mirroring track: it pulls frames through its
//! [`StreamSender`], serves the receiver's repair requests, forces key frames
//! and, for video, follows the link with the encoder's bitrate.
//!
//! One thread per track: draining both `appsink`s on one thread ties Opus,
//! with a frame every 10 ms, to the video cadence and chops the sound.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use gst::prelude::*;
use gstreamer as gst;
use tokio::sync::oneshot;

use nd_core::pipeline::{self, H264Encoder};
use nd_core::{NdError, Result};

use super::sender::{RECEIVER_SILENCE_TIMEOUT, StreamSender};
use crate::rate;
use crate::rtcp::Nack;

/// How long a track may go without its first encoded frame.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
/// The least time between key frames the receiver asks for: it keeps asking
/// while unhappy, and every key frame costs the encoder its budget.
const KEY_FRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(500);
/// How often each thread logs what it sent.
const STATS_INTERVAL: Duration = Duration::from_secs(5);
/// NACK entries taken from the inbox per turn.
const MAX_NACKS_PER_TURN: usize = 256;

/// What the threads of one session share with each other and with the
/// session task.
#[derive(Clone)]
pub(super) struct SessionSignals {
    /// Set when the session ends; every thread and the pacer stop.
    pub(super) stop: Arc<AtomicBool>,
    /// Feedback packets accepted from the receiver: its sign of life.
    pub(super) receiver_alive: Arc<AtomicU64>,
    /// The receiver asked for a key frame, or a repair it asked for expired.
    pub(super) key_frame_wanted: Arc<AtomicBool>,
    /// The session's outcome; the first thread to finish or fail sets it.
    outcome: Arc<Mutex<Option<oneshot::Sender<Result<()>>>>>,
    /// Tracks still streaming; the last to end reports success.
    running: Arc<AtomicUsize>,
}

impl SessionSignals {
    pub(super) fn new(
        stop: Arc<AtomicBool>,
        tracks: usize,
    ) -> (Self, oneshot::Receiver<Result<()>>) {
        let (outcome, finished) = oneshot::channel();
        let signals = Self {
            stop,
            receiver_alive: Arc::default(),
            key_frame_wanted: Arc::default(),
            outcome: Arc::new(Mutex::new(Some(outcome))),
            running: Arc::new(AtomicUsize::new(tracks)),
        };
        (signals, finished)
    }

    fn fail(&self, err: NdError) {
        self.finish(Err(err));
    }

    fn track_ended(&self) {
        if self.running.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.finish(Ok(()));
        }
    }

    fn finish(&self, result: Result<()>) {
        if let Some(outcome) = self
            .outcome
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = outcome.send(result);
        }
    }
}

/// What only the video thread does: forcing key frames, retuning the encoder,
/// and reporting where frames are lost before the network.
pub(super) struct VideoControl {
    /// The video `appsink`; the force-key-unit event travels upstream from it.
    element: Option<gst::Element>,
    pipeline: gst::Pipeline,
    encoder: H264Encoder,
    ceiling_kbps: u32,
    rate: rate::RateControl,
    baseline: rate::Baseline,
    last_rate: Instant,
    last_key_frame: Option<Instant>,
    /// Buffers counted at the capture source and after the encoder.
    captured: Option<Arc<AtomicU64>>,
    encoded: Option<Arc<AtomicU64>>,
    counted: (u64, u64),
}

impl VideoControl {
    /// `ceiling_kbps` is the bitrate agreed with the receiver; adaptation
    /// only ever goes below it.
    pub(super) fn new(pipeline: &gst::Pipeline, encoder: H264Encoder, ceiling_kbps: u32) -> Self {
        Self {
            element: pipeline.by_name(pipeline::MIRROR_VIDEO_SINK),
            pipeline: pipeline.clone(),
            encoder,
            ceiling_kbps,
            rate: rate::RateControl::new(ceiling_kbps),
            baseline: rate::Baseline::default(),
            last_rate: Instant::now(),
            last_key_frame: None,
            captured: pipeline::count_buffers(pipeline, pipeline::CAPTURE_SOURCE_NAME),
            encoded: pipeline::count_buffers(pipeline, pipeline::ENCODER_NAME),
            counted: (0, 0),
        }
    }

    /// Forces an IDR when this track needs one to resync, at once, or when
    /// the receiver asked, no more often than [`KEY_FRAME_REQUEST_INTERVAL`].
    fn request_key_frame(&mut self, resync: bool, signals: &SessionSignals) {
        let asked = || {
            self.last_key_frame
                .is_none_or(|at| at.elapsed() >= KEY_FRAME_REQUEST_INTERVAL)
                && signals.key_frame_wanted.swap(false, Ordering::Relaxed)
        };
        if !resync && !asked() {
            return;
        }
        self.last_key_frame = Some(Instant::now());
        if let Some(element) = &self.element {
            tracing::info!(resync, "forcing a key frame");
            let event = gst::event::CustomUpstream::new(
                gst::Structure::builder("GstForceKeyUnit")
                    .field("all-headers", true)
                    .field("running-time", gst::ClockTime::NONE)
                    .field("count", 0u32)
                    .build(),
            );
            let _ = element.send_event(event);
        }
    }

    fn adapt_bitrate(&mut self, sender: &StreamSender, packets_resent: u64) {
        if self.last_rate.elapsed() < rate::UPDATE_INTERVAL {
            return;
        }
        self.last_rate = Instant::now();
        let seen =
            self.baseline
                .advance(sender.stats.packets, packets_resent, sender.frames_dropped);
        if let Some(kbps) = self.rate.observe(seen) {
            let applied = pipeline::set_encoder_bitrate(&self.pipeline, self.encoder, kbps);
            tracing::info!(
                kbps,
                ceiling_kbps = self.ceiling_kbps,
                applied,
                resent = seen.packets_resent,
                of = seen.packets_sent,
                dropped = seen.frames_dropped,
                "bitrate adjusted to what the link is carrying"
            );
        }
    }

    /// Where the frames come from and where they stop: the far end cannot
    /// tell a compositor producing few frames from an encoder that cannot
    /// keep up.
    fn log_throughput(&mut self, frames_sent: u64, seconds: u64) {
        let read = |counter: &Option<Arc<AtomicU64>>| {
            counter
                .as_ref()
                .map_or(0, |count| count.load(Ordering::Relaxed))
        };
        let (captured, encoded) = (read(&self.captured), read(&self.encoded));
        tracing::info!(
            captured_fps = (captured - self.counted.0) / seconds,
            encoded_fps = (encoded - self.counted.1) / seconds,
            sent_fps = frames_sent / seconds,
            "pipeline throughput"
        );
        self.counted = (captured, encoded);
    }
}

/// One track's thread state.
pub(super) struct Worker {
    sender: StreamSender,
    nacks: Receiver<Vec<Nack>>,
    signals: SessionSignals,
    video: Option<VideoControl>,
    nacks_seen: u64,
    packets_resent: u64,
    nacks_expired: u64,
    started: Instant,
    last_stats: Instant,
    /// The receiver's sign of life as last read, and when it last moved.
    last_alive: (u64, Instant),
}

impl Worker {
    pub(super) fn new(
        sender: StreamSender,
        nacks: Receiver<Vec<Nack>>,
        signals: SessionSignals,
        video: Option<VideoControl>,
    ) -> Self {
        let alive = signals.receiver_alive.load(Ordering::Relaxed);
        Self {
            sender,
            nacks,
            signals,
            video,
            nacks_seen: 0,
            packets_resent: 0,
            nacks_expired: 0,
            started: Instant::now(),
            last_stats: Instant::now(),
            last_alive: (alive, Instant::now()),
        }
    }

    /// Streams until the session stops, the track ends or something fails;
    /// a failure becomes the session's outcome.
    pub(super) fn run(mut self) {
        while !self.signals.stop.load(Ordering::Acquire) {
            // Also while waiting for samples or ACKs, and before the first RTP
            // packet, as Open Screen's sender does.
            self.sender.send_report_if_due();
            self.sender.probe_stalled_receiver();
            if self.sender.frame_id == 0 && self.started.elapsed() > FIRST_FRAME_TIMEOUT {
                self.signals.fail(NdError::Gst(format!(
                    "no encoded frames arrived within {} seconds",
                    FIRST_FRAME_TIMEOUT.as_secs()
                )));
                return;
            }
            self.serve_repairs();
            if self.receiver_went_quiet() {
                self.signals
                    .fail(NdError::Protocol("the receiver stopped responding".into()));
                return;
            }
            let resync = self.sender.take_key_frame_request();
            if let Some(video) = &mut self.video {
                video.request_key_frame(resync, &self.signals);
            }
            match self.sender.pump() {
                Ok(true) => {}
                Ok(false) => break,
                Err(err) => {
                    self.signals.fail(err);
                    return;
                }
            }
            if let Some(video) = &mut self.video {
                video.adapt_bitrate(&self.sender, self.packets_resent);
            }
            self.log_stats_if_due();
        }
        self.signals.track_ended();
    }

    /// Takes the NACKs that arrived and resends one burst of repairs, queued
    /// ones included. An expired repair needs a key frame instead.
    fn serve_repairs(&mut self) {
        let mut pending: Vec<Nack> = Vec::new();
        while let Ok(nacks) = self.nacks.try_recv() {
            pending.extend(nacks);
            if pending.len() > MAX_NACKS_PER_TURN {
                break;
            }
        }
        self.nacks_seen += pending.len() as u64;
        let outcome = self.sender.retransmit(&pending);
        self.packets_resent += outcome.sent as u64;
        self.nacks_expired += outcome.expired as u64;
        if outcome.expired > 0 && self.video.is_some() {
            self.signals.key_frame_wanted.store(true, Ordering::Relaxed);
        }
    }

    /// The receiver talks constantly; [`RECEIVER_SILENCE_TIMEOUT`] without a
    /// word means the session ended on its side.
    fn receiver_went_quiet(&mut self) -> bool {
        let seen = self.signals.receiver_alive.load(Ordering::Relaxed);
        if seen != self.last_alive.0 {
            self.last_alive = (seen, Instant::now());
            return false;
        }
        self.last_alive.1.elapsed() > RECEIVER_SILENCE_TIMEOUT
    }

    /// One line per track every [`STATS_INTERVAL`]. Read it as:
    ///
    /// - `fps` below the configured rate: the capture or the encoder is not
    ///   producing frames;
    /// - `pts_jumps`: the gaps are in the source timestamps, so the frames
    ///   were never made;
    /// - `worst_send_ms` high with `pts_jumps` at zero: the pacer holding
    ///   sends back, unless it keeps climbing;
    /// - `dropped`/`window_full` climbing: the receiver is behind and flow
    ///   control skips frames;
    /// - `resent` climbing: the link is losing packets.
    fn log_stats_if_due(&mut self) {
        let window = self.last_stats.elapsed();
        if window < STATS_INTERVAL {
            return;
        }
        let seconds = window.as_secs().max(1);
        let (frames, pts_jumps, worst_jump, worst_send) = self.sender.flow.take();
        tracing::info!(
            stream = self.sender.label(),
            fps = frames / seconds,
            pts_jumps,
            worst_jump_ms = worst_jump,
            worst_send_ms = worst_send,
            dropped = self.sender.frames_dropped,
            window_full = self.sender.dropped_window,
            unacked = self.sender.unacked_frames(),
            nacks = self.nacks_seen,
            resent = self.packets_resent,
            expired = self.nacks_expired,
            "backlog control"
        );
        if let Some(video) = &mut self.video {
            video.log_throughput(frames, seconds);
        }
        self.last_stats = Instant::now();
    }
}
