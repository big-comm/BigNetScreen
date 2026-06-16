//! Abstração de captura de tela.
//!
//! Duas implementações concretas (no crate `nd-capture`) por trás do mesmo
//! trait, escolhidas em runtime conforme o ambiente:
//!
//! - **Portal** (`ashpd` / xdg-desktop-portal `ScreenCast`): obrigatório sob
//!   Flatpak e funciona em qualquer desktop (KDE, GNOME, …).
//! - **Mutter direto** (`org.gnome.Mutter.ScreenCast` via D-Bus): caminho de
//!   menor latência no GNOME nativo, inclui captura de **monitor virtual**.
//!
//! Esta separação é o que permite a meta "nativo + Flatpak" sem `#ifdef`s
//! espalhados pelo código.

use async_trait::async_trait;
use std::os::fd::OwnedFd;

use crate::Result;

/// O que será capturado.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceType {
    /// Um monitor físico inteiro.
    Monitor,
    /// Uma janela específica (requer backend com suporte a window-capture).
    Window,
    /// Um monitor **virtual** criado sob demanda (caminho Mutter; ideal p/ WFD).
    Virtual,
}

/// Stream PipeWire pronto para alimentar o pipeline de encode.
///
/// `pipewire_fd` é o descritor remoto entregue pelo portal/Mutter; `node_id`
/// identifica o nó dentro desse fd. Ambos são consumidos por `pipewiresrc`.
#[derive(Debug)]
pub struct CaptureSource {
    /// Descritor do socket PipeWire (posse transferida ao pipeline).
    pub pipewire_fd: OwnedFd,
    /// Nó PipeWire a consumir.
    pub node_id: u32,
    /// Tipo efetivo da fonte.
    pub source_type: SourceType,
    /// Dimensões conhecidas, quando o backend as informa (None = negociar).
    pub size: Option<(u32, u32)>,
}

/// Backend de captura. Implementações devem ser thread-safe.
#[async_trait]
pub trait CaptureBackend: Send + Sync {
    /// Identificador curto para logs/telemetria (`"portal"`, `"mutter"`).
    fn id(&self) -> &'static str;

    /// Verifica se este backend pode operar no ambiente atual.
    async fn is_available(&self) -> bool;

    /// Inicia a captura e devolve o stream PipeWire.
    async fn start(&self, source_type: SourceType) -> Result<CaptureSource>;

    /// Encerra a sessão de captura e libera recursos no compositor.
    async fn stop(&self) -> Result<()>;
}
