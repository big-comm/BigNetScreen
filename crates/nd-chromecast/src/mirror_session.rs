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

use nd_core::pipeline::{self, StreamConfig, MIRROR_AUDIO_SINK, MIRROR_VIDEO_SINK};
use nd_core::sink::{SinkState, SinkStatus};
use nd_core::{NdError, Result};

use crate::cast::CastChannel;
use crate::mirror::{self, MirrorConfig, Negotiated, OfferedStream, MIRRORING_APP_ID};
use crate::rtcp::{
    build_sender_report, classify_receiver_packet, ntp_timestamp, parse_cast_feedback, Nack,
    ReceiverPacket, SenderStats,
};
use crate::rtp::{encrypt_frame, Frame, Packetizer};

/// How long to wait for a frame before checking for cancellation.
const PULL_TIMEOUT: Duration = Duration::from_millis(250);
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
/// A retransmission request identifies the frame in **8 bits**. Past 256
/// entries there would be two frames with the same id and we would resend the
/// wrong one, which is worse than not resending at all: the receiver would
/// assemble a corrupted frame.
const RETRANSMIT_HISTORY_MAX: usize = 200;
/// The receiver silence that ends the session.
///
/// It talks constantly (reports and retransmission requests). When it stops,
/// the session died on that end — and without this limit we kept streaming to
/// a closed port while the interface claimed everything was fine.
const RECEIVER_SILENCE_TIMEOUT: Duration = Duration::from_secs(8);
/// The interval between *sender reports*.
const RTCP_INTERVAL: Duration = Duration::from_millis(500);

/// The *sender report* is **off by default** — and that is known debt, not a
/// design decision.
///
/// Measured in the field against a real Chromecast, varying one factor at a
/// time:
///
/// | RTCP | Socket | Result |
/// | --- | --- | --- |
/// | off | one per stream | picture appears with low lag, then **freezes** |
/// | on | one per stream | the receiver closes the mirroring app |
/// | on | shared | the receiver closes the mirroring app |
///
/// In other words: the freeze comes from the missing report (without it the
/// receiver loses the mapping between the RTP timestamp and the clock and
/// stops scheduling frames), but the packet we emit today is **rejected** —
/// plain RFC 3550 is not enough, Cast uses compound packets with extended
/// reports of its own.
///
/// Sending something that kills the session is worse than sending nothing, so
/// it stays off until the format is right. `BIGNETSCREEN_CAST_RTCP=1` turns it
/// on to carry the investigation forward.
fn rtcp_enabled() -> bool {
    std::env::var("BIGNETSCREEN_CAST_RTCP").is_ok()
}

/// A frame kept for retransmission: the id truncated to 8 bits (which is what
/// the request carries), the send instant, and the packets ready to resend.
type HistoryEntry = (u8, std::time::Instant, Vec<Vec<u8>>);
/// A stream's window of recent frames.
type History = std::collections::VecDeque<HistoryEntry>;

/// Streams one track (video or audio) from the `appsink` to the receiver.
///
/// It runs on a thread of its own: `appsink` is a blocking API and the hot
/// path must not compete for room with the async runtime.
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
    target: SocketAddr,
    time_base: u32,
    frame_id: u32,
    label: &'static str,
    /// Recent packets, per frame, for serving retransmission requests.
    ///
    /// The key is the id truncated to 8 bits, which is what the request
    /// carries; the instant is used to expire by age (see
    /// [`RETRANSMIT_HISTORY`]).
    history: History,
    stats: SenderStats,
    /// Outgoing-flow diagnostics: discontinuity in the frames' timestamps (a
    /// hole born before the network) and the largest wall-clock gap between
    /// two sends (a late sender thread).
    flow: FlowWatch,
    last_rtp_timestamp: u32,
    last_report: std::time::Instant,
    rtcp_enabled: bool,
}

impl StreamSender {
    fn new(
        pipeline: &gst::Pipeline,
        element_name: &str,
        stream: &OfferedStream,
        socket: std::sync::Arc<UdpSocket>,
        target: SocketAddr,
        time_base: u32,
        label: &'static str,
    ) -> Result<Self> {
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
            target,
            time_base,
            frame_id: 0,
            label,
            history: std::collections::VecDeque::new(),
            stats: SenderStats::default(),
            flow: FlowWatch::default(),
            last_rtp_timestamp: 0,
            last_report: std::time::Instant::now(),
            rtcp_enabled: rtcp_enabled(),
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

        let Some(buffer) = sample.buffer() else {
            return Ok(true);
        };
        let Ok(map) = buffer.map_readable() else {
            return Ok(true);
        };

        // A key frame depends on nothing; the rest reference the previous
        // one. That is how the receiver knows where it can start decoding.
        let is_key = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
        let reference = (!is_key && self.frame_id > 0).then(|| self.frame_id - 1);

        // The timestamp goes in the stream's negotiated time base (90 kHz for video).
        let pts = buffer.pts().unwrap_or(gst::ClockTime::ZERO);
        let rtp_timestamp =
            ((pts.nseconds() as u128 * self.time_base as u128) / 1_000_000_000u128) as u32;

        let encrypted = encrypt_frame(&self.keys.key, &self.keys.iv_mask, self.frame_id, &map);
        let packets = self.packetizer.packetize(&Frame {
            frame_id: self.frame_id,
            reference_frame_id: reference,
            rtp_timestamp,
            payload: &encrypted,
        });

        for packet in packets.iter() {
            if let Err(err) = self.socket.send(packet) {
                // One lost datagram does not kill the session; the receiver
                // asks for whatever is missing to be resent.
                tracing::debug!(stream = self.label, %err, "failed to send a packet");
            } else {
                self.stats.record(packet.len());
            }
        }
        // Keep it for a possible retransmission before moving on.
        let now = std::time::Instant::now();
        self.history
            .push_back(((self.frame_id & 0xFF) as u8, now, packets));
        prune_history(&mut self.history, now);

        // A 10 ms Opus frame for audio; at 30 fps video expects ~33 ms.
        let esperado = if self.is_video() {
            Duration::from_millis(33)
        } else {
            Duration::from_millis(10)
        };
        self.flow.record(pts.nseconds(), esperado);

        self.last_rtp_timestamp = rtp_timestamp;
        self.send_report_if_due();

        tracing::trace!(
            stream = self.label,
            frame = self.frame_id,
            key = is_key,
            bytes = map.len(),
            "frame sent"
        );

        self.frame_id = self.frame_id.wrapping_add(1);
        Ok(true)
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
        let mut sent = 0;
        let mut expired = 0;
        for nack in nacks {
            let Some((_, _, packets)) = self.history.iter().find(|(id, _, _)| *id == nack.frame_id)
            else {
                // Already out of the history. The receiver will wait for it
                // forever, so the caller needs to know: for video we can start
                // over with a key frame.
                expired += 1;
                continue;
            };
            match nack.packet_ids() {
                // A request for the whole frame.
                None => {
                    for packet in packets {
                        if self.socket.send(packet).is_ok() {
                            sent += 1;
                        }
                    }
                }
                Some(ids) => {
                    for id in ids {
                        if let Some(packet) = packets.get(id as usize) {
                            if self.socket.send(packet).is_ok() {
                                sent += 1;
                            }
                        }
                    }
                }
            }
        }
        Retransmission { sent, expired }
    }

    /// Off by default — see [`rtcp_enabled`].
    fn send_report_if_due(&mut self) {
        if !self.rtcp_enabled || self.last_report.elapsed() < RTCP_INTERVAL {
            return;
        }
        self.last_report = std::time::Instant::now();
        let report = build_sender_report(
            self.packetizer.ssrc(),
            ntp_timestamp(std::time::SystemTime::now()),
            self.last_rtp_timestamp,
            self.stats,
        );
        if let Err(err) = self.socket.send(&report) {
            tracing::debug!(stream = self.label, %err, "falha ao enviar sender report");
        }
    }

    #[allow(dead_code)]
    fn target(&self) -> SocketAddr {
        self.target
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
    /// Maior salto observado, em ms.
    worst_pts_jump_ms: u64,
    /// The largest wall-clock gap between two sends, in ms.
    worst_send_gap_ms: u64,
}

impl FlowWatch {
    /// Records a frame. `expected` is the nominal interval between frames.
    fn record(&mut self, pts_ns: u64, expected: Duration) {
        let now = std::time::Instant::now();
        if let Some(prev) = self.last_pts_ns {
            let delta = pts_ns.saturating_sub(prev);
            let limite = expected.as_nanos() as u64 * 2;
            if delta > limite {
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
fn prune_history(history: &mut History, now: std::time::Instant) {
    while history
        .front()
        .is_some_and(|(_, sent, _)| now.duration_since(*sent) > RETRANSMIT_HISTORY)
        || history.len() > RETRANSMIT_HISTORY_MAX
    {
        history.pop_front();
    }
}

/// The outcome of one round of retransmissions.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Retransmission {
    /// Pacotes efetivamente reenviados.
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

    // 1. Canal de controle e app de espelhamento.
    let channel = CastChannel::connect(receiver_ip).await?;
    let app = channel.launch(MIRRORING_APP_ID).await?;

    // 2. Negotiation: the receiver returns the UDP port and accepts (or not)
    //    each offered stream.
    status.set(SinkState::WaitSocket);
    let (width, height) = StreamConfig::fit_within(size, pipeline::CHROMECAST_MAX_RESOLUTION);
    let driver = crate::session::detect_gpu_driver();
    let encoder = *pipeline::encoder_candidates(driver)
        .first()
        .ok_or_else(|| NdError::Unsupported("no H.264 encoder available".into()))?;

    let cfg = StreamConfig {
        width,
        height,
        encoder,
        audio: pipeline::AudioSource::detect(),
        ..Default::default()
    };
    let mirror_cfg = MirrorConfig {
        width,
        height,
        fps: cfg.fps,
        max_bitrate: cfg.scaled_bitrate_kbps() * 1000,
        with_audio: true,
        ..Default::default()
    };

    let session = mirror::negotiate(&channel, &app, receiver_ip, &mirror_cfg).await?;
    let result = stream(&cfg, &video, &session, status, &mut cancel).await;

    let _ = channel.stop_app(&app).await;
    result
}

async fn stream(
    cfg: &StreamConfig,
    video: &pipeline::VideoSource,
    session: &Negotiated,
    status: &SinkStatus,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    status.set(SinkState::WaitStreaming);

    let desc = pipeline::mirror_pipeline_description(cfg, video);
    let (gst_pipeline, mut events) = pipeline::build_pipeline(&desc, cfg.latency_ms())?;

    let (ip, port) = session.target();
    let target = SocketAddr::new(ip, port);

    let mut senders = Vec::new();
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
    tracing::debug!(
        origem = ?socket.local_addr().ok(),
        destino = %target,
        "mirroring session socket"
    );

    if let Some(stream) = session.video() {
        senders.push(StreamSender::new(
            &gst_pipeline,
            MIRROR_VIDEO_SINK,
            stream,
            socket.clone(),
            target,
            mirror::VIDEO_TIME_BASE,
            "video",
        )?);
    }
    if let Some(stream) = session.audio() {
        match StreamSender::new(
            &gst_pipeline,
            MIRROR_AUDIO_SINK,
            stream,
            socket.clone(),
            target,
            mirror::AUDIO_TIME_BASE,
            "audio",
        ) {
            Ok(sender) => senders.push(sender),
            // The session goes on without audio: video is what matters here,
            // and the receiver accepted the streams individually.
            Err(err) => tracing::warn!(%err, "no audio track in this session"),
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
    let mut nack_inboxes: std::collections::HashMap<u32, std::sync::mpsc::Sender<Vec<Nack>>> =
        Default::default();
    let mut nack_receivers = Vec::new();
    for sender in &senders {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<Nack>>();
        nack_inboxes.insert(sender.ssrc(), tx);
        nack_receivers.push(rx);
    }

    let listener = socket.clone();
    let want_key_frame = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let key_flag = want_key_frame.clone();
    // How many packets the receiver has sent us. The sender threads use the
    // counter as a sign of life: if it stops rising, the session died on that
    // end (see `RECEIVER_SILENCE_TIMEOUT`).
    let receiver_alive = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let alive_counter = receiver_alive.clone();
    std::thread::Builder::new()
        .name("cast-rtcp-rx".into())
        .spawn(move || {
            let _ = listener.set_read_timeout(Some(Duration::from_millis(500)));
            let mut buf = [0u8; 2048];
            loop {
                let Ok(size) = listener.recv(&mut buf) else {
                    continue;
                };
                if classify_receiver_packet(&buf[..size]).is_none() {
                    continue;
                }
                alive_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // Cast packets are compound: every block has to be walked, or
                // the feedback stays invisible.
                for kind in crate::rtcp::parse_compound(&buf[..size]) {
                    if kind == ReceiverPacket::PictureLossIndication {
                        key_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                if let Some(feedback) = parse_cast_feedback(&buf[..size]) {
                    if feedback.nacks.is_empty() {
                        continue;
                    }
                    if let Some(inbox) = nack_inboxes.get(&feedback.sender_ssrc) {
                        if inbox.send(feedback.nacks).is_err() {
                            break;
                        }
                    }
                }
            }
        })
        .map_err(|e| NdError::Gst(e.to_string()))?;

    tracing::info!(
        destino = %target,
        streams = senders.len(),
        "iniciando espelhamento Cast"
    );

    gst_pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| NdError::Gst(e.to_string()))?;
    status.set(SinkState::Streaming);

    // **One thread per stream.** Alternating between video and audio on a
    // single thread tied the audio to the video's cadence: with 10 ms frames,
    // audio has to be drained ~100 times per second, and at 30 fps it was
    // drained 30 — the result was constant overflow and choppy sound.
    let video_element = gst_pipeline.by_name(MIRROR_VIDEO_SINK);
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
    let done_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(done_tx)));
    let mut threads = Vec::new();

    for (sender, nack_rx) in senders.into_iter().zip(nack_receivers) {
        let label = sender.label();
        let is_video = sender.is_video();
        let key_flag = want_key_frame.clone();
        let video_element = video_element.clone();
        let done_tx = done_tx.clone();
        let receiver_alive = receiver_alive.clone();

        let handle = std::thread::Builder::new()
            .name(format!("cast-{label}"))
            .spawn(move || {
                let mut sender = sender;
                let mut nacks_seen = 0u64;
                let mut packets_resent = 0u64;
                let mut nacks_expired = 0u64;
                let mut last_stats = std::time::Instant::now();
                // State of the receiver's sign of life.
                let mut last_seen = receiver_alive.load(std::sync::atomic::Ordering::Relaxed);
                let mut last_seen_at = std::time::Instant::now();

                loop {
                    // Serve this stream's retransmissions.
                    let mut pending: Vec<Nack> = Vec::new();
                    while let Ok(nacks) = nack_rx.try_recv() {
                        pending.extend(nacks);
                        if pending.len() > 256 {
                            break;
                        }
                    }
                    if !pending.is_empty() {
                        nacks_seen += pending.len() as u64;
                        let outcome = sender.retransmit(&pending);
                        packets_resent += outcome.sent as u64;
                        nacks_expired += outcome.expired as u64;
                        // A request that can no longer be served: the receiver
                        // would wait for a frame that no longer exists. For
                        // video we can start over with a key frame — that is
                        // what unfreezes the picture.
                        if outcome.expired > 0 && is_video {
                            key_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    }

                    // Has the receiver gone quiet? Then the session ended on
                    // that end, and carrying on only wastes network while the
                    // interface lies that all is well.
                    let seen = receiver_alive.load(std::sync::atomic::Ordering::Relaxed);
                    if seen != last_seen {
                        last_seen = seen;
                        last_seen_at = std::time::Instant::now();
                    } else if seen > 0 && last_seen_at.elapsed() > RECEIVER_SILENCE_TIMEOUT {
                        if let Ok(mut slot) = done_tx.lock() {
                            if let Some(tx) = slot.take() {
                                let _ = tx.send(Err(NdError::Protocol(
                                    "the receiver stopped responding".into(),
                                )));
                            }
                        }
                        return;
                    }

                    // A key-frame request only makes sense for video.
                    if is_video && key_flag.swap(false, std::sync::atomic::Ordering::Relaxed) {
                        if let Some(element) = &video_element {
                            tracing::info!("the receiver asked for a key frame");
                            let event = gst::event::CustomDownstream::new(
                                gst::Structure::builder("GstForceKeyUnit")
                                    .field("all-headers", true)
                                    .build(),
                            );
                            let _ = element.send_event(event);
                        }
                    }

                    match sender.pump() {
                        Ok(true) => {}
                        Ok(false) => break,
                        Err(err) => {
                            if let Ok(mut slot) = done_tx.lock() {
                                if let Some(tx) = slot.take() {
                                    let _ = tx.send(Err(err));
                                }
                            }
                            return;
                        }
                    }

                    if last_stats.elapsed() >= Duration::from_secs(5) {
                        let (quadros, saltos, pior_salto, pior_envio) = sender.flow.take();
                        tracing::debug!(
                            stream = label,
                            quadros,
                            saltos_de_pts = saltos,
                            pior_salto_ms = pior_salto,
                            pior_envio_ms = pior_envio,
                            "outgoing flow"
                        );
                        tracing::debug!(
                            stream = label,
                            nacks = nacks_seen,
                            reenviados = packets_resent,
                            expirados = nacks_expired,
                            "retransmission requests"
                        );
                        last_stats = std::time::Instant::now();
                    }
                }

                if let Ok(mut slot) = done_tx.lock() {
                    if let Some(tx) = slot.take() {
                        let _ = tx.send(Ok(()));
                    }
                }
            })
            .map_err(|e| NdError::Gst(e.to_string()))?;
        threads.push(handle);
    }

    let outcome = tokio::select! {
        result = &mut done_rx => result.unwrap_or(Ok(())),
        event = futures::StreamExt::next(&mut events) => match event {
            Some(pipeline::PipelineEvent::Error { message, debug: details }) => {
                tracing::error!(%message, %details, "pipeline do espelhamento falhou");
                Err(NdError::Gst(message))
            }
            _ => Ok(()),
        },
        _ = cancel.changed() => Ok(()),
    };

    let _ = gst_pipeline.set_state(gst::State::Null);
    // The threads exit on their own as soon as the appsink reports end of stream.
    for handle in threads {
        let _ = handle.join();
    }

    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Builds a history of `count` frames spaced `step` apart.
    fn history_of(count: usize, step: Duration) -> (History, std::time::Instant) {
        let start = std::time::Instant::now();
        let mut history = History::new();
        for i in 0..count {
            history.push_back((
                (i & 0xFF) as u8,
                start + step * i as u32,
                vec![vec![0u8; 32]],
            ));
        }
        (history, start + step * count.saturating_sub(1) as u32)
    }

    #[test]
    fn history_holds_two_seconds_of_audio() {
        // The case that killed the session in the field: Opus audio at ~100
        // frames per second. With a fixed cap of 16 frames only 160 ms were
        // left, and the receiver asked for frames from over a second ago.
        let step = Duration::from_millis(10);
        let (mut history, now) = history_of(300, step);
        prune_history(&mut history, now);

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
    fn history_expires_by_age_not_by_count() {
        // Video at 30 fps: 2 s holds ~60 frames, well under the cap. Age is
        // what governs here, not the count.
        let step = Duration::from_millis(33);
        let (mut history, now) = history_of(200, step);
        prune_history(&mut history, now);

        assert!(history.len() < RETRANSMIT_HISTORY_MAX, "{}", history.len());
        let mais_velho = now.duration_since(history.front().unwrap().1);
        assert!(mais_velho <= RETRANSMIT_HISTORY, "{mais_velho:?}");
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
