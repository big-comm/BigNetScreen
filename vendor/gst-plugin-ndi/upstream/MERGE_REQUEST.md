# ndi: expose the receiver count on ndisink and add runtime diagnostics

Target: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs (branch `main`)
Patch: `0001-ndi-receiver-count-and-runtime-info.patch` (apply with `git apply`)

## Why

An application publishing a screen over NDI has no way to tell whether anyone
is receiving it: the sink reports "playing" as soon as the sender exists. The
runtime knows (`NDIlib_send_get_no_connections`), and OBS's NDI plugin
(DistroAV) polls it once a second to log connections. Likewise, when the
runtime cannot be used, applications need to distinguish "libndi is not
installed" (offer to install it) from "libndi is installed but
`NDIlib_initialize` refuses this CPU" (installing again will not help), and
to log which runtime version was found. None of this is reachable through the
elements today.

## What

1. `ndisink` gains a read-only `connections` (gint, default -1) property.
   While rendering, the sender is asked for its connection count at most once
   per second; the value is stored in an atomic and `notify::connections` is
   emitted when it changes. `-1` until the first frame after `start()`.

2. `gstndi::runtime_info()` loads the runtime and returns
   `RuntimeInfo::{Missing(String), CpuUnsupported{version}, Ready{version}}`,
   using `NDIlib_version` and `NDIlib_initialize`. Both symbols are added to
   the FFI table; `SendInstance::connections()` wraps
   `NDIlib_send_get_no_connections(instance, 0)`.

No behaviour changes for existing users; the two new symbols exist in every
NDI 5 and 6 runtime.

## Testing

`cargo check -p gst-plugin-ndi` and `cargo clippy -p gst-plugin-ndi -D warnings`
are clean on `main` (d6998e2). End to end, with NDI runtime 6.3.2 on Linux, a
sender fed by `videotestsrc` reports `connections == 1` within a second of an
`ndisrc` receiver connecting, and `runtime_info()` returns
`Ready { version: Some("NDI SDK LINUX ... 6.3.2.0") }`.
