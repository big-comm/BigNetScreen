# DLNA / UPnP renderers

`nd-dlna` discovers renderers and controls playback; `nd-core` captures, encodes and serves the live MPEG-TS stream. Capture permissions, bounded queues and valid H.264 decoding must survive changes to the transport. Build with `cargo build --locked -p nd-service`; validate with `cargo test --locked -p nd-core -p nd-dlna`.

---

## Resolution is measured in pixels

A portal can announce compositor coordinates instead of physical pixels. A 2560×1440 desktop at fractional scaling announced 2327×1309: using that announcement as the encoding size reduced the transmitted image to 2326×1308.

The DLNA path now negotiates `VideoTarget::UpTo` against the actual video caps. The preference is a ceiling: a smaller source stays smaller, and a larger one is scaled within that ceiling. The conservative default is 1080p; an explicit higher preference is allowed. This is sender policy, not a mode negotiated with the television. The frame-rate setting is likewise an encoding target, not confirmation of the TV's displayed frame rate.

Capture starts on the receiver's first valid GET so its first keyframe is retained. Before then, the actual resolution is unknown. The optional DIDL-Lite `res@resolution` attribute is omitted rather than populated with logical coordinates or a guessed ceiling. The bitstream carries the encoded dimensions. Once caps arrive, the session publishes them and updates the encoder/multiplex rate where the encoder supports live changes. A slow handshake must not miss this update or put a streaming session back into “preparing”.

## What determines delay

The path is capture → encoding → MPEG-TS → HTTP/TCP → the receiver's media player. Shortening a sender queue cannot directly empty a buffer already inside the TV. Network round-trip time is not picture delay.

The sender already uses H.264 without B-frames, periodic keyframes and repeated codec headers, small upstream queues, and an unsynchronized socket sink. It declares a live, non-seekable stream in both HTTP and DIDL-Lite. Keep those declarations consistent; a file-download transfer mode or invented duration/length does not request lower latency.

AVTransport:1 has no portable playback-buffer control. AVTransport:3's optional `SyncOffset` belongs to the ConnectionManager CLOCKSYNC feature; it is not a universal prebuffer setting for HTTP playback. Vendor extensions must be read from the particular receiver and verified before use. The Panasonic VIErA examined on 2026-09-22 advertises neither a buffer control nor CLOCKSYNC controls. This observation does not establish a minimum delay for other receivers. See the [AVTransport specification, §5.2.31](https://openconnectivity.org/wp-content/uploads/2015/11/UPnP-av-AVTransport-Service.pdf).

## Formats and receiver compatibility

| Option | Current decision and evidence limit |
| --- | --- |
| H.264/AAC in 188-byte MPEG-TS over continuous HTTP | Retained. It starts without a completed file or media segment; packet and codec headers are repeated. Current field reception and local tests cover this path. |
| Constant multiplex rate using null TS packets | Retained. It prevents nearly static screens from producing too few bytes to fill a byte-counted receiver buffer. The [muxer documents this padding](https://gstreamer.freedesktop.org/documentation/mpegtsmux/GstBaseTsMux.html). Padding spends bandwidth and is not a congestion controller. |
| 192-byte timestamped MPEG-TS / AC-3 | Earlier project notes report playback with no latency improvement on one Panasonic. Not rerun in this revision; not evidence for every receiver or a reason to change the default. |
| Fragmented MP4 or HLS | Possible only where the receiver supports the complete live format, not merely ordinary MP4 files. Fragment/segment accumulation can add delay; no measured improvement justifies changing the default. |
| Advertised DLNA profile names | Do not claim a specific profile unless codec, audio, dimensions, level and transport all conform. Reading one model's profile list cannot establish limits for thousands of other receivers. |

Earlier field notes reported roughly 6 seconds with a 0.32 Mbit/s unpadded stream, 2 seconds at 8 Mbit/s and 1.5 seconds at 20 Mbit/s on a Panasonic 75GX880. Those are historical observations, not measurements repeated by this revision or a protocol-wide latency floor. New format or padding changes require same-content, same-receiver before/after measurements.

## Sender queue limits

The DLNA socket queue has a byte limit calculated from two seconds at the initial target multiplex rate, clamped to 1–8 MiB. This limits retained media; it is neither reserved memory nor extra playback delay. The small mux output queue remains non-leaky.

`mpegtsmux` output does not preserve the H.264 `DELTA_UNIT` markings expected by `multisocketsink` keyframe recovery. A soft-limit jump could therefore cut into a transport/PES packet sequence rather than resume at a complete decodable keyframe. DLNA uses no such recovery: a reader exceeding the hard limit is disconnected, the server returns a queue-limit error, and the session tears down normally. Restarting the transmission starts a fresh stream. Healthy readers keep the same continuous stream and incur no intentional waiting for this limit. See [GStreamer's socket queue semantics](https://gstreamer.freedesktop.org/documentation/tcp/multisocketsink.html).

## Discovery is SSDP, not mDNS

The two share nothing but multicast. `avahi-browse` cannot see a DLNA television at all, so a network that looks empty to it may be full of renderers, and both providers have to run. The search names `MediaRenderer:1` rather than `ssdp:all`; both work against real hardware, and the narrow one spares us discarding every router and printer. A device is keyed by its `USN`, not its address, so a television that returns on a new DHCP lease is the same television.

## The ordering that matters

A renderer fetches the URL **from inside `SetAVTransportURI`**, not after `Play`. Proven by pointing one at an unreachable address: it answered `500` with UPnP `errorCode 716, Resource not found`, a verdict it could only reach by trying. So the stream server must already be *accepting* when the handshake runs, not merely listening. Handing the URL over first and serving afterwards deadlocks the SOAP call against our own accept loop, and the session dies reporting a network error against the control URL — which names the wrong culprit entirely.

Expect the television to open and close a few connections before it settles: it probes with `HEAD` and `getcontentFeatures.dlna.org: 1` before committing to a single `GET`.

## Framing, and the trap in it

Ask for `Connection: close` and a Panasonic will answer with a `Content-Length` and hold the socket open anyway. A client that reads to EOF hangs for its whole timeout on every call. Frame by `Content-Length`, with EOF only as the fallback.

The same asymmetry runs the other way: our *response* carries no length, because the stream has no end. `transferMode.dlna.org: Streaming` and `DLNA.ORG_FLAGS=8d100000…` (sender-paced, s0- and sn-increasing, streaming transfer mode, DLNA 1.5) are what tell the renderer this is live content rather than a file to download. The same flags go in the DIDL-Lite `res` element, because a renderer that sees the two disagree believes the metadata.

## Evidence and limits

Every measurement here comes from one renderer, a Panasonic 75GX880 over Ethernet. It is the interoperability oracle for this protocol in the same way a physical Chromecast is for Cast, and one model cannot speak for the installed base — another television may disagree in details only it knows. `cargo run -p nd-dlna --example dlna_probe` runs discovery, the description and each SOAP action in order and prints what came back; it is how the ordering and framing defects above were found, and it is the first thing to reach for when a new device misbehaves.

The DLNA guidelines themselves are member-only documents. The open implementations that encode them — `libdlna`, `GUPnP`, `dleyna` — agree on the flag bits and are cited in [references](references.md).

Untested: any renderer that is not this television, and the `ContentDirectory` half of the protocol (serving our own media library), which is a different feature and not implemented.
