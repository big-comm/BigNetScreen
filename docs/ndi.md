# NDI publishing

The open-source GStreamer NDI plugin (MPL-2.0) is compiled into native
BigNetScreen builds; users do not install `gst-plugin-ndi`. No proprietary
runtime or SDK is bundled or downloaded during the application build.
NDI publishing requires a separately installed proprietary runtime, loaded only
when NDI is used. Opening the application does not start an NDI publication.

## Native packages

The BigNetScreen package lists `libndi` as an optional dependency. Miracast,
Chromecast and the rest of the application work without it. On Arch Linux and
derivatives, the [AUR package ndi-sdk](https://aur.archlinux.org/packages/ndi-sdk)
provides `libndi`. It contains proprietary software; installation is the user's
choice.

When NDI is selected without a usable runtime, a modal explains the proprietary
dependency before requesting screen capture. Native x86_64 Arch-family installations
offer an explicit Install button and a link to the AUR recipe. Installation:

1. Requests Polkit authentication through `pkexec` to install `base-devel` and
   `git` with `pacman --needed --noconfirm`.
2. Downloads the external `ndi-sdk` recipe into a private temporary directory.
3. Runs `makepkg --syncdeps --install --noconfirm` as the user. Its `PACMAN_AUTH`
   uses `pkexec` for dependency and package installation only.
4. Removes temporary build/download files and asks the user to restart the app.
   The GStreamer plugin caches failed runtime loads until process exit.

The app remains open during installation and reports authentication cancellation
or build errors. No application build or startup installs the runtime. Other
distributions, sandboxed apps and systems without Polkit/pacman receive manual
installation guidance and a link to the official NDI SDK page instead.

Avahi is used for discovery.
The package install/upgrade hook enables and starts `avahi-daemon.service` for
network discovery. A masked service is not unmasked; installation reports a
warning if service configuration fails.

This repository maintains only the application package recipe,
`pkgbuild/PKGBUILD`. Building and installing it does not require `libndi`.

Copying only the executable does not install the runtime. To use NDI with source
builds or other package formats, install an NDI 5/6 runtime and Avahi separately.
For source builds, enable discovery explicitly:

```sh
sudo systemctl enable --now avahi-daemon.service
```

If the runtime is outside the library search path, set `NDI_RUNTIME_DIR_V6`
(or `NDI_RUNTIME_DIR_V5`) to its library directory before launching the app.
Restart after installing a missing runtime.

`gst-inspect-1.0 ndisink` is not a test for this integration: an external GStreamer
process cannot see the plugin compiled into BigNetScreen. No separate system
plugin is required.

## Use the stream

1. In Settings, choose the computer name, resolution, FPS and audio inputs.
2. Under Home → Publish with NDI, choose Screen, Window or Extra screen.
3. On a receiver on the same LAN, select `BigNetScreen — <computer name>`.
   The receiver may prefix it with the sender's hostname.
4. Disconnect stops publication and capture.

For an OBS receiver on Linux, install OBS and DistroAV from Flathub:

```sh
flatpak install flathub com.obsproject.Studio com.obsproject.Studio.Plugin.DistroAV
```

Follow the [DistroAV installation guide](https://github.com/DistroAV/DistroAV/wiki/1.-Installation)
for the Avahi D-Bus permission required by current OBS Flatpak releases, or for
Windows/macOS installation. In OBS, add an **NDI Source** to a scene and select
the publication. The receiver's Flatpak runtime does not satisfy the native
sender's runtime dependency.

A receiver need not be present before publication starts. Sources are visible
to NDI receivers on the network; use a trusted LAN. Wired Gigabit is recommended
for high bandwidth video. A same-computer OBS test is possible, but does not
verify transmission between separate machines.

This sends raw video and audio through standard high bandwidth NDI. The app
does not offer NDI reception, HX encoding or automatic firewall changes. Native
packaging is the supported target; the Flatpak manifest does not bundle the
vendor runtime.

## Validation

```sh
cargo test -p nd-ndi --locked
# Requires the runtime, Avahi and access to the host network:
cargo test -p nd-ndi --locked ndi_loopback_receives_video_and_audio -- --ignored --nocapture
```

The ordinary tests cover built-in plugin registration and raw video/audio caps
without a vendor runtime. The opt-in loopback test publishes 720p30, discovers
the source and receives nonempty video and audio buffers through the real NDI
runtime. It does not replace testing OBS or a receiver on a second computer.

Suggested external receiver checks: 720p30/1080p30/custom size, microphone/system
audio, A/V synchronization, receiver join/leave, repeated Start/Stop, capture
cancellation, CPU/RSS/network use.

The bundled GStreamer plugin uses MPL-2.0; the vendor runtime retains its own
license. Runtime distributors must retain the required license and third-party notices.
Distribution requirements remain those of the selected SDK.
NDI® is a registered trademark of Vizrt NDI AB.

References:
- [GStreamer plugin](https://github.com/GStreamer/gst-plugins-rs/blob/main/net/ndi/README.md)
- [Static plugin integration](https://github.com/GStreamer/gst-plugins-rs#static-linking)
- [NDI SDK licensing](https://docs.ndi.video/all/developing-with-ndi/sdk/licensing)
- [Linux requirements](https://docs.ndi.video/all/developing-with-ndi/sdk/platform-considerations)
