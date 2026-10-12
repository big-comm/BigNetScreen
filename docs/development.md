# Development

## Native setup

You need Linux, Rust 1.98.1 or newer (the MSRV in `Cargo.toml`), a C compiler and linker, pkg-config, gettext and the development files of:

| Library | Minimum | Why that version |
| --- | --- | --- |
| GTK 4 | 4.22 | `big-gtk-kit` enables the `v4_22` API |
| libadwaita | 1.9 | `big-gtk-kit` enables the `v1_9` API |
| GStreamer, gst-plugins-base | 1.18 | the NDI plugin enables the `v1_18` API |

`big-gtk-kit` is the shared BigLinux widget crate, fetched from <https://github.com/biglinux/big-gtk-kit> at the commit pinned in `Cargo.lock`, so the first build needs network access.

Runtime plugins vary by transport: base/good/bad/ugly/libav, PipeWire capture, audio capture and encoders; browser viewing also needs `whepserversink` from gst-plugins-rs (`gst-plugin-rswebrtc` on Arch) and the GStreamer libnice plugin; its tests fail without them. `pkgbuild/PKGBUILD` lists the full set. Native Miracast requires NetworkManager with Wi-Fi Direct support. The NDI runtime and hardware encoders are optional, not substitutes for the software test path.

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

On a machine with the `bignetscreen` package installed, start yours before opening the window: otherwise D-Bus activates the installed service. A service started later takes the bus name over from a running one, which stops its sessions and exits.

`make` builds the release binaries and translations; `make install` takes `PREFIX` and `DESTDIR` (see [packaging](packaging.md)).

## Tests

Use isolated HOME and XDG directories for tests and run the gates one after another in one Cargo target directory. `vendor/gst-plugin-ndi` and `vendor/relm4` are project source; target directories, toolchains and Cargo caches are not.
