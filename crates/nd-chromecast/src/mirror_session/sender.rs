//! One mirroring track on the wire: encoded frames from an `appsink`,
//! encrypted and packetised into Cast RTP, kept for retransmission, and
//! described by RTCP sender reports.
//!
//! [`super::worker`] drives a [`StreamSender`] on its own thread; the ACK
//! window it consults is filled by [`super::feedback`].

use std::collections::{HashSet, VecDeque};
use std::net::UdpSocket;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;

use nd_core::pipeline::{MIRROR_AUDIO_SINK, MIRROR_VIDEO_SINK};
use nd_core::{NdError, Result};

use super::pacing::{BURST_BUDGET_BYTES, BURST_INTERVAL, BurstPacer};
use crate::flow::{AckWindow, MAX_UNACKED_FRAMES};
use crate::mirror::{self, OfferedStream, StreamKeys};
use crate::rtcp::{Nack, SenderStats, build_sender_report, ntp_timestamp};
use crate::rtp::{Frame, Packetizer, encrypt_frame};

/// How long to wait for a frame before checking for cancellation.
const PULL_TIMEOUT: Duration = Duration::from_millis(20);
/// Packets waiting to be resent, across every NACK of the stream.
const MAX_REPAIR_QUEUE: usize = 4096;
/// The largest encoded frame sent; anything larger ends the session.
const MAX_ENCODED_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// How long acknowledged frames stay available for retransmission.
///
/// The receiver asks for what it lost and waits for it; a frame gone from the
/// history freezes the picture until the next key frame, and stalls audio
/// outright. The bound is time, not a frame count: Opus runs at 100 frames a
/// second, so a count that holds half a second of video holds a fraction of
/// that of audio.
const RETRANSMIT_HISTORY: Duration = Duration::from_secs(2);
/// The frame cap on the history.
///
/// A retransmission request names the frame in 8 bits, so the history has to
/// stay inside half that range: past it two live frames share an id, and
/// resending the wrong one makes the receiver assemble a corrupted frame.
/// Nothing older than [`MAX_UNACKED_FRAMES`] can be asked for: the sender
/// never runs further ahead than that.
const RETRANSMIT_HISTORY_MAX: usize = MAX_UNACKED_FRAMES as usize;
/// The receiver silence that ends the session.
///
/// It talks constantly (reports and retransmission requests); when it stops,
/// the session ended on that end, and streaming on would send to a closed
/// port while the interface showed a working session.
pub(super) const RECEIVER_SILENCE_TIMEOUT: Duration = Duration::from_secs(8);
/// The interval between sender reports.
const RTCP_INTERVAL: Duration = Duration::from_millis(500);

/// Sender reports are on unless `BIGNETSCREEN_CAST_RTCP` turns them off for
/// diagnosis: a long-lived stream needs its A/V clock mapping.
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

/// The encoded timestamp in the pipeline's running-time domain.
///
/// x264 and NVENC offset PTS by 1000 hours to allow negative DTS; sending that
/// raw value beside a running-time sender report desynchronises video and
/// audio.
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

/// A running time in a stream's RTP time base. The product is computed in
/// 128 bits: in 64 it overflows after about 57 hours at 90 kHz. The result
/// wraps at 32 bits as RTP timestamps do.
fn rtp_ticks(running_ns: u64, time_base: u32) -> u32 {
    (u128::from(running_ns) * u128::from(time_base) / 1_000_000_000) as u32
}

/// A frame kept for retransmission: full id, send instant and encrypted
/// packets. Only the lookup of an incoming NACK uses the wire id's low byte.
type HistoryEntry = (u32, Instant, Vec<Vec<u8>>);
/// A stream's window of recent frames.
type History = VecDeque<HistoryEntry>;

fn video_frame_interval(fps: u32) -> Duration {
    // Round up so timestamp rounding cannot turn two frames into a false gap.
    Duration::from_nanos(1_000_000_000u64.div_ceil(u64::from(fps.max(1))))
}

/// Streams one track (video or audio) from its `appsink` to the receiver.
///
/// It runs on a thread of its own ([`super::worker`]): `appsink` is a blocking
/// API, and Opus frames every 10 ms cannot wait on the video cadence.
pub(super) struct StreamSender {
    sink: gst_app::AppSink,
    packetizer: Packetizer,
    keys: StreamKeys,
    /// The socket shared by every stream in the session.
    ///
    /// The receiver learns the sender's address from the first packet and ties
    /// the session to that IP:port pair. One socket per stream would make
    /// video and audio arrive from different source ports, and the receiver
    /// would treat half the traffic as belonging to another session.
    socket: Arc<UdpSocket>,
    /// Keeps this stream's packets inside the burst ceiling.
    pacer: BurstPacer,
    repair_queue: VecDeque<(u32, usize)>,
    queued_repairs: HashSet<(u32, usize)>,
    time_base: u32,
    expected_frame_interval: Duration,
    pub(super) frame_id: u32,
    label: &'static str,
    /// Recent packets, per frame, for serving retransmission requests.
    ///
    /// Full frame ids are retained. Age expiration applies only after ACK;
    /// unacknowledged frames remain available throughout the bounded window.
    history: History,
    /// Expanded monotonic receiver checkpoint and the bounded sent window.
    pub(super) checkpoint: Arc<Mutex<AckWindow>>,
    /// A key frame this sender needs to resync. Only this track's thread may
    /// take it ([`Self::take_key_frame_request`]) and force the IDR.
    key_frame_request: bool,
    /// Waiting for that IDR: until it arrives every P-frame references frames
    /// that were dropped and never reached the receiver.
    resyncing: bool,
    resync_requested: bool,
    /// Frames not sent: refused by a full ACK window, or video waiting for a
    /// key frame after such a refusal.
    pub(super) frames_dropped: u64,
    /// The part of `frames_dropped` refused by a full ACK window.
    pub(super) dropped_window: u64,
    pub(super) stats: SenderStats,
    /// Outgoing-flow diagnostics: discontinuity in the frames' timestamps (a
    /// hole born before the network) and the largest wall-clock gap between
    /// two sends (a late sender thread).
    pub(super) flow: FlowWatch,
    pub(super) clock_origin: SystemTime,
    pipeline: gst::Pipeline,
    last_report: Option<Instant>,
    has_media: bool,
    rtcp_enabled: bool,
    last_probe: Instant,
}

impl StreamSender {
    /// The `appsink` and label follow from the track itself, so a video
    /// frame can never go out under the audio SSRC.
    pub(super) fn new(
        pipeline: &gst::Pipeline,
        stream: &OfferedStream,
        socket: Arc<UdpSocket>,
        fps: u32,
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
                video_frame_interval(fps)
            } else {
                Duration::from_millis(10)
            },
            frame_id: 0,
            label,
            history: VecDeque::new(),
            checkpoint: Arc::default(),
            key_frame_request: false,
            resyncing: stream.is_video,
            resync_requested: false,
            frames_dropped: 0,
            dropped_window: 0,
            stats: SenderStats::default(),
            flow: FlowWatch::default(),
            clock_origin: SystemTime::UNIX_EPOCH,
            pipeline: pipeline.clone(),
            last_report: None,
            has_media: false,
            rtcp_enabled: rtcp_enabled(),
            last_probe: Instant::now(),
        })
    }

    /// Pulls one frame and sends it. `Ok(false)` when the stream has ended.
    pub(super) fn pump(&mut self) -> Result<bool> {
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

        let has_room = {
            let window = self
                .checkpoint
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if window.pending() != 0 && window.last_progress.elapsed() > RECEIVER_SILENCE_TIMEOUT {
                return Err(NdError::Protocol(format!(
                    "the receiver stopped acknowledging the {} track",
                    self.label
                )));
            }
            window.can_send()
        };

        // A key frame depends on nothing; the rest reference the previous
        // one. That is how the receiver knows where it can start decoding.
        let is_key = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);

        // Before encrypting and packetising: a frame the receiver has no room
        // for costs the same CPU and network as one it can use. "No room" is
        // the eight-bit frame window alone; a tighter age or media-time budget
        // drops audio whenever an ACK is slow to cross a wireless network.
        if self.skip_while_receiver_catches_up(is_key, has_room) {
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
        self.send_frame(&map, is_key, pts)
    }

    /// Encrypts, packetises and sends one frame under the next id, and keeps
    /// it for retransmission. `Ok(false)` when the session stopped mid-frame.
    fn send_frame(&mut self, frame: &[u8], is_key: bool, pts: gst::ClockTime) -> Result<bool> {
        let reference = (!is_key && self.frame_id > 0).then(|| self.frame_id - 1);

        let rtp_timestamp = rtp_ticks(pts.nseconds(), self.time_base);

        let encrypted = encrypt_frame(&self.keys.key, &self.keys.iv_mask, self.frame_id, frame);
        let packets = self.packetizer.packetize(&Frame {
            frame_id: self.frame_id,
            reference_frame_id: reference,
            rtp_timestamp,
            payload: &encrypted,
        });

        // The acknowledgement read here prunes the history below; one read
        // earlier than the send only keeps a little more for repairs.
        let acknowledged = {
            let mut window = self
                .checkpoint
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            window.sent(self.frame_id);
            window.checkpoint()
        };
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
        let now = Instant::now();
        self.history.push_back((self.frame_id, now, packets));
        prune_history(&mut self.history, now, acknowledged);

        self.flow
            .record(pts.nseconds(), self.expected_frame_interval);

        self.send_report_if_due();

        tracing::trace!(
            stream = self.label,
            frame = self.frame_id,
            key = is_key,
            bytes = frame.len(),
            "frame sent"
        );

        self.frame_id = self.frame_id.checked_add(1).ok_or_else(|| {
            NdError::Protocol(
                "Cast frame counter exhausted; reconnect to rotate encryption keys".into(),
            )
        })?;
        Ok(true)
    }

    /// Whether to drop this frame without allocating it an id.
    ///
    /// Both tracks keep inside the eight-bit wire window, startup included:
    /// a low audio bitrate does not exempt it. After video drops a frame only
    /// an IDR restarts the dependency chain, so it is requested once there is
    /// room to send it, and every frame until it arrives is dropped too.
    fn skip_while_receiver_catches_up(&mut self, is_key: bool, has_room: bool) -> bool {
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
                    self.key_frame_request = true;
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

    /// Whether a key frame is needed to resync, clearing the request.
    pub(super) fn take_key_frame_request(&mut self) -> bool {
        std::mem::take(&mut self.key_frame_request)
    }

    pub(super) fn unacked_frames(&self) -> u32 {
        self.checkpoint
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pending()
    }

    pub(super) fn ssrc(&self) -> u32 {
        self.packetizer.ssrc()
    }

    pub(super) fn label(&self) -> &str {
        self.label
    }

    pub(super) fn is_video(&self) -> bool {
        self.time_base == mirror::VIDEO_TIME_BASE
    }

    /// Queues the packets the receiver declared lost and resends one burst of
    /// the queue. Without retransmission one lost datagram leaves a frame
    /// incomplete and the receiver waits for it forever.
    pub(super) fn retransmit(&mut self, nacks: &[Nack]) -> Retransmission {
        let acknowledged = self
            .checkpoint
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .checkpoint();
        let mut expired = 0;
        // Deduplicate whole-frame and bitmap requests. The work queue keeps
        // full frame ids, so delayed work cannot alias a newer eight-bit id.
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
        let started = Instant::now();
        let mut bytes = 0;
        let mut sent = 0;
        // One burst of repair work per turn; retain the rest for the next turn
        // instead of restarting a large whole-frame NACK at packet zero.
        while bytes + crate::rtp::MAX_PACKET_SIZE <= BURST_BUDGET_BYTES
            && started.elapsed() < BURST_INTERVAL
            && !self.pacer.stop.load(Ordering::Acquire)
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

    /// Resends the newest packet when the receiver has acknowledged nothing
    /// for an RTCP interval, so a lost tail packet still draws feedback.
    pub(super) fn probe_stalled_receiver(&mut self) {
        if self.last_probe.elapsed() < RTCP_INTERVAL {
            return;
        }
        let needs_probe = {
            let window = self
                .checkpoint
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            window.pending() != 0 && window.last_progress.elapsed() >= RTCP_INTERVAL
        };
        if needs_probe {
            self.last_probe = Instant::now();
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
    pub(super) fn send_report_if_due(&mut self) {
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
        // NTP and RTP describe the same instant: NTP is anchored once to the
        // pipeline clock, so a wall-clock step cannot move it against RTP.
        let Some(now) = self
            .clock_origin
            .checked_add(Duration::from_nanos(running.nseconds()))
        else {
            return;
        };
        self.last_report = Some(Instant::now());
        let timestamp = rtp_ticks(running.nseconds(), self.time_base);
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
/// ("the sound stutters"): either the hole comes from the pipeline, and the
/// frames' timestamps jump, or the sender thread was late.
#[derive(Debug, Default)]
pub(super) struct FlowWatch {
    /// The previous frame's timestamp (ns).
    last_pts_ns: Option<u64>,
    /// The previous send's instant.
    last_send: Option<Instant>,
    /// Frames sent since the last report.
    frames: u64,
    /// Timestamp jumps larger than twice the expected interval.
    pts_jumps: u64,
    /// The largest jump observed, in ms.
    worst_pts_jump_ms: u64,
    /// The largest wall-clock gap between two sends, in ms.
    ///
    /// Expect this above the frame interval: [`BurstPacer`] spreads a key
    /// frame over several windows, holding back the send that follows it. A
    /// high number with `pts_jumps = 0` is pacing at work, not a late thread.
    worst_send_gap_ms: u64,
}

impl FlowWatch {
    /// Records a frame. `expected` is the nominal interval between frames.
    fn record(&mut self, pts_ns: u64, expected: Duration) {
        let now = Instant::now();
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
    pub(super) fn take(&mut self) -> (u64, u64, u64, u64) {
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

/// Keeps in the history only what the receiver may still ask for: drops
/// acknowledged frames older than [`RETRANSMIT_HISTORY`], and anything past
/// [`RETRANSMIT_HISTORY_MAX`]. Unacknowledged frames stay regardless of age.
fn prune_history(history: &mut History, now: Instant, acknowledged: i64) {
    while history.front().is_some_and(|(id, sent, _)| {
        i64::from(*id) <= acknowledged && now.duration_since(*sent) > RETRANSMIT_HISTORY
    }) || history.len() > RETRANSMIT_HISTORY_MAX
    {
        history.pop_front();
    }
}

/// The outcome of one round of retransmissions.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Retransmission {
    /// Packets actually resent.
    pub(super) sent: usize,
    /// Requests for frames already gone from the history.
    pub(super) expired: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pts_converted_through_segment() {
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
    fn missing_pts_is_an_error() {
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
    fn flow_gap_follows_video_fps() {
        for (fps, expected_jumps) in [(24, 0), (30, 0), (60, 1)] {
            let mut flow = FlowWatch::default();
            let interval = video_frame_interval(fps);
            flow.record(0, interval);
            flow.record(50_000_000, interval);
            assert_eq!(flow.pts_jumps, expected_jumps, "{fps} fps");
        }
    }

    #[test]
    fn flow_tolerates_timestamp_rounding() {
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

    #[test]
    fn history_keeps_the_bounded_audio_window() {
        // Opus at 100 frames a second: the receiver asks for frames from over
        // a second ago, which a small frame cap would already have dropped.
        let step = Duration::from_millis(10);
        let (mut history, now) = history_of(300, step);
        prune_history(&mut history, now, i64::MAX);

        let held = history.len() as u32 * step;
        assert!(
            held >= Duration::from_secs(1),
            "only {held:?} of audio in the history"
        );
        // And never 256 entries or more, or two frames share the same 8-bit id
        // and the wrong one is resent.
        assert!(history.len() <= RETRANSMIT_HISTORY_MAX);
        const _: () = assert!(RETRANSMIT_HISTORY_MAX < 256);
    }

    #[test]
    fn history_covers_repairable_frames() {
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
    fn history_expires_by_age() {
        // Video at 30 fps: 2 s holds ~60 frames, well under the cap. Age is
        // what governs here, not the count.
        let step = Duration::from_millis(33);
        let (mut history, now) = history_of(200, step);
        prune_history(&mut history, now, i64::MAX);

        assert!(history.len() < RETRANSMIT_HISTORY_MAX, "{}", history.len());
        let oldest = now.duration_since(history.front().unwrap().1);
        assert!(oldest <= RETRANSMIT_HISTORY, "{oldest:?}");
    }

    #[test]
    fn full_window_keeps_unacked_frames() {
        let (mut history, now) = history_of(120, Duration::from_millis(100));
        prune_history(&mut history, now, -1);
        assert_eq!(history.len(), 120);
        assert_eq!(history.front().unwrap().0, 0);
    }

    #[test]
    fn rtp_ticks_follow_stream_time_base() {
        let second = 1_000_000_000;
        assert_eq!(rtp_ticks(second, mirror::VIDEO_TIME_BASE), 90_000);
        assert_eq!(rtp_ticks(second, mirror::AUDIO_TIME_BASE), 48_000);
        // Past 57 hours the product overflows 64 bits; the result still only
        // wraps at 32 bits, as an RTP timestamp does.
        let three_days = 72 * 3600 * second;
        let exact = 72 * 3600 * 90_000_u64;
        assert_eq!(
            rtp_ticks(three_days, mirror::VIDEO_TIME_BASE),
            (exact % (1 << 32)) as u32
        );
    }

    #[test]
    fn corrected_sender_reports_default_on() {
        assert!(resolve_rtcp_enabled(None));
        assert!(resolve_rtcp_enabled(Some("1")));
        for value in ["0", "false", "OFF", "no"] {
            assert!(!resolve_rtcp_enabled(Some(value)));
        }
    }

    /// A video sender fed through an `appsrc`, sending to a local socket.
    fn video_sender() -> (StreamSender, gst_app::AppSrc, UdpSocket, gst::Pipeline) {
        gst::init().unwrap();
        let pipeline = gst::parse::launch(&format!(
            "appsrc name=frames format=time ! appsink name={MIRROR_VIDEO_SINK} sync=false"
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let frames = pipeline
            .by_name("frames")
            .unwrap()
            .downcast::<gst_app::AppSrc>()
            .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.connect(peer.local_addr().unwrap()).unwrap();
        let (_, offer) = mirror::build_offer(&mirror::MirrorConfig::default(), 1).unwrap();
        let stream = offer.streams.iter().find(|s| s.is_video).unwrap();
        let pacer = BurstPacer::new(Arc::default());
        let sender = StreamSender::new(&pipeline, stream, Arc::new(socket), 30, pacer).unwrap();
        (sender, frames, peer, pipeline)
    }

    /// Pumps until the sender has taken the frame just pushed, sent or dropped.
    fn pump_frame(sender: &mut StreamSender) {
        let before = (sender.frame_id, sender.frames_dropped);
        for _ in 0..100 {
            assert!(sender.pump().unwrap());
            if (sender.frame_id, sender.frames_dropped) != before {
                return;
            }
        }
        panic!("the pushed frame never reached the sender");
    }

    fn push(frames: &gst_app::AppSrc, index: u64, key: bool) {
        let mut buffer = gst::Buffer::from_slice(vec![0u8; 64]);
        let buffer_mut = buffer.get_mut().unwrap();
        buffer_mut.set_pts(gst::ClockTime::from_mseconds(33 * index));
        if !key {
            buffer_mut.set_flags(gst::BufferFlags::DELTA_UNIT);
        }
        frames.push_buffer(buffer).unwrap();
    }

    #[test]
    fn dropped_video_resumes_at_key_frame() {
        let (mut sender, frames, peer, pipeline) = video_sender();
        let window = MAX_UNACKED_FRAMES;
        for index in 0..u64::from(window) {
            push(&frames, index, index == 0);
            pump_frame(&mut sender);
        }
        assert_eq!(sender.frame_id, window);

        // No room: the frame is dropped without taking an id.
        push(&frames, 200, false);
        pump_frame(&mut sender);
        assert_eq!((sender.frame_id, sender.dropped_window), (window, 1));

        // Room again, but interframes reference what was dropped: they wait
        // for a key frame, which is asked for once.
        sender
            .checkpoint
            .lock()
            .unwrap()
            .acknowledge((window - 1) as u8, None);
        push(&frames, 201, false);
        pump_frame(&mut sender);
        assert_eq!(sender.frame_id, window);
        assert!(sender.take_key_frame_request());
        push(&frames, 202, false);
        pump_frame(&mut sender);
        assert!(!sender.take_key_frame_request());
        assert_eq!(sender.frames_dropped, 3);

        // The key frame takes the next id and references nothing.
        peer.set_nonblocking(true).unwrap();
        let mut packet = [0u8; 1500];
        while peer.recv(&mut packet).is_ok() {}
        push(&frames, 203, true);
        pump_frame(&mut sender);
        assert_eq!(sender.frame_id, window + 1);
        let mut last = None;
        while let Ok(size) = peer.recv(&mut packet) {
            // Sender reports share the socket; RTP carries payload type 101.
            if packet[1] & 0x7f == mirror::RTP_PAYLOAD_H264 {
                last = Some(packet[..size].to_vec());
            }
        }
        let rtp = last.expect("the key frame was sent");
        assert_eq!(
            rtp[12] & 0b1100_0000,
            0b1000_0000,
            "key frame, no reference"
        );
        assert_eq!(rtp[13], window as u8);
        pipeline.set_state(gst::State::Null).unwrap();
    }

    /// Builds a history of `count` frames spaced `step` apart.
    fn history_of(count: usize, step: Duration) -> (History, Instant) {
        let start = Instant::now();
        let mut history = History::new();
        for i in 0..count {
            history.push_back((i as u32, start + step * i as u32, vec![vec![0u8; 32]]));
        }
        (history, start + step * count.saturating_sub(1) as u32)
    }
}
