# Media queue and playback validation

2026-09-17. Target report: PC to Amazon Fire TV over Miracast.

## Changes

- Visible queue; remove individual files or clear the queue during selection and playback.
- Clear invalidates pending file-picker inspection. Tiles follow queue selection across library changes.
- Pause/resume, seek backward/forward 10 seconds, next file, elapsed time and duration.
- Miracast playback stays on the media page. Controls operate on the local file pipeline.
- Cast controls follow receiver capabilities; queue edits preserve HTTP file indexes.
- Eight new messages translated and checked in all 29 locales.

## Checks

- Workspace tests: 241 passed, 5 ignored.
- Ignored GTK queue integration test run separately with Broadway: passed. Covers off-grid removal, clear, stale picker results, tile selection and playback button sensitivity.
- Local GStreamer fixture: pause, seek while paused, resume both tracks and seek backward.
- Miracast transport fixture: production H.264/AAC/MPEG-TS/RTP sender, independent receiver decoding packet bytes after pause/resume and seeks. Test receiver follows demux pad replacement when stream metadata changes.
- Mock sink: removing pending/current files and skipping wait for stream teardown.
- Cast unit tests: session matching, capabilities, retained duration and bounded seek requests.
- Strict workspace Clippy, formatting and gettext checks passed.
- Native package built; all 29 compiled catalogs contain the new messages.

## Hardware follow-up

No physical Fire TV tested in this change. Confirm pause/resume, audio synchronization after seeking, long pauses, next file and clearing during playback on Bruno's receiver. Next file can renegotiate the Miracast connection; playback is not gapless. Cast controls also await physical receiver validation.

Artifacts: `build/media-controls/` contains the application package, NDI runtime package and SHA256 sums. Release executable: `target/release/bignetscreen`.
