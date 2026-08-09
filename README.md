<div align="center">

# BigNetScreen

**Mirror your Linux screen to Miracast (Wi-Fi Display) and Chromecast receivers.**

A clean **Rust** rewrite — light, low latency, modern interface.

[![License: GPL-3.0-or-later](https://img.shields.io/badge/License-GPL%203.0--or--later-blue.svg)](COPYING)
[![Language: Rust](https://img.shields.io/badge/Language-Rust-CE412B.svg)](https://www.rust-lang.org/)
[![GTK 4 / libadwaita](https://img.shields.io/badge/GTK4-libadwaita-4A86CF.svg)](https://gtk.org)
[![Translations: 29 languages](https://img.shields.io/badge/Translations-29%20languages-brightgreen.svg)](po/)

</div>

---

## What it does

Pick a receiver from the list, choose a screen or a window, and your desktop
shows up on the other device. Two protocols, no configuration:

| | Transport | Best for |
| --- | --- | --- |
| **Miracast** (Wi-Fi Display) | a direct Wi-Fi Direct link | using the computer on the big screen |
| **Chromecast** | your local network | video, slides, presenting |

Both paths carry **system audio** alongside the picture, captured from the
default output's monitor — the sound keeps playing on your laptop *and* comes
out of the receiver, the same way a browser's "share tab audio" works.

## Status

Both protocols work end to end and have been validated against real hardware —
a Samsung Miracast TV, a Samsung LSP3 Miracast projector and Xiaomi Chromecast
projectors.

- **Miracast**: 1920x1080@60, hardware encoding, negotiated straight from the
  sink's declared native mode.
- **Chromecast**: uses the receiver's **mirroring app** (encrypted RTP with
  retransmission), not the media player. It falls back to an HTTP stream only
  on devices with no mirroring support.
- **Not yet tested**: capturing a **virtual monitor** (an extended desktop
  rather than a mirrored one).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the roadmap and the open items.

## About latency

The numbers here are measured, not estimated, and the distinction matters:
most of the delay you see is usually **not** in the sender.

Measured with an on-screen millisecond stopwatch photographed next to the
receiver's image, at 1920x1080@60 with VA-API encoding:

| Segment | Time |
| --- | --- |
| Inside our pipeline (capture → encode → packetise → network) | **~85 ms** (worst case ~125 ms) |
| Network, decoding and the receiver's own image processing | **the remainder** |

On one Miracast projector the end-to-end total came to ~400 ms, meaning roughly
315 ms of it belonged to the projector. On receivers with a low-latency or
"game" picture mode, the same ~85 ms of sender-side work yields a far smaller
total.

You can reproduce the sender-side measurement yourself:

```sh
BIGNETSCREEN_LATENCY=1 RUST_LOG=info,nd_core=info bignetscreen
# → latency inside the pipeline (no network, no receiver)
#   element=udpsink0 avg_ms=83 worst_ms=125
```

## Installing

### Arch / BigLinux (native — full features)

```sh
makepkg -si          # see PKGBUILD
```

### Flatpak

```sh
flatpak-builder --user --install build build-aux/flatpak/br.com.biglinux.BigNetScreen.yaml
```

> Under Flatpak, **Chromecast works fully; Miracast does not.** The sandbox has
> no access to NetworkManager and firewalld on the system bus, which Wi-Fi
> Direct requires. The app says so in the interface instead of failing
> silently.

## Building from source

System dependencies: `gtk4 ≥ 4.10`, `libadwaita ≥ 1.7`, `gstreamer ≥ 1.20` with
`gst-plugins-{base,good,bad,ugly}`, `gst-libav` (AAC), `gst-plugin-pipewire`,
plus the Rust toolchain. For Miracast: `networkmanager`, and `firewalld` if you
use it.

```sh
make run                     # builds the translations and opens the GUI
make check                   # fmt + clippy -D warnings + tests
sudo make install PREFIX=/usr
```

Or straight through Cargo, without installed translations:

```sh
cargo run -p nd-gui
cargo test --workspace
```

## Configuration

Everything works with no configuration; these exist for debugging and for
unusual setups.

| Variable | Effect |
| --- | --- |
| `RUST_LOG=debug` | verbose logging (defaults to `info`) |
| `BIGNETSCREEN_CAPTURE=portal\|mutter` | force the capture backend |
| `BIGNETSCREEN_ENCODER=x264enc` | force an encoder (by default it tries the GPU and falls back to software on its own) |
| `BIGNETSCREEN_PIPELINE_LATENCY_MS=N` | force the pipeline latency (`0` = automatic) |
| `BIGNETSCREEN_LATENCY=1` | measure and log the per-stage latency |
| `BIGNETSCREEN_LOCALEDIR=<dir>` | translations outside the install prefix |
| `NETWORK_DISPLAYS_DUMMY=1` | inject fake receivers to exercise the UI |

## How it works

A Cargo workspace, with everything protocol-related kept out of the GUI:

| Crate | Role |
| --- | --- |
| `nd-core` | The `Provider`/`Sink`/`CaptureBackend` traits, shared types and the **GStreamer pipeline construction**. No GUI. |
| `nd-capture` | Screen capture: the desktop portal (`ashpd`) and Mutter directly (`zbus`). |
| `nd-net` | NetworkManager (Wi-Fi Direct), firewalld and GPU driver detection. |
| `nd-chromecast` | mDNS discovery, the Cast channel (protobuf over TLS), the mirroring session (RTP/RTCP) and the HTTP stream server. |
| `nd-wfd` | RTSP server, WFD M1–M7 negotiation and Miracast cast orchestration. |
| `nd-gui` | The **relm4 + libadwaita** application (the `bignetscreen` binary). |

Two design decisions worth knowing about:

**Radio silence while streaming.** Scanning for Wi-Fi Direct peers and streaming
compete for the same antenna, and every scan hop mutes the link for tens of
milliseconds. Video recovers by asking for retransmission; audio has a playout
deadline, and whatever misses it becomes a hole in the sound. So discovery
scanning pauses for the duration of a session — see
[`nd-core::radio`](crates/nd-core/src/radio.rs).

**No proprietary components.** Every dependency is MIT, Apache-2.0, BSD, ISC,
Zlib or GPL-compatible, and `cargo deny` enforces that in CI (see
[`deny.toml`](deny.toml)). The Cast protocol was implemented using Google's own
open-source [Open Screen](https://chromium.googlesource.com/openscreen/) as a
specification reference, with none of its code linked in.

## Development

```sh
make check                          # fmt + clippy -D warnings + the full test suite
cargo test --workspace
```

Examples that talk to real hardware:

```sh
cargo run -p nd-net --example p2p_scan                     # Wi-Fi Direct peers
cargo run -p nd-wfd --example wfd_negotiate                # a Miracast sink's capabilities
cargo run -p nd-wfd --example wfd_cast -- screen           # cast the screen over Miracast
cargo run -p nd-chromecast --example mdns_scan             # find a receiver's IP
cargo run -p nd-chromecast --example cast_status -- <IP>   # read-only, safe
cargo run -p nd-capture --example spike_capture_encode
```

The stream server **without needing a Chromecast** (it exercises the whole
path):

```sh
cargo run -p nd-chromecast --example cc_stream_local
# in another terminal, with the printed URL:
curl -s -o /tmp/cc.mkv --max-time 5 '<URL>' && ffprobe /tmp/cc.mkv
```

Real casting — ⚠️ **this takes over the receiver**:

```sh
cargo run -p nd-chromecast --example cc_cast -- <IP>            # test pattern
cargo run -p nd-chromecast --example cc_cast -- <IP> screen 30  # the screen, for 30s
cargo run -p nd-chromecast --example cc_mirror -- <IP>          # low-latency mirroring
```

## Translating

The interface ships in **29 languages**. The source strings are English;
catalogues live in [`po/`](po/) and are handled with standard gettext tooling.

```sh
./po/update-pot.sh                   # refresh the template from the sources
msginit --locale=fr --input=po/bignetscreen.pot --output=po/fr.po
echo fr >> po/LINGUAS
```

See [`po/README.md`](po/README.md) for details.

## Credits

- Reference project:
  [GNOME Network Displays](https://gitlab.gnome.org/GNOME/gnome-network-displays)
  — this is a clean rewrite rather than a fork, and that project served as a
  protocol reference.
- Rust rewrite and tuning: **[BigCommunity](https://github.com/big-comm)** /
  Tales A. Mendonça.

## License

[GPL-3.0-or-later](COPYING).
