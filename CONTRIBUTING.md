# Contributing

Start with the [architecture](ARCHITECTURE.md) and [development setup](docs/development.md).

## Propose a focused change

For a bug, identify the observed behavior, affected transport, exact commit and a minimal reproduction. For performance, record source/negotiated resolution and FPS, encoder, network and receiver model/firmware. Separate sender pipeline timing from end-to-end latency, and compare equal quality settings.

Use a topic branch, preserve unrelated modifications and add a regression test that fails before the fix. Document external API decisions with an official reference and the dependency version actually tested. Community traces and issue reports are hypotheses until reproduced. Do not rewrite licensed vendored code or remove fixture data as generic cleanup.

Run targeted tests first, then the [required gates](docs/testing.md). Include the commands and their outcomes in the pull request, and list unavailable hardware, missing plugins and ignored tests.

## Protocol invariants

- Capture carries the portal's FD and `pipewire-serial` together. Use the full u64 serial as `target-object`; the node ID is a fallback only when the serial is absent. A malformed serial fails closed, and a failed authorized node never falls back to another node.
- Cast RTP mirroring and the HTTP path are separate. Mirroring keeps access-unit alignment, timestamps mapped through SEGMENT, correct audio/video clocks, bounded ACK windows across the 8-bit frame-ID wrap, bounded repair history and cancellation-aware pacing.
- A dropped interframe needs resynchronization at a keyframe. Frames never sent get no transmission ID, and unimplemented RTP extensions are not advertised.
- Cast teardown is targeted STOP, confirmation, application CLOSE, platform CLOSE. The heartbeat stays live during STOP, cleanup errors are kept, another sender's session is never stopped, and the runtime stays alive until cleanup ends.
- WFD PIDs, H.264 constraints and keepalives stay on the WFD path; Cast profile and bitrate choices do not apply there.
- GStreamer bitrate units and mutable properties vary by encoder. Check the official docs and test negotiated caps, not pipeline-string substrings. Static receiver limits are not congestion control.
- Network input, queued work, retained logs and allocations have explicit bounds. HTTP tokens, PINs and stream URLs are secrets and stay out of logs. Current Cast identity/TLS limitations are documented in [SECURITY.md](SECURITY.md).

## Code

Keep the core GUI-free and put shared policy in the owning crate rather than in UI or backend constants. Propagate useful errors and update tests with behavior. New `unsafe` needs a `SAFETY` comment and a test; a new dependency must solve a demonstrated problem that the standard library does not. Comments explain why a workaround exists, its source and when it can go.

`vendor/gst-plugin-ndi` and `vendor/relm4` are tracked source with local patches; keep their licenses and patch notes with them.

## Translations and documentation

UI text uses English message IDs through `tr!`/`tr_n!`. A new source file that calls them must be listed in `po/POTFILES.in`. Run `./po/update-pot.sh` to refresh the template and merge it into the catalogues, then validate each changed catalogue with `msgfmt --check`. See [po/README.md](po/README.md). Preserve placeholders and plurals, review PT-BR as Brazilian Portuguese, and never replace a correct translation with English or a machine placeholder.

Update the closest maintained guide when behavior changes: user documentation in the READMEs and SUPPORT.md, contributor procedures here and in `docs/`. Reproducible tools go under `scripts/`. Never check in personal paths, tokens, screenshots of private content, caches or built binaries; runtime state belongs in XDG directories and build output in ignored directories.

## Pull request evidence

Describe the problem, the fix, risks and rollback, tests run, and any remaining hardware checks. Commit subjects name the subsystem and the change in the imperative. Tags, packages and stable status follow [releasing](docs/releasing.md).
