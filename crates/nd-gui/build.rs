//! Makes the packaged version reach the binary.
//!
//! The version shown in the interface has to be the one the **package** was
//! built with. This project's packaging numbers releases by date
//! (`pkgver=$(date +%y.%m.%d)`), which no `Cargo.toml` can know: bumping a
//! version field by hand before every build is a step that gets forgotten, and
//! then the About dialog claims a version nobody shipped.
//!
//! So `APP_VERSION` in the build environment wins, and `Cargo.toml` is the
//! fallback for a plain `cargo build`.
//!
//! The `rerun-if-env-changed` line is the part that is easy to leave out and
//! expensive to miss: without it Cargo reuses the previous compilation, and a
//! rebuilt package keeps reporting the version of the one before it.

fn main() {
    println!("cargo:rerun-if-env-changed=APP_VERSION");
}
