# Validation

## Required automated gates

Run the smallest affected crate first, then these gates one after another:

```sh
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo test --locked --workspace --doc
cargo build --locked --release -p nd-gui
python3 scripts/check-project.py
python3 scripts/test-project-tools.py
make locale
```

The declared MSRV is Rust 1.98.1 and has its own CI job. CI (`.github/workflows/ci.yml`) runs the Rust gates, the MSRV check, the desktop entry and AppStream validators, `msgfmt --check` on every catalogue, a POT freshness check, both project scripts and `cargo audit`. `cargo deny check` applies the license, source and advisory policy in `deny.toml`. An advisory check without a current advisory database counts as not run.

The Cast HTTP integration test runs a real GStreamer encoder and muxer and downloads from the real server; a release also decodes a capture independently, for example with FFmpeg. Protocol writer tests exercise the wire format against simulated peers, so physical reconnection belongs to the hardware matrix.

Graphical tests are ignored by default because they need a display. Run only the relevant ones, with a timeout, in a private graphical session with its own D-Bus: some ignored tests use the host firewall, P2P or NDI hardware. Inspect representative wide and compact states and closure while cleanup is pending.

The layout harness renders every page and breakpoint through GTK's Broadway backend and captures each state from a headless browser. Use one new, empty directory for both halves; the script needs the Python Playwright package and stops after the last state, `settings-compact`:

```sh
gtk4-broadwayd -a 127.0.0.1 -p 19091 :91 &
scripts/layout-screenshots.py "$dir" --browser /path/to/chromium &
GDK_BACKEND=broadway BROADWAY_DISPLAY=:91 GSK_RENDERER=broadway GTK_A11Y=none \
BIGNETSCREEN_LAYOUT_DIR="$dir" \
cargo test -p nd-gui --locked layout_pages_and_breakpoints -- --ignored --nocapture
```

For window teardown, run `navigation_tree_finalizes_after_window_close` with
`--ignored --nocapture` in a private graphical session. The test opens the real
root component, exercises navigation, destroys the window and requires its panel
to finalize through a qdata destructor. `BIGNETSCREEN_LEAK_CYCLES=25` raises the
default five cycles for profiling. Keep handlers on descendants weak toward
their ancestors: Relm4's generated widget tree hides that ownership from a
source-only leak lint.

Run `save_error_dialog_releases_its_window --ignored --nocapture` in a separate
isolated test process with `XDG_CONFIG_HOME=/proc/bns-leak-test`. That unwritable
configuration path forces the real asynchronous save failure without changing
user files. The gate requires finalization both after the close response and
after destroying the parent while its error dialog is still open. A response
handler must not retain the parent that owns its dialog.

For the idle-service resource gate, use a private D-Bus/XDG session, disable
auto-discovery and virtual audio, and launch `bignetscreend --foreground`.
Point its system-bus address at that private bus and PulseAudio at a nonexistent
socket so the gate cannot change host networking or sound. Record the launched
PID and `/proc/PID/exe`; after five seconds of settling, sample `stat`,
`smaps_rollup`, `io`, `fd` and `task`, wait 60 seconds without AT-SPI traversal,
and sample again. Repeat three times. Compare context switches only for thread
IDs present in both samples, since threads that exit between samples skew an
aggregate. With no session or pending audio reconciliation, expect zero periodic
engine work, no disk writes and no descriptor growth. Streaming and pending
audio use the polling branch; keep the initial audio reconciliation in any
timer guard.

For optional NDI availability, trace `openat`/`statx` with thread IDs in an
isolated session. Reading `os-release` and checking installer executables must
run on a blocking worker outside GTK callbacks. Availability checks never start
the installer. The install task repeats the eligibility check before any
subprocess; a delayed offer is discarded if the NDI error has already cleared.

## Hardware acceptance matrix

For each supported/advertised receiver family, record model, firmware, desktop/portal, encoder/driver and network. Run at least 30 minutes with motion and audio, then 20 cycles of connect → transmit → Stop → reconnect. Repeat closure via the window, cancellation during negotiation, receiver shutdown, network interruption and recovery.

Cast tests must distinguish RTP mirroring and HTTP fallback, check sustained FPS/latency, keyframe recovery, RTCP clocks and that the receiver remains available. Test legacy and newer receiver limits rather than assuming 1080p60 or 4K. WFD needs a representative Wi-Fi Direct receiver and cleanup of P2P/firewall state. Browser viewing needs an external browser/ICE path, wrong PIN/lockout and disconnection. NDI needs the real optional runtime and a receiver. Capture needs both old node-ID portals and new serial-bearing responses on representative desktops.

Keep credentials and screen content out of public reports. List every ignored or unavailable check with its reason.
