# Architecture

BigNetScreen separates capture, transport and presentation. Ten Rust crates share a GUI-free core. Capture authorization, protocol-specific media contracts and orderly session cleanup must survive changes to the interface.

Build the window and service with `cargo build --locked -p nd-gui -p nd-service`. Validate changes across these boundaries with `cargo test --locked --workspace`; see [testing](docs/testing.md) for graphical and hardware checks.

---

Closing a window should not end a presentation. The session service owns discovery and transmission; the GTK window is a client that can leave and reconnect. The previous architecture narrative is retained in [history](docs/history/architecture-before-stable-review.md); its measurements and test counts are not evidence for the current tree.

## Ownership and flow

```text
GTK/Relm4 UI ── D-Bus actions ── nd-service session
    │                                          │
    └── status and errors ◄─────────────────────┤
                                               │
portal or Mutter ── CaptureSource ── GStreamer ──┤
                          │                    ├─ Cast RTP/RTCP + Cast TLS control
                          └─ FD + stable ID    ├─ Cast HTTP MPEG-TS + Cast control
                                               ├─ WFD MPEG-TS/RTP + RTSP
                                               ├─ DLNA HTTP MPEG-TS + UPnP
                                               ├─ NDI publishing
                                               └─ WebRTC/WHEP browser publishing
```

`nd-core` owns SourceType, CaptureSource, Provider/Sink traits, configuration and pipeline construction. `nd-capture` implements the ScreenCast portal and native Mutter fallback. `nd-net` owns NetworkManager LAN addresses, Wi-Fi Direct and temporary firewalld leases; `nd-wfd` handles RTSP negotiation. `nd-chromecast` owns mDNS and both Cast transports; `nd-dlna` owns SSDP and UPnP playback. `nd-ndi` and `nd-webrtc` adapt their GStreamer plugins. `nd-service` owns session lifetime and exports state through D-Bus. `nd-gui` owns presentation, never codec policy copied into widgets.

The local `vendor/gst-plugin-ndi` is tracked, licensed source with local changes. The external Cargo offline vendor used by a review environment is a **different directory** and is not part of a source release.

## Capture identity

The portal's restricted PipeWire FD and stream identity must survive until the encoding pipeline stops. Portal v6 supplies a u64 serial; the source targets that serial with `target-object`. Older portals and the Mutter backend currently use node IDs. A serial does not authorize access by itself: the portal FD still matters. The small `portal_start` adapter parses the raw Start response because the locked ashpd version does not expose the new property; remove that adapter only when the replacement API preserves it and the compatibility tests pass.

Monitor authorization can be restored through portal tokens; windows request a fresh choice. Backend cancellation calls stop to release the compositor session. Virtual-monitor capabilities and actual negotiated dimensions are compositor-dependent. Do not force producer caps or fall back to an arbitrary camera when negotiation fails.

## Execution and lifecycle

GTK/Relm4 runs on the GLib main context. Tokio handles D-Bus/network work; GStreamer owns its streaming threads. Cast mirroring currently has bounded packet history and per-stream workers sharing a pacing budget. Do not block the UI with socket or process waits.

The service retains the active sink even if discovery refreshes its record. Stop signals the session, and the original session task returns only after cleanup. File playback follows the same rule: cancellation keeps its handle until cleanup finishes. Errors remain published until an explicit action replaces them. Closing the window flushes preferences and leaves the transmission running. An open window renews a lease every minute; once no session or client needs it, the service can exit after its idle grace period.

Preferences are applied in memory immediately and saved off the GTK thread. Writers serialize before reading the latest values, so an older delayed save cannot restore stale preferences. Starting a transmission waits for publication and service reload. Changing the virtual sound output is deferred until the current session ends; selective audio never falls back to capturing all computer sound.

External players use the `PlayerApi = 2` contract on `br.com.biglinux.BigNetScreen1`, at `/br/com/biglinux/BigNetScreen` under bus name `br.com.biglinux.BigNetScreen.Service`. `StartPlayer(target, paths, start)` starts only while idle and returns a random session ID. `ControlPlayer(id, command, argument, path)` accepts `play`, `pause`, `seek-to` (seconds), `volume` (0–1), `mute`, `unmute`, `stop`, and the existing queue commands. The engine checks both the ID and the caller's unique D-Bus name. Players must use this scoped stop, not the GUI's global `Stop`. A successful control reply means the command was queued; receiver state and control errors arrive through properties.

`PlayerSession` reports the ID, owner, receiver and optional volume/mute capabilities alongside the unchanged `Media` tuple. Clients inspect `PlayerApi` before using the extension and consume property changes instead of repeatedly rediscovering devices. Checking `ListActivatableNames`/`ListNames` does not start discovery; opening a proxy can activate the service. Do not change the global discovery preference just to open a player picker. A player-owned session ends when its D-Bus connection disappears, whereas a session started by the BigNetScreen GUI retains the window-independent lifetime described above. Loss of the session bus shuts down the daemon after bounded protocol cleanup.

`StartPlayerUrl(target, uri, content_type, title, audio, start)` accepts HTTP(S) media. Both start calls carry `(seconds, paused, volume, muted, height)` as a D-Bus structure; volume is 0–1. `height` picks the 16:9 frame a decoded file is sent in (240–2160, or 0 for the smallest frame that holds the file's own picture); a receiver that fetches the file itself ignores it. Version 2 added that field. Cast loads without autoplay, confirms the requested per-media volume/mute, then plays if requested. DLNA defers an initially paused item's handshake until Play; its decoder receives the initial volume/mute and seek before the renderer receives buffers. Queue transitions retain the last effective volume/mute and pause state. URLs are omitted from public file lists and normal diagnostic messages because they can carry access tokens.

The player extension accepts local files supported by `MediaFile::inspect` and online media. It does not yet expose transcoding options or player effects. Client integrations must resolve those requirements before replacing paths that already provide them. D-Bus contract tests prove activation, wire compatibility and ownership; they do not prove compatibility with physical Chromecast or DLNA receivers.

Discovery providers initialize independently with a deadline. Receiver registries and event queues have bounds. DLNA description requests run as announcements arrive, with four concurrent requests; dropping its stream cancels the scan. SSDP binds one source per active LAN connection reported by NetworkManager and refreshes that list each sweep; when NetworkManager is unavailable it uses the kernel-selected route. VPN/tunnel profiles are excluded from enumeration. The window reads all D-Bus properties once, then applies changed properties and reconnects after service loss.

Cast STOP addresses only the session this sender launched. Heartbeat continues while confirmation is pending, then application and platform connections close. HTTP fallback must not begin on top of an unconfirmed previous session. On WFD, radio discovery pauses only after P2P group formation; firewall changes are runtime leases, not permanent service configuration.

## Media paths are not interchangeable

| Path | Payload and important constraints |
| --- | --- |
| Cast mirroring | Raw H.264 and Opus access units in Cast RTP, AES-CTR payloads, RTCP feedback; no muxer/player buffer. |
| Cast HTTP | H.264/AAC MPEG-TS, token/IP-scoped HTTP server, Default Media Receiver prebuffering. |
| Miracast | Negotiated mode/profile, WFD-specific MPEG-TS PIDs and RTSP keepalives. |
| DLNA | UPnP AVTransport and MPEG-TS over HTTP; receiver profile and playback buffering differ from Cast. |
| WebRTC | Bundled Rust plugin plus system webrtcbin, ICE and DTLS-SRTP; local PIN/WHEP proxy. |
| NDI | Tracked plugin loads an optional external proprietary runtime. |

Encoded PTS is not necessarily running time. Segment conversion and coherent RTCP clock mapping are required; encoder offsets can be extremely large. Queue limits, frame-ID windows and pacing bound local backlog. Cast adjusts encoder bitrate from retransmissions and dropped frames, within negotiated limits; this is not a bandwidth estimate or proof of sustained FPS. The GUI's TCP response measurement is network response, not Wi-Fi signal strength or end-to-end picture delay.

## Contracts and remaining limits

See [testing](docs/testing.md) for deterministic and hardware gates, [Cast](docs/chromecast.md) for protocol-specific notes and [security](SECURITY.md) for trust boundaries. Receiver authentication, network behavior and compositor integration must not be inferred from a successful compile. Release status belongs in the review evidence and checklist, not in permanent architecture as an evergreen success claim.

When changing behavior, ask: who owns the session after the window closes (Execution and lifecycle)? Which authorized PipeWire object is captured (Capture identity)? Which protocol owns the media constraints (Media paths are not interchangeable)?
