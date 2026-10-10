//! Probes on a running pipeline: how many buffers left an element, how old a
//! frame is at each stage, and what the capture negotiated.
//!
//! Nothing here changes what is sent. The session crates install
//! [`count_buffers`] on every run; the rest is behind environment switches
//! because it costs a probe per buffer.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gst::prelude::*;
use gstreamer as gst;

use super::{CAPTURE_SOURCE_NAME, ENCODER_NAME};

/// Asks the pipeline for the **minimum** latency it can sustain.
///
/// Only meaningful once `Playing`. This is the number that separates
/// "aggressive tuning" from "the sink goes black": setting anything below this
/// minimum makes sinks drop late buffers. Querying it and logging the value is
/// what keeps latency from being picked by guesswork again.
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

/// Counts the frames the encoder actually produces, once a second.
///
/// The question it answers cannot be answered from the other end: a receiver
/// showing a stuttering picture may be losing frames on the network, or may
/// never have been sent them. This says which — it counts what left the
/// encoder, before anything can be lost.
///
/// Enabled by `BIGNETSCREEN_FPS_LOG=1`, because it is a diagnostic and its
/// place is in a bug report, not in every session's log.
pub fn instrument_framerate(pipeline: &gst::Pipeline) {
    use gst::prelude::*;

    if std::env::var("BIGNETSCREEN_FPS_LOG").as_deref() != Ok("1") {
        return;
    }
    // Both ends of the pipeline, because they answer different questions. A
    // compositor only repaints what changed, so a still screen legitimately
    // produces almost no capture buffers — and `pipewiresrc`'s keepalive then
    // resends the last one, which is a real frame rate of one per second. Told
    // apart from an encoder that cannot keep up only by counting both.
    for (element, pad, label) in [
        (
            CAPTURE_SOURCE_NAME,
            "src",
            "frames arriving from the compositor",
        ),
        (ENCODER_NAME, "src", "frames leaving the encoder"),
    ] {
        let Some(pad) = pipeline
            .by_name(element)
            .and_then(|element| element.static_pad(pad))
        else {
            tracing::debug!(element, "not in this pipeline; frame rate not instrumented");
            continue;
        };
        let frames = std::sync::atomic::AtomicU64::new(0);
        let since = std::sync::Mutex::new(std::time::Instant::now());
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                let bytes = buffer.size() as u64;
                let count = frames.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                let mut mark = since.lock().unwrap_or_else(|e| e.into_inner());
                let elapsed = mark.elapsed();
                if elapsed >= std::time::Duration::from_secs(1) {
                    tracing::info!(
                        fps = format!("{:.1}", count as f64 / elapsed.as_secs_f64()),
                        last_frame_bytes = bytes,
                        "{label}"
                    );
                    frames.store(0, std::sync::atomic::Ordering::Relaxed);
                    *mark = std::time::Instant::now();
                }
            }
            gst::PadProbeReturn::Ok
        });
    }
}

/// Counts buffers leaving a named element, for as long as the pipeline lives.
///
/// Two atomics, installed unconditionally, because the one question every
/// report of a slow picture turns on — were those frames ever made? — took a
/// special run with `BIGNETSCREEN_FPS_LOG=1` to answer, three times over. A
/// compositor delivering 7 frames a second and an encoder keeping up with it
/// look identical from the far end of the pipeline.
///
/// `None` when the element is not in this pipeline, which is normal: not every
/// path has a capture.
pub fn count_buffers(pipeline: &gst::Pipeline, element: &str) -> Option<Arc<AtomicU64>> {
    use gst::prelude::*;

    let pad = pipeline.by_name(element)?.static_pad("src")?;
    let count = Arc::new(AtomicU64::new(0));
    let counter = count.clone();
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        counter.fetch_add(1, Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    });
    Some(count)
}

fn pad_running_time(pad: &gst::Pad, pts: gst::ClockTime) -> Option<gst::ClockTime> {
    let event = pad.sticky_event::<gst::event::Segment>(0)?;
    event
        .segment()
        .downcast_ref::<gst::ClockTime>()?
        .to_running_time(pts)
}

/// Measure sender-side frame age using each pad's SEGMENT, not raw encoder PTS.
/// This diagnostic excludes network transit and receiver buffering/decoding.
pub fn instrument_latency(pipeline: &gst::Pipeline) {
    use gst::prelude::*;

    // A probe on **every** element, not just the sink: knowing the total is
    // 83 ms says nothing about where those milliseconds are born. With the
    // frame's age at each stage's output, the step between two neighbours is
    // the cost of that stage.
    let mut found = 0;
    for element in pipeline.iterate_elements().into_iter().flatten() {
        let Some(pad) = element
            .static_pad("src")
            .or_else(|| element.static_pad("sink"))
        else {
            continue;
        };
        let name = element.name().to_string();
        let state = std::sync::Mutex::new((std::time::Instant::now(), 0u64, 0u64, 0u64));

        pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            let Some(buffer) = info.buffer() else {
                return gst::PadProbeReturn::Ok;
            };
            let Some(pts) = buffer.pts() else {
                return gst::PadProbeReturn::Ok;
            };
            // Frame age = now (running time) − the frame's running time.
            let Some(parent) = pad.parent_element() else {
                return gst::PadProbeReturn::Ok;
            };
            let (Some(clock), Some(base)) = (parent.clock(), parent.base_time()) else {
                return gst::PadProbeReturn::Ok;
            };
            let Some(now) = clock.time().checked_sub(base) else {
                return gst::PadProbeReturn::Ok;
            };
            let Some(running_time) = pad_running_time(pad, pts) else {
                return gst::PadProbeReturn::Ok;
            };
            let Some(age) = now.checked_sub(running_time) else {
                return gst::PadProbeReturn::Ok;
            };

            let ms = age.mseconds();
            if let Ok(mut guard) = state.lock() {
                let (ref mut last, ref mut n, ref mut sum, ref mut worst) = *guard;
                *n += 1;
                *sum += ms;
                *worst = (*worst).max(ms);
                if last.elapsed() >= std::time::Duration::from_secs(5) {
                    tracing::info!(
                        sink = %name,
                        mean_ms = *sum / (*n).max(1),
                        worst_ms = *worst,
                        frames = *n,
                        "latency inside the pipeline (no network, no receiver)"
                    );
                    *last = std::time::Instant::now();
                    *n = 0;
                    *sum = 0;
                    *worst = 0;
                }
            }
            gst::PadProbeReturn::Ok
        });
        found += 1;
    }
    tracing::debug!(sinks = found, "latency probes installed");
}

/// What the capture settled on, and whether it could have done better.
///
/// Returns the negotiated caps and what the source says when asked for a GPU
/// buffer specifically.
///
/// The two answer different questions. Negotiated caps of plain `video/x-raw`
/// mean the frame is in system memory, but not why: the compositor may offer
/// nothing else, or our own scaler and converter may have forced the download
/// by being unable to accept a GPU buffer. Asking with a `memory:DMABuf` filter
/// separates those — `pipewiresrc` builds that feature from the modifiers the
/// producer advertises (`gstpipewireformat.c`), so an empty answer means there
/// is nothing to import and a non-empty one means importing it is available to
/// be taken.
pub fn capture_memory(pipeline: &gst::Pipeline) -> Option<(String, String)> {
    use gst::prelude::*;

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
    fn diagnostics_map_the_encoder_pts_offset_through_its_segment() {
        use gst::prelude::*;
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
