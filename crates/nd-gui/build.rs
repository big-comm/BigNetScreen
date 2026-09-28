//! Stamps the build date into the binary as its version.
//!
//! The distribution publishes rolling builds straight from the branch, so
//! `pkgbuild/PKGBUILD` names each one `$(date +%y.%m.%d)`. The About dialog and
//! `--version` have to say the same thing, and the only way to be sure they do
//! is to ask the same program for the same format — `date` reads the local
//! clock and the local timezone, which no calculation from a Unix timestamp
//! would reproduce without pulling in a timezone database.
//!
//! `SOURCE_DATE_EPOCH` wins when a reproducible build sets it.

fn main() {
    println!("cargo::rerun-if-env-changed=SOURCE_DATE_EPOCH");
    // Rerun whenever anything this binary is built from changes, which is
    // when it is rebuilt: the stamp is then its real build date. Listing only
    // `build.rs` kept the date of whenever the script last changed on every
    // binary rebuilt since. Rerunning on every build would recompile the GUI
    // each time for nothing; Cargo has no way to expire a script by the clock.
    for input in [
        "build.rs",
        "../../crates",
        "../../vendor",
        "../../Cargo.lock",
        "../../Cargo.toml",
    ] {
        println!("cargo::rerun-if-changed={input}");
    }

    let mut date = std::process::Command::new("date");
    date.arg("+%y.%m.%d");
    if let Ok(epoch) = std::env::var("SOURCE_DATE_EPOCH") {
        date.arg(format!("--date=@{epoch}"));
    }
    let output = date.output().expect("running `date` to stamp the version");
    let version = String::from_utf8(output.stdout).expect("`date` output is not text");
    let version = version.trim();
    assert!(
        output.status.success() && !version.is_empty(),
        "`date` did not produce a version: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    println!("cargo::rustc-env=BIGNETSCREEN_VERSION={version}");
}
