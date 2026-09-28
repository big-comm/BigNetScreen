# Security

## Intended deployment

BigNetScreen is for **trusted local networks**, not an Internet-facing screen-sharing service. Desktop content can include confidential material; review the selected source and receiver before authorizing capture. No component should be run as root to share a screen.

Cast receivers are identified by trust on first use, as VLC does. After TLS, the sender sends a DeviceAuth challenge and verifies that the receiver's device certificate key signed a fresh nonce together with this session's TLS certificate. The first key a receiver proves is remembered under the identity chosen from discovery (`$XDG_DATA_HOME/bignetscreen/cast-receivers`); a later connection proving another key, or none, is refused. The device certificate is **not** chained to Google's Cast root CA and revocation is not checked, so the first connection is not authenticated: an impostor present then would be remembered instead of the genuine receiver. A receiver that never answered the challenge is accepted without an identity check. Cast RTP payload encryption is not a substitute for authenticated receiver identity. These remain explicit security limitations.

Cast HTTP/file servers bind to the interface toward the receiver and require a random path token plus the expected source IP. These restrictions do not provide TLS or protect against a hostile LAN/on-path adversary. The browser page, PIN exchange and WHEP control front door use HTTP; WebRTC media uses its own DTLS-SRTP transport. A PIN controls access, not confidentiality of the HTTP exchange. Do not port-forward these services.

## Defensive boundaries

Network messages and HTTP headers/bodies have size limits. Sessions and queues need deadlines/capacity limits; cancellation must not leave a partial Cast control frame followed by another message. Files are served from the explicit selected list, not arbitrary URL paths. Persistent restore tokens use private file permissions and must not be copied into bug reports. Firewall rules are runtime-scoped and pre-existing user rules are not ours to remove.

Flatpak uses portal authorization and the minimum declared permissions for its supported paths. NDI's proprietary runtime is optional and not shipped with BigNetScreen. The optional AUR installation flow executes a third-party recipe only after a user action; users must trust that recipe and its upstream before authorizing installation.

## Reporting a vulnerability

Use the repository's private vulnerability-reporting feature when enabled. Otherwise contact the maintainer identified in `pkgbuild/PKGBUILD` privately before opening a public issue. Include commit, affected transport, prerequisites and a minimal non-sensitive reproduction. Do not post private screens, HTTP stream tokens, portal restore tokens, passwords, certificates containing private keys or full packet captures publicly.

Fixes should include a regression test and a statement of which threat is addressed. Unknown receiver identity, missing hardware validation and dependency advisories must remain visible in release notes until resolved. No audit here is a security certification.
