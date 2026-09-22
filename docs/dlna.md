# DLNA / UPnP renderers

## What this protocol is for

Reach, not responsiveness. A DLNA renderer is the television that predates Chromecast, AirPlay and Miracast, it is driven over ordinary Ethernet, and it needs no Wi-Fi adapter on the sending machine. The picture arrives about a second and a half late and **nothing on our side can make it arrive sooner**, so the interface labels it `delayed a few seconds` beside the existing `instant response` and `quick response`. Offer it for watching a screen, never for working on one.

## Why the delay cannot be tuned

The renderer decides its own prebuffer and exposes no way to ask for a smaller one. The whole UPnP surface of a Panasonic 75GX880 was read action by action — `AVTransport`, `RenderingControl`, `ConnectionManager` — and none of them carries a buffer, delay or latency control. Cast is different only because it negotiates a `targetDelay` we set to zero; DLNA has no equivalent field anywhere, and the DLNA flag bits (`libdlna`, `GUPnP`, guidelines §7.3.37.2) describe the *content*, not a latency request.

What the sender does control is how fast that buffer fills, because it is counted in **bytes**. Measured on the same television and content, changing only the multiplex rate:

| multiplex rate | delay |
|---|---|
| ~0.32 Mbit/s (unpadded, trivially compressible) | 6 s |
| 8 Mbit/s | 2 s |
| 20 Mbit/s | 1.5 s |

Hence `mux_bitrate_bps` on `ts_http_pipeline_description`: the muxer pads with null packets to what the encoder already targets. Cast passes `None` — that path was never measured with padding, and a rate is not a thing to change on a guess.

The timestamped `_T` profile family (`video/vnd.dlna.mpeg-tts`, 192-byte packets, `m2ts-mode`) was tried with an explicit `DLNA.ORG_PN` and AC-3 audio on the theory that a television routes it through its broadcast path. It played and the delay was identical, so the simpler 188-byte stream with a wildcard profile wins on the tie.

## Resolution and frame rate come from the device's own profile list

`GetProtocolInfo` is the answer to both, and it has to be read carefully. A
Panasonic 75GX880 — a 4K set — declares only `AVC_TS_HD_*` and `MPEG_TS_SD_*`,
nothing above HD, which is why `DLNA_MAX_RESOLUTION` is 1920x1080. That ceiling
is the television's, not ours.

The frame rate is in the profile *name*: the `_24`, `_50` and `_60` in
`AVC_TS_HD_24_AC3`, `AVC_TS_HD_50_AC3` and `AVC_TS_HD_60_AC3` are hertz, and
this set lists all three. Reading those families as a resolution class and
capping at 30 overrode a person's setting of 60 with a number nothing had asked
for.

It may matter for more than smoothness. At 30 the picture overflowed the panel;
at 60 it fit, with the geometry provably identical on our side — 1920x1080
encoded from a 1920x1080 capture, `rescaled=false` in the log. One observation
each way, so treat it as a correlation and not a mechanism. The plausible
reading is that 30 matched none of the profiles the set declares and it fell
into a different scaling path. If a renderer ever scales oddly, check the frame
rate against its profile list before suspecting anything else, and remember that
`<res resolution="WxH">` is what tells it the geometry rather than leaving it to
be inferred.

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
