# Layout and media previews

2026-09-17. Based on the eight supplied desktop mockups.

- System accent and named GTK surface colors; no fixed application palette.
- Larger sidebar navigation, page headings, sharing cards, receiver rows and settings icons.
- Sidebar stays visible; page breakpoints stack controls on narrow layouts. Minimum window width: 760 px. Short media pages scroll.
- Video frames, embedded album art, conventional adjacent cover files, duration and artist metadata. Missing artwork retains a themed fallback.
- Silent GStreamer preroll on two background workers; bounded decoded-preview cache. Images do not determine grid or queue row size.
- Editable queue, playback controls, discovery and settings behavior preserved.

## Validation

- GUI crate: 15 tests passed, including video frame/aspect ratio and embedded music artwork/metadata.
- Three existing ignored GTK tests passed separately: queue edits, lazy loading/bounded decoding, discovery switch synchronization.
- Layout harness captured all pages, sharing choices, light/dark media and compact layouts. CSS parsed without errors.
- Strict GUI Clippy and formatting passed. All 29 catalogs checked; no new untranslated messages.
- Release executable and native package generated; complete application window checked in an isolated Broadway session. No host installation.

The screenshot harness uses generated media fixtures and does not transmit to receivers. Start `gtk4-broadwayd -a 127.0.0.1 -p 19091 :91`, then run `tools/layout-screenshots.py /tmp/bns-layout-new --browser /path/to/chromium` in another terminal. Requires the Python Playwright package. Use a fresh output directory.

```sh
GDK_BACKEND=broadway BROADWAY_DISPLAY=:91 GSK_RENDERER=broadway GTK_A11Y=none \
BIGNETSCREEN_LAYOUT_DIR=/tmp/bns-layout-new \
cargo test -p nd-gui --locked layout_pages_and_breakpoints -- --ignored --nocapture
```

Use an isolated `XDG_CONFIG_HOME` with `user-dirs.dirs` pointing to empty media directories when capturing fixtures only. Broadway needs its own renderer; Cairo snapshots did not produce valid screenshots in this GTK environment.

Preview extraction uses [GStreamer's playbin API](https://gstreamer.freedesktop.org/documentation/playback/playbin.html), with explicit fake sinks to avoid audio output or video windows.

## Home composition follow-up

Receiver heading/count and compact rows; equal-height receiver/tips cards; three illustrated tips; compact icon-labelled NDI actions. Vector marks inherit theme colors and avoid missing theme icons. Initial discovery-disabled state now shows the correct empty placeholder. Seven new messages translated in all 29 locales. Layout capture and strict Clippy passed; local release executable rebuilt.

Discovery now uses a labelled "Find compatible devices" button in the receiver heading. Header refresh and sidebar toggle removed. All 29 translations checked; button output, layout capture and strict Clippy passed. Local release executable rebuilt.
