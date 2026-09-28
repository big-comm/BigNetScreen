# Memory and system-resource review — 2026-09-27

Local review of `35d31c3`, with individual correction commits and no push.
Raw traces, profiles, census files, executable hashes and harnesses are in
`~/.cache/bns-resources/`. Upstream drafts and suggested patches are in
`~/relatos-upstream/bignetscreen-*`; none were submitted or installed system-wide.

## Corrections and evidence

| Owner | Failure before | Result after |
| --- | --- | --- |
| GUI navigation (`555edb2`) | The button's closure strongly owned its ancestor split; first destroyed window had `new=1, fin=0`. | Weak reference; qdata finalization `new=25, fin=25`, repeated three times. |
| Relm4 0.11 (`1401170`) | Global shutdown registry retained 100 completed component channels, 51,200 bytes, for 20 additional root creations. | Closed senders pruned; focused regression passes. Release heaptrack 25-minus-5-cycle retention falls from 57.22 KB to 656 bytes, with no remaining RuntimeSenders entries. |
| Save-error dialog (`1a498f9`) | Destroying the parent with its save-error dialog open retained the parent. Normal response alone concealed the cycle. | Weak parent capture; real asynchronous write failure followed by response/forced destruction finalizes 10/10 windows in each of three runs. |
| Service idle polling (`949fefb`) | No discovery/session: 311, 311 and 313 voluntary context switches in surviving threads per 60 seconds; 4–5 CPU ticks. | Three runs: zero such switches and zero CPU ticks; no descriptor growth or disk writes. Pending initial audio and active sessions still poll. |
| NDI availability | The same caller thread performed `openat("/etc/os-release")`; UI callbacks also queried installer files. | Probe runs in `spawn_blocking`; strace caller TID 266322 versus file-opening worker 266323. Eligibility is rechecked before installation; stale offers are discarded. No installation was executed. |

The Relm4 override retains live channels and the most recently closed batch until
another registration. Capacity follows peak concurrent components, not total
creations. `c34418e` removes the registry package's redundant `Cargo.toml.orig`
backup to satisfy the existing source-hygiene gate; source and licenses remain.

The root Clippy configuration bans nine known leaking GTK/libadwaita API paths.
A temporary compile probe produced all nine expected disallowed-API errors;
the probe was removed, and normal workspace Clippy passed. These unused APIs
were prevention checks, not nine newly observed application leaks.

## Native-library controls

Same machine, Cairo, isolated KWin/AT-SPI, GTK 4.22.4, libadwaita 1.9.3,
GLib 2.88.3 and GStreamer 1.28.7. All surfaces were actually opened and closed;
census differences, not absolute construction counts, were compared.

| Surface | Installed-library slope per five cycles | Private patched result |
| --- | --- | --- |
| About | +15 AdwBreakpoint | No retained types in three repeats. |
| File selector | +10 GCancellable, +5 GtkGestureLongPress | No retained types in three repeats. |
| Folder selector | +10 GCancellable, +5 GtkGestureLongPress | No retained types in three repeats. |

A minimal independent GTK control reproduced these library slopes. Private
builds of the same library versions isolated each fix; `/proc/PID/maps` verified
the loaded paths. The application sweep also passed all three surfaces with no
allowlist (`app-private-after.json`). The final breakpoint patch additionally
preserves object lifetime during removal and disconnects handlers on external
references; its two native tests and three subsequent About sweeps passed.

GTK fixes attach the missing gesture to its widget and release GtkPathBar's
asynchronous operation reference on error/cancellation. Cancelled operations
must not dereference an already destroyed pathbar. The libadwaita patch frees
owned breakpoint elements. See the adjacent upstream drafts for source patches.

The original reftrace output interleaved concurrent records and was rejected.
A private tracer serializing complete records with `flockfile` made it possible
to attribute the remaining cancellables. Its patch is also saved upstream-side.

## Memory, threads, descriptors and disk

The root component was recreated in one process; terminating one executable per
window would conceal the Relm4 retention. Release profiling used
`CARGO_PROFILE_RELEASE_DEBUG=true CARGO_PROFILE_RELEASE_STRIP=false` and
`heaptrack_print -f pure-after-25.zst -d pure-after-5.zst -l`, with an equivalent
before pair. Remaining 656-byte differential is attributed to GLib thread/TLS
initialization, not proof that every possible allocation is leak-free.

A temporary instrumented root test then displayed lists of 0, 16 and 256
receivers for 25 create/destroy cycles, three repetitions per size. Every run
finalized 25/25 panels. Descriptors stayed at 17 and thread counts stayed constant
within each run. With default allocation, anonymous memory sometimes stepped
up or back down by about 7.9 MiB, rather than increasing with each cycle.
Repeating the same binary with `MALLOC_TRIM_THRESHOLD_=0` isolated allocator
retention:

| Receiver count | Anonymous growth, cycles 5 → 25, three runs (KiB) |
| --- | --- |
| 0 | 12, 20, 52 |
| 16 | 28, 32, 28 |
| 256 | 24, 28, 20 |

Maximum observed slope was 2.6 KiB/cycle; no positive dependence on receiver
count was observed in retained growth. This is a measured bound for this journey,
not a universal memory budget. Both Pss and Anonymous were recorded; RSS was not
used. Cross-version/renderer numbers were not compared. All temporary source
probes were edited out; their patches/binaries remain with the raw evidence.

The service's three idle after-runs had Anonymous −4 KiB, stable descriptors,
zero `write_bytes`, and one startup worker exiting. Context switches were
compared only for surviving thread IDs; subtracting aggregate counts across
exiting threads had produced invalid negative values in an earlier probe.
The disconnected GUI had 9–15 CPU ticks and 4–8 KiB stderr writes per minute from
service reconnect diagnostics; this is not the connected production idle case.
No AT-SPI traversal ran during the 60-second sampling windows.

Settings already debounce writes by 400 ms, remove their pending source on
shutdown, and persist off the GTK thread. Shared atomic persistence is also used
for credentials, so its fsync contract was preserved. Media scanning, decoding
and duration inspection already use workers; thumbnail decoding has two slots
and a 16 MiB pixel cache. Protocol input, repair history, discovery lists and
media-command queues have explicit bounds and existing tests. Read-only review
of child-process owners found cancellation/timeouts and `kill_on_drop` on pactl
and installer commands. Actual NDI installation/host sound changes were not run.

## Validation and limits

Rust/cargo 1.98.1, clippy 0.1.98, rustfmt 1.9.0; edition 2024, declared MSRV 1.93
was not independently tested. Heavy jobs used one target directory, one lock,
one build job, incremental disabled, debug info disabled for ordinary gates,
executable TMPDIR under `~/.cache`, recorded PIDs and deadlines.

- Static leak lint: 107 files, zero errors/warnings. It missed the generated
  Relm4 ancestor closure; the lifecycle test supplied the missing evidence.
- Workspace tests: 414 passed, zero failed, 14 ignored, including doc-test stages.
- Workspace all-target check and Clippy with `-D warnings`: passed.
- Five isolated GTK tests passed separately: navigation, save-error teardown,
  discovery switch, lazy media thumbnails, and media queue edits.
- Private-bus service integration and standalone Relm4 regression: passed.
- Source hygiene: passed after removing the redundant vendor manifest backup.
  Project-tool tests: four passed. Formatting and `git diff --check`: passed.

The final NDI-only change passed the GUI owner's 18 tests and all-target Clippy
again, after removing the temporary thread probe.
No translations, package recipes, system libraries or installed units changed.
No cold-page-cache startup improvement is claimed. Physical receiver sessions,
portal capture, hardware encoders, host firewall/sound mutation, NDI runtime,
system boot/activation, the screenshot layout suite and sandboxed embedded-cover
loader remain outside the executed matrix. The full test log lists every ignored
case; skipped checks are not successes. Native fallback chooser results do not
certify a portal implementation or another renderer.

An initial combined library sweep exceeded its 240-second wrapper after three
successful controls and part of the application run. That incomplete application
result was not counted; a separate bounded application run completed cleanly.
All graphical work used isolated sessions. No signals were sent by process-name
matching. No upstream report or Git commit was pushed.
