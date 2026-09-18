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

pub fn can_install() -> bool {
    // The current AUR ndi-sdk recipe supports x86_64 only.
    cfg!(target_arch = "x86_64")
        && !nd_capture::is_sandboxed()
        && std::fs::read_to_string("/etc/os-release")
            .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
            .is_ok_and(|release| arch_based(&release))
        && ["/usr/bin/bash", "/usr/bin/pkexec", "/usr/bin/pacman"]
            .iter()
            .all(|path| Path::new(path).is_file())
}

pub async fn install() -> Result<(), String> {
    if !can_install() {
        return Err("Automatic NDI installation is unavailable on this system".into());
    }
    let output = tokio::process::Command::new("/usr/bin/bash")
        .args(["--noprofile", "--norc", "-c", INSTALL_SCRIPT])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        let log = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let tail: String = log.chars().rev().take(4000).collect();
        Err(format!(
            "{}\n{}",
            output.status,
            tail.chars().rev().collect::<String>()
        ))
    }
}

#[cfg(test)]
mod tests {
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
