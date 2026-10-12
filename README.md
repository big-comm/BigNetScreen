# BigNetScreen

**Put your Linux screen where people can see it: a TV, a projector, or a web browser.**

[Português brasileiro](README.pt-BR.md) · [Get started](#try-it) · [Developer guide](docs/development.md) · [Report compatibility](SUPPORT.md)

Present a window without sharing the rest of your desktop, send local video and music to a television, or create a virtual screen on a supported compositor. BigNetScreen brings screen capture, receiver discovery and streaming controls into one native GTK4/libadwaita application, written in Rust.

## What you can do

| Destination | What it needs | What to expect |
| --- | --- | --- |
| Chromecast / Google Cast | Computer and receiver on the same trusted LAN | Raw-RTP mirroring where supported; HTTP media fallback otherwise. Files and music can be sent too. |
| Miracast / Wi-Fi Display | Native installation, compatible Wi-Fi Direct hardware and receiver | A direct wireless link, managed through NetworkManager. |
| DLNA television | Computer and television on the same trusted LAN | A live stream played by the television's media player, which buffers more than mirroring does. Files and music can be sent too. |
| Web browser | The gst-plugins-rs WebRTC plugin and the libnice plugin on the sender | A local page and PIN, without installing a viewer application. |
| NDI receiver | Separately installed proprietary NDI runtime | Optional publishing for compatible production tools. Other paths do not need NDI. |

Hardware H.264 encoding is preferred where the driver and plugins can actually produce frames; software encoding remains available. Resolution, frame rate and quality depend on the source, receiver, encoder and network. No single latency or FPS figure applies to every combination.

The current version is a release candidate. What it takes to call a build stable, including tests on physical receivers, is in the [release section](CONTRIBUTING.md#releasing) of the contributor guide.

## Try it

On a distribution providing BigNetScreen, install its `bignetscreen` package through your software manager. Otherwise follow the [native build instructions](docs/development.md). The repository contains a Flatpak manifest, but no Flathub package is published.

1. Open **BigNetScreen** and put the receiver in its Cast or Screen Mirroring mode.
2. Choose your screen, a window, or a virtual screen, then select the destination. Approve the desktop's capture dialog.
3. For browser viewing, open the address or QR code shown by the app on the viewing device and enter its PIN.
4. Use **Stop** before changing destination. Closing the window does not stop sharing: a background service keeps the transmission running until you stop it.

For a first test, use a private home or office network, a short presentation and a receiver close to the access point. Guest-network isolation, VPN routes and firewall rules can prevent discovery or the receiver's connection back to your computer. [Troubleshooting](SUPPORT.md) explains how to tell these failures apart.

## Boundaries worth knowing

Cast mirroring and Cast HTTP are different transports. Mirroring sends encoded access units over Cast RTP; HTTP serves H.264/AAC in MPEG-TS to a media player that may prebuffer seconds. Switching to HTTP is a compatibility fallback, not a low-latency guarantee.

Under Flatpak, capture goes through the desktop portal and Miracast is unavailable, because it needs NetworkManager on the system bus. Virtual screens depend on the desktop. Window capture asks for a fresh selection each time; the portal can remember a screen authorization.

Use trusted networks only. A Chromecast is remembered by the device key it proves on first use, but that key is not checked against Google's certificate authority, so the first connection is not authenticated. The browser page, its PIN and the HTTP fallback are not encrypted. Do not expose the application's ports to the Internet; the [security model](SECURITY.md) has the details.

## Build, improve, and share results

The [architecture](ARCHITECTURE.md) maps the crates; the [contributor guide](CONTRIBUTING.md) explains how to submit a focused change with evidence. Protocol notes, tests and packaging instructions are in [docs](docs/).

A useful contribution can be small: report your receiver and firmware with a reproducible result, improve a translation, or test repeated reconnections. If BigNetScreen solves a problem for you, star the repository and tell other Linux users about it.

## License

BigNetScreen is [GPL-3.0-or-later](COPYING). The tracked NDI GStreamer plugin keeps its MPL-2.0 license and documented local patches. The proprietary NDI runtime is not included. Cast, Miracast, DLNA and NDI names identify interoperability targets, not affiliation or certification.
