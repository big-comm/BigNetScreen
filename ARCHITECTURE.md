# Architecture

BigNetScreen separates capture, transport and presentation. Eight Rust crates share a GUI-free core. The previous development narrative is retained in [history](docs/history/architecture-before-stable-review.md); its measurements and test counts are not evidence for the current tree.

## Ownership and flow

```text
GTK/Relm4 UI ── user action / cancellation ── session
    │                                          │
    └── status and errors ◄─────────────────────┤
                                               │
portal or Mutter ── CaptureSource ── GStreamer ──┤
                          │                    ├─ Cast RTP/RTCP + Cast TLS control
                          └─ FD + stable ID    ├─ Cast HTTP MPEG-TS + Cast control
                                               ├─ WFD MPEG-TS/RTP + RTSP
                                               ├─ NDI publishing
                                               └─ WebRTC/WHEP browser publishing
```

`nd-core` owns SourceType, CaptureSource, Provider/Sink traits, configuration and pipeline construction. `nd-capture` implements the ScreenCast portal and native Mutter fallback. `nd-net` owns NetworkManager P2P and temporary firewalld leases; `nd-wfd` handles RTSP negotiation. `nd-chromecast` owns mDNS and both Cast transports. `nd-ndi` and `nd-webrtc` adapt their GStreamer plugins. `nd-gui` owns presentation and session lifetime, never codec policy copied into UI widgets.

The local `vendor/gst-plugin-ndi` is tracked, licensed source with local changes. The external Cargo offline vendor used by a review environment is a **different directory** and is not part of a source release.

## Capture identity

The portal's restricted PipeWire FD and stream identity must survive until the encoding pipeline stops. Portal v6 supplies a u64 serial; the source targets that serial with `target-object`. Older portals and the Mutter backend currently use node IDs. A serial does not authorize access by itself: the portal FD still matters. The small `portal_start` adapter parses the raw Start response because the locked ashpd version does not expose the new property; remove that adapter only when the replacement API preserves it and the compatibility tests pass.

Monitor authorization can be restored through portal tokens; windows request a fresh choice. Backend cancellation calls stop to release the compositor session. Virtual-monitor capabilities and actual negotiated dimensions are compositor-dependent. Do not force producer caps or fall back to an arbitrary camera when negotiation fails.

## Execution and lifecycle

GTK/Relm4 runs on the GLib main context. Tokio handles D-Bus/network work; GStreamer owns its streaming threads. Cast mirroring currently has bounded packet history and per-stream workers sharing a pacing budget. Do not block the UI with socket or process waits.

The GUI retains the active sink even if discovery refreshes its record. Stop signals the session, and the original session task returns only after cleanup. Closing the window requests that same cleanup and waits with a deadline; a failure leaves the error visible instead of reporting a clean disconnect. Drop is only emergency best-effort cleanup, not the normal disconnect protocol.

Cast STOP addresses only the session this sender launched. Heartbeat continues while confirmation is pending, then application and platform connections close. HTTP fallback must not begin on top of an unconfirmed previous session. On WFD, radio discovery pauses only after P2P group formation; firewall changes are runtime leases, not permanent service configuration.

## Media paths are not interchangeable

| Path | Payload and important constraints |
| --- | --- |
| Cast mirroring | Raw H.264 and Opus access units in Cast RTP, AES-CTR payloads, RTCP feedback; no muxer/player buffer. |
| Cast HTTP | H.264/AAC MPEG-TS, token/IP-scoped HTTP server, Default Media Receiver prebuffering. |
| Miracast | Negotiated mode/profile, WFD-specific MPEG-TS PIDs and RTSP keepalives. |
| WebRTC | Bundled Rust plugin plus system webrtcbin, ICE and DTLS-SRTP; local PIN/WHEP proxy. |
| NDI | Tracked plugin loads an optional external proprietary runtime. |

Encoded PTS is not necessarily running time. Segment conversion and coherent RTCP clock mapping are required; encoder offsets can be extremely large. Queue limits, frame-ID windows and pacing bound local backlog, but are not proof of sustained FPS or a complete adaptive Cast congestion controller.

## Contracts and remaining limits

See [testing](docs/testing.md) for deterministic and hardware gates, [Cast](docs/chromecast.md) for protocol-specific notes and [security](SECURITY.md) for trust boundaries. Receiver authentication, network behavior and compositor integration must not be inferred from a successful compile. Release status belongs in the review evidence and checklist, not in permanent architecture as an evergreen success claim.
