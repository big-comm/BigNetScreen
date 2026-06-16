//! Construção dos pipelines GStreamer.
//!
//! **Este módulo é o ativo mais valioso do port.** A latência ("sem lag") é
//! ~90% configuração de pipeline + encode por hardware, e é idêntica em C e
//! Rust. Os valores abaixo são portados do projeto C de referência na branch
//! HEAD (os valores *bons*, antes de o working tree os reverter para 500 ms).
//!
//! Princípios:
//! - filas curtas (poucos ms), latência de pipeline baixa;
//! - encoder zero-latency (sem lookahead, sem B-frames);
//! - preferir **encode por hardware (VAAPI)** quando estável, com proteção
//!   contra o driver Intel `xe` (que trava no encode — quirk real do C).

use crate::{NdError, Result};
use gstreamer as gst;
use gst::prelude::*;

/// Inicializa o GStreamer uma única vez no processo.
pub fn init() -> Result<()> {
    gst::init().map_err(|e| NdError::Gst(e.to_string()))
}

/// Cria um `gst::Bin` a partir de uma descrição estilo `gst-launch`.
pub fn build_bin(description: &str) -> Result<gst::Element> {
    gst::parse::bin_from_description(description, true)
        .map(|bin| bin.upcast())
        .map_err(|e| NdError::Gst(e.to_string()))
}

// ------------------------------------------------------------------------
// Tuning de baixa latência (portado do HEAD C — NÃO usar os 500 ms do diff)
// ------------------------------------------------------------------------

/// Latência total do pipeline WFD (x264/VAAPI). O patch de 500 ms do working
/// tree era específico do openh264; aqui é o valor enxuto padrão.
pub const WFD_PIPELINE_LATENCY_MS: u64 = 20;
/// Latência do jitter buffer RTP (rtpbin/rtpjitterbuffer).
pub const WFD_RTP_JITTER_MS: u64 = 20;
/// Fila de vídeo antes do mux: poucos buffers, ~1 quadro.
pub const WFD_VIDEO_QUEUE_BUFFERS: u32 = 3;
pub const WFD_VIDEO_QUEUE_MS: u64 = 30;
/// Fila de áudio: pequena e limitada (o C tinha 100000 — bug).
pub const WFD_AUDIO_QUEUE_BUFFERS: u32 = 4;
pub const WFD_AUDIO_QUEUE_MS: u64 = 40;
/// openh264 com `usage-type=screen` tem picos; só ele justifica fila maior.
pub const OPENH264_PIPELINE_LATENCY_MS: u64 = 200;

/// Encoders H.264 candidatos, em ordem de preferência. Espelha
/// `encoder_priority()` do C: VAAPI > x264 > openh264.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum H264Encoder {
    /// `vah264enc` (GStreamer `va` plugin, moderno) — melhor opção.
    VaH264,
    /// `vaapih264enc` (plugin `vaapi` legado).
    VaapiH264,
    /// `x264enc` software, tune=zerolatency.
    X264,
    /// `openh264enc` software (fallback).
    OpenH264,
}

impl H264Encoder {
    /// Nome do elemento GStreamer.
    pub fn element(self) -> &'static str {
        match self {
            H264Encoder::VaH264 => "vah264enc",
            H264Encoder::VaapiH264 => "vaapih264enc",
            H264Encoder::X264 => "x264enc",
            H264Encoder::OpenH264 => "openh264enc",
        }
    }

    /// Peso de prioridade (maior = preferido), como em `encoder_priority()`.
    pub fn priority(self) -> u32 {
        match self {
            H264Encoder::VaH264 => 100,
            H264Encoder::VaapiH264 => 90,
            H264Encoder::X264 => 50,
            H264Encoder::OpenH264 => 40,
        }
    }
}

/// Driver KMS detectado (para decidir se o encode por HW é confiável).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuDriver {
    I915,
    /// Intel `xe`: encode VAAPI **trava** — preferir software (quirk do C).
    Xe,
    Amdgpu,
    Nvidia,
    Unknown,
}

/// Escolhe o melhor encoder disponível.
///
/// `available` é o conjunto de elementos presentes (consultar o registry do
/// GStreamer); `driver` vem de `nd-capture`/`nd-net`. Regra: se o driver for
/// `Xe`, descartar VAAPI mesmo presente.
pub fn select_encoder(available: &[H264Encoder], driver: GpuDriver) -> Option<H264Encoder> {
    available
        .iter()
        .copied()
        .filter(|enc| {
            let is_hw = matches!(enc, H264Encoder::VaH264 | H264Encoder::VaapiH264);
            !(is_hw && driver == GpuDriver::Xe)
        })
        .max_by_key(|enc| enc.priority())
}

/// Parâmetros de uma sessão de cast (resolução/fps/bitrate negociados).
#[derive(Clone, Copy, Debug)]
pub struct StreamConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub encoder: H264Encoder,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60, // preferir 60 Hz quando o sink anuncia (o C força 30)
            bitrate_kbps: 0, // 0 = derivar do teto escalado por resolução
            encoder: H264Encoder::X264,
        }
    }
}

impl StreamConfig {
    /// Teto de bitrate (CBR) escalado por resolução — porta o
    /// `get_max_bitrate_kbit()` do C (6/9/10/14/20 Mbit por faixa).
    pub fn scaled_bitrate_kbps(&self) -> u32 {
        if self.bitrate_kbps != 0 {
            return self.bitrate_kbps;
        }
        match self.width * self.height {
            n if n <= 640 * 480 => 6_000,
            n if n <= 1280 * 720 => 9_000,
            n if n <= 1920 * 1080 => 10_000,
            n if n <= 2560 * 1440 => 14_000,
            _ => 20_000,
        }
    }

    /// Flags do `x264enc` para zero-latency (porta do bloco C).
    fn x264_props(&self) -> String {
        format!(
            "x264enc tune=zerolatency speed-preset=ultrafast \
             rc-lookahead=0 sync-lookahead=0 bframes=0 b-adapt=false \
             sliced-threads=true aud=true cabac=false ref=1 pass=cbr \
             vbv-buf-capacity=50 key-int-max={gop} bitrate={br}",
            gop = self.fps * 2,
            br = self.scaled_bitrate_kbps(),
        )
    }
}

/// Monta a descrição do pipeline WFD (Miracast):
/// `pipewiresrc → videoconvert → videoscale → videorate → encoder →
///  h264parse → mpegtsmux → rtpmp2tpay`.
///
/// Observações de correção portadas do C:
/// - `videorate` é **obrigatório** entre a fonte (taxa variável do Mutter) e o
///   capsfilter de taxa fixa, senão a negociação falha após o PLAY;
/// - ordem `convert → scale` (converter para I420 antes de escalar é mais
///   barato que escalar em RGBA);
/// - `h264parse config-interval=-1` reinsere SPS/PPS a cada IDR.
pub fn wfd_pipeline_description(cfg: &StreamConfig) -> String {
    let enc = match cfg.encoder {
        H264Encoder::X264 => cfg.x264_props(),
        other => format!("{} bitrate={}", other.element(), cfg.scaled_bitrate_kbps()),
    };
    format!(
        "pipewiresrc do-timestamp=true keepalive-time=1000 resend-last=true ! \
         videoconvert n-threads=0 ! \
         videoscale ! videorate drop-only=true ! \
         video/x-raw,format=I420,width={w},height={h},framerate={fps}/1 ! \
         queue max-size-buffers={vqb} max-size-time={vqms}000000 max-size-bytes=0 leaky=downstream ! \
         {enc} ! h264parse config-interval=-1 ! \
         mpegtsmux alignment=7 ! rtpmp2tpay",
        w = cfg.width,
        h = cfg.height,
        fps = cfg.fps,
        vqb = WFD_VIDEO_QUEUE_BUFFERS,
        vqms = WFD_VIDEO_QUEUE_MS,
        enc = enc,
    )
}

/// Monta a descrição do pipeline Chromecast (H.264/MKV):
/// `… → h264parse → matroskamux → multisocketsink`.
///
/// `matroskamux` com clusters curtos (50–100 ms) melhora o tempo até o
/// primeiro quadro no receptor.
pub fn chromecast_pipeline_description(cfg: &StreamConfig) -> String {
    let enc = match cfg.encoder {
        H264Encoder::X264 => cfg.x264_props(),
        other => format!("{} bitrate={}", other.element(), cfg.scaled_bitrate_kbps()),
    };
    format!(
        "pipewiresrc do-timestamp=true ! \
         videoconvert n-threads=0 ! videoscale ! videorate drop-only=true ! \
         video/x-raw,format=I420,width={w},height={h},framerate={fps}/1 ! \
         queue max-size-buffers={vqb} max-size-time={vqms}000000 leaky=downstream ! \
         {enc} ! h264parse config-interval=-1 ! \
         matroskamux streamable=true min-cluster-duration=50000000 \
         max-cluster-duration=100000000 ! multisocketsink sync=true",
        w = cfg.width,
        h = cfg.height,
        fps = cfg.fps,
        vqb = WFD_VIDEO_QUEUE_BUFFERS,
        vqms = WFD_VIDEO_QUEUE_MS,
        enc = enc,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_scales_with_resolution() {
        let mut cfg = StreamConfig::default();
        cfg.width = 1280;
        cfg.height = 720;
        assert_eq!(cfg.scaled_bitrate_kbps(), 9_000);
        cfg.width = 3840;
        cfg.height = 2160;
        assert_eq!(cfg.scaled_bitrate_kbps(), 20_000);
    }

    #[test]
    fn xe_driver_disables_hardware_encode() {
        let avail = [H264Encoder::VaH264, H264Encoder::X264];
        // Com driver estável, VAAPI vence.
        assert_eq!(
            select_encoder(&avail, GpuDriver::I915),
            Some(H264Encoder::VaH264)
        );
        // Com Intel xe, cai para software.
        assert_eq!(
            select_encoder(&avail, GpuDriver::Xe),
            Some(H264Encoder::X264)
        );
    }

    #[test]
    fn pipelines_contain_videorate() {
        // videorate é obrigatório (negociação de caps com o Mutter).
        let cfg = StreamConfig::default();
        assert!(wfd_pipeline_description(&cfg).contains("videorate"));
        assert!(chromecast_pipeline_description(&cfg).contains("videorate"));
    }
}
