//! Integration with the system's network services over D-Bus.
//!
//! - NetworkManager: Wi-Fi Direct discovery and groups for Miracast
//!   ([`p2p`]), and the LAN addresses DLNA discovery binds to.
//! - firewalld: the WFD ports, opened on the P2P interface's zone for the
//!   length of a session ([`firewall`]).
//! - The receiver round trip shown while streaming ([`probe`]).
//! - The GPU's KMS driver, which decides whether hardware encoding is used
//!   ([`detect_gpu_driver`]).
//!
//! The system bus is unavailable under Flatpak, so the D-Bus parts degrade to
//! errors or no-ops there.

pub mod firewall;
mod networkmanager;
pub mod p2p;
pub mod probe;

use std::path::Path;

use nd_core::pipeline::GpuDriver;

/// Detects the primary GPU's KMS driver.
///
/// Reads the target of the `/sys/class/drm/card*/device/driver` symlink, whose
/// file name is the kernel module (`i915`, `xe`, `amdgpu`, `nvidia`…). The
/// first card with a known driver wins.
pub fn detect_gpu_driver() -> GpuDriver {
    detect_gpu_driver_in(Path::new("/sys/class/drm"))
}

fn detect_gpu_driver_in(drm_root: &Path) -> GpuDriver {
    let Ok(entries) = std::fs::read_dir(drm_root) else {
        tracing::debug!("DRM sysfs unavailable; assuming an unknown GPU");
        return GpuDriver::Unknown;
    };

    let mut cards: Vec<_> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        // `card0`, `card1`; not connectors (`card0-HDMI-A-1`) or `renderD128`.
        .filter(|name| {
            name.starts_with("card") && name["card".len()..].chars().all(|c| c.is_ascii_digit())
        })
        .collect();
    // `card0` is usually the primary GPU.
    cards.sort();

    for card in cards {
        let link = drm_root.join(&card).join("device").join("driver");
        let Ok(target) = std::fs::read_link(&link) else {
            continue;
        };
        let Some(module) = target.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let driver = GpuDriver::from_kernel_module(module);
        tracing::info!(%card, %module, ?driver, "KMS driver detected");
        if driver != GpuDriver::Unknown {
            return driver;
        }
    }

    GpuDriver::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_sysfs_is_not_fatal() {
        assert_eq!(
            detect_gpu_driver_in(Path::new("/definitely/does/not/exist")),
            GpuDriver::Unknown
        );
    }

    #[test]
    fn the_first_card_with_a_known_driver_wins() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("nd-net-drm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (entry, module) in [
            ("card0-HDMI-A-1", "i915"),
            ("renderD128", "i915"),
            ("card0", "simpledrm"),
            ("card1", "amdgpu"),
        ] {
            let device = root.join(entry).join("device");
            std::fs::create_dir_all(&device).unwrap();
            symlink(
                format!("../../bus/pci/drivers/{module}"),
                device.join("driver"),
            )
            .unwrap();
        }
        let driver = detect_gpu_driver_in(&root);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(driver, GpuDriver::Amdgpu);
    }
}
