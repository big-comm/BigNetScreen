# BigNetScreen

**Put your Linux screen where people can see it: a TV, a projector, or a web browser.**

[Português brasileiro](README.pt-BR.md) · [Get started](#try-it) · [Developer guide](docs/development.md) · [Report compatibility](SUPPORT.md)

Present a window without sharing the rest of your desktop, watch local media on a bigger display, or create a virtual screen on a supported compositor. BigNetScreen brings screen capture, receiver discovery and streaming controls into one native GTK4/libadwaita application, written in Rust.

## What you can do

| Destination | What it needs | What to expect |
| --- | --- | --- |
| Chromecast / Google Cast | Computer and receiver on the same trusted LAN | Raw-RTP mirroring where supported; HTTP media fallback otherwise. |
| Miracast / Wi-Fi Display | Native installation, compatible Wi-Fi Direct hardware and receiver | A direct wireless link, managed through NetworkManager. |
| Web browser | GStreamer WebRTC/ICE plugins on the sender | A local page and PIN, without installing a viewer application. |
| NDI receiver | Separately installed proprietary NDI runtime | Optional publishing for compatible production tools. Other paths do not need NDI. |

Hardware H.264 encoding is preferred where the driver and plugins can actually produce frames; software encoding remains available. Resolution, frame rate and quality depend on the source, receiver, encoder and network. No single latency or FPS figure applies to every combination.

**Release status:** this branch is undergoing release-candidate validation. Automated checks and receiver tests are separate requirements; see the [release checklist](docs/releasing.md). A working pipeline or a passing unit test is not a certification for every TV.

## Try it

On a distribution providing BigNetScreen, install its `bignetscreen` package through your software manager. Otherwise use the [native build instructions](docs/development.md). This repository also contains a Flatpak manifest; it is not a claim that a published Flathub package exists.

1. Open **BigNetScreen** and put the receiver in its Cast or Screen Mirroring mode.
2. Choose your screen, a window, or an available virtual monitor, then select the destination. Approve the desktop's capture dialog.
3. For browser viewing, open the address or QR code shown by the app and enter its PIN.
4. Use **Stop** before changing destination. Closing the window requests cleanup and waits rather than abandoning the receiving session.

For a first test, use a private home/office network, a short presentation and a receiver close to the access point. Guest-network isolation, VPN routes and firewall rules can prevent discovery or the receiver's connection back to your computer. [Troubleshooting and useful diagnostics](SUPPORT.md) explain how to distinguish these failures.

## A few important boundaries

Cast mirroring and Cast HTTP are different transports. Mirroring sends encoded access units over Cast RTP; HTTP serves H.264/AAC in MPEG-TS to a media player that may prebuffer seconds. Switching to HTTP is a compatibility fallback, not a low-latency guarantee.

Portal capture is used under Flatpak. Miracast needs system networking APIs and is supported by the native installation. Virtual monitor availability depends on the desktop/backend. Window capture asks for a fresh selection; persistent screen authorization remains controlled by the portal.

Use trusted networks only. Cast device-identity authentication is not yet implemented, and the browser PIN/control page and HTTP fallback are not end-to-end encrypted. Do not expose the application's ports to the Internet. Read the [security model](SECURITY.md) before deployment on a shared or hostile network.

## Build, improve, and share results

The [architecture](ARCHITECTURE.md) maps the eight crates; the [contributor guide](CONTRIBUTING.md) explains how to submit a focused change with evidence. [AGENTS.md](AGENTS.md) contains the shared engineering rules used by maintainers and coding agents. Protocol notes, reproducible tests and packaging instructions are indexed in [docs](docs/README.md).

A useful contribution can be small: report your receiver/firmware and a reproducible result, improve a translation, or help test repeated reconnections. If BigNetScreen solves a problem for you, star the repository and share it with other Linux users. Compatibility reports are more valuable than unqualified performance claims.

## License

BigNetScreen is [GPL-3.0-or-later](COPYING). The tracked NDI GStreamer plugin has its own MPL-2.0 license and documented local patches. The proprietary NDI runtime is **not** included. Cast, Miracast and NDI names identify interoperability targets, not affiliation or certification.
