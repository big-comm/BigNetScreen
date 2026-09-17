# Review follow-up

Implemented 2026-09-16. Scope: reviewed defects, optional NDI sender, resolution choices.

## Changes

| Review findings | Implemented correction |
| --- | --- |
| 1 | Removed raw Cast payloads and secret URLs from logs. |
| 2 | Owned RTCP/sender workers; stop flag, socket timeout, thread joins and pipeline guards cover startup failures. |
| 3–5 | Active sink independent of discovery; cancellable discovery, serialized P2P scanning; immediate refresh no longer creates another polling timer. |
| 6 | Cancellation covers capture setup; portal/Mutter sessions retained during startup; cooperative P2P cancellation disconnects activations. |
| 7–8 | Media startup immediately returns an owned session; Stop waits for remote cleanup; playlists stop advancing; photo timeout awaits teardown. |
| 9 | Decoder EOS ends synthetic tracks; finite media queues retain frames and playback is clock-paced. Photos remain live until stopped. |
| 10–11 | RTSP read/deadline survives keepalives; bounded headers/body; malformed/truncated input rejected; negotiation and keepalive deadlines; RTP port overflow rejected. |
| 12 | WFD resolution/FPS selected from advertised modes before M4; encoding uses that exact mode. Missing AAC capability is explicit. |
| 13 | Periodic correlated media status requests; stale session/content events ignored; silence/errors fail visibly instead of advancing. |
| 14 | Stable protocol IDs replace name-based deduplication; rediscovery refreshes metadata without replacing the active handle. |
| 15 | Cast encoder candidates must produce frames in a bounded probe; WFD probes every candidate; missing frames/feedback terminate mirroring. |
| 16–17 | File connections owned in a bounded JoinSet; drop cancels clients. Opened-file metadata drives lengths/ranges; empty files, suffix ranges, HEAD, invalid methods and request limits handled. |
| 18 | Media uses the discovered control port; live HTTP honors the configured local port. |
| 19 | Removed speculative default-audio restoration. Diagnostics are read-only, asynchronous and time-bounded. |
| 20–21 | Updated rustls, anyhow, event-listener, gettext-rs and yanked spin. Declared Rust 1.93; added minimum-toolchain CI job; corrected license-check documentation. |
| 22 | Firewall failures propagate and roll back partial changes. Host-mutating test is explicit opt-in and releases its lease. |

Additional corrections: bounded Cast/pipeline/NACK queues; control event draining;
upstream keyframe requests; conservative 1200-byte RTP payloads; rejected Cast
tracks drain without blocking; sender-report counters exclude the RTP header and
use a common pipeline clock origin. Atomic private settings/token writes;
rotation/layout-mode/color/underscan preservation; current physical layout retained
at virtual-screen teardown; topology changes avoid stale restoration. Media folder
scanning runs off the GTK thread with bounded results. Settings reloads block
feedback signals. All 29 gettext catalogs now cover every active UI message.

## Features

- NDI: separate Home publishing controls for screen, window and virtual screen.
  Raw UYVY video plus F32 audio through `ndisinkcombiner`/`ndisink`; named source,
  shared session cancellation, missing-plugin/runtime errors. No vendor binaries
  bundled. See [setup and distribution notes](ndi.md).
- Resolution: existing presets plus 1280×800, 1600×900, 1920×1200, 2560×1600,
  3440×1440 and custom even dimensions, 160–7680 pixels. Aspect preserved;
  smaller sources not enlarged. Virtual-screen sizing follows the preference.
- FPS presets: 24, 25, 30, 50 and 60. New preferences apply next session.

## Validation

- Workspace tests: **234 passed; one explicitly ignored firewall integration test**.
- Regression coverage: HTTP shutdown/empty files/ranges; RTCP thread teardown;
  finite music and video with/without audio; video frame count/pacing; playlist
  cancellation/photo teardown; stale media status; partial/oversized RTSP;
  negotiated WFD preferences; custom settings; portrait geometry; private writes.
- NDI raw video/audio branches: synthetic source → negotiated caps → fake sinks.
- Workspace build, `cargo fmt --check`, strict Clippy and `git diff --check`: passed.
- All PO catalogs pass `msgfmt --check`; POT extraction is deterministic;
  all 29 catalogs have 156/156 translated entries with no fuzzy markers.
  Compiled lookups/plurals checked; CI YAML parses. See
  [localization validation](localization-validation.md).
- RustSec audit: no advisories or warnings with database commit
  `e2e6404` (2026-09-14), checked 2026-09-16.

## Unverified and remaining protocol work

- Samsung LSP3 Miracast and Intel VA encoding exercised on GNOME/Mutter;
  see [hardware validation](projector-validation.md). Cast/NDI interoperability
  and end-to-end latency remain unverified.
- NDI plugin unavailable locally. Runtime/network sending needs installation and
  a real receiver; synthetic tests do not establish end-to-end NDI operation.
- Visual GUI validation remains pending. An optional isolated KWin helper was
  unavailable; KWin is not required for the native GNOME/Mutter application.
- Local Rust 1.93 and Flatpak installation/build were not executed. CI now checks
  the declared minimum; Flatpak still needs generated Cargo sources and codec
  inventory validation. Native NDI is the documented target.
- Cast sender reports remain experimental and disabled by default pending
  real-device protocol validation. Retransmission is not clock synchronization.
- Cast device authentication/pinning was not added; the existing certificate
  policy is not proof of Google receiver identity. Vendor NDI/GPL distribution
  compatibility must be reviewed before bundling components.
