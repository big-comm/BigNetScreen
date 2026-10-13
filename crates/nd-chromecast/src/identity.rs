//! Which device answers for each Cast receiver, remembered on first use.
//!
//! Trust on first use, as VLC does with a Chromecast: the first connection to
//! a receiver records the key that proved its identity, and a later
//! connection that proves a different key, or stops proving one, is
//! refused. VLC remembers the TLS key; a Chromecast rotates that every two
//! days, so what is remembered here is the device key from DeviceAuth (see
//! `cast::device_auth`), which stays with the hardware.
//!
//! This does not establish that the first device was genuine, only that it
//! is still the same one. Receivers are keyed by the identity the person
//! chose from discovery, not by anything the device says about itself: an
//! impostor at the same address may name itself anything.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use nd_core::{NdError, Result};

/// The token the interface turns into a sentence (`describe_refusal`).
pub const IDENTITY_CHANGED: &str = "receiver-identity-changed";

/// Receivers remembered; past this the oldest are forgotten.
const MAX_REMEMBERED: usize = 256;

/// Serialises read-modify-write of the file between concurrent sessions.
static STORE: Mutex<()> = Mutex::new(());

#[cfg(test)]
fn store_path() -> Option<PathBuf> {
    // Tests that connect to a local receiver must not touch the real file.
    Some(
        std::env::temp_dir()
            .join(format!("nd-cast-identity-{}", std::process::id()))
            .join("cast-receivers"),
    )
}

#[cfg(not(test))]
fn store_path() -> Option<PathBuf> {
    nd_core::persistence::app_dir(nd_core::persistence::BaseDir::Data)
        .map(|dir| dir.join("cast-receivers"))
}

/// Accepts the receiver when it proves the key it proved before, and
/// remembers the key of one seen for the first time.
pub fn check_and_remember(receiver: &str, device_key: Option<&[u8]>) -> Result<()> {
    let Some(path) = store_path() else {
        tracing::warn!("no data directory; the receiver's identity cannot be remembered");
        return Ok(());
    };
    check_in(&path, receiver, device_key)
}

/// Forgets the key remembered for `receiver`, so that whichever device
/// answers next is remembered in its place. The person's decision, after the
/// receiver was refused for [`IDENTITY_CHANGED`].
pub fn forget(receiver: &str) -> Result<()> {
    match store_path() {
        Some(path) => forget_in(&path, receiver),
        None => Ok(()),
    }
}

fn forget_in(path: &Path, receiver: &str) -> Result<()> {
    let _held = STORE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let text = read_store(path)?;
    let name = hex(receiver.as_bytes());
    let kept: String = text
        .lines()
        .filter(|line| line.split_once(' ').is_none_or(|(n, _)| n != name))
        .map(|line| format!("{line}\n"))
        .collect();
    if kept.len() == text.len() {
        return Ok(());
    }
    nd_core::persistence::write_private(path, kept.as_bytes()).map_err(|err| {
        NdError::Protocol(format!(
            "the receiver's identity could not be forgotten: {err}"
        ))
    })
}

fn read_store(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        // Unreadable is not empty: treating it so would trust any device again.
        Err(err) => Err(NdError::Protocol(format!(
            "the remembered receiver identities could not be read: {err}"
        ))),
    }
}

fn check_in(path: &Path, receiver: &str, device_key: Option<&[u8]>) -> Result<()> {
    // Only remembering a new key writes. The store is replaced atomically, so
    // a check reads it without waiting behind another receiver's write; a
    // write takes the lock and decides again on a fresh read.
    if remembered(&read_store(path)?, receiver, device_key)?.is_none() {
        return Ok(());
    }
    let _held = STORE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(contents) = remembered(&read_store(path)?, receiver, device_key)? else {
        return Ok(());
    };
    nd_core::persistence::write_private(path, contents.as_bytes()).map_err(|err| {
        NdError::Protocol(format!(
            "the receiver's identity could not be remembered: {err}"
        ))
    })?;
    tracing::info!("remembered the receiver's identity key");
    Ok(())
}

/// The verdict on `receiver` against the store's `text`: an error when the
/// device proves another key or none after proving one, the new store
/// contents when its key is to be remembered, and `None` when nothing changes.
fn remembered(text: &str, receiver: &str, device_key: Option<&[u8]>) -> Result<Option<String>> {
    let name = hex(receiver.as_bytes());
    let mut entries: Vec<(&str, &str)> = text.lines().filter_map(|l| l.split_once(' ')).collect();
    let known = entries
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, key)| *key);

    match (known, device_key) {
        (Some(known), Some(key)) if known == hex(key) => Ok(None),
        (Some(_), Some(_)) => Err(NdError::Protocol(format!(
            "the device answering for this receiver proved a different identity key \
             than the first time: {IDENTITY_CHANGED}"
        ))),
        // A device that proved itself before cannot drop the proof: that is
        // how an impostor would get around the check.
        (Some(_), None) => Err(NdError::Protocol(format!(
            "the device answering for this receiver no longer proves its identity: \
             {IDENTITY_CHANGED}"
        ))),
        (None, None) => {
            tracing::warn!("the receiver did not prove its identity; nothing to remember");
            Ok(None)
        }
        (None, Some(key)) => {
            let key = hex(key);
            entries.push((&name, &key));
            let skip = entries.len().saturating_sub(MAX_REMEMBERED);
            let mut contents = String::new();
            for (n, k) in &entries[skip..] {
                contents.push_str(n);
                contents.push(' ');
                contents.push_str(k);
                contents.push('\n');
            }
            Ok(Some(contents))
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nd-cast-identity-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("cast-receivers")
    }

    #[test]
    fn check_waits_only_to_write() {
        let path = store("no-wait");
        check_in(&path, "tv", Some(b"key")).unwrap();
        // Another receiver is mid-write.
        let _held = STORE.lock().unwrap();
        let (done, checked) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send((
                check_in(&path, "tv", Some(b"key")).is_ok(),
                check_in(&path, "other", None).is_ok(),
            ));
        });
        assert_eq!(
            checked.recv_timeout(std::time::Duration::from_secs(5)),
            Ok((true, true))
        );
    }

    #[test]
    fn only_first_key_is_accepted() {
        let path = store("tofu");
        check_in(&path, "tv._googlecast._tcp.local.", Some(b"key-a")).unwrap();
        check_in(&path, "tv._googlecast._tcp.local.", Some(b"key-a")).unwrap();
        let changed = check_in(&path, "tv._googlecast._tcp.local.", Some(b"key-b")).unwrap_err();
        assert!(changed.to_string().ends_with(IDENTITY_CHANGED));
        let dropped = check_in(&path, "tv._googlecast._tcp.local.", None).unwrap_err();
        assert!(dropped.to_string().ends_with(IDENTITY_CHANGED));
        // Another receiver is its own first use.
        check_in(&path, "other._googlecast._tcp.local.", Some(b"key-b")).unwrap();
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn forgotten_receiver_pins_next_key() {
        let path = store("forget");
        check_in(&path, "tv", Some(b"key-a")).unwrap();
        check_in(&path, "other", Some(b"key-a")).unwrap();
        forget_in(&path, "tv").unwrap();
        check_in(&path, "tv", Some(b"key-b")).unwrap();
        check_in(&path, "tv", Some(b"key-a")).unwrap_err();
        // The other receiver keeps its key.
        check_in(&path, "other", Some(b"key-b")).unwrap_err();
        // Nothing to forget is not an error.
        forget_in(&path, "never-seen").unwrap();
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn unproved_receiver_stays_unknown() {
        let path = store("none");
        check_in(&path, "tv", None).unwrap();
        assert!(!path.exists());
        check_in(&path, "tv", Some(b"key-a")).unwrap();
        check_in(&path, "tv", Some(b"key-a")).unwrap();
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn store_evicts_the_oldest() {
        let path = store("bound");
        for i in 0..=MAX_REMEMBERED {
            check_in(&path, &format!("tv-{i}"), Some(b"key")).unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), MAX_REMEMBERED);
        let first = format!("{} ", hex(b"tv-0"));
        assert!(!text.lines().any(|line| line.starts_with(&first)));
        // Forgotten, so a new key is a first use again.
        check_in(&path, "tv-0", Some(b"other")).unwrap();
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
