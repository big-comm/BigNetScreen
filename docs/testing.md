# Validation

## Required automated gates

Run the smallest affected crate first. A final candidate must record the exact commit, compiler, native libraries, commands, exit codes and logs.

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

Use `--offline` with a supplied vendor. The declared MSRV is Rust 1.98.1; keep testing it in its own CI job, since success on a newer toolchain does not prove it. CI should also validate Desktop/AppStream metadata, PO catalogues, deterministic POT extraction, shell scripts, manifests and source archive contents. Dependency advisory checks require a current advisory database; unavailable data means not run, not clean.

The Cast HTTP integration test runs a real GStreamer encoder/muxer and downloads from the real server. Transport sync bytes are useful, but a release should also decode a capture independently (for example FFmpeg). A protocol writer test that exercises 20 cycles is not a TLS receiver test or 20 physical reconnections. Describe each accurately.

GUI layout tests marked ignored need a display. Run only the relevant display tests under a bounded Xvfb/D-Bus session; do not execute all ignored tests, because some use the host firewall, P2P or NDI hardware. Inspect representative wide/compact states and closure while cleanup is pending.

For window teardown, run `navigation_tree_finalizes_after_window_close` with
`--ignored --nocapture` in a private graphical session. The test opens the real
root component, exercises navigation, destroys the window and requires its panel
to finalize through a qdata destructor. `BIGNETSCREEN_LEAK_CYCLES=25` increases
the default five cycles for profiling. Keep handlers on descendants weak toward
their ancestors: Relm4's generated widget tree can hide that ownership from a
source-only leak lint. Process exit alone does not prove widget finalization.

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
IDs present in both samples: subtracting aggregates across exiting threads can
produce a false negative. With no session or pending audio reconciliation,
expect zero periodic engine work, no disk writes and no descriptor growth.
Commands/discovery remain event-driven; streaming and pending audio still need
the polling branch. Keep the initial audio reconciliation in any timer guard.

For optional NDI availability, trace `openat`/`statx` with thread IDs in an
isolated session. Reading `os-release` and checking installer executables must
run on a blocking worker, not in a GTK callback. Do not start the installer to
test availability. The install task repeats the eligibility check before any
subprocess; a delayed offer is discarded if the NDI error has already cleared.

## Hardware acceptance matrix

For each supported/advertised receiver family, record model, firmware, desktop/portal, encoder/driver and network. Run at least 30 minutes with motion and audio, then 20 cycles of connect → transmit → Stop → reconnect. Repeat closure via the window, cancellation during negotiation, receiver shutdown, network interruption and recovery.

Cast tests must distinguish RTP mirroring and HTTP fallback, check sustained FPS/latency, keyframe recovery, RTCP clocks and that the receiver remains available. Test legacy and newer receiver limits rather than assuming 1080p60 or 4K. WFD needs a representative Wi-Fi Direct receiver and cleanup of P2P/firewall state. Browser viewing needs an external browser/ICE path, wrong PIN/lockout and disconnection. NDI needs the real optional runtime and a receiver. Capture needs both old node-ID portals and new serial-bearing responses on representative desktops.

Keep credentials and screen content out of public reports. List every ignored/unavailable check, its reason and owner. Stable approval requires maintainer acceptance of security limitations as well as passing deterministic tests and the physical matrix.
