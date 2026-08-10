//! Answers one question with bytes instead of opinions: **is the compositor
//! drawing the pointer into the stream?**
//!
//! ```sh
//! cargo run -p nd-capture --example cursor_probe -- [monitor|virtual]
//! ```
//!
//! It captures the same screen twice — once with `cursor-mode: hidden`, once
//! with `cursor-mode: embedded` — and compares the frames. Identical frames
//! mean the cursor is not being composited, however loudly the documentation
//! says the mode is supported. Different frames mean it is.
//!
//! For this to mean anything the pointer must be **sitting still over the
//! screen being captured**, and nothing else on it may move.

use std::time::Duration;

use gstreamer::prelude::*;

use nd_capture::MutterBackend;
use nd_core::capture::{CaptureBackend, SourceType};
use nd_core::pipeline;

/// Grabs one frame with the given cursor mode.
async fn grab(source_type: SourceType, cursor_mode: u32) -> nd_core::Result<(Vec<u8>, usize)> {
    let backend = MutterBackend::new();
    backend.set_cursor_mode(cursor_mode).await;
    backend.set_virtual_size(1920, 1080).await;

    let source = backend.start(source_type).await?;
    // **Raw** frames, not encoded ones. Comparing H.264 output proves nothing:
    // any difference at all re-encodes the whole frame, so two captures of the
    // same still screen come out wildly different byte-wise. On raw video a
    // still screen gives byte-identical frames, and a drawn pointer shows up as
    // a small, localised difference.
    // A PNG alongside the byte comparison: counting differing pixels says
    // *something* changed, never *what*. The file lets a human look.
    let png = format!("/tmp/bignetscreen-cursor-{cursor_mode}.png");
    let desc = format!(
        "{} ! videoconvert ! video/x-raw,format=RGB \
         ! tee name=t \
         t. ! queue ! appsink name={} emit-signals=false sync=false max-buffers=2 drop=true \
         t. ! queue ! videoconvert ! pngenc snapshot=false \
              ! multifilesink location={png} max-files=1",
        source.video_source().description(),
        pipeline::MIRROR_VIDEO_SINK,
    );
    let (gst_pipeline, _events) = pipeline::build_pipeline(&desc, 0)?;
    gst_pipeline
        .set_state(gstreamer::State::Playing)
        .map_err(|e| nd_core::NdError::Gst(e.to_string()))?;

    let sink = gst_pipeline
        .by_name(pipeline::MIRROR_VIDEO_SINK)
        .and_then(|e| e.downcast::<gstreamer_app::AppSink>().ok())
        .ok_or_else(|| nd_core::NdError::Gst("video appsink not found".into()))?;

    // The first frames can be a key frame of an unsettled screen; a few frames
    // in, the picture is stable and the only thing that can differ between the
    // two runs is the pointer.
    let mut frame = Vec::new();
    let mut width = 0usize;
    for _ in 0..40 {
        if let Some(sample) = sink.try_pull_sample(gstreamer::ClockTime::from_mseconds(250)) {
            if let Some(caps) = sample.caps() {
                if let Some(structure) = caps.structure(0) {
                    if let Ok(w) = structure.get::<i32>("width") {
                        width = w as usize;
                    }
                }
            }
            if let Some(buffer) = sample.buffer() {
                if let Ok(map) = buffer.map_readable() {
                    frame = map.to_vec();
                }
            }
        }
    }

    let _ = gst_pipeline.set_state(gstreamer::State::Null);
    drop(source);
    backend.stop().await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    eprintln!("     frame saved to {png}");
    Ok((frame, width))
}

#[tokio::main]
async fn main() -> nd_core::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    // `virtual-as-monitor`: create the extra screen with RecordVirtual, then
    // capture it with RecordMonitor. The point is the pointer — Mutter draws it
    // in a monitor stream and not in a virtual one.
    if std::env::args().nth(1).as_deref() == Some("virtual-as-monitor") {
        return virtual_as_monitor().await;
    }

    let source_type = match std::env::args().nth(1).as_deref() {
        Some("virtual") => SourceType::Virtual,
        _ => SourceType::Monitor,
    };
    eprintln!("capturing {source_type:?} — keep the pointer still over that screen\n");

    eprintln!("1/2  cursor hidden…");
    let (hidden, width) = grab(source_type, 0).await?;
    eprintln!("2/2  cursor embedded…");
    let (embedded, _) = grab(source_type, 1).await?;

    if hidden.is_empty() || embedded.is_empty() {
        eprintln!("no frames captured; nothing can be concluded");
        return Ok(());
    }

    let differing = hidden.iter().zip(&embedded).filter(|(a, b)| a != b).count();
    let total = hidden.len().min(embedded.len());
    let pct = differing as f64 * 100.0 / total as f64;

    println!(
        "\nframe sizes: hidden {} B, embedded {} B",
        hidden.len(),
        embedded.len()
    );
    println!("bytes differing: {differing} of {total} ({pct:.2}%)");
    // A count alone cannot tell a pointer from noise. Where the differing
    // pixels *are* can: a pointer is a compact block of roughly 24x24, while
    // anything else is scattered.
    let stride_px = width.max(1);
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (usize::MAX, usize::MAX, 0usize, 0usize);
    let mut pixels = 0usize;
    for i in (0..total).step_by(3) {
        if hidden[i] != embedded[i]
            || hidden[i + 1] != embedded[i + 1]
            || hidden[i + 2] != embedded[i + 2]
        {
            let pixel = i / 3;
            let (x, y) = (pixel % stride_px, pixel / stride_px);
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
            pixels += 1;
        }
    }

    if pixels == 0 {
        println!("\n=> the compositor is NOT drawing the pointer: the frames are identical");
        return Ok(());
    }

    let (w, h) = (max_x - min_x + 1, max_y - min_y + 1);
    println!("pixels changed: {pixels}");
    println!("area: {w}x{h} at ({min_x},{min_y})");
    let compact = w <= 64 && h <= 64;
    if compact {
        println!("\n=> a pointer-sized block was drawn: the cursor IS in the stream");
    } else {
        println!(
            "\n=> the change is spread over {w}x{h}: too large for a pointer, so this \
             measures something else"
        );
    }
    Ok(())
}

/// Creates a virtual monitor and captures it as a **monitor**, saving a PNG.
///
/// The virtual monitor's own stream has to stay alive — the screen exists only
/// for as long as something consumes it — so it is kept running into a
/// `fakesink` while a second session captures the same screen by connector.
async fn virtual_as_monitor() -> nd_core::Result<()> {
    let conn = zbus::Connection::session()
        .await
        .map_err(|e| nd_core::NdError::Capture(e.to_string()))?;
    let before = nd_capture::display_config::connectors(&conn).await;

    let keeper = MutterBackend::new();
    keeper.set_virtual_size(1920, 1080).await;
    let virtual_source = keeper.start(SourceType::Virtual).await?;

    // Something must consume the stream or the screen never materialises.
    let desc = format!(
        "{} ! videoconvert ! fakesink sync=false",
        virtual_source.video_source().description()
    );
    let (keep_alive, _events) = pipeline::build_pipeline(&desc, 0)?;
    keep_alive
        .set_state(gstreamer::State::Playing)
        .map_err(|e| nd_core::NdError::Gst(e.to_string()))?;

    let connector = nd_capture::display_config::wait_for_virtual_connector(
        &conn,
        &before,
        Duration::from_secs(30),
    )
    .await
    .ok_or_else(|| nd_core::NdError::Capture("the virtual monitor never appeared".into()))?;
    eprintln!("virtual monitor is connector {connector}; capturing it as a monitor");
    // A generous window: the person has to read the message, find the edge of
    // their screen, push the pointer across and leave it there. Six seconds was
    // not enough and produced a capture with no pointer in it, which proves
    // nothing either way.
    for remaining in (1..=20).rev() {
        if remaining % 5 == 0 || remaining <= 3 {
            eprintln!("  move the pointer onto the extra screen — capturing in {remaining}s");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    eprintln!("  capturing now — keep the pointer still");

    let shooter = MutterBackend::new();
    shooter.set_monitor_connector(Some(connector)).await;
    shooter.set_cursor_mode(1).await;
    let monitor_source = shooter.start(SourceType::Monitor).await?;

    let png = "/tmp/bignetscreen-virtual-as-monitor.png";
    let desc = format!(
        "{} ! videoconvert ! pngenc snapshot=false ! multifilesink location={png} max-files=1",
        monitor_source.video_source().description()
    );
    let (shot, _events) = pipeline::build_pipeline(&desc, 0)?;
    shot.set_state(gstreamer::State::Playing)
        .map_err(|e| nd_core::NdError::Gst(e.to_string()))?;
    tokio::time::sleep(Duration::from_secs(6)).await;

    let _ = shot.set_state(gstreamer::State::Null);
    drop(monitor_source);
    shooter.stop().await?;
    let _ = keep_alive.set_state(gstreamer::State::Null);
    drop(virtual_source);
    keeper.stop().await?;

    println!("\nsaved {png} — open it and look for the pointer");
    Ok(())
}
