# Architecture and Roadmap — BigNetScreen (Rust)

A clean rewrite of BigNetScreen. The original C project was treated as a
**reference specification / test oracle** during the port.

## Principles

1. **A GUI-free, testable core** (`nd-core`). Protocol and tuning do not depend
   on GTK.
2. **Abstractions isolate environment and protocol.** Two axes of variation:
   - *Capture*: portal vs. Mutter (needed for Flatpak vs. native).
   - *Protocol*: Chromecast vs. WFD (the `Provider`/`Sink` traits).
3. **Errors through `Result`**, never through a pointer — this rules out the
   class of bug that crashed the C daemon (`GError` by value).
4. **Failure is always visible.** No error path may end in a silent log entry:
   the user needs to know *why* a receiver did not show up.
5. **Tuning applied, not merely declared.** A latency constant that never
   reaches `gst_pipeline_set_latency` is documentation, not engineering.

## Crate map

```
nd-core         traits + types + pipeline.rs (GStreamer tuning)   ← the heart
  ├── nd-capture   PortalBackend (ashpd) | MutterBackend (zbus)
  ├── nd-net       NetworkManager + firewalld + GPU driver (native only)
  ├── nd-chromecast  mDNS + Cast (protobuf/TLS) + mirroring + HTTP stream server
  ├── nd-wfd       RTSP(7236) + WFD M1–M7 negotiation + P2P (uses nd-net)
  └── nd-gui       relm4 + libadwaita  → the `bignetscreen` binary
```

### Key abstractions (in `nd-core`)

- `capture::CaptureBackend` — `start(SourceType) -> CaptureSource{fd, node_id}`,
  plus `supported_sources()` (not every portal offers window/virtual capture).
  `CaptureSource::video_source()` is the **only** legitimate constructor of the
  pipeline's source: it guarantees the right `fd=`/`path=`.
- `provider::Provider` — `discover()` returns a stream of `DiscoveryEvent`,
  which includes `ProviderUnavailable { reason }` so the UI can explain
  failures.
- `sink::Sink` — an explicit state machine (`SinkState`) backed by `SinkStatus`,
  where `Error` is **terminal** and poisoning-proof.
- `pipeline` — encoder selection from **GStreamer's registry** crossed with the
  KMS driver, a per-encoder description (each with its own bitrate unit and
  low-latency flags), and `build_pipeline()`, which applies the latency,
  delivers bus errors over a channel and returns the pipeline in `Ready`.
- `radio` — radio silence while streaming: discovery scanning pauses so it does
  not fight the session for the antenna.

## Concurrency model

- GStreamer + relm4 on the glib main context.
- A tokio runtime (relm4's worker threads) for discovery, the Cast channel and
  sockets; bridged to the GUI through relm4 messages. No callback soup, no
  hand-rolled races.
- D-Bus through **signals**, not polling: NetworkManager's
  `PeerAdded`/`PeerRemoved`.

## Settled decisions

| Topic | Decision |
| --- | --- |
| GUI | relm4 0.11 + libadwaita 0.9 (AdwToolbarView/StatusPage/Banner/Spinner) |
| Protocols | Chromecast **and** WFD (both in scope); AirPlay discovery only |
| Distribution | Native (PKGBUILD + Makefile) **and** Flatpak |
| i18n | gettext, English `msgid`s, extraction through `po/extract.py`, 29 languages |
| Capture | Portal by default; Mutter directly only for a virtual monitor or `BIGNETSCREEN_CAPTURE=mutter`, falling back to the portal automatically if it fails |
| Chromecast streaming | The receiver's mirroring app when available (RTP), with the hand-written HTTP path as the fallback |

### The Flatpak × Miracast caveat

WFD/Miracast needs NetworkManager (Wi-Fi Direct) plus firewalld on the
**system** bus, both blocked inside the Flatpak sandbox. Therefore:
**Chromecast runs fully under Flatpak; WFD runs fully only in the native
build.** The app detects the environment (`nd_capture::is_sandboxed`) and says
so in the interface, rather than simply showing nothing.

## Current state

### Done
- [x] A Cargo workspace with 6 crates, `clippy -D warnings` and `fmt` clean,
      173 tests (including an integration test that downloads the stream from
      the real server).
- [x] `pipeline`: VA-API/NVENC/V4L2/x264/openh264 encoders with **per-encoder**
      low-latency flags, `vapostproc` on the GPU path, applied latency, an AAC
      audio branch, a `constrained-baseline` capsfilter.
- [x] KMS driver detection, plus runtime evidence-based encoder fallback.
- [x] Discovery: mDNS (one daemon for Chromecast+AirPlay) and signal-driven
      WFD-P2P, with `StartFind` renewal and reconnection with backoff.
- [x] The Cast channel: validated TLS (chain + validity period), its own
      heartbeat, `requestId` correlation, a message size cap.
- [x] **Chromecast casting end to end**: the stream server with a 128-bit token
      in the path plus an allowlist for the receiver's IP, handing the socket to
      `multisocketsink` (no copying), `LAUNCH` + `LOAD` and a clean teardown.
- [x] **Cast mirroring** (app `0F5096E8`): OFFER/ANSWER negotiation, AES-CTR-128
      encrypted RTP, retransmission driven by the receiver's feedback, one
      thread per stream. Validated in the field: good picture, low input lag,
      clean audio.
- [x] **Miracast working end to end**, validated against real hardware: M1–M7
      with the format negotiated from the sink's `native` field, `CSeq`
      correlation, status checking, timeouts, the M16 keepalive and the
      conformance requirements in the table below. It mirrors the screen at
      1920x1080@60.
- [x] firewalld: opens 7236/tcp and 16384-16385/udp in the P2P interface's zone.
- [x] **Real system audio** (`pulsesrc device=@DEFAULT_MONITOR@`), falling back
      to silence where capture is unavailable — the audio branch is not
      optional on WFD.
- [x] GUI: visible errors, rescanning, an empty state with a deadline, i18n, a
      **stop button**, a choice between whole screen and a window, and each
      protocol's latency expectation shown in the list itself.
- [x] Packaging: `.desktop`, AppStream metainfo, icon, `Makefile`, `PKGBUILD`,
      Flatpak manifest; CI with fmt/clippy/test/data validation/audit.

### Outstanding
- [ ] Merging duplicate devices in the list (the Samsung shows up as AirPlay
      *and* as Miracast; it is the same piece of equipment).
- [ ] Wiring the **virtual monitor** into the interface (the backend exists,
      but this path has never been tested).
- [ ] The Cast RTCP sender report (see the debt below).

## WFD conformance — what separates a picture from a black screen

Validated in the field against a Samsung Projector LSP3 (2026-08-07). None of
these items produces an error: the sink accepts the whole negotiation, answers
200 OK to everything, keeps the session alive with keepalives — and displays
nothing. They do not show up in `ffprobe` either, which is far too tolerant to
serve as an oracle here.

| Requirement | Symptom when violated |
| --- | --- |
| Video on PID **0x1011**, audio on **0x1100** (`mux.sink_4113`/`sink_4352`) | `mpegtsmux` picks PIDs on its own and the sink cannot find the video |
| `rtpmp2tpay perfect-rtptime=false` | the RTP timestamp comes from the buffer count rather than the clock, and the sink cannot schedule display |
| **One slice per frame** (`num-slices=1`, no `sliced-threads`) | hardware decoders refuse multi-slice frames |
| Pipeline latency **≥ the minimum it reports** | `basesink` drops late buffers; measured: a real minimum of 41 ms, while the code forced 20 ms |
| The video mode chosen from the sink's `native` field | a Full HD projector received 1680x1050 and rescaled it (or refused) |
| The *source* sends the **M16** keepalive | the sink drops the session after ~60 s |

## Latency: measured, and mostly not ours

Measured with an on-screen millisecond stopwatch photographed next to the
receiver's image, at 1920x1080@60 with VA-API encoding:

| Segment | Time |
| --- | --- |
| Inside our pipeline (capture → encode → packetise → network) | **~85 ms** (worst case ~125 ms) |
| Network, decoding and the receiver's own image processing | the remainder |

On one Miracast projector the end-to-end total came to ~400 ms, meaning roughly
315 ms belonged to the projector. `BIGNETSCREEN_LATENCY=1` reproduces the
sender-side measurement per pipeline stage.

An earlier claim of "~40 ms" in this document referred to the **pipeline's**
internal minimum, not an end-to-end figure. The distinction matters: it is the
difference between what we can optimise and what the receiving device costs.

### The Chromecast paths

**Mirroring (`0F5096E8`, the default when available).** Cast Streaming receives
**encoded access units**, not a container. The pipeline ends in `appsink` — no
`matroskamux`, no HTTP server and no media player on the other end, and
therefore none of the pre-buffering that costs seconds. The protocol is not
proprietary: OFFER/ANSWER over the Cast channel plus its own RTP/UDP with an
AES-128-CTR encrypted payload, all implemented in the
[Open Screen Library](https://github.com/google/openscreen), Google's own open
source.

Two lessons that cost dearly, both validated in the field:

- **Retransmission is not optional.** The transport is UDP with no error
  correction; the receiver declares what it lost and *waits* for the resend. A
  single lost datagram freezes the picture forever, while the receiver keeps
  sending feedback dozens of times per second and looks perfectly healthy.
- **The retransmission history must be measured in time, not frames.** With a
  fixed 16 frames, audio held only 160 ms; the receiver asked for frames from
  over a second ago, the sound stalled and the device ended the session itself.

**HTTP (the fallback, for devices without mirroring).** The Default Media
Receiver is a file player: it pre-buffers before starting and keeps that slack
forever. Measuring first: the receiver reports `currentTime` in its
`MEDIA_STATUS` messages, and `time since connecting − currentTime` **is** the
depth of its buffer (`LagMeter`). Playing back at 1.2× consumes the slack;
`DrainController` **converges from above**, starting from a safe value and
tightening after each stable stretch, with any target that stalled becoming a
permanent floor. Starting aggressive and loosening on stalls was measured and is
worse: every stall is a visible stutter. This brought 2.6 s down to ~1 s.

**Open debt — Cast RTCP.** Measured varying one factor at a time against a real
receiver:

| RTCP | Socket | Result |
| --- | --- | --- |
| off | one per stream | picture appears with low lag, then **freezes** |
| on | one per stream | the receiver closes the app |
| on | shared | the receiver closes the app |

The freeze comes from the missing *sender report* — without it the receiver
loses the mapping between the RTP timestamp and the clock. But the plain
RFC 3550 packet we emit is rejected: Cast uses **compound** RTCP with extended
reports of its own (`compound_rtcp_builder.cc` in Open Screen). Sending
something that kills the session is worse than sending nothing, so it stays off
by default (`BIGNETSCREEN_CAST_RTCP=1` turns it on). Retransmission currently
covers the freeze in practice.

## Radio silence while streaming

Scanning for Wi-Fi Direct peers and streaming compete for the same antenna, and
each scan hop mutes the link for tens of milliseconds. Video recovers by asking
for retransmission; audio has a playout deadline and whatever misses it becomes
a hole in the sound.

This went unnoticed for a long time because protocol work gets tested through
example programs, which look for nothing while they stream. Through the GUI —
which keeps scanning so a device switched on later still appears — the very same
code produced choppy audio and extra delay. `nd_core::radio` now pauses
discovery scanning for the duration of a session. On the WFD path the silence
only begins **after** the P2P group is formed, since forming it depends on an
active scan.

## Encoder selection: evidence, not a blacklist

The C project blocked VA-API on Intel's `xe` driver because it hung during
encoding. In the field, on a Core Ultra 200H with `xe`, `vah264enc` encodes
1080p60 stably at ~13% CPU and with **less** delay than x264 — the blacklist had
become pure cost.

Driver blacklists get the hardware that existed when they were written right.
In their place the decision is made at runtime: `encoder_candidates()` orders
hardware before software, and `MonitoredPipeline` counts the frames leaving the
encoder. An encoder that accepts the configuration and **produces no frames** is
discarded and the next takes over — which is exactly the silent failure the
blacklist was trying to guess at. `BIGNETSCREEN_ENCODER=x264enc` forces the
choice manually.

## Debts and quirks carried over from C (status)

| Quirk | Status |
| --- | --- |
| `videorate` required between source and the fixed capsfilter | done (and moved ahead of scaling) |
| Intel `xe` driver: VAAPI hangs → software | **removed**: it does not hold on current hardware; replaced by runtime detection |
| 500 ms latency only makes sense for openh264 | confirmed in the field: a real minimum of 41 ms; the default became automatic, and the fixed headroom stayed only on openh264 |
| Chromecast TLS needs real validation | done (chain + validity period; `UNKNOWN_CA`/`BAD_IDENTITY` accepted by design) |
| Names arriving over mDNS must be sanitised | done (`sink::sanitize_name`) |
| A 100000-buffer audio queue | done (`AUDIO_QUEUE_BUFFERS = 4` on the muxed path) |
| Aspect ratio when scaling | `add-borders=true` made explicit: `vapostproc` defaults to `false` and a 16:10 screen came out stretched on a 16:9 panel |
| Chromecast's `multisocketsink` at `sync=false`, 8 KiB blocks | done (with `sync=true` the latency was a whole pipeline) |
| The pipeline must leave `Null` before accepting clients | done in `build_pipeline` (the refusal was only a `WARNING`; the cast delivered zero bytes) |
| Mutter's virtual monitor emits no frames until caps are negotiated | mitigated with `keepalive-time`/`resend-last`; the `intervideosink` bridge is still missing if it turns up in practice |
