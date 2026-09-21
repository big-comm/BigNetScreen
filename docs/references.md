# Technical references

Consult the locked dependency version as well as current upstream documentation. These are design references, not proof that every code path was executed or every receiver is compatible.

- [Cargo commands and testing](https://doc.rust-lang.org/cargo/commands/cargo-test.html), [profiles and memory-related build settings](https://doc.rust-lang.org/cargo/reference/profiles.html).
- [GStreamer clocks, segments and synchronization](https://gstreamer.freedesktop.org/documentation/application-development/advanced/clocks.html), [MPEG-TS muxer](https://gstreamer.freedesktop.org/documentation/mpegtsmux/mpegtsmux.html), [appsink](https://gstreamer.freedesktop.org/documentation/app/appsink.html).
- [ScreenCast portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html), [PipeWire GStreamer source](https://gitlab.freedesktop.org/pipewire/pipewire/-/blob/master/src/gst/gstpipewiresrc.c).
- [Google Cast media support](https://developers.google.com/cast/docs/media), [Open Screen source](https://chromium.googlesource.com/openscreen/), [Android Cast session management](https://developers.google.com/cast/docs/android_sender/integrate).
- [GTK close-request](https://docs.gtk.org/gtk4/signal.Window.close-request.html), [Flatpak permissions](https://docs.flatpak.org/en/latest/sandbox-permissions.html).
- [makepkg configuration](https://man.archlinux.org/man/makepkg.conf.5), [Arch clean build tools](https://man.archlinux.org/man/archbuild.1).
- [AGENTS.md format](https://agents.md/).

The original review and development reports are retained under audit and history. Check their dates and explicit environment limitations before reusing a measurement. Community reports must include an identifiable receiver/firmware and reproduce locally before being elevated to an implementation requirement.
