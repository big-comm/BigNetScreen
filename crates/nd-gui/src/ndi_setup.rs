//! Optional runtime installation. No vendor code is shipped with the app.

use std::path::Path;
use std::process::Stdio;

const INSTALL_SCRIPT: &str = include_str!("install_ndi.sh");
pub const AUR_URL: &str = "https://aur.archlinux.org/packages/ndi-sdk";
pub const GUIDE_URL: &str = "https://ndi.video/for-developers/ndi-sdk/";

fn arch_based(os_release: &str) -> bool {
    os_release.lines().any(|line| {
        let Some((key, value)) = line.split_once('=') else {
            return false;
        };
        matches!(key.trim(), "ID" | "ID_LIKE")
            && value
                .trim()
                .trim_matches(['\'', '"'])
                .split_whitespace()
                .any(|id| matches!(id, "arch" | "biglinux" | "bigcommunity" | "manjaro"))
    })
}

pub async fn can_install() -> bool {
    tokio::task::spawn_blocking(|| {
        // The current AUR ndi-sdk recipe supports x86_64 only.
        cfg!(target_arch = "x86_64")
            && !nd_capture::is_sandboxed()
            && std::fs::read_to_string("/etc/os-release")
                .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
                .is_ok_and(|release| arch_based(&release))
            && [
                "/usr/bin/bash",
                "/usr/bin/pkexec",
                "/usr/bin/pacman",
                "/usr/bin/timeout",
            ]
            .iter()
            .all(|path| Path::new(path).is_file())
    })
    .await
    .unwrap_or(false)
}

pub async fn install() -> Result<(), String> {
    if !can_install().await {
        return Err("Automatic NDI installation is unavailable on this system".into());
    }
    // GNU timeout bounds the entire subprocess group, not just our wait.
    // The installer is opt-in; no package-manager command is run by tests.
    let mut child = tokio::process::Command::new("/usr/bin/timeout")
        .args([
            "--signal=TERM",
            "--kill-after=10s",
            "30m",
            "/usr/bin/bash",
            "--noprofile",
            "--norc",
            "-c",
            INSTALL_SCRIPT,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| error.to_string())?;
    let stdout = child.stdout.take().ok_or("installer stdout unavailable")?;
    let stderr = child.stderr.take().ok_or("installer stderr unavailable")?;
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(31 * 60), async {
        tokio::join!(child.wait(), read_log_tail(stdout), read_log_tail(stderr))
    })
    .await
    .map_err(|_| "NDI installation exceeded its deadline".to_string())?;
    let (status, stdout, stderr) = outcome;
    let status = status.map_err(|error| error.to_string())?;
    let stdout = stdout.map_err(|error| error.to_string())?;
    let stderr = stderr.map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "{}\n{}\n{}",
            status,
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        ))
    }
}

/// Drain both pipes continuously while retaining a bounded diagnostic tail.
async fn read_log_tail<R: tokio::io::AsyncRead + Unpin>(mut reader: R) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    const LIMIT: usize = 4096;
    let mut tail = Vec::with_capacity(LIMIT);
    let mut chunk = [0u8; LIMIT];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            return Ok(tail);
        }
        let discard = (tail.len() + n).saturating_sub(LIMIT);
        tail.drain(..discard);
        tail.extend_from_slice(&chunk[..n]);
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn installer_output_is_drained_but_retained_memory_is_bounded() {
        let mut data = vec![b'a'; 1024 * 1024];
        data.extend_from_slice(b"END");
        let tail = super::read_log_tail(data.as_slice()).await.unwrap();
        assert_eq!(tail.len(), 4096);
        assert!(tail.ends_with(b"END"));
        assert!(super::read_log_tail(&b""[..]).await.unwrap().is_empty());
    }
    use super::*;

    #[test]
    fn installer_targets_arch_family_only() {
        for release in [
            "ID=arch",
            "ID=biglinux",
            "ID=bigcommunity\nID_LIKE=arch",
            "ID=endeavouros\nID_LIKE=\"arch\"",
            "ID=example\nID_LIKE='arch manjaro'",
        ] {
            assert!(arch_based(release), "{release}");
        }
        for release in [
            "ID=ubuntu\nID_LIKE=debian",
            "ID=fedora",
            "NAME=arch\nID=debian",
            "ID=archlinux-fake",
            "",
        ] {
            assert!(!arch_based(release), "{release}");
        }
    }
}
