# Samsung projector validation

Date: 2026-09-16 (America/Sao_Paulo). Native GNOME/Wayland, Mutter,
Intel Xe GPU, Samsung Projector LSP3 (The Freestyle).

## Confirmed

- Discovery: Wi-Fi Direct Miracast and LAN AirPlay. No Google Cast service
  advertised by this projector during the scan. Other receivers were excluded.
- 1280×720 at 30 FPS: synthetic moving ball and quiet 440 Hz tone;
  user confirmed both picture and sound. A 90-second file completed normally.
- Hardware encoding: `vah264enc`, with actual encoded frames during startup.
- Initial teardown: Wi-Fi Direct disconnected; primary Wi-Fi remained connected.
- Mutter screen capture: primary `eDP-1` produced frames; projector negotiated
  1920×1080 at 60 FPS. This is the negotiated rate, not a measured display rate.
- Custom virtual screen: Mutter created `Meta-0` at 1600×900; projector accepted
  the exact 1600×900 at 60 FPS VESA mode. Hardware encoder produced frames.
- First virtual-screen teardown preserved the active physical mode, position,
  scale, rotation, primary flag and global layout properties. No virtual
  connector remained. Mutter changed preferred-scale metadata, not active scale.
- Repeated virtual test after the fix: explicit layout restoration succeeded;
  active modes and logical/global layout matched the pre-test state. Virtual
  connector removed, P2P disconnected, primary Wi-Fi connected.

## Corrections from hardware testing

- Layout restoration now waits for Mutter's asynchronous connector removal
  before deciding that physical topology changed. The initial test exposed an
  early return in this check, even though this single-monitor layout survived.
- WFD discovery diagnostics no longer interpret D-Bus access denial as proof
  that `wpa_supplicant` was built without Wi-Fi Display support.
- The diagnostic sender now selects a specific peer, uses a bounded session,
  sends synthetic media audio, and cleans up on timeout/startup errors.

Focused validation: capture tests 7 passed; network tests 16 passed, one explicit
firewall integration test ignored; strict capture/WFD Clippy, workspace build,
formatting and diff checks passed.

## Reproduction

Use `cargo run -p nd-wfd --example wfd_cast`. Set `WFD_PEER` to the intended
receiver's exact MAC address. The example now supports `WFD_TEST_SECONDS`
(default 180 seconds), stops discovery after connection, and releases capture,
firewall leases and the P2P group on session errors or timeout.

`WFD_TEST_MEDIA` selects a synthetic video with its own audio. Without it,
the example sends silence. `screen` and `virtual` select desktop capture.
`WFD_MAX_RES` caps the negotiated size. Use a temporary `XDG_CONFIG_HOME`
to test settings without changing the user's preferences.

Logs for this session: `/tmp/bignetscreen-projector-720.log` and
`/tmp/bignetscreen-projector-1080.log` and `/tmp/bignetscreen-projector-virtual.log`.

## Remaining

- NDI: `ndisink` is absent. Requires plugin/runtime and an NDI receiver.
  Successful Miracast does not validate NDI or AirPlay streaming.
- No physical Chromecast receiver test, end-to-end latency measurement,
  long-duration soak test or microphone capture test.
- GUI controls and portal selection still need interactive validation.
- User confirmed the synthetic 720p picture/audio. Desktop and virtual modes
  have sender-side evidence; visual confirmation remains pending.
