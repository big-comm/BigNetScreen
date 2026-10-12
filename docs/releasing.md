# Release checklist

Only maintainers approve and publish a stable release or create a tag.

## Required record

Record the commit, Rust and native dependency versions, source checksum, and the gate commands with their results. All [automated gates](testing.md) must pass in a complete environment; list skipped hardware, display and advisory checks separately. Test the assembled native package and Flatpak if those formats will be advertised.

Confirm that `bignetscreen --version` prints the package's build date, the newest AppStream release entry names the Cargo workspace version, POT extraction is repeatable, translations compile, local documentation links resolve, and no private paths or secrets are shipped. The source archive contains every tracked runtime and build file, including `vendor/`, and no target directories, Cargo caches, toolchains or local `.cargo` source overrides. The package's `pkgver` is a build date, so record the commit separately.

## Acceptance beyond automation

Complete the receiver and compositor matrix in [testing](testing.md), particularly sustained sessions and repeated Cast reconnection after Stop and window closure. Test install, upgrade and removal on the target rolling-release distribution. Run current security and dependency advisory checks and resolve or accept each [SECURITY.md](../SECURITY.md) limitation.

Release notes separate fixes from measurements, describe remaining limitations and list the hardware actually tested; unsupported, untested and broken are different statuses.
