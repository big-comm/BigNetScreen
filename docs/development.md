# Development

## Native setup

You need Linux, Rust 1.98.1 or newer (the MSRV in `Cargo.toml`), a C compiler and linker, pkg-config, gettext and the development files of:

| Library | Minimum | Why that version |
| --- | --- | --- |
| GTK 4 | 4.22 | `big-gtk-kit` enables the `v4_22` API |
| libadwaita | 1.9 | `big-gtk-kit` enables the `v1_9` API |
| GStreamer, gst-plugins-base, gst-plugins-bad | 1.22 | the bundled WebRTC plugin enables the `v1_22` API |

`big-gtk-kit` is the shared BigLinux widget crate, fetched from <https://github.com/biglinux/big-gtk-kit> at the commit pinned in `Cargo.lock`, so the first build needs network access.

Runtime plugins vary by transport: base/good/bad/ugly/libav, PipeWire capture, audio capture and encoders; browser viewing also needs the GStreamer libnice plugin. `pkgbuild/PKGBUILD` lists the full set. Native Miracast requires NetworkManager with Wi-Fi Direct support. The NDI runtime and hardware encoders are optional, not substitutes for the software test path.

## Build and run from a checkout

The window is a D-Bus client of the session service, `bignetscreend`. An installed package lets D-Bus start the service on demand; a checkout does not install that activation file, so start the service yourself first:

```sh
git clone https://github.com/big-comm/BigNetScreen.git
cd BigNetScreen
cargo run --locked -p nd-service --bin bignetscreend -- --foreground
```

`--foreground` keeps it from exiting when idle. In a second terminal, compile the translations and open the window:

```sh
make run
```

On a machine with the `bignetscreen` package installed, start yours before opening the window: otherwise D-Bus activates the installed service. A service started later takes the bus name over from a running one, but the old process keeps any session it was running.

`make` builds the release binaries and translations; `make install` takes `PREFIX` and `DESTDIR` (see [packaging](packaging.md)). A sandbox-built binary linked to isolated native libraries is a test artifact, not a portable distribution package.

## Offline review bundle

The code and buildenv archives unpack side by side into `bignetscreen-audit`; neither is part of this repository. Read BUILD-ENV.md and inspect setup.sh before running it. It provides Cargo sources, headers/.pc files and link symlinks, not a guarantee that every runtime library exists in every recreated executor.

```sh
cd bignetscreen-audit
./setup.sh
source ./build-env.sh
export PATH=/path/to/isolated/rust-1.98.1/bin:$PATH
export CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
cd repo
cargo test --offline --locked -p nd-core -p nd-chromecast
```

Verify rustc/cargo/clippy/rustfmt versions and actual linker dependencies. A successful pkg-config query only proves metadata exists. Do not replace libc/Python/graphics packages to repair a review sandbox: extract compatible missing files into a private prefix and record it. Never ship absolute sandbox paths or `.cargo/config.toml` that points into a local vendor.

Use isolated HOME and XDG directories for tests. Keep one Cargo target directory and sequential gates; see [executor safety](executor-safety.md). Do not copy target, toolchains or Cargo caches into checkpoints. Do preserve `vendor/gst-plugin-ndi` because it is actual project source.
