//! A file sent as an MPEG transport stream must announce its picture in the
//! very first program table.
//!
//! An integration test, not a unit test beside the pipeline code: it encodes
//! for real, and run alongside the unit tests it starves the timing-sensitive
//! Miracast playback test of CPU.

use gst::prelude::*;
use gstreamer as gst;
use nd_core::pipeline::{self, AudioSource, PipelineGuard, StreamConfig, VideoSource, VideoTarget};

#[test]
fn a_file_stream_announces_its_picture_in_the_first_program_table() {
    pipeline::init().unwrap();
    let path = std::env::temp_dir().join(format!("nd-first-pmt-{}.webm", std::process::id()));
    let fixture = gst::parse::launch(&format!(
        "webmmux name=mux ! filesink location=\"{}\" \
         videotestsrc num-buffers=60 ! video/x-raw,width=320,height=240,framerate=30/1 \
         ! vp8enc deadline=1 ! mux. \
         audiotestsrc num-buffers=94 ! audio/x-raw,rate=48000 ! vorbisenc ! mux.",
        path.display()
    ))
    .unwrap();
    let guard = PipelineGuard::new(fixture.clone().downcast::<gst::Pipeline>().unwrap());
    fixture.set_state(gst::State::Playing).unwrap();
    fixture
        .bus()
        .unwrap()
        .timed_pop_filtered(gst::ClockTime::from_seconds(10), &[gst::MessageType::Eos])
        .expect("fixture written");
    drop(guard);

    let cfg = StreamConfig {
        width: 320,
        height: 240,
        audio: AudioSource::MediaFile,
        ..Default::default()
    };
    let source = VideoSource::Media {
        source: nd_core::media::MediaSource::File(path.clone()),
        kind: nd_core::media::MediaKind::Video,
        title: "first table".into(),
    };
    let description =
        pipeline::ts_http_pipeline_description(&cfg, &source, VideoTarget::Exact((320, 240)), None);
    let start = description.find("multisocketsink").unwrap();
    let end = description[start..].find(" audiomixer").unwrap() + start;
    let description = format!(
        "{}appsink name=ts sync=false{}",
        &description[..start],
        &description[end..]
    );
    let (built, _events) = pipeline::build_pipeline(&description, cfg.latency_ms()).unwrap();
    let guard = PipelineGuard::new(built);
    guard.pipeline().set_state(gst::State::Playing).unwrap();
    let sink = guard.pipeline().by_name("ts").unwrap();

    // The first PMT section (PID 0x20, payload start) and its stream types.
    let mut ts = Vec::new();
    let stream_types = loop {
        let sample = sink
            .emit_by_name::<Option<gst::Sample>>(
                "try-pull-sample",
                &[&gst::ClockTime::from_seconds(5)],
            )
            .expect("the multiplex must produce data");
        ts.extend_from_slice(&sample.buffer().unwrap().map_readable().unwrap());
        let table = ts.as_chunks::<188>().0.iter().find(|p| {
            p[0] == 0x47
                && (u16::from(p[1] & 0x1f) << 8 | u16::from(p[2])) == 0x20
                && p[1] & 0x40 != 0
        });
        if let Some(packet) = table {
            let mut at = 4 + if (packet[3] >> 4) & 3 == 3 {
                1 + usize::from(packet[4])
            } else {
                0
            };
            at += 1 + usize::from(packet[at]);
            let t = &packet[at..];
            let length = usize::from(t[1] & 0x0f) << 8 | usize::from(t[2]);
            let mut i = 12 + (usize::from(t[10] & 0x0f) << 8 | usize::from(t[11]));
            let mut types = Vec::new();
            while i < 3 + length - 4 {
                types.push(t[i]);
                i += 5 + (usize::from(t[i + 3] & 0x0f) << 8 | usize::from(t[i + 4]));
            }
            break types;
        }
    };
    drop(guard);
    std::fs::remove_file(&path).unwrap();
    assert!(
        stream_types.contains(&0x1b) && stream_types.contains(&0x0f),
        "the first program table must list H.264 and AAC, got {stream_types:x?}"
    );
}

#[test]
fn the_position_is_what_is_playing_not_what_was_read() {
    use nd_core::media::{FilePlaybackControl, MediaKind, MediaSource, PlaybackStart};
    pipeline::init().unwrap();
    let path = std::env::temp_dir().join(format!("nd-position-{}.webm", std::process::id()));
    let fixture = gst::parse::launch(&format!(
        "webmmux name=mux ! filesink location=\"{}\" \
         videotestsrc num-buffers=600 ! video/x-raw,width=160,height=90,framerate=30/1 \
         ! vp8enc deadline=1 ! mux. \
         audiotestsrc num-buffers=938 ! audio/x-raw,rate=48000 ! vorbisenc ! mux.",
        path.display()
    ))
    .unwrap();
    let guard = PipelineGuard::new(fixture.clone().downcast::<gst::Pipeline>().unwrap());
    fixture.set_state(gst::State::Playing).unwrap();
    fixture
        .bus()
        .unwrap()
        .timed_pop_filtered(gst::ClockTime::from_seconds(20), &[gst::MessageType::Eos])
        .expect("fixture written");
    drop(guard);

    let video = VideoSource::Media {
        source: MediaSource::File(path.clone()),
        kind: MediaKind::Video,
        title: "position".into(),
    };
    let description = format!(
        "{} ! fakesink sync=false {} ! fakesink sync=false",
        video.description(),
        AudioSource::MediaFile.description()
    );
    let (built, _events) = pipeline::build_pipeline(&description, 0).unwrap();
    let guard = PipelineGuard::new(built);
    let control = FilePlaybackControl::default();
    control.attach(guard.pipeline());
    control
        .prepare(PlaybackStart {
            seconds: 5.0,
            paused: false,
            volume: 1.0,
            muted: false,
            height: 0,
        })
        .unwrap();
    guard.pipeline().set_state(gst::State::Playing).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let seconds = control.state().seconds;
    drop(guard);
    std::fs::remove_file(&path).unwrap();
    // The decoder had read far past this: the file is 20 s and local.
    assert!(
        (5.8..7.5).contains(&seconds),
        "1.5 s after starting at 5 s, playback is at {seconds} s"
    );
}

#[test]
fn the_original_size_is_read_from_the_file() {
    use nd_core::media::{tv_frame, MediaSource};
    pipeline::init().unwrap();
    let path = std::env::temp_dir().join(format!("nd-tv-frame-{}.webm", std::process::id()));
    let fixture = gst::parse::launch(&format!(
        "videotestsrc num-buffers=5 ! video/x-raw,width=640,height=268,framerate=24/1 \
         ! vp8enc deadline=1 ! webmmux ! filesink location=\"{}\"",
        path.display()
    ))
    .unwrap();
    let guard = PipelineGuard::new(fixture.clone().downcast::<gst::Pipeline>().unwrap());
    fixture.set_state(gst::State::Playing).unwrap();
    fixture
        .bus()
        .unwrap()
        .timed_pop_filtered(gst::ClockTime::from_seconds(10), &[gst::MessageType::Eos])
        .expect("fixture written");
    drop(guard);
    let frame = tv_frame(&MediaSource::File(path.clone()), 0);
    std::fs::remove_file(&path).unwrap();
    // 2.39:1 at 640 wide: the 16:9 frame that holds it.
    assert_eq!(frame, Some((640, 360)));
    // Unreadable: no guess.
    assert_eq!(tv_frame(&MediaSource::File(path), 0), None);
}
