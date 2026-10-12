# Packaging and rolling-release resilience

The installed application is a native Rust executable, not a Python package. A Python minor-version change does not justify adding a launcher that scans site-packages. Development helpers use `python3`; they are not installed as the product's runtime. Native ABI and GStreamer plugin availability remain real rolling-release concerns and should be checked by the distribution's packaging/build tests.

`pkgbuild/PKGBUILD` derives `pkgver`/`pkgrel` from the build clock, because the distribution publishes rolling builds of the branch. The UI About dialog and `--version` report the same `yy.mm.dd`: `crates/nd-gui/build.rs` runs the same `date` with the same format, honouring `SOURCE_DATE_EPOCH` when a reproducible build sets one. The Cargo workspace version is crate metadata and differs from that number. The date names a build, so anything announced as stable must name the commit it came from.

`make install` supports PREFIX, DESTDIR and CARGO_TARGET_DIR. Build as an unprivileged user; stage installation into an empty DESTDIR, inspect its files and run the distribution's package validator before publishing. Keep the GPL license and optional plugin's license/provenance. Do not ship the proprietary NDI runtime as if it were GPL code.

`pkgbuild/pkgbuild.install` enables and starts `avahi-daemon.service` on installation, because NDI discovery does not work without it and the distribution ships images that are never configured by hand. Upgrades leave the service as the administrator set it. That is the only service a hook touches; NDI and P2P failures are reported as application guidance. Global firewall disabling and permanent permissive rules are not packaging fixes, and a temporary application lease never owns a port that was already open.

The Flatpak manifest uses a portal-provided PipeWire FD rather than exposing the entire PipeWire socket. Portal D-Bus access is granted by Flatpak by default. Retain only actual network, display, audio, graphics and optional Avahi permissions. Test the assembled sandbox: syntax validation alone does not prove runtime/plugin completeness.

The release checklist requires a clean distribution build and an installation, upgrade and removal test. Preserve source hashes and exact build parameters; a deterministic source archive still builds different binaries on different systems.

## Archive the revision a release was built from

`makepkg` builds the checkout that holds `pkgbuild/PKGBUILD` in place, so the
package itself needs nothing generated. What
a published release does need is a record of *which* commit it came from:

```sh
python3 scripts/dist.py --output ../bignetscreen-release-source
cd ../bignetscreen-release-source
sha256sum -c SHA256SUMS
# Commit and SOURCE_DATE_EPOCH are in source-build.json.
```

The helper produces the exact Git source archive (including the tracked NDI
plugin), its SHA-256 and that provenance file. It refuses a dirty worktree and
refuses to overwrite an existing output directory. It does not install anything,
create a tag, build a package, or certify the resulting binary.
