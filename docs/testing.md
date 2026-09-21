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

Use `--offline` with a supplied vendor. Test the MSRV separately; success on Rust 1.98.1 does not prove it. CI should also validate Desktop/AppStream metadata, PO catalogues, deterministic POT extraction, shell scripts, manifests and source archive contents. Dependency advisory checks require a current advisory database; unavailable data means not run, not clean.

The Cast HTTP integration test runs a real GStreamer encoder/muxer and downloads from the real server. Transport sync bytes are useful, but a release should also decode a capture independently (for example FFmpeg). A protocol writer test that exercises 20 cycles is not a TLS receiver test or 20 physical reconnections. Describe each accurately.

GUI layout tests marked ignored need a display. Run only the relevant display tests under a bounded Xvfb/D-Bus session; do not execute all ignored tests, because some use the host firewall, P2P or NDI hardware. Inspect representative wide/compact states and closure while cleanup is pending.

## Hardware acceptance matrix

For each supported/advertised receiver family, record model, firmware, desktop/portal, encoder/driver and network. Run at least 30 minutes with motion and audio, then 20 cycles of connect → transmit → Stop → reconnect. Repeat closure via the window, cancellation during negotiation, receiver shutdown, network interruption and recovery.

Cast tests must distinguish RTP mirroring and HTTP fallback, check sustained FPS/latency, keyframe recovery, RTCP clocks and that the receiver remains available. Test legacy and newer receiver limits rather than assuming 1080p60 or 4K. WFD needs a representative Wi-Fi Direct receiver and cleanup of P2P/firewall state. Browser viewing needs an external browser/ICE path, wrong PIN/lockout and disconnection. NDI needs the real optional runtime and a receiver. Capture needs both old node-ID portals and new serial-bearing responses on representative desktops.

Keep credentials and screen content out of public reports. List every ignored/unavailable check, its reason and owner. Stable approval requires maintainer acceptance of security limitations as well as passing deterministic tests and the physical matrix.
