# Release checklist

A review can produce a release candidate; only maintainers approve and publish a stable release. Do not create a tag as a side effect of running tests.

## Required record

Record immutable HEAD/tree, Rust/native dependency versions, source checksum, exact gate commands and results. All [automated gates](testing.md) must pass in a complete environment. Track skipped hardware/display tests and dependency/advisory checks separately. Test the assembled native package and Flatpak if those formats will be advertised.

Confirm the Cargo and UI versions agree, POT extraction is repeatable, translations compile, local documentation links resolve, no private paths or secrets are shipped, and the source archive contains every tracked runtime/build file including vendor/gst-plugin-ndi. A full source archive must not contain target, Cargo caches, toolchains or local .cargo source overrides. The package's own `pkgver` is a build date, so record the commit separately.

Inspect changes in small commits. Produce an incremental patch against the stated baseline, an independent Git bundle and source archive. Extract elsewhere, verify manifest/HEAD/tree, run git fsck and targeted tests. Confirm no owned build/test process remains alive before handing off.

## Acceptance beyond automation

Complete the receiver/compositor matrix in docs/testing.md, particularly sustained sessions and repeated real Cast reconnection after Stop/window closure. Test install/upgrade/remove on the target rolling-release distribution. Run current security/dependency advisory checks and explicitly accept or resolve SECURITY.md limitations.

Publish only after those gates have named evidence. Release notes must separate fixes from measurements, describe remaining limitations and list hardware actually tested. Unsupported, untested and broken are different statuses. A build that passed against a simulated receiver is not a universal compatibility claim.
