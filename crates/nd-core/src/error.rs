//! Tipo de erro unificado do núcleo.
//!
//! Em Rust o tratamento de erro é por `Result`, eliminando por construção a
//! classe de bug "GError por valor / NULL-deref" que derrubava o daemon do
//! projeto C (ver auditoria, `nd-daemon.c:86`).

use thiserror::Error;

/// Erro de qualquer camada do núcleo.
#[derive(Debug, Error)]
pub enum NdError {
    /// Falha originada no GStreamer (init, parse de pipeline, mudança de estado).
    #[error("gstreamer: {0}")]
    Gst(String),

    /// Falha na captura de tela (portal/Mutter).
    #[error("captura: {0}")]
    Capture(String),

    /// Falha de protocolo (negociação WFD/RTSP, canal Cast).
    #[error("protocolo: {0}")]
    Protocol(String),

    /// Falha de rede (D-Bus, NetworkManager, firewalld, sockets).
    #[error("rede: {0}")]
    Network(String),

    /// Funcionalidade indisponível no ambiente atual (ex.: WFD sob Flatpak).
    #[error("não suportado: {0}")]
    Unsupported(String),

    /// Operação cancelada (ex.: usuário desconectou durante a negociação).
    #[error("cancelado")]
    Cancelled,
}

/// Alias padrão do crate.
pub type Result<T> = std::result::Result<T, NdError>;
