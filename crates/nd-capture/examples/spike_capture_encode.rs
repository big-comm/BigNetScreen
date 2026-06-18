//! Spike da Fase 0: prova a stack de mídia em Rust ponta-a-ponta.
//!
//! Captura a tela via portal (`ashpd`) → `pipewiresrc` → `x264enc` → arquivo
//! MP4. Valida que captura + encode funcionam antes de investir nos protocolos.
//!
//! ```sh
//! cargo run -p nd-capture --example spike_capture_encode
//! ```
//!
//! Precisa de uma sessão gráfica: o portal abre um diálogo para escolher o
//! monitor. Saída: `/tmp/bignetscreen-spike.mp4` (≈ 8 s de gravação).

use std::os::fd::AsRawFd;
use std::time::Duration;

use gstreamer as gst;
use gst::prelude::*;
use nd_capture::PortalBackend;
use nd_core::capture::{CaptureBackend, SourceType};

const OUTPUT: &str = "/tmp/bignetscreen-spike.mp4";
const RECORD_SECS: u64 = 8;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    gst::init()?;

    tracing::info!("solicitando captura ao portal (escolha um monitor no diálogo)…");
    let backend = PortalBackend::new();
    let source = backend.start(SourceType::Monitor).await?;
    tracing::info!(node = source.node_id, "stream PipeWire obtido");

    // O fd precisa permanecer aberto enquanto o pipeline grava; `source` é
    // mantido vivo no escopo até o fim.
    let fd = source.pipewire_fd.as_raw_fd();
    let desc = format!(
        "pipewiresrc fd={fd} path={node} do-timestamp=true ! \
         videoconvert n-threads=0 ! videorate ! video/x-raw,format=I420 ! \
         x264enc tune=zerolatency speed-preset=ultrafast bitrate=8000 ! \
         h264parse ! mp4mux ! filesink location={out}",
        fd = fd,
        node = source.node_id,
        out = OUTPUT,
    );
    tracing::info!("pipeline: {desc}");

    let pipeline = gst::parse::launch(&desc)?
        .downcast::<gst::Pipeline>()
        .expect("gst::parse::launch devolve um Pipeline");

    pipeline.set_state(gst::State::Playing)?;
    tracing::info!("gravando por {RECORD_SECS}s…");
    tokio::time::sleep(Duration::from_secs(RECORD_SECS)).await;

    // EOS para o mp4mux fechar o container corretamente.
    pipeline.send_event(gst::event::Eos::new());
    let bus = pipeline.bus().expect("pipeline tem bus");
    if let Some(msg) = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(5),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    ) {
        if let gst::MessageView::Error(err) = msg.view() {
            tracing::error!("erro do pipeline: {} ({:?})", err.error(), err.debug());
        }
    }

    pipeline.set_state(gst::State::Null)?;
    backend.stop().await?;
    drop(source); // só agora é seguro fechar o fd

    tracing::info!("pronto: {OUTPUT}");
    Ok(())
}
