# Development

## Native setup

Use Linux, Rust/Cargo, a C compiler/linker, pkg-config and gettext. The declared MSRV is in Cargo.toml; this review runs Rust 1.98.1. The dependencies are GTK4 (at least the selected 4.10 API), libadwaita (1.7 API), GLib, GStreamer and development headers matching the runtime. GStreamer 1.26 is the review environment; check the locked gstreamer-rs requirements before lowering the supported floor.

Runtime plugin needs vary by transport: base/good/bad/ugly/libav, PipeWire capture, audio capture and encoders; browser viewing additionally needs the GStreamer libnice plugin. Native Miracast requires compatible NetworkManager/P2P. NDI runtime and hardware encoders are optional, not substitutes for the software test path.

```sh
git clone https://github.com/big-comm/BigNetScreen.git
cd BigNetScreen
cargo build --locked -p nd-gui
make locale
BIGNETSCREEN_LOCALEDIR="$PWD/build/locale" cargo run --locked -p nd-gui
```

Select the branch/commit you intend to test before building. For a release build use `cargo build --release --locked -p nd-gui`. A sandbox-built binary linked to isolated native libraries is a test artifact, not a portable distribution package.

## Offline review bundle

The code and buildenv archives unpack side by side into `bignetscreen-audit`. Read BUILD-ENV.md and inspect setup.sh before running it. It provides Cargo sources, headers/.pc files and link symlinks, not a guarantee that every runtime library exists in every recreated executor.

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
