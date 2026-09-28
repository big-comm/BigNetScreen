//! A complete Cast mirroring session: negotiate, encode and stream.
//!
//! ```text
//!   capture ──► pipeline (appsink, no container)
//!                    │  H.264 / Opus frames
//!                    ▼
//!            AES-CTR-128 encryption per frame
//!                    ▼
//!            packetised into Cast RTP
//!                    ▼
//!                UDP ──► receiver
//! ```
//!
//! No muxer, no HTTP and no media player on the other end: the delay becomes
//! the negotiated `targetDelay` (the receiver's jitter buffer) rather than the
//! Default Media Receiver's seconds of pre-buffering.
//!
//! ## Two lessons that cost dearly (both validated in the field)
//!
//! **Retransmission is not optional.** The transport is UDP with no error
//! correction. The receiver declares what it lost in feedback blocks and
//! *waits* for the resend; without it, a single lost datagram leaves the frame
//! incomplete and the picture freezes forever. The symptom is deceptive: the
//! receiver keeps sending feedback dozens of times per second, looking
//! perfectly healthy.
//!
//! **One thread per stream.** Opus audio goes out every 10 ms (100 frames/s)
//! and video at 30. Draining both `appsink`s in alternation on a single thread
//! tied the audio to the video's cadence, and the sound came out choppy —
//! perfect picture, unrecognisable audio.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::Duration;

use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;

use nd_core::pipeline::{self, MIRROR_AUDIO_SINK, MIRROR_VIDEO_SINK, StreamConfig};
use nd_core::sink::{SinkState, SinkStatus};
use nd_core::{NdError, Result};

use crate::cast::CastChannel;
use crate::flow::{AckWindow, MAX_UNACKED_FRAMES};
use crate::mirror::{self, MIRRORING_APP_ID, MirrorConfig, Negotiated, OfferedStream};
use crate::rate;
use crate::rtcp::{
    Nack, SenderStats, build_sender_report, classify_receiver_packet, ntp_timestamp,
    parse_cast_feedbacks, picture_loss_for,
};
use crate::rtp::{Frame, Packetizer, encrypt_frame};

/// How long to wait for a frame before checking for cancellation.
const PULL_TIMEOUT: Duration = Duration::from_millis(20);
const MAX_REPAIR_QUEUE: usize = 4096;
const MAX_ENCODED_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// The rate a burst of packets may reach, in bits per second.
///
/// Open Screen's `kDefaultMaxBurstBitrate`, and its header explains the whole
/// point of pacing: sending in measured bursts lets the operating system
/// "collect many small packets into a short-term buffer", which "can be
/// critical for good performance over shared-medium networks (such as 802.11
/// WiFi)". It is a ceiling on how *fast*, never on how much.
///
/// Handing a whole frame to the socket at once instead overruns the queue the
/// access point keeps for the receiver, and the loss comes back in clusters.
/// Measured against a Google TV Stick at 1080p30: one cluster of
/// retransmission requests per second, in step with the one-second GOP,
/// because the key frame is the largest burst of the second — 935 packets
/// resent every five seconds, about 18% of everything sent.
const MAX_BURST_BITRATE: usize = 24 * 1024 * 1024;
/// The window the burst ceiling is measured over (Open Screen's
/// `kDefaultBurstInterval`).
const BURST_INTERVAL: Duration = Duration::from_millis(10);
/// What one burst window may carry, in bytes.
const BURST_BUDGET_BYTES: usize =
    MAX_BURST_BITRATE / 8 * BURST_INTERVAL.as_millis() as usize / 1000;

/// The most video the sender can actually push, in kbit/s.
///
/// The burst budget resets at each window and unspent room does not carry, so
/// [`MAX_BURST_BITRATE`] is a ceiling on the *average* rate and not only on
/// bursts. Asking the encoder for more than the pacer can drain buys no
/// detail: the surplus piles up in front of the socket and the picture falls
/// further behind for as long as the session runs.
///
/// 2160p60 is where the two met. The bitrate table asks 30 Mbit for it, against
/// a 24 Mbit pacer, and the delay grew without bound — while 2160p30, which
/// asks 20 Mbit, stayed fine. Reported from use, in those words.
///
/// An eighth is left free. A stream that fills the pacer exactly has no room to
/// resend a lost packet without adding backlog it can never drain, and the
/// audio track and the RTP and Cast headers on every packet come out of the
/// same budget — together about 3% of it at this packet size.
///
/// An eighth and not more: 1440p60 asks 21 Mbit and works, so the cap has to
/// clear it. It holds 2160p60 and nothing below.
pub(crate) const MAX_VIDEO_BITRATE_KBPS: u32 = MAX_BURST_BITRATE as u32 / 1000 / 8 * 7;
/// How much worth of frames is kept for retransmission.
///
/// The receiver asks for what it lost back; without this history it waits
/// forever and the picture freezes.
///
/// The size has to be measured in **time**, not in a number of frames: with a
/// fixed 16 frames, video held half a second, but audio — which runs at ~100
/// frames per second — held 160 ms. Measured in the field, the receiver asked
/// for 35 audio frames back and only 3 still existed; it waited forever for
/// the other 32, the sound stalled and the device ended the mirroring on its
/// own.
const RETRANSMIT_HISTORY: Duration = Duration::from_secs(2);
/// The frame cap on the history.
///
/// A retransmission request identifies the frame in **8 bits**, so the window
/// has to stay well inside half that range: past it, two live frames share an
/// id and we would resend the wrong one — worse than not resending at all,
/// because the receiver then assembles a corrupted frame. Nothing older than
/// [`MAX_UNACKED_FRAMES`] is worth keeping anyway: the sender never runs
/// further ahead than that.
const RETRANSMIT_HISTORY_MAX: usize = MAX_UNACKED_FRAMES as usize;
/// The receiver silence that ends the session.
///
/// It talks constantly (reports and retransmission requests). When it stops,
/// the session died on that end — and without this limit we kept streaming to
/// a closed port while the interface claimed everything was fine.
const RECEIVER_SILENCE_TIMEOUT: Duration = Duration::from_secs(8);
/// The interval between *sender reports*.
const RTCP_INTERVAL: Duration = Duration::from_millis(500);

/// A/V clock reports are required for a long-lived stream. Both x264 and NVENC
/// add a 1000-hour PTS offset; converting through SEGMENT fixes the old mismatch.
/// Keep an explicit diagnostic opt-out, not an absent-by-default clock.
fn rtcp_enabled() -> bool {
    resolve_rtcp_enabled(std::env::var("BIGNETSCREEN_CAST_RTCP").ok().as_deref())
}

fn resolve_rtcp_enabled(value: Option<&str>) -> bool {
    !value.is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

/// Translate the encoded timestamp into the shared pipeline running-time domain.
/// In particular, x264 offsets PTS by 1000 hours to allow negative DTS. Sending
/// that raw value paired with a running-time SR desynchronises video and audio.
fn sample_running_time(sample: &gst::Sample) -> Result<gst::ClockTime> {
    sample
        .buffer()
        .and_then(|buffer| buffer.pts())
        .and_then(|pts| {
            sample
                .segment()?
                .downcast_ref::<gst::ClockTime>()?
                .to_running_time(pts)
        })
        .ok_or_else(|| NdError::Gst("encoded frame has no valid running-time timestamp".into()))
}

/// One bounded burst budget shared by audio, video, reports and repairs.
/// The mutex protects only accounting; neither sleep nor send holds it.
#[derive(Clone)]
struct BurstPacer {
    budget: std::sync::Arc<std::sync::Mutex<BurstBudget>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

struct BurstBudget {
    started: std::time::Instant,
    bytes: usize,
}

impl BurstBudget {
    fn reserve(&mut self, now: std::time::Instant, bytes: usize) -> Duration {
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= BURST_INTERVAL {
            self.started = now;
            self.bytes = 0;
        }
        if self.bytes + bytes <= BURST_BUDGET_BYTES {
            self.bytes += bytes;
            Duration::ZERO
        } else {
            BURST_INTERVAL.saturating_sub(now.saturating_duration_since(self.started))
        }
    }
}

impl BurstPacer {
    fn new(stop: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self {
            budget: std::sync::Arc::new(std::sync::Mutex::new(BurstBudget {
                started: std::time::Instant::now(),
                bytes: 0,
            })),
            stop,
        }
    }

    fn send(&self, socket: &UdpSocket, packet: &[u8]) -> std::io::Result<usize> {
        if packet.len() > BURST_BUDGET_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "packet exceeds pacing budget",
            ));
        }
        loop {
            if self.stop.load(std::sync::atomic::Ordering::Acquire) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "Cast stopped",
                ));
            }
            let wait = self
                .budget
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .reserve(std::time::Instant::now(), packet.len());
            if wait.is_zero() {
                return socket.send(packet);
            }
            // Small slices make a multi-window keyframe cancellable too.
            std::thread::sleep(wait.min(Duration::from_millis(2)));
        }
    }
}

/// A frame kept for retransmission: FULL id, send instant and encrypted packets.
/// Only lookup of a newly received NACK uses the wire id's low byte.
type HistoryEntry = (u32, std::time::Instant, Vec<Vec<u8>>);
/// A stream's window of recent frames.
type History = std::collections::VecDeque<HistoryEntry>;

fn video_frame_interval(fps: u32) -> Duration {
    // Round up so timestamp rounding cannot turn two frames into a false gap.
    Duration::from_nanos(1_000_000_000u64.div_ceil(u64::from(fps.max(1))))
}

/// Streams one track (video or audio) from the `appsink` to the receiver.
///
/// It runs on a thread of its own: `appsink` is a blocking API and the hot
/// path must not compete for room with the async runtime.
#[derive(Clone, Copy)]
struct SenderTiming {
    fps: u32,
}

struct StreamSender {
    sink: gst_app::AppSink,
    packetizer: Packetizer,
    keys: crate::mirror::StreamKeys,
    /// The socket **shared** by every stream in the session.
    ///
    /// The receiver learns the sender's address from the first packet and ties
    /// the session to that IP:port pair. One socket per stream would make
    /// video and audio arrive from different source ports, and the receiver
    /// would treat half the traffic as belonging to another session.
    socket: std::sync::Arc<UdpSocket>,
    /// Keeps this stream's packets inside the burst ceiling.
    pacer: BurstPacer,
    repair_queue: std::collections::VecDeque<(u32, usize)>,
    queued_repairs: std::collections::HashSet<(u32, usize)>,
    time_base: u32,
    expected_frame_interval: Duration,
    frame_id: u32,
    label: &'static str,
    /// Recent packets, per frame, for serving retransmission requests.
    ///
    /// Full frame ids are retained. Age expiration applies only after ACK;
    /// unacknowledged frames remain available throughout the bounded window.
    history: History,
    /// How many packets the receiver has asked to have sent again.
    nacks_seen: u64,
    /// Expanded monotonic receiver checkpoint and the bounded sent window.
    checkpoint: std::sync::Arc<std::sync::Mutex<AckWindow>>,
    /// Shared with the thread loop, which turns it into a forced IDR.
    /// Raised by this sender when it must resync, and honoured without the
    /// rate limit the receiver's own requests are held to.
    urgent_key_frame: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Waiting for that IDR: until it arrives every P-frame references frames
    /// that were dropped and never reached the receiver.
    resyncing: bool,
    resync_requested: bool,
    /// Frames dropped to keep the backlog inside the window.
    frames_dropped: u64,
    /// Which gate refused, because "the picture froze" names none of them and
    /// the three have different fixes: the frame was older than the sender's
    /// own budget, too much media is still unacknowledged, or the 8-bit frame
    /// window is full.
    dropped_window: u64,
    stats: SenderStats,
    /// Outgoing-flow diagnostics: discontinuity in the frames' timestamps (a
    /// hole born before the network) and the largest wall-clock gap between
    /// two sends (a late sender thread).
    flow: FlowWatch,
    clock_origin: std::time::SystemTime,
    pipeline: gst::Pipeline,
    last_report: Option<std::time::Instant>,
    has_media: bool,
    rtcp_enabled: bool,
    last_probe: std::time::Instant,
}

impl StreamSender {
    /// Which `appsink` and which label a track uses follow from the track
    /// itself, so they are derived here rather than repeated at both call
    /// sites — the two used to be passed in alongside `stream`, and a
    /// mismatched pair would have sent video frames down the audio SSRC.
    fn new(
        pipeline: &gst::Pipeline,
        stream: &OfferedStream,
        socket: std::sync::Arc<UdpSocket>,
        timing: SenderTiming,
        checkpoint: std::sync::Arc<std::sync::Mutex<AckWindow>>,
        urgent_key_frame: std::sync::Arc<std::sync::atomic::AtomicBool>,
        pacer: BurstPacer,
    ) -> Result<Self> {
        let (element_name, label) = if stream.is_video {
            (MIRROR_VIDEO_SINK, "video")
        } else {
            (MIRROR_AUDIO_SINK, "audio")
        };
        let sink = pipeline
            .by_name(element_name)
            .ok_or_else(|| NdError::Gst(format!("element {element_name} not found")))?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| NdError::Gst(format!("{element_name} is not an appsink")))?;

        Ok(Self {
            sink,
            packetizer: Packetizer::new(stream.ssrc, stream.payload_type),
            keys: stream.keys.clone(),
            socket,
            pacer,
            repair_queue: Default::default(),
            queued_repairs: Default::default(),
            time_base: if stream.is_video {
                mirror::VIDEO_TIME_BASE
            } else {
                mirror::AUDIO_TIME_BASE
            },
            expected_frame_interval: if stream.is_video {
                video_frame_interval(timing.fps)
            } else {
                Duration::from_millis(10)
            },
            frame_id: 0,
            label,
            history: std::collections::VecDeque::new(),
            nacks_seen: 0,
            checkpoint,
            urgent_key_frame,
            resyncing: stream.is_video,
            resync_requested: false,
            frames_dropped: 0,
            dropped_window: 0,
            stats: SenderStats::default(),
            flow: FlowWatch::default(),
            clock_origin: std::time::SystemTime::UNIX_EPOCH,
            pipeline: pipeline.clone(),
            last_report: None,
            has_media: false,
            rtcp_enabled: rtcp_enabled(),
            last_probe: std::time::Instant::now(),
        })
    }

    /// Pulls one frame and sends it. `Ok(false)` when the stream has ended.
    fn pump(&mut self) -> Result<bool> {
        let sample = match self.sink.try_pull_sample(gst::ClockTime::from_mseconds(
            PULL_TIMEOUT.as_millis() as u64,
        )) {
            Some(sample) => sample,
            None => {
                if self.sink.is_eos() {
                    return Ok(false);
                }
                return Ok(true); // there simply was no frame ready yet
            }
        };

        let buffer = sample
            .buffer()
            .ok_or_else(|| NdError::Gst("encoded Cast sample has no buffer".into()))?;
        let pts = sample_running_time(&sample)?;

        {
            let window = self
                .checkpoint
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if window.pending() != 0 && window.last_progress.elapsed() > RECEIVER_SILENCE_TIMEOUT {
                return Err(NdError::Protocol(format!(
                    "the receiver stopped acknowledging the {} track",
                    self.label
                )));
            }
        }

        // A key frame depends on nothing; the rest reference the previous
        // one. That is how the receiver knows where it can start decoding.
        let is_key = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);

        // Before encrypting and packetising: a frame the receiver has no room
        // for costs the same CPU and the same network as one it can use.
        //
        // "No room" is the eight-bit frame window and nothing else now. Two
        // other tests used to share this decision and both were measured out
        // of it: an age test that refused 533 of 567 dropped frames, and a
        // media-time budget that refused every audio frame it ever dropped —
        // 121 of 121 in the session that ended with the receiver hanging up.
        // Opus frames are 10 ms and that budget was 66, so six frames of
        // tolerance for an acknowledgement to cross a wireless network, which
        // it routinely does not. The stream was being punched full of holes to
        // protect a receiver that was never more than seven frames behind out
        // of a hundred and twenty.
        if self.skip_while_receiver_catches_up(is_key) {
            return Ok(true);
        }

        let map = buffer.map_readable().map_err(|_| {
            NdError::Gst("cannot map encoded Cast frame; dependency chain cannot be skipped".into())
        })?;
        if map.len() > MAX_ENCODED_FRAME_BYTES {
            return Err(NdError::Gst(
                "encoded Cast frame exceeds the 4 MiB safety limit".into(),
            ));
        }
        let reference = (!is_key && self.frame_id > 0).then(|| self.frame_id - 1);

        // The timestamp goes in the stream's negotiated time base (90 kHz for video).
        let rtp_timestamp =
            ((pts.nseconds() as u128 * self.time_base as u128) / 1_000_000_000u128) as u32;

        let encrypted = encrypt_frame(&self.keys.key, &self.keys.iv_mask, self.frame_id, &map);
        let packets = self.packetizer.packetize(&Frame {
            frame_id: self.frame_id,
            reference_frame_id: reference,
            rtp_timestamp,
            payload: &encrypted,
        });

        self.checkpoint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sent(self.frame_id);
        // Open Screen sends the initial clock mapping before the first RTP
        // packet, but never before a valid encoded frame establishes a track.
        self.has_media = true;
        self.send_report_if_due();
        for packet in &packets {
            match self.pacer.send(&self.socket, packet) {
                Ok(_) => self.stats.record(packet.len().saturating_sub(12)),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => return Ok(false),
                Err(err) => return Err(NdError::Network(err.to_string())),
            }
        }
        // Keep it for a possible retransmission before moving on.
        let now = std::time::Instant::now();
        self.history.push_back((self.frame_id, now, packets));
        let acknowledged = self
            .checkpoint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .checkpoint();
        prune_history(&mut self.history, now, acknowledged);

        self.flow
            .record(pts.nseconds(), self.expected_frame_interval);

        self.send_report_if_due();

        tracing::trace!(
            stream = self.label,
            frame = self.frame_id,
            key = is_key,
            bytes = map.len(),
            "frame sent"
        );

        self.frame_id = self.frame_id.checked_add(1).ok_or_else(|| {
            NdError::Protocol(
                "Cast frame counter exhausted; reconnect to rotate encryption keys".into(),
            )
        })?;
        Ok(true)
    }

    /// Enforce the wire-id window for BOTH tracks, including startup. Audio's
    /// low bitrate does not exempt it from the eight-bit protocol constraint.
    /// After video drops, only an IDR can restart the dependency chain. Request
    /// it when a slot is available, not while the forced IDR would be dropped.
    fn skip_while_receiver_catches_up(&mut self, is_key: bool) -> bool {
        let has_room = self
            .checkpoint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .can_send();
        if !has_room {
            self.dropped_window += 1;
            if self.is_video() {
                self.resyncing = true;
                self.resync_requested = false;
            }
            self.frames_dropped += 1;
            return true;
        }
        if self.resyncing {
            if !is_key {
                if !self.resync_requested {
                    self.urgent_key_frame
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    self.resync_requested = true;
                }
                self.frames_dropped += 1;
                return true;
            }
            self.resyncing = false;
            self.resync_requested = false;
        }
        false
    }

    fn unacked_frames(&self) -> u32 {
        self.checkpoint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending()
    }

    /// Sends a *sender report* when the interval elapses.
    ///
    fn ssrc(&self) -> u32 {
        self.packetizer.ssrc()
    }

    fn label(&self) -> &'static str {
        self.label
    }

    fn is_video(&self) -> bool {
        self.time_base == crate::mirror::VIDEO_TIME_BASE
    }

    /// Resends the packets the receiver declared lost.
    ///
    /// This is what prevents the freeze: without retransmission, one lost
    /// datagram leaves the frame incomplete and the receiver stops advancing.
    fn retransmit(&mut self, nacks: &[Nack]) -> Retransmission {
        let acknowledged = self
            .checkpoint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .checkpoint();
        self.nacks_seen += nacks.len() as u64;
        let mut expired = 0;
        // Deduplicate whole-frame and bitmap requests. Keep FULL frame ids in
        // the work queue so delayed work cannot alias a newer eight-bit id.
        for nack in nacks.iter().take(256) {
            let Some((frame_id, _, packets)) = self
                .history
                .iter()
                .find(|(id, _, _)| *id as u8 == nack.frame_id)
            else {
                expired += 1;
                continue;
            };
            if i64::from(*frame_id) <= acknowledged {
                continue;
            }
            let ids = nack
                .packet_ids()
                .map(|ids| ids.into_iter().map(usize::from).collect::<Vec<_>>())
                .unwrap_or_else(|| (0..packets.len()).collect());
            for id in ids {
                if self.repair_queue.len() >= MAX_REPAIR_QUEUE {
                    break;
                }
                let key = (*frame_id, id);
                if id < packets.len() && self.queued_repairs.insert(key) {
                    self.repair_queue.push_back(key);
                }
            }
        }
        let started = std::time::Instant::now();
        let mut bytes = 0;
        let mut sent = 0;
        // One burst of repair work per turn; retain the rest for the next turn
        // instead of restarting a large whole-frame NACK at packet zero.
        while bytes + crate::rtp::MAX_PACKET_SIZE <= BURST_BUDGET_BYTES
            && started.elapsed() < BURST_INTERVAL
            && !self.pacer.stop.load(std::sync::atomic::Ordering::Acquire)
        {
            let Some(key @ (frame_id, id)) = self.repair_queue.pop_front() else {
                break;
            };
            self.queued_repairs.remove(&key);
            if i64::from(frame_id) <= acknowledged {
                continue;
            }
            let Some(packet) = self
                .history
                .iter()
                .find(|(frame, _, _)| *frame == frame_id)
                .and_then(|(_, _, packets)| packets.get(id))
            else {
                expired += 1;
                continue;
            };
            if self.pacer.send(&self.socket, packet).is_ok() {
                sent += 1;
                bytes += packet.len();
                self.stats.record(packet.len().saturating_sub(12));
            }
        }
        Retransmission { sent, expired }
    }

    fn probe_stalled_receiver(&mut self) {
        if self.last_probe.elapsed() < RTCP_INTERVAL {
            return;
        }
        let needs_probe = {
            let window = self
                .checkpoint
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            window.pending() != 0 && window.last_progress.elapsed() >= RTCP_INTERVAL
        };
        if needs_probe {
            self.last_probe = std::time::Instant::now();
            if let Some(packet) = self
                .history
                .back()
                .and_then(|(_, _, packets)| packets.last())
                && self.pacer.send(&self.socket, packet).is_ok()
            {
                self.stats.record(packet.len().saturating_sub(12));
            }
        }
    }

    /// Enabled by default; see [`rtcp_enabled`] for the diagnostic opt-out.
    fn send_report_if_due(&mut self) {
        if !self.rtcp_enabled
            || !self.has_media
            || self
                .last_report
                .is_some_and(|last| last.elapsed() < RTCP_INTERVAL)
        {
            return;
        }
        let Some(running) = self.pipeline.current_running_time() else {
            return;
        };
        // NTP and RTP describe the SAME instant. Anchor NTP once to the
        // pipeline clock, instead of letting a wall-clock step alter RTP.
        let Some(now) = self
            .clock_origin
            .checked_add(Duration::from_nanos(running.nseconds()))
        else {
            return;
        };
        self.last_report = Some(std::time::Instant::now());
        let timestamp =
            ((u128::from(running.nseconds()) * u128::from(self.time_base)) / 1_000_000_000) as u32;
        let report = build_sender_report(
            self.packetizer.ssrc(),
            ntp_timestamp(now),
            timestamp,
            self.stats,
        );
        if let Err(err) = self.pacer.send(&self.socket, &report) {
            tracing::debug!(stream = self.label, %err, "failed to send a sender report");
        }
    }
}

/// Watches how regular the output is.
///
/// It exists to separate two explanations that sound identical to a listener
/// ("the sound stutters"): either the hole already comes from the pipeline —
/// in which case the frames' timestamps jump — or the hole is ours, because
/// the sender thread was late. Without measuring, both cases lead to the wrong
/// fix.
#[derive(Debug, Default)]
struct FlowWatch {
    /// The previous frame's timestamp (ns).
    last_pts_ns: Option<u64>,
    /// The previous send's instant.
    last_send: Option<std::time::Instant>,
    /// Frames sent since the last report.
    frames: u64,
    /// Timestamp jumps larger than twice the expected interval.
    pts_jumps: u64,
    /// The largest jump observed, in ms.
    worst_pts_jump_ms: u64,
    /// The largest wall-clock gap between two sends, in ms.
    ///
    /// Expect this to sit above the frame interval once [`BurstPacer`] is in
    /// play: a key frame no longer leaves in one shot, so the send that
    /// follows it is held back on purpose. A *rising* number alongside
    /// `pts_jumps = 0` is pacing working, not the sender falling behind.
    worst_send_gap_ms: u64,
}

impl FlowWatch {
    /// Records a frame. `expected` is the nominal interval between frames.
    fn record(&mut self, pts_ns: u64, expected: Duration) {
        let now = std::time::Instant::now();
        if let Some(prev) = self.last_pts_ns {
            let delta = pts_ns.saturating_sub(prev);
            let limit = expected.as_nanos() as u64 * 2;
            if delta > limit {
                self.pts_jumps += 1;
                self.worst_pts_jump_ms = self.worst_pts_jump_ms.max(delta / 1_000_000);
            }
        }
        if let Some(prev) = self.last_send {
            let gap = now.duration_since(prev).as_millis() as u64;
            self.worst_send_gap_ms = self.worst_send_gap_ms.max(gap);
        }
        self.last_pts_ns = Some(pts_ns);
        self.last_send = Some(now);
        self.frames += 1;
    }

    /// Returns the accumulated figures and restarts the count.
    fn take(&mut self) -> (u64, u64, u64, u64) {
        let out = (
            self.frames,
            self.pts_jumps,
            self.worst_pts_jump_ms,
            self.worst_send_gap_ms,
        );
        self.frames = 0;
        self.pts_jumps = 0;
        self.worst_pts_jump_ms = 0;
        self.worst_send_gap_ms = 0;
        out
    }
}

/// Drops from the history whatever is no longer useful to resend.
///
/// Kept apart from [`StreamSender::pump`] so it can be tested: the history's
/// size is precisely what used to kill the session, and a mistake here would
/// only surface as "the sound stuttered and the projector went back to its
/// home screen".
fn prune_history(history: &mut History, now: std::time::Instant, acknowledged: i64) {
    while history.front().is_some_and(|(id, sent, _)| {
        i64::from(*id) <= acknowledged && now.duration_since(*sent) > RETRANSMIT_HISTORY
    }) || history.len() > RETRANSMIT_HISTORY_MAX
    {
        history.pop_front();
    }
}

/// The outcome of one round of retransmissions.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Retransmission {
    /// Packets actually resent.
    sent: usize,
    /// Requests that could no longer be served: the frame left the history.
    expired: usize,
}

/// Does the receiver lack mirroring support (is the HTTP path worth a try)?
///
/// This tells "this device has no mirroring app" apart from "the stream broke
/// midway". Only the first case justifies falling back to the Default Media
/// Receiver: restarting over HTTP after a network failure would merely trade
/// one problem for another, at twice the delay.
///
/// The distinction is carried by the **error type**, not by matching on
/// message text. It used to be a list of substrings, which quietly stopped
/// working the moment those messages were reworded — the fallback would have
/// silently never fired again.
pub fn is_unsupported(err: &NdError) -> bool {
    matches!(err, NdError::Unsupported(_))
}

/// Runs a mirroring session until cancellation or end of stream.
pub async fn run(
    receiver_ip: IpAddr,
    // The control port the receiver announced over mDNS.
    receiver_port: u16,
    video: pipeline::VideoSource,
    size: (u32, u32),
    status: &SinkStatus,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    status.set(SinkState::Connecting);

    // Silence Wi-Fi Direct scanning for as long as this lasts: it uses the
    // same antenna and interrupts the link to the receiver (see
    // `nd_core::radio`).
    let _radio = nd_core::radio::quiet();

    // 1. Control channel and mirroring app.
    if *cancel.borrow() {
        return Ok(());
    }
    let channel = CastChannel::connect_to(receiver_ip, receiver_port).await?;
    if *cancel.borrow() {
        channel.close().await;
        return Ok(());
    }
    let app = match channel.launch(MIRRORING_APP_ID).await {
        Ok(app) => app,
        Err(err) => {
            channel.close().await;
            return Err(err);
        }
    };
    let result = async {
        if *cancel.borrow() { return Ok(()); }

        // 2. Negotiation: the receiver returns the UDP port and accepts (or not)
        //    each offered stream.
        status.set(SinkState::WaitSocket);
        let (width, height) = StreamConfig::fit_within(
            size,
            StreamConfig::preferred_or(pipeline::CHROMECAST_MAX_RESOLUTION),
        );
        let mut cfg = StreamConfig {
            width,
            height,
            // The Cast paths have no frame rate to negotiate against, so the
            // preference is the whole answer, capped at what H.264 mirroring
            // receivers accept.
            fps: StreamConfig::capped_fps(60),
            // Three frames of rate-control budget, and this one stays.
            //
            // It was removed on the reasoning that the encoder knows its own
            // job, which was right about the key-frame interval and wrong here.
            // Switching a browser tab changes the whole screen at once, and an
            // encoder with no budget spends it: one enormous frame, which on
            // this path is several hundred UDP datagrams the pacer then lets
            // out a burst at a time. Reported immediately as the receiver being
            // slow to show a big change, because the first update cannot
            // arrive until the last packet of that frame has.
            //
            // The difference from the key-frame interval is what the encoder
            // can know. It knows how to spend bits on a picture. It does not
            // know that here a frame *is* a burst of datagrams on a paced link
            // — that is a property of this transport, and telling it so is the
            // one kind of configuration worth keeping.
            vbv_frames: pipeline::CAST_VBV_FRAMES,
            //
            // The receiver asks for a key frame when it needs one, and NACK
            // repairs loss before it comes to that. Scheduling one a second on
            // top only makes the picture pulse.
            gop_seconds: 0,
            // Google documents H.264 High for Cast playback. The supplied
            // fixed-QP samples save about 13%; mirroring interoperability
            // still needs validation across the receiver matrix.
            profile: pipeline::H264Profile::High,
            audio: pipeline::AudioSource::detect(),
            ..Default::default()
        };
        // The encoder is chosen before the receiver has spoken, because the
        // pipeline has to exist before there is a size to offer. A GPU that
        // encodes the portal's guess encodes the real thing: the two differ by
        // the compositor's scale factor, not by a capability.
        let driver = crate::session::detect_gpu_driver();
        cfg.encoder = pipeline::working_encoder(driver, pipeline::Acceleration::preferred(), cfg).await?;
        // The candidate list is logged before the probes run; this is the one
        // that actually encoded a frame, which is the only one that matters.
        // The thread count is also the slice count for software encoding, and
        // it was invisible: a report of "still multi-threaded" could not be
        // answered from a log that never said how many.
        tracing::info!(
            encoder = ?cfg.encoder,
            threads = (!cfg.encoder.is_hardware()).then(pipeline::software_encode_threads),
            "encoder in use"
        );
        if *cancel.borrow() {
            return Ok(());
        }

        // Build and start the capture now. Until it negotiates, every size in
        // play is the portal's compositor-space guess — `size_or` documents why
        // that is not pixels — and the OFFER must not carry a guess.
        let ceiling = StreamConfig::preferred_or(pipeline::CHROMECAST_MAX_RESOLUTION);
        // The GPU path cannot scale, so it is only eligible while the screen
        // fits inside the ceiling — which is what the portal's size, wrong as
        // it is about pixels, is accurate enough to decide.
        let gpu = pipeline::gpu_path_requested()
            && cfg.encoder == pipeline::H264Encoder::NvH264
            && size.0 <= ceiling.0
            && size.1 <= ceiling.1;
        tracing::info!(gpu, "mirroring capture path");
        let desc = pipeline::mirror_pipeline_description(&cfg, &video, ceiling, gpu);
        let (gst_pipeline, events) = pipeline::build_pipeline(&desc, cfg.latency_ms())?;
        let _capture = StreamWorkers {
            pipeline: gst_pipeline.clone(),
            stop: Default::default(),
            threads: Vec::new(),
        };
        gst_pipeline
            .set_state(gst::State::Playing)
            .map_err(|err| NdError::Gst(err.to_string()))?;
        // Frames produced before the senders exist are dropped by the leaky
        // video queue; nothing reaches the network until the ANSWER arrives.
        if let Some(negotiated) = pipeline::negotiated_video_size(
            &gst_pipeline,
            std::time::Instant::now() + Duration::from_secs(5),
        ) {
            (cfg.width, cfg.height) = negotiated;
        }
        // Delivered against encoded, so one line says whether the scaler is
        // doing anything. `portal` is what the OFFER used to be built from.
        let delivered = pipeline::delivered_capture_size(&gst_pipeline);
        let (caps, dmabuf) = pipeline::capture_memory(&gst_pipeline)
            .unwrap_or_else(|| ("unknown".into(), "unknown".into()));
        tracing::info!(
            delivered = ?delivered.map(|(w, h)| format!("{w}x{h}")),
            encoding = format!("{}x{}", cfg.width, cfg.height),
            portal = format!("{width}x{height}"),
            rescaled = delivered.is_some_and(|size| size != (cfg.width, cfg.height)),
            // Whether the frame is still on the GPU when we get it, and whether
            // it could have been. See `pipeline::capture_memory`.
            %caps,
            %dmabuf,
            "capture negotiated"
        );

        // Offer the bitrate this mode needs. A bigger, faster picture that is
        // sent at 1080p30's bitrate does not arrive safer, it arrives smeared.
        // The receiver's own `maxBitRate` still caps this in `constrain`.
        cfg.bitrate_kbps = 0;
        let wanted = cfg.scaled_bitrate_kbps();
        cfg.bitrate_kbps = mirror::cast_bitrate_kbps(&cfg);
        if cfg.bitrate_kbps < wanted {
            tracing::info!(
                wanted_kbps = wanted,
                sending_kbps = cfg.bitrate_kbps,
                "this mode wants more bitrate than the sender can push; a lower \
                 frame rate would spend the same bits on fewer, sharper frames"
            );
        }
        let mirror_cfg = MirrorConfig {
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
            max_bitrate: cfg.bitrate_kbps * 1000,
            with_audio: true,
            ..Default::default()
        };

        let session = tokio::select! {
            result = mirror::negotiate(&channel, &app, receiver_ip, &mirror_cfg) => result?,
            _ = cancel.changed() => return Ok(()),
        };

        let offered = (cfg.width, cfg.height);
        session.answer.constrain(&mut cfg, mirror_cfg.target_delay_ms, session.audio().is_some())?;
        if *cancel.borrow() { return Ok(()); }
        // Only a receiver that asked for something smaller makes the scaler
        // work. Left alone, the capture reaches the encoder untouched.
        if (cfg.width, cfg.height) != offered {
            pipeline::pin_video_size(&gst_pipeline, &cfg)?;
        }
        tracing::info!(width = cfg.width, height = cfg.height, fps = cfg.fps,
            bitrate_kbps = cfg.bitrate_kbps,
            rescaled = (cfg.width, cfg.height) != offered,
            "Cast transmission mode after receiver constraints");
        // After the negotiation, not before: what goes on the wall is what the
        // receiver agreed to.
        status.set_link(nd_core::sink::StreamLink {
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
            endpoint: Some(std::net::SocketAddr::new(receiver_ip, receiver_port)),
            receivers: None,
        });

        {
            let (stop, mut stopped) = tokio::sync::watch::channel(*cancel.borrow());
            let streaming = stream(&cfg, gst_pipeline.clone(), events, &session, status,
                &mut stopped);
            tokio::pin!(streaming);
            loop {
                tokio::select! {
                    result = &mut streaming => break result,
                    _ = cancel.changed() => {
                        let _ = stop.send(true);
                        break streaming.await;
                    }
                    event = channel.next_event() => {
                        let outcome = match event {
                            None => Some(Err(NdError::Protocol("Cast control channel closed".into()))),
                            Some(event) if event.closes(&app) => Some(Ok(())),
                            _ => None,
                        };
                        if let Some(outcome) = outcome {
                            let _ = stop.send(true);
                            let stopped = streaming.await;
                            break outcome.and(stopped);
                        }
                    }
                }
            }
        }
    }.await;

    channel.finish_app(&app, result).await
}

/// Stops the pipeline before joining workers, including failed startup.
struct StreamWorkers {
    pipeline: gst::Pipeline,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Drop for StreamWorkers {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        let _ = self.pipeline.set_state(gst::State::Null);
        for thread in self.threads.drain(..) {
            if thread.join().is_err() {
                tracing::error!("a Cast worker panicked during shutdown");
            }
        }
    }
}

/// The pipeline arrives already built and running: its negotiated size is what
/// the OFFER was written from, so it cannot be created here.
async fn stream(
    cfg: &StreamConfig,
    gst_pipeline: gst::Pipeline,
    mut events: pipeline::PipelineEvents,
    session: &Negotiated,
    status: &SinkStatus,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    if *cancel.borrow() {
        return Ok(());
    }
    status.set(SinkState::WaitStreaming);

    let mut workers = StreamWorkers {
        pipeline: gst_pipeline.clone(),
        stop: Default::default(),
        threads: Vec::new(),
    };

    let (ip, port) = session.target();
    let target = SocketAddr::new(ip, port);

    let mut senders = Vec::new();
    let want_key_frame = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let urgent_key_frame = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // The receiver's acknowledgement, one slot per stream, keyed by the SSRC
    // its feedback carries. `-1` until it first speaks.
    let mut checkpoints: std::collections::HashMap<
        u32,
        std::sync::Arc<std::sync::Mutex<AckWindow>>,
    > = Default::default();
    // A single socket for the whole session (see `StreamSender::socket`).
    let bind: SocketAddr = if target.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let socket =
        std::sync::Arc::new(UdpSocket::bind(bind).map_err(|e| NdError::Network(e.to_string()))?);
    socket
        .connect(target)
        .map_err(|e| NdError::Network(e.to_string()))?;
    socket
        .set_write_timeout(Some(Duration::from_millis(50)))
        .map_err(|e| NdError::Network(e.to_string()))?;
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|e| NdError::Network(e.to_string()))?;
    let pacer = BurstPacer::new(workers.stop.clone());
    tracing::debug!(
        source = ?socket.local_addr().ok(),
        target = %target,
        "mirroring session socket"
    );

    if let Some(stream) = session.video() {
        let checkpoint = std::sync::Arc::new(std::sync::Mutex::new(AckWindow::default()));
        checkpoints.insert(stream.ssrc, checkpoint.clone());
        senders.push(StreamSender::new(
            &gst_pipeline,
            stream,
            socket.clone(),
            SenderTiming { fps: cfg.fps },
            checkpoint,
            urgent_key_frame.clone(),
            pacer.clone(),
        )?);
    }
    if let Some(stream) = session.audio() {
        let checkpoint = std::sync::Arc::new(std::sync::Mutex::new(AckWindow::default()));
        checkpoints.insert(stream.ssrc, checkpoint.clone());
        match StreamSender::new(
            &gst_pipeline,
            stream,
            socket.clone(),
            SenderTiming { fps: cfg.fps },
            checkpoint,
            urgent_key_frame.clone(),
            // Audio paces against itself, not against video.
            //
            // A burst window is 10 ms and carries 30 KB. A 1080p60 frame at
            // 15 Mbit averages 31 KB, so an ordinary video frame fills the
            // window on its own and a large one fills several — and an Opus
            // packet that arrives in the meantime waits for a window with room
            // in it. That is a hundred and sixty bytes, once every 10 ms,
            // queueing behind megabits: audio drifting behind the picture,
            // which is how it was reported.
            //
            // The ceiling exists to keep a burst from swamping the link. Audio
            // is 128 kbit/s against a 24 Mbit/s limit and cannot swamp
            // anything, so it gets a budget of its own rather than a share of
            // the one video is always exhausting.
            BurstPacer::new(workers.stop.clone()),
        ) {
            Ok(sender) => senders.push(sender),
            // The session goes on without audio: video is what matters here,
            // and the receiver accepted the streams individually.
            Err(err) => tracing::warn!(%err, "no audio track in this session"),
        }
    }
    for (name, accepted) in [
        (MIRROR_VIDEO_SINK, session.video().is_some()),
        (MIRROR_AUDIO_SINK, session.audio().is_some()),
    ] {
        if !accepted && let Some(element) = gst_pipeline.by_name(name) {
            element.set_property("drop", true);
            element.set_property("max-buffers", 1u32);
            element.set_property("wait-on-eos", false);
        }
    }
    if senders.is_empty() {
        return Err(NdError::Unsupported(
            "the receiver accepted no usable stream".into(),
        ));
    }

    // The receiver talks back: reports, NACKs and key-frame requests. Ignoring
    // that channel meant losing all the diagnostics — and the retransmission
    // request is what keeps the picture from freezing.
    //
    // The requests arrive identified by the stream's SSRC, so each one gets its
    // own inbox.
    let mut nack_inboxes: std::collections::HashMap<u32, std::sync::mpsc::SyncSender<Vec<Nack>>> =
        Default::default();
    let mut nack_receivers = Vec::new();
    for sender in &senders {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<Nack>>(64);
        nack_inboxes.insert(sender.ssrc(), tx);
        nack_receivers.push(rx);
    }

    let receiver_ssrcs: std::collections::HashMap<u32, u32> = session
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
    let video_ssrc = session.video().map(|stream| stream.ssrc);
    let listener = socket.clone();
    let key_flag = want_key_frame.clone();
    // How many packets the receiver has sent us. The sender threads use the
    // counter as a sign of life: if it stops rising, the session died on that
    // end (see `RECEIVER_SILENCE_TIMEOUT`).
    let receiver_alive = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let alive_counter = receiver_alive.clone();
    let stopped = workers.stop.clone();
    let receiver_thread = std::thread::Builder::new()
        .name("cast-rtcp-rx".into())
        .spawn(move || {
            let mut buf = [0u8; 65_536];
            while !stopped.load(std::sync::atomic::Ordering::Acquire) {
                let Ok(size) = listener.recv(&mut buf) else {
                    continue;
                };
                if classify_receiver_packet(&buf[..size]).is_none() {
                    continue;
                }
                if let Some(sender_ssrc) = video_ssrc
                    && let Some(receiver_ssrc) = receiver_ssrcs.get(&sender_ssrc)
                    && picture_loss_for(&buf[..size], *receiver_ssrc, sender_ssrc)
                {
                    key_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    alive_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                for feedback in parse_cast_feedbacks(&buf[..size]) {
                    if receiver_ssrcs.get(&feedback.sender_ssrc) != Some(&feedback.receiver_ssrc) {
                        continue;
                    }
                    let Some(window) = checkpoints.get(&feedback.sender_ssrc) else {
                        continue;
                    };
                    if !window
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .acknowledge(feedback.checkpoint_frame_id, feedback.reference_time)
                    {
                        continue;
                    }
                    alive_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if feedback.nacks.is_empty() {
                        continue;
                    }
                    if let Some(inbox) = nack_inboxes.get(&feedback.sender_ssrc)
                        && matches!(
                            inbox.try_send(feedback.nacks),
                            Err(std::sync::mpsc::TrySendError::Disconnected(_))
                        )
                    {
                        break;
                    }
                }
            }
        })
        .map_err(|e| NdError::Gst(e.to_string()))?;
    workers.threads.push(receiver_thread);

    tracing::info!(
        target = %target,
        streams = senders.len(),
        "starting Cast mirroring"
    );

    gst_pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| NdError::Gst(e.to_string()))?;
    status.set(SinkState::Streaming);
    let now = std::time::SystemTime::now();
    let running = gst_pipeline
        .current_running_time()
        .unwrap_or(gst::ClockTime::ZERO);
    let clock_origin = now
        .checked_sub(Duration::from_nanos(running.nseconds()))
        .unwrap_or(now);
    for sender in &mut senders {
        sender.clock_origin = clock_origin;
    }

    // **One thread per stream.** Alternating between video and audio on a
    // single thread tied the audio to the video's cadence: with 10 ms frames,
    // audio has to be drained ~100 times per second, and at 30 fps it was
    // drained 30 — the result was constant overflow and choppy sound.
    // Where the frames come from and where they stop. Reported every window
    // beside what the sender managed, because the far end of the pipeline
    // cannot tell a compositor producing seven frames a second from an encoder
    // that cannot keep up — and that is the first thing every slow picture
    // turns out to be.
    let captured = pipeline::count_buffers(&gst_pipeline, pipeline::CAPTURE_SOURCE_NAME);
    let encoded = pipeline::count_buffers(&gst_pipeline, pipeline::ENCODER_NAME);

    // What the encoder was told to aim for, and the handle that retunes it.
    let ceiling_kbps = cfg.bitrate_kbps;
    let encoder = cfg.encoder;
    let rate_pipeline = gst_pipeline.clone();

    let video_element = gst_pipeline.by_name(MIRROR_VIDEO_SINK);
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
    let done_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(done_tx)));

    let remaining = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(senders.len()));
    for (sender, nack_rx) in senders.into_iter().zip(nack_receivers) {
        let remaining = remaining.clone();
        let label = sender.label();
        let is_video = sender.is_video();
        let key_flag = want_key_frame.clone();
        let urgent_flag = urgent_key_frame.clone();
        let video_element = video_element.clone();
        let done_tx = done_tx.clone();
        let receiver_alive = receiver_alive.clone();
        let stopped = workers.stop.clone();
        let captured = captured.clone();
        let encoded = encoded.clone();
        let rate_pipeline = rate_pipeline.clone();

        let handle = std::thread::Builder::new()
            .name(format!("cast-{label}"))
            .spawn(move || {
                let mut sender = sender;
                let mut nacks_seen = 0u64;
                let mut packets_resent = 0u64;
                let mut nacks_expired = 0u64;
                let mut last_stats = std::time::Instant::now();
                let mut counted = (0u64, 0u64);
                // Only video adapts. The audio track is 128 kbit of Opus, and
                // lowering it would save less than it costs to explain.
                let mut rate = is_video.then(|| rate::RateControl::new(ceiling_kbps));
                let mut last_rate = std::time::Instant::now();
                let mut rate_baseline = (0u64, 0u64, 0u64);
                let mut last_key_request: Option<std::time::Instant> = None;
                let started_at = std::time::Instant::now();
                // State of the receiver's sign of life.
                let mut last_seen = receiver_alive.load(std::sync::atomic::Ordering::Relaxed);
                let mut last_seen_at = std::time::Instant::now();

                while !stopped.load(std::sync::atomic::Ordering::Acquire) {
                    // Also runs while waiting for samples/ACKs, and before the
                    // first RTP packet, as in Open Screen's sender.
                    sender.send_report_if_due();
                    sender.probe_stalled_receiver();
                    if sender.frame_id == 0 && started_at.elapsed() > Duration::from_secs(10) {
                        if let Some(tx) = done_tx
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .take()
                        {
                            let _ = tx.send(Err(NdError::Gst(
                                "no encoded frames arrived within 10 seconds".into(),
                            )));
                        }
                        return;
                    }
                    // Serve this stream's retransmissions.
                    let mut pending: Vec<Nack> = Vec::new();
                    while let Ok(nacks) = nack_rx.try_recv() {
                        pending.extend(nacks);
                        if pending.len() > 256 {
                            break;
                        }
                    }
                    nacks_seen += pending.len() as u64;
                    // Drain queued repairs even when no new NACK arrived.
                    let outcome = sender.retransmit(&pending);
                    packets_resent += outcome.sent as u64;
                    nacks_expired += outcome.expired as u64;
                    if outcome.expired > 0 && is_video {
                        key_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    }

                    // Has the receiver gone quiet? Then the session ended on
                    // that end, and carrying on only wastes network while the
                    // interface lies that all is well.
                    let seen = receiver_alive.load(std::sync::atomic::Ordering::Relaxed);
                    if seen != last_seen {
                        last_seen = seen;
                        last_seen_at = std::time::Instant::now();
                    } else if last_seen_at.elapsed() > RECEIVER_SILENCE_TIMEOUT {
                        if let Ok(mut slot) = done_tx.lock()
                            && let Some(tx) = slot.take()
                        {
                            let _ = tx.send(Err(NdError::Protocol(
                                "the receiver stopped responding".into(),
                            )));
                        }
                        return;
                    }

                    // A key-frame request only makes sense for video.
                    //
                    // The rate limit is for the receiver, which asks repeatedly
                    // while it is unhappy and would otherwise have the encoder
                    // spending its budget on key frames. Our own resync is not
                    // that: it is raised once per resync, and until it is
                    // answered every frame is being dropped anyway — so waiting
                    // half a second to honour it is half a second of frozen
                    // picture bought for nothing.
                    let urgent = urgent_flag.swap(false, std::sync::atomic::Ordering::Relaxed);
                    if is_video
                        && (urgent
                            || last_key_request
                                .is_none_or(|at| at.elapsed() >= Duration::from_millis(500)))
                        && (urgent || key_flag.swap(false, std::sync::atomic::Ordering::Relaxed))
                    {
                        last_key_request = Some(std::time::Instant::now());
                        if let Some(element) = &video_element {
                            tracing::info!("the receiver asked for a key frame");
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

                    match sender.pump() {
                        Ok(true) => {}
                        Ok(false) => break,
                        Err(err) => {
                            if let Ok(mut slot) = done_tx.lock()
                                && let Some(tx) = slot.take()
                            {
                                let _ = tx.send(Err(err));
                            }
                            return;
                        }
                    }

                    if let Some(control) = rate.as_mut()
                        && last_rate.elapsed() >= rate::UPDATE_INTERVAL
                    {
                        last_rate = std::time::Instant::now();
                        let sent = u64::from(sender.stats.packets);
                        let seen = rate::Window {
                            packets_sent: sent.wrapping_sub(rate_baseline.0),
                            packets_resent: packets_resent - rate_baseline.1,
                            frames_dropped: sender.frames_dropped - rate_baseline.2,
                        };
                        rate_baseline = (sent, packets_resent, sender.frames_dropped);
                        if let Some(kbps) = control.observe(seen) {
                            let applied =
                                pipeline::set_encoder_bitrate(&rate_pipeline, encoder, kbps);
                            tracing::info!(
                                kbps,
                                ceiling_kbps,
                                applied,
                                resent = seen.packets_resent,
                                of = seen.packets_sent,
                                dropped = seen.frames_dropped,
                                "bitrate adjusted to what the link is carrying"
                            );
                        }
                    }

                    let window = last_stats.elapsed();
                    if window >= Duration::from_secs(5) {
                        let (frames, pts_jumps, worst_jump, worst_send) = sender.flow.take();
                        // One line, at `info`, because "the picture is late" is
                        // never diagnosable from the sentence alone and this is
                        // what we end up asking for every time. Read it as:
                        //
                        // - `fps` below the configured rate: nothing downstream
                        //   is at fault, the capture or the encoder is not
                        //   producing frames.
                        // - `pts_jumps`: the gaps are in the source timestamps,
                        //   so the frames were never made.
                        // - `worst_send_ms` high with `pts_jumps` at zero: the
                        //   pacer is holding sends back, which is it working —
                        //   unless the number keeps climbing.
                        // - `dropped`/`window_full` climbing: the receiver is
                        //   behind and our flow control is skipping frames.
                        // - `resent` climbing: the link is losing packets.
                        tracing::info!(
                            stream = label,
                            fps = frames / window.as_secs().max(1),
                            pts_jumps,
                            worst_jump_ms = worst_jump,
                            worst_send_ms = worst_send,
                            dropped = sender.frames_dropped,
                            window_full = sender.dropped_window,
                            unacked = sender.unacked_frames(),
                            nacks = nacks_seen,
                            resent = packets_resent,
                            expired = nacks_expired,
                            "backlog control"
                        );
                        if is_video {
                            let read =
                                |counter: &Option<std::sync::Arc<std::sync::atomic::AtomicU64>>| {
                                    counter.as_ref().map_or(0, |count| {
                                        count.load(std::sync::atomic::Ordering::Relaxed)
                                    })
                                };
                            let (now_captured, now_encoded) = (read(&captured), read(&encoded));
                            let seconds = window.as_secs().max(1);
                            tracing::info!(
                                captured_fps = (now_captured - counted.0) / seconds,
                                encoded_fps = (now_encoded - counted.1) / seconds,
                                sent_fps = frames / seconds,
                                "pipeline throughput"
                            );
                            counted = (now_captured, now_encoded);
                        }
                        last_stats = std::time::Instant::now();
                    }
                }

                if remaining.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) == 1
                    && let Ok(mut slot) = done_tx.lock()
                    && let Some(tx) = slot.take()
                {
                    let _ = tx.send(Ok(()));
                }
            })
            .map_err(|e| NdError::Gst(e.to_string()))?;
        workers.threads.push(handle);
    }

    let outcome = loop {
        tokio::select! {
            result = &mut done_rx => break result.unwrap_or_else(|_| {
                Err(NdError::Gst("Cast workers exited without a completion status".into()))
            }),
            event = futures::StreamExt::next(&mut events) => match event {
                Some(pipeline::PipelineEvent::Error { message, debug: details }) => {
                    tracing::error!(%message, %details, "the mirroring pipeline failed");
                    break Err(NdError::Gst(message));
                }
                Some(pipeline::PipelineEvent::Warning { message }) => {
                    tracing::warn!(%message, "mirroring pipeline warning");
                }
                Some(pipeline::PipelineEvent::Eos) => break Ok(()),
                None => break Err(NdError::Gst("mirroring pipeline event channel closed".into())),
            },
            _ = cancel.changed() => break Ok(()),
        }
    };

    // Joining bounded worker threads must not block the async runtime/UI.
    tokio::task::spawn_blocking(move || drop(workers))
        .await
        .map_err(|e| NdError::Gst(format!("Cast worker shutdown failed: {e}")))?;

    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_mode_asks_the_encoder_for_more_video_than_the_pacer_can_drain() {
        // Reported from use: 2160p30 was fine and 2160p60 fell further and
        // further behind. The table asked 30 Mbit for it and the pacer drains
        // 24 — the surplus had nowhere to go but the queue in front of the
        // socket.
        let pacer_kbps =
            (BURST_BUDGET_BYTES * 8) as u32 * (1000 / BURST_INTERVAL.as_millis() as u32) / 1000;
        for (width, height) in [
            (640, 480),
            (1280, 720),
            (1920, 1080),
            (2560, 1440),
            (3840, 2160),
        ] {
            for fps in [30, 60] {
                let cfg = nd_core::pipeline::StreamConfig {
                    width,
                    height,
                    fps,
                    bitrate_kbps: 0,
                    ..Default::default()
                };
                let asked = crate::mirror::cast_bitrate_kbps(&cfg);
                assert!(
                    asked < pacer_kbps,
                    "{width}x{height}@{fps} asks {asked} kbps against a {pacer_kbps} kbps pacer"
                );
            }
        }
        // And the cap has to be what is doing the holding, not a table that
        // happens to stay small.
        let uncapped = nd_core::pipeline::StreamConfig {
            width: 3840,
            height: 2160,
            fps: 60,
            bitrate_kbps: 0,
            ..Default::default()
        };
        assert!(uncapped.scaled_bitrate_kbps() > MAX_VIDEO_BITRATE_KBPS);
        assert_eq!(
            crate::mirror::cast_bitrate_kbps(&uncapped),
            MAX_VIDEO_BITRATE_KBPS
        );
        // 1440p60 asks 21 Mbit and works; the cap must not start holding it.
        let works = nd_core::pipeline::StreamConfig {
            width: 2560,
            height: 1440,
            fps: 60,
            bitrate_kbps: 0,
            ..Default::default()
        };
        assert_eq!(
            crate::mirror::cast_bitrate_kbps(&works),
            works.scaled_bitrate_kbps()
        );
    }

    #[test]
    fn encoded_pts_is_converted_through_its_segment() {
        gst::init().unwrap();
        let offset = gst::ClockTime::from_seconds(1000 * 3600);
        let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
        segment.set_start(offset);
        let mut buffer = gst::Buffer::new();
        buffer
            .get_mut()
            .unwrap()
            .set_pts(offset + gst::ClockTime::from_mseconds(33));
        let sample = gst::Sample::builder()
            .buffer(&buffer)
            .segment(&segment)
            .build();
        assert_eq!(
            sample_running_time(&sample).unwrap(),
            gst::ClockTime::from_mseconds(33)
        );
    }

    #[test]
    fn missing_pts_is_not_silently_sent_as_time_zero() {
        gst::init().unwrap();
        let buffer = gst::Buffer::new();
        let segment = gst::FormattedSegment::<gst::ClockTime>::new();
        let sample = gst::Sample::builder()
            .buffer(&buffer)
            .segment(&segment)
            .build();
        assert!(sample_running_time(&sample).is_err());
    }

    #[test]
    fn only_negotiation_failures_fall_back_to_http() {
        // The HTTP path costs seconds of delay: it is only worth taking when
        // the device genuinely has no mirroring support.
        assert!(is_unsupported(&NdError::Unsupported(
            "the receiver refused to start app 0F5096E8: NOT_FOUND".into()
        )));
        assert!(is_unsupported(&NdError::Unsupported(
            "the receiver refused the mirroring offer: {}".into()
        )));
        assert!(is_unsupported(&NdError::Unsupported(
            "the receiver accepted no usable stream".into()
        )));

        // A network or pipeline failure mid-session must **not** restart over
        // HTTP: that would trade one problem for another, at twice the delay.
        assert!(!is_unsupported(&NdError::Network(
            "connection dropped".into()
        )));
        assert!(!is_unsupported(&NdError::Gst("the encoder failed".into())));
        assert!(!is_unsupported(&NdError::Protocol(
            "the receiver stopped responding".into()
        )));
        assert!(!is_unsupported(&NdError::Cancelled));
    }

    #[test]
    fn flow_gap_detection_respects_configured_video_fps() {
        for (fps, expected_jumps) in [(24, 0), (30, 0), (60, 1)] {
            let mut flow = FlowWatch::default();
            let interval = video_frame_interval(fps);
            flow.record(0, interval);
            flow.record(50_000_000, interval);
            assert_eq!(flow.pts_jumps, expected_jumps, "{fps} fps");
        }
    }

    #[test]
    fn flow_tolerates_two_video_frames_with_timestamp_rounding() {
        for fps in [24, 30, 60, 120, 144] {
            let mut flow = FlowWatch::default();
            let interval = video_frame_interval(fps);
            for frame in [0, 2, 4, 6, 8] {
                flow.record(frame * 1_000_000_000 / u64::from(fps), interval);
            }
            assert_eq!(flow.pts_jumps, 0, "{fps} fps");
        }
        assert_eq!(video_frame_interval(0), Duration::from_secs(1));
    }

    #[test]
    fn flow_keeps_opus_ten_millisecond_cadence() {
        let mut flow = FlowWatch::default();
        let interval = Duration::from_millis(10);
        for pts in [0, 10_000_000, 30_000_000] {
            flow.record(pts, interval);
        }
        assert_eq!(flow.pts_jumps, 0);
        flow.record(60_000_000, interval);
        assert_eq!(flow.pts_jumps, 1);
        assert_eq!(flow.worst_pts_jump_ms, 30);
    }

    /// Builds a history of `count` frames spaced `step` apart.
    fn history_of(count: usize, step: Duration) -> (History, std::time::Instant) {
        let start = std::time::Instant::now();
        let mut history = History::new();
        for i in 0..count {
            history.push_back((i as u32, start + step * i as u32, vec![vec![0u8; 32]]));
        }
        (history, start + step * count.saturating_sub(1) as u32)
    }

    #[test]
    fn history_keeps_the_bounded_audio_window() {
        // The case that killed the session in the field: Opus audio at ~100
        // frames per second. With a fixed cap of 16 frames only 160 ms were
        // left, and the receiver asked for frames from over a second ago.
        let step = Duration::from_millis(10);
        let (mut history, now) = history_of(300, step);
        prune_history(&mut history, now, i64::MAX);

        let held = history.len() as u32 * step;
        assert!(
            held >= Duration::from_secs(1),
            "only {held:?} of audio in the history"
        );
        // And never 256 entries or more, or two frames share the same 8-bit id
        // and we would resend the wrong frame.
        assert!(history.len() <= RETRANSMIT_HISTORY_MAX);
        // And never 256 or more: two frames would share the same 8-bit id.
        const _: () = assert!(RETRANSMIT_HISTORY_MAX < 256);
    }

    #[test]
    fn a_burst_window_carries_about_one_key_frame_worth() {
        // 24 Mbit/s over 10 ms. Getting the unit conversion wrong here is
        // silent: too large and pacing never engages, too small and every
        // frame is spread over a sleep it did not need.
        // Open Screen writes the ceiling as `24 << 20`, so the megabit here is
        // 1048576 bits and not 1000000 — 3_145_728 bytes a second.
        assert_eq!(BURST_BUDGET_BYTES, 31_457);
        // Which is 26 packets of the size this sender emits: a whole ordinary
        // frame at 1080p30/10 Mbit fits in roughly one window, and only a key
        // frame is spread across several.
        assert_eq!(BURST_BUDGET_BYTES / crate::rtp::MAX_PACKET_SIZE, 26);
    }

    #[test]
    fn burst_budget_resets_by_time_and_never_holds_a_stale_partial_window() {
        let now = std::time::Instant::now();
        let mut budget = BurstBudget {
            started: now,
            bytes: 0,
        };
        assert_eq!(budget.reserve(now, BURST_BUDGET_BYTES), Duration::ZERO);
        assert_eq!(budget.reserve(now, 1), BURST_INTERVAL);
        assert_eq!(budget.reserve(now + BURST_INTERVAL, 1), Duration::ZERO);
        assert_eq!(budget.bytes, 1);
    }

    #[test]
    fn stopped_pacer_never_sends_another_packet() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let pacer = BurstPacer::new(stop);
        assert_eq!(
            pacer.send(&socket, &[1]).unwrap_err().kind(),
            std::io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn audio_never_waits_for_a_window_video_has_emptied() {
        // A clone shares the budget, which is what video's own senders want
        // and what audio must not have.
        let video = BurstPacer::new(Default::default());
        assert!(std::sync::Arc::ptr_eq(&video.clone().budget, &video.budget));
        let audio = BurstPacer::new(Default::default());
        assert!(!std::sync::Arc::ptr_eq(&audio.budget, &video.budget));

        // Why it matters, in the arithmetic that made it a report: a window
        // holds about one average video frame — 31457 bytes against 31250 at
        // 1080p60 and 15 Mbit — so an average frame very nearly empties it and
        // anything above average takes more than one window. Whatever shares
        // the budget waits for a window video has left something in.
        let window_bytes = BURST_BUDGET_BYTES;
        let average_video_frame = 15_000_000 / 8 / 60;
        assert!(
            average_video_frame * 10 > window_bytes * 9,
            "a video frame no longer fills a burst window: \
             {average_video_frame} against {window_bytes}"
        );
        // An Opus packet is three orders of magnitude smaller and arrives once
        // per window. It cannot congest a 24 Mbit/s ceiling.
        let opus_packet = 128_000 / 8 / 100;
        assert!(opus_packet * 50 < window_bytes, "{opus_packet}");
    }

    #[test]
    fn the_history_covers_everything_that_can_still_be_asked_for() {
        // The sender never runs further ahead than the window, so a frame
        // outside the history is a frame the receiver has already given up on.
        // If the history were shorter, requests would expire while the frame
        // was still in flight and every one of them would force a key frame.
        assert_eq!(RETRANSMIT_HISTORY_MAX, MAX_UNACKED_FRAMES as usize);
        // And the window has to stay inside half the 8-bit id range, or two
        // frames in flight share an id.
        const _: () = assert!(MAX_UNACKED_FRAMES < 128);
    }

    #[test]
    fn history_expires_by_age_not_by_count() {
        // Video at 30 fps: 2 s holds ~60 frames, well under the cap. Age is
        // what governs here, not the count.
        let step = Duration::from_millis(33);
        let (mut history, now) = history_of(200, step);
        prune_history(&mut history, now, i64::MAX);

        assert!(history.len() < RETRANSMIT_HISTORY_MAX, "{}", history.len());
        let mais_velho = now.duration_since(history.front().unwrap().1);
        assert!(mais_velho <= RETRANSMIT_HISTORY, "{mais_velho:?}");
    }

    #[test]
    fn unacknowledged_frames_do_not_expire_while_the_window_is_full() {
        let (mut history, now) = history_of(120, Duration::from_millis(100));
        prune_history(&mut history, now, -1);
        assert_eq!(history.len(), 120);
        assert_eq!(history.front().unwrap().0, 0);
    }

    #[test]
    fn rtp_timestamp_conversion_uses_the_stream_time_base() {
        // One second in nanoseconds, in video's 90 kHz time base.
        let pts_ns: u128 = 1_000_000_000;
        let ticks = ((pts_ns * mirror::VIDEO_TIME_BASE as u128) / 1_000_000_000u128) as u32;
        assert_eq!(ticks, 90_000);

        // And in audio's 48 kHz time base.
        let ticks = ((pts_ns * mirror::AUDIO_TIME_BASE as u128) / 1_000_000_000u128) as u32;
        assert_eq!(ticks, 48_000);
    }

    #[test]
    fn timestamp_conversion_survives_long_sessions() {
        // Two different limits, and only one of them is our problem.
        //
        // The RTP counter is 32 bits: at 90 kHz it wraps in ~13.3 h. That
        // belongs to the protocol, and the receiver handles the overflow.
        let wrap_hours = (u32::MAX as f64) / mirror::VIDEO_TIME_BASE as f64 / 3600.0;
        assert!((13.0..14.0).contains(&wrap_hours), "{wrap_hours}");

        // The **intermediate product**, though, is ours: in 64 bits it would
        // overflow at around 57 h of session and the timestamp would come out
        // wrong with no warning. That is why the arithmetic is done in 128
        // bits.
        let overflow_hours = (u64::MAX as f64) / mirror::VIDEO_TIME_BASE as f64 / 1e9 / 3600.0;
        assert!(overflow_hours < 100.0, "{overflow_hours}");

        let a_day_ns: u128 = 24 * 3600 * 1_000_000_000u128;
        let ticks = (a_day_ns * mirror::VIDEO_TIME_BASE as u128) / 1_000_000_000u128;
        assert_eq!(ticks, 24 * 3600 * 90_000, "a conta em 128 bits fica exata");
    }
}

#[cfg(all(test, target_os = "linux"))]
mod lifecycle_tests {
    use super::*;
    fn rx_threads() -> usize {
        std::fs::read_dir("/proc/self/task")
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| {
                std::fs::read_to_string(e.path().join("comm"))
                    .unwrap_or_default()
                    .trim()
                    == "cast-rtcp-rx"
            })
            .count()
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn rtcp_thread_is_joined_on_stop() {
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        let cfg = StreamConfig {
            width: 320,
            height: 240,
            fps: 30,
            encoder: pipeline::H264Encoder::X264,
            audio: pipeline::AudioSource::Silence,
            ..Default::default()
        };
        let (_, offer) = mirror::build_offer(
            &MirrorConfig {
                width: 320,
                height: 240,
                ..Default::default()
            },
            1,
        )
        .unwrap();
        let negotiated = Negotiated {
            receiver_ip: peer.local_addr().unwrap().ip(),
            answer: mirror::Answer {
                udp_port: peer.local_addr().unwrap().port(),
                send_indexes: vec![0, 1],
                ssrcs: vec![100002, 100004],
                video_limits: None,
                audio_limits: None,
            },
            offer,
        };
        let before = rx_threads();
        // The session builds and starts the pipeline before negotiating now,
        // so the test does the same rather than handing `stream` a source.
        let desc = pipeline::mirror_pipeline_description(
            &cfg,
            &pipeline::VideoSource::Test,
            pipeline::CHROMECAST_MAX_RESOLUTION,
            false,
        );
        let (gst_pipeline, events) = pipeline::build_pipeline(&desc, cfg.latency_ms()).unwrap();
        // The session's own guard: a panic or early return must not dispose a
        // running pipeline, whose streaming threads would then use freed state.
        let _guard = pipeline::PipelineGuard::new(gst_pipeline.clone());
        gst_pipeline.set_state(gst::State::Playing).unwrap();
        // Armed only now: a cold plugin registry can make the build above slower
        // than the deadline, and `stream` would then return before starting.
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            tx.send(true).unwrap();
        });
        stream(
            &cfg,
            gst_pipeline,
            events,
            &negotiated,
            &SinkStatus::new(),
            &mut rx,
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let after = rx_threads();
        assert_eq!(after, before);
    }
    #[test]
    fn corrected_sender_reports_are_default_and_can_be_disabled_explicitly() {
        assert!(resolve_rtcp_enabled(None));
        assert!(resolve_rtcp_enabled(Some("1")));
        for value in ["0", "false", "OFF", "no"] {
            assert!(!resolve_rtcp_enabled(Some(value)));
        }
    }
}
