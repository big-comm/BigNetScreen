//! Integração com serviços de rede do sistema (via D-Bus, build nativo).
//!
//! - **NetworkManager**: criação/descoberta de grupos Wi-Fi Direct (P2P) para
//!   o WFD-P2P, e detecção do driver KMS da GPU (para [`nd_core::pipeline`]).
//! - **firewalld**: garante a zona que abre a porta 7236/tcp só na interface
//!   P2P (porta o `nd-firewalld.c`).
//!
//! Indisponível sob Flatpak (sem barramento de sistema) — por isso fica
//! isolado neste crate, habilitado só no build nativo.

pub mod p2p;

use nd_core::pipeline::GpuDriver;
use nd_core::Result;

/// Detecta o driver KMS da GPU primária lendo
/// `/sys/class/drm/card*/device/driver` (porta `detect_gpu_kms_driver()`).
/// **TODO Fase 3.** Por ora devolve `Unknown` (encode por software).
pub fn detect_gpu_driver() -> GpuDriver {
    GpuDriver::Unknown
}

/// Garante a zona firewalld do WFD. **TODO Fase 3.**
pub async fn ensure_firewall_zone() -> Result<()> {
    Ok(())
}

/// Remove a zona firewalld ao encerrar. **TODO Fase 3.**
pub async fn release_firewall_zone() -> Result<()> {
    Ok(())
}
