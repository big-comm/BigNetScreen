# Built-in NDI validation

2026-09-17, Linux x86_64, GStreamer 1.28.7.

- GStreamer NDI 0.15.3 compiled statically into `nd-ndi`.
- Registration and raw video/audio tests pass without a system NDI plugin/runtime.
- Official NDI 6.3.2 runtime packaged with its license notices; SDK archive
  SHA-256 matches the recipe. Tested from a temporary package root, not installed.
- Real 720p30 loopback: publication discovered through Avahi; received 10 video
  and 30 audio buffers. Test uses the same sender pipeline description as the app.
- Focused Clippy (`nd-ndi`, `nd-gui`, all targets, warnings denied) passes.
- Workspace tests: 235 passed, 4 opt-in tests skipped; NDI loopback run separately.
  Network tests required execution outside the sandbox.
- Debug and release builds pass. Native x86_64 application/runtime packages
  generated under `build/packages`; dependency metadata, install hook, setup guide
  and all 29 compiled catalogs verified inside the archives.
- All 29 catalogs contain 156 translated entries; no fuzzy/untranslated entries.
  Changed message translated with Luna xhigh and independently checked.
- Package install hooks checked with mocked systemctl, including masked-service
  failure. No host service configuration was changed.
- aarch64 runtime recipe staging selects an AArch64 ELF library; not executed.

Limits: loopback does not validate a second machine, OBS, live desktop capture,
audio synchronization or aarch64 runtime execution. The vendor library has
namcap warnings for partial RELRO and unused compatibility libraries; it is
packaged unchanged. Namcap cannot evaluate the main recipe's existing dynamic
`date` version fields; use makepkg metadata and the built package instead.
