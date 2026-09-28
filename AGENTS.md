# Engineering instructions

Read this file, `ARCHITECTURE.md`, and the relevant crate before changing code. This is the single maintained operational instruction file; do not create competing assistant-specific copies. A tool that does not discover AGENTS.md automatically must be configured to read it explicitly.

## Project map

BigNetScreen is a Rust workspace with a native GTK4/libadwaita/Relm4 UI, not a Python application. Python helpers are development tools only.

| Location | Responsibility |
| --- | --- |
| `crates/nd-core` | Shared traits, capture sources, settings, media and GStreamer pipelines |
| `crates/nd-capture` | ScreenCast portal and native Mutter backends |
| `crates/nd-net` | NetworkManager P2P, firewalld leases and hardware probing |
| `crates/nd-chromecast` | mDNS, Cast control, RTP/RTCP, HTTP and media sessions |
| `crates/nd-wfd` | Miracast RTSP negotiation and streaming |
| `crates/nd-ndi` | Optional NDI publisher |
| `crates/nd-webrtc` | Browser publisher, WHEP front door and PIN |
| `crates/nd-gui` | Presentation, user actions and application lifetime |
| `vendor/gst-plugin-ndi` | **Tracked source**, not a disposable dependency cache |
| `vendor/relm4` | **Tracked source**; bounds shutdown-channel retention (see `PATCHES.md`) |

## Work and evidence

Inspect → reproduce → fix → targeted tests → full gates → diff → restore-test. Preserve unrelated work and previous patches. Do not select a checkpoint by keyword counts or documentation claims: compare Git trees and source behavior. Do not silently skip a failed test, change an assertion to hide a regression, or treat missing dependencies as passing tests.

Classify failures as CODE, TEST, ENV, DEPENDENCY or UNKNOWN from the actual log. Exit 127 is usually a missing executable/PATH issue, not a compiler diagnostic. Distinguish analyzed, changed, tested, not run and hardware-dependent in the handoff. Missing log/JSON is NOT_RUN, never success. No claims of native-language certification or universal receiver compatibility.

## Build and test

This review environment uses Rust **1.98.1**, isolated and verified with rustc/cargo/clippy/rustfmt version output. Reuse supplied offline tools; never install rustup or change system libraries merely to satisfy a sandbox. The manifest's MSRV is a separate CI gate, not implied by testing 1.98.1.

```sh
cargo fmt --all -- --check
cargo test --locked -p nd-core -p nd-chromecast
cargo test --locked -p nd-capture
cargo check --locked --workspace --all-targets
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
```

Add `--offline` when using the provided vendor. Run commands sequentially. Expand coverage to doctests, packaging, translations and graphical tests as described in `docs/testing.md`. Do not run ignored hardware tests against the host without authorization.

## Protocol invariants

* Capture carries the portal's FD and `pipewire-serial` together. Use the full u64 serial as `target-object`; node ID is legacy fallback only when serial is absent. Malformed serials fail closed. Never capture an arbitrary node when the authorized one fails.
* Cast RTP mirroring is **not** the HTTP path. Keep access-unit alignment, timestamps mapped through SEGMENT, correct audio/video clocks, bounded ACK windows across 8-bit wrap, bounded repair history and cancellation-aware pacing.
* Dropped interframes require resynchronization at a keyframe. Do not allocate transmission IDs for frames never sent. Do not advertise RTP extensions that are not implemented.
* Cast teardown is targeted STOP → confirmation → application CLOSE → platform CLOSE. Keep heartbeat live during STOP. Preserve cleanup errors, do not stop another sender's session, and keep the GUI/runtime alive through cleanup.
* WFD-specific PIDs, H.264 constraints and keepalives must stay on the WFD path. Do not copy Cast profile/bitrate choices into it.
* GStreamer bitrate units and mutable properties vary by encoder. Check official docs and test negotiated caps, not just pipeline-string substrings. Static receiver limits are not congestion control.
* Network input, queued work, retained logs and allocations need explicit bounds. HTTP tokens and PINs are secrets. Do not log credentials or stream URLs. Current Cast identity/TLS limitations remain documented in SECURITY.md.

## Maintainability

Keep a GUI-free core, propagate useful errors and update tests with behavior. Put shared policy in the owning crate, not duplicated UI/backend constants. Prefer small typed changes over broad textual auto-fixes. New unsafe blocks need a safety argument and tests; a dependency should solve a demonstrated problem, not duplicate a standard API.

Source and comments are English; UI strings use gettext `tr!`/`tr_n!`. Regenerate the POT deterministically and update PT-BR for new text. Never overwrite correct translations with English or machine-generated placeholders. Runtime state belongs in XDG directories, build output in ignored directories. No version-specific Python site-packages launchers: the product is a Rust binary.

Documentation for users stays in the READMEs and SUPPORT.md; contributor procedures in CONTRIBUTING.md and docs; dated reports under docs/history or audit. Explain why a workaround exists, its source and removal condition rather than narrating development. Keep external licenses/patch notes with tracked vendor code.

## Constrained executor

Check cgroup memory and active process groups before each expensive stage. With 4 GiB and no swap, use `CARGO_BUILD_JOBS=1`, `CARGO_INCREMENTAL=0`, and dev/test debug info disabled. Use one build target directory and one lock for heavy jobs. Every long command needs a deadline, bounded log, PID, exit status and whole-group cleanup. Do not restart while its previous child remains active. Use short status reads; do not run GUI tests without timeout. Prefer a plain shell or `python3 -S` for stdlib helpers to avoid unrelated Python startup plugins. Details: `docs/executor-safety.md`.

## Completion

Review `git diff --check`, staged changes and each failed/ignored gate. A checkpoint includes all tracked source (including the NDI vendor), Git history or a verifiable bundle, manifests and the exact test results—not target directories, toolchains or rebuildable dependencies. Extract separately, verify HEAD/tree/hashes, run git fsck and at least one actual targeted test. Never publish a tag or claim stable hardware support without maintainer authorization and the release checklist.
