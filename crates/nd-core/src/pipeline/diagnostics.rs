//! Probes on a running pipeline: how many buffers left an element, how old a
//! frame is at each stage, and what the capture negotiated.
//!
//! Nothing here changes what is sent. The session crates install
//! [`count_buffers`] on every run; the per-buffer logging is behind
//! environment switches.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use gst::prelude::*;
use gstreamer as gst;

use super::{CAPTURE_SOURCE_NAME, ENCODER_NAME};

/// Asks a playing pipeline for the minimum latency it can sustain, and logs it.
///
/// A pipeline latency set below this makes sinks drop every buffer as late.
pub fn query_min_latency_ms(pipeline: &gst::Pipeline) -> Option<u64> {
    let mut query = gst::query::Latency::new();
    if !pipeline.query(&mut query) {
        return None;
    }
    let (live, min, max) = query.result();
    let min_ms = min.mseconds();
    tracing::info!(
        live,
        min_ms,
        max_ms = max.map(|m| m.mseconds()),
        "minimum latency the pipeline can sustain"
    );
    Some(min_ms)
}

/// Logs, once a second, the frame rate arriving from the compositor and
/// leaving the encoder, when `BIGNETSCREEN_FPS_LOG=1`.
///
/// Both ends, because a stuttering receiver may be losing frames on the
/// network or never have been sent them, and a still screen legitimately
/// produces almost no capture buffers. Only the pair tells a quiet compositor
/// from an encoder that cannot keep up.
pub fn instrument_framerate(pipeline: &gst::Pipeline) {
    if std::env::var("BIGNETSCREEN_FPS_LOG").as_deref() != Ok("1") {
        return;
    }
    for (element, label) in [
        (CAPTURE_SOURCE_NAME, "frames arriving from the compositor"),
        (ENCODER_NAME, "frames leaving the encoder"),
    ] {
        let Some(pad) = pipeline
            .by_name(element)
            .and_then(|element| element.static_pad("src"))
        else {
            tracing::debug!(element, "not in this pipeline; frame rate not instrumented");
            continue;
        };
        let frames = AtomicU64::new(0);
        let since = Mutex::new(Instant::now());
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                let count = frames.fetch_add(1, Ordering::Relaxed) + 1;
                let mut mark = since.lock().unwrap_or_else(PoisonError::into_inner);
                let elapsed = mark.elapsed();
                if elapsed >= Duration::from_secs(1) {
                    tracing::info!(
                        fps = format!("{:.1}", count as f64 / elapsed.as_secs_f64()),
                        last_frame_bytes = buffer.size(),
                        "{label}"
                    );
                    frames.store(0, Ordering::Relaxed);
                    *mark = Instant::now();
                }
            }
            gst::PadProbeReturn::Ok
        });
    }
}

/// Counts buffers leaving a named element, for as long as the pipeline lives.
///
/// Cheap enough to install on every session, so a report of a slow picture
/// already says whether the frames were ever made. `None` when the element is
/// not in this pipeline: not every path has a capture.
pub fn count_buffers(pipeline: &gst::Pipeline, element: &str) -> Option<Arc<AtomicU64>> {
    let pad = pipeline.by_name(element)?.static_pad("src")?;
    let count = Arc::new(AtomicU64::new(0));
    let counter = count.clone();
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        counter.fetch_add(1, Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    });
    Some(count)
}

/// Where `pts` falls on the pad's running-time line, through its SEGMENT.
///
/// Encoders may offset timestamps by hours, so a raw PTS is not comparable
/// with the clock.
fn pad_running_time(pad: &gst::Pad, pts: gst::ClockTime) -> Option<gst::ClockTime> {
    let event = pad.sticky_event::<gst::event::Segment>(0)?;
    event
        .segment()
        .downcast_ref::<gst::ClockTime>()?
        .to_running_time(pts)
}

/// How long ago, in running time, the buffer passing `pad` was captured.
fn frame_age(pad: &gst::Pad, buffer: &gst::BufferRef) -> Option<gst::ClockTime> {
    let parent = pad.parent_element()?;
    let now = parent.clock()?.time().checked_sub(parent.base_time()?)?;
    now.checked_sub(pad_running_time(pad, buffer.pts()?)?)
}

/// Frame ages seen at one element since the last report.
struct AgeWindow {
    since: Instant,
    frames: u64,
    total_ms: u64,
    worst_ms: u64,
}

/// Logs every five seconds how old frames are as they leave each element,
/// when `BIGNETSCREEN_LATENCY` is set.
///
/// Every element gets a probe, so the step between two neighbours is the cost
/// of that stage. The ages cover this machine only: no network transit and no
/// receiver buffering.
pub fn instrument_latency(pipeline: &gst::Pipeline) {
    let mut found = 0;
    for element in pipeline.iterate_elements().into_iter().flatten() {
        let Some(pad) = element
            .static_pad("src")
            .or_else(|| element.static_pad("sink"))
        else {
            continue;
        };
        let name = element.name().to_string();
        let window = Mutex::new(AgeWindow {
            since: Instant::now(),
            frames: 0,
            total_ms: 0,
            worst_ms: 0,
        });
        pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            let Some(age) = info.buffer().and_then(|buffer| frame_age(pad, buffer)) else {
                return gst::PadProbeReturn::Ok;
            };
            let ms = age.mseconds();
            let mut window = window.lock().unwrap_or_else(PoisonError::into_inner);
            window.frames += 1;
            window.total_ms += ms;
            window.worst_ms = window.worst_ms.max(ms);
            if window.since.elapsed() >= Duration::from_secs(5) {
                tracing::info!(
                    sink = %name,
                    mean_ms = window.total_ms / window.frames,
                    worst_ms = window.worst_ms,
                    frames = window.frames,
                    "latency inside the pipeline (no network, no receiver)"
                );
                *window = AgeWindow {
                    since: Instant::now(),
                    frames: 0,
                    total_ms: 0,
                    worst_ms: 0,
                };
            }
            gst::PadProbeReturn::Ok
        });
        found += 1;
    }
    tracing::debug!(sinks = found, "latency probes installed");
}

/// The capture's negotiated caps, and what it offers when asked for a DMA-BUF.
///
/// Plain `video/x-raw` caps say the frame is in system memory but not why:
/// the compositor may offer nothing else, or the scaler may have forced the
/// download. `pipewiresrc` builds the `memory:DMABuf` feature from the
/// modifiers the producer advertises (`gstpipewireformat.c`), so `"none"`
/// means there is nothing to import and anything else means an import was
/// available.
pub fn capture_memory(pipeline: &gst::Pipeline) -> Option<(String, String)> {
    let pad = pipeline.by_name(CAPTURE_SOURCE_NAME)?.static_pad("src")?;
    let negotiated = pad.current_caps()?.to_string();
    let dmabuf = gst::Caps::builder("video/x-raw")
        .features(["memory:DMABuf"])
        .build();
    let offered = pad.query_caps(Some(&dmabuf));
    let dmabuf = if offered.is_empty() {
        "none".to_string()
    } else {
        offered.to_string()
    };
    Some((negotiated, dmabuf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::init;

    #[test]
    fn encoder_pts_maps_through_segment() {
        init().unwrap();
        let pad = gst::Pad::builder(gst::PadDirection::Src).build();
        pad.set_active(true).unwrap();
        let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
        let offset = gst::ClockTime::from_seconds(1000 * 3600);
        segment.set_start(offset);
        pad.store_sticky_event(&gst::event::Segment::new(&segment))
            .unwrap();
        assert_eq!(pad_running_time(&pad, offset), Some(gst::ClockTime::ZERO));
        assert_eq!(
            pad_running_time(&pad, offset + gst::ClockTime::from_mseconds(50)),
            Some(gst::ClockTime::from_mseconds(50))
        );
    }
}
