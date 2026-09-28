# Local additions to gst-plugin-ndi

This directory is gst-plugin-ndi 0.15.3 from
<https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs> (MPL-2.0, see
`LICENSE-MPL-2.0`), with the following additions. They are kept small so they
can be proposed upstream and this copy dropped afterwards.

## `ndisink`: read-only `connections` property

`NDIlib_send_get_no_connections` is polled at most once per second while
frames are rendered, and the result is exposed as a read-only integer property
(`-1` until the first frame). A `notify::connections` signal fires whenever the
count changes. This lets an application tell the user whether anyone is
actually watching the published stream.

## `runtime_info()`

Loads the runtime and returns whether it is missing, present but refusing to
initialise on this CPU (`NDIlib_initialize` returned false), or ready, along
with the version string from `NDIlib_version`. The elements alone cannot make
that distinction, and the error the user is shown should.

## Upstream submission

The same change against gst-plugins-rs `main` is in `upstream/`, with the
merge request text. Once it is merged and released, point `nd-ndi` back at
crates.io and delete this directory.

## Changed files

- `Cargo.toml` (description/readme; `quick-xml` 0.41, see below)
- `src/lib.rs`
- `src/ndi.rs`
- `src/ndisys.rs`
- `src/ndisink/imp.rs`

## `quick-xml` 0.41

The released 0.15.3 asks for `quick-xml` 0.40, which is affected by
RUSTSEC-2026-0194 and RUSTSEC-2026-0195 (quadratic duplicate-attribute check,
unbounded namespace allocation). The receiving side (`ndisrcdemux`) parses
closed-caption XML from the network, so the requirement is raised to the fixed
0.41 even though BigNetScreen only publishes. Drop this
change once an upstream release requires 0.41 or later.
