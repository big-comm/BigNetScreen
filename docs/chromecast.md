# Chromecast / Google Cast

## Two transports

Mirroring uses the receiver's Cast Streaming application, OFFER/ANSWER over the control channel, encrypted H.264/Opus access units in Cast RTP and RTCP feedback. There is no container on this path. The HTTP fallback launches the Default Media Receiver and serves H.264/AAC MPEG-TS (`video/mp2t`). MP2T is listed by Google's media documentation; receiver buffering still makes HTTP a different latency/compatibility choice.

## Clock, flow and decoder contracts

Convert encoded timestamps through the sample's SEGMENT before building RTP timestamps. Encoder PTS offsets are not elapsed session time. Audio/video Sender Reports must describe coherent reference-time/RTP pairs. Do not infer that a simple sender report is invalid just because another implementation also supports compound RTCP.

Frame IDs are expanded locally; the wire truncates them to eight bits. Bound the unacknowledged span and pending media duration, including startup before the first ACK. Reordered/stale feedback must not advance state incorrectly. Never advance an ID for a discarded encoded frame, and resume dependent video only at a keyframe after a drop. Repair history and queues are bounded. Pacing is cancellation-aware and shared by streams; these bounds are not a complete adaptive bandwidth estimator.

Honor receiver-advertised dimensions/FPS/pixels-per-second/bitrate and reject malformed negotiation data. H.264 High interoperability and fallback policy need receiver validation; a generic h264 offer is not a negotiated guarantee of every profile/level. Do not advertise adaptive_playout_delay while the packet extension is unimplemented. Session SSRCs follow Open Screen's audio/video priority ranges instead of being reused constants.

## Ending a session

Keep the channel and heartbeat alive while targeted STOP is confirmed. Verify that the launched session ID is absent, then close its application connection and finally the platform connection. Do not reboot the receiver, stop another sender's session or start fallback over an unconfirmed old session. Preserve a teardown error even when the user requested cancellation. Window closure must retain the async runtime long enough for the same cleanup.

## Evidence and limits

Primary references are Google's Cast media docs and Open Screen source. The Android SDK describes session management for its own API; do not equate sender disconnection with stopping every receiver application. Community repositories/issues can supply traces and hardware hypotheses, but test those hypotheses against the protocol and target firmware. See [references](references.md).

Current limitations include missing DeviceAuth identity verification and incomplete adaptive congestion control. Sender-side limits do not promise increased receiver FPS. No unit test substitutes for the [physical reconnection/soak matrix](testing.md). Record the exact tree and mode with every measurement; old review counts and PSNR/SSIM experiments are not current validation.
