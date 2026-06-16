//! Abstração de **sink** (receptor): o ciclo de vida de uma sessão de cast.
//!
//! Espelha o bom design do `NdSink` do projeto C, mas com a máquina de estados
//! explícita num `enum` (em vez de inteiros soltos) e ciclo de vida `async`
//! (sem a sopa de callbacks + `GCancellable` manual).

use async_trait::async_trait;

use crate::capture::CaptureSource;
use crate::Result;

/// Protocolo do receptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinkKind {
    /// Google Chromecast (mDNS + Cast + HTTP).
    Chromecast,
    /// Wi-Fi Display sobre Wi-Fi Direct / P2P.
    WfdP2p,
    /// Wi-Fi Display sobre infraestrutura (MICE, LAN comum).
    WfdMice,
    /// Sink falso para testes (`NETWORK_DISPLAYS_DUMMY`).
    Dummy,
}

/// Estados possíveis de uma sessão. A transição para [`SinkState::Error`] é
/// terminal e **não** deve ser silenciosamente sobrescrita por `Disconnected`
/// (bug real do C: `closed_cb` mascarava o erro na UI).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinkState {
    /// Ocioso, disponível para conectar.
    Disconnected,
    /// Garantindo a zona de firewall (apenas WFD/P2P no build nativo).
    EnsuringFirewall,
    /// Aguardando o socket de transporte do receptor.
    WaitSocket,
    /// Conectado; aguardando o início do fluxo de mídia.
    WaitStreaming,
    /// Transmitindo.
    Streaming,
    /// Erro terminal; `message` descreve a causa para a UI.
    Error,
}

/// Dados estáveis de identificação de um sink (para a lista da GUI).
#[derive(Clone, Debug)]
pub struct SinkInfo {
    /// Identificador único e estável da instância (UUID).
    pub id: String,
    /// Nome amigável anunciado pelo receptor.
    pub display_name: String,
    /// Protocolo.
    pub kind: SinkKind,
    /// Endereço/host quando aplicável (IP do Chromecast, MAC P2P, …).
    pub address: Option<String>,
}

/// Receptor conectável. Implementações devem ser thread-safe.
#[async_trait]
pub trait Sink: Send + Sync {
    /// Identificação estável para a UI.
    fn info(&self) -> SinkInfo;

    /// Estado atual da máquina.
    fn state(&self) -> SinkState;

    /// Mensagem associada ao estado [`SinkState::Error`], se houver.
    fn error_message(&self) -> Option<String> {
        None
    }

    /// Inicia o cast a partir de uma fonte de captura já aberta.
    async fn start_stream(&self, source: CaptureSource) -> Result<()>;

    /// Encerra o cast e retorna o sink a [`SinkState::Disconnected`].
    async fn stop_stream(&self) -> Result<()>;
}
