//! Integration with the system's network services (over D-Bus, native build).
//!
//! - **NetworkManager**: creating/discovering Wi-Fi Direct (P2P) groups for
//!   WFD-P2P ([`p2p`]).
//! - **firewalld**: opens port 7236/tcp (RTSP) and the RTP ports **on the P2P
//!   interface only**, and undoes it on exit ([`firewall`]).
//! - Detection of the GPU's KMS driver, which decides whether hardware
//!   encoding is used ([`detect_gpu_driver`]).
//!
//! Unavailable under Flatpak (no system bus) — which is why it is isolated in
//! this crate.

pub mod firewall;
pub mod p2p;

use std::path::Path;

use nd_core::pipeline::GpuDriver;

/// Detects the primary GPU's KMS driver.
///
/// Reads the target of the `/sys/class/drm/card*/device/driver` symlink, whose
/// file name is the kernel module (`i915`, `xe`, `amdgpu`, `nvidia`…).
///
/// This is **not** cosmetic: while this function returned a hardcoded
/// `Unknown`, no driver-specific decision could ever fire.
pub fn detect_gpu_driver() -> GpuDriver {
    detect_gpu_driver_in(Path::new("/sys/class/drm"))
}

/// A testable version of [`detect_gpu_driver`], parameterised by the sysfs root.
pub fn detect_gpu_driver_in(drm_root: &Path) -> GpuDriver {
    let Ok(entries) = std::fs::read_dir(drm_root) else {
        tracing::debug!("DRM sysfs unavailable; assuming an unknown GPU");
        return GpuDriver::Unknown;
    };

    let mut cards: Vec<_> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        // "card0", "card1"… (ignora "card0-HDMI-A-1" e "renderD128")
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
        tracing::info!(%card, %module, ?driver, "driver KMS detectado");
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
    fn detects_the_real_driver_on_this_machine() {
        // It does not assert which one (that varies per machine), only that
        // the function runs without panicking over the real sysfs.
        let driver = detect_gpu_driver();
        tracing::info!(?driver, "driver detectado");
    }
}
