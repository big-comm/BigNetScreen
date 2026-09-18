# Built-in NDI validation

## Optional installation, 2026-09-18

- Auxiliary runtime recipe removed from all local Git history. No SDK or vendor
  binary found in tracked history. Remote history has not been replaced.
- `libndi` is optional; the x86_64 Arch-family consent dialog offers installation
  from the external AUR recipe. Other systems get manual setup guidance.
- Missing-runtime and installed-runtime preflight tests pass without capture or
  publishing. The installed-runtime test uses an external library under `/tmp`.
- Installer mock checks cover success, cancelled authentication, failed clone,
  failed build, unprivileged build execution and temporary directory cleanup.
  ShellCheck, strict Clippy and 16 GUI tests pass.
- Real consent dialog checked in isolated Broadway. No installation performed
  on the host; live Polkit/AUR installation remains untested.
- All 12 dialog messages checked in 29 compiled catalogs. Local release rebuilt.

## Earlier streaming validation

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
