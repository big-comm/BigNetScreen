<div align="center">

# BigNetScreen

**Transmita sua tela Linux para receptores Miracast (Wi-Fi Display) e Chromecast.**

Reescrita limpa em **Rust** — leve, sem lag, interface moderna.

[![License: GPL-3.0-or-later](https://img.shields.io/badge/License-GPL%203.0--or--later-blue.svg)](COPYING)
[![Language: Rust](https://img.shields.io/badge/Language-Rust-CE412B.svg)](https://www.rust-lang.org/)
[![GTK 4 / libadwaita](https://img.shields.io/badge/GTK4-libadwaita-4A86CF.svg)](https://gtk.org)

</div>

> ⚠️ **Estado: scaffold inicial (Fase 1).** O workspace compila e abre a janela.
> A implementação dos protocolos está em andamento — ver [ARCHITECTURE.md](ARCHITECTURE.md).

## Por que Rust

Esta é uma **reescrita do zero**, não um fork. O projeto C original (um fork do
GNOME Network Displays) está arquivado em [`bkp/`](bkp/) e serve de **referência
de protocolo** durante o port — em especial o tuning de pipeline GStreamer e os
*quirks* de dispositivos. Uma auditoria do C encontrou ~50 bugs de memória/ciclo
de vida (use-after-free, vazamentos, `GError` por valor que derrubava o daemon);
em Rust, essa classe inteira de bug deixa de existir por construção.

> Nota honesta: a "leveza/sem lag" vem ~90% do **tuning do pipeline GStreamer +
> encode por hardware (VAAPI)**, que é idêntico em C e Rust. O ganho do Rust é
> correção, manutenção e uma base limpa — o tuning é portado em
> [`nd-core::pipeline`](crates/nd-core/src/pipeline.rs).

## Arquitetura (workspace Cargo)

| Crate | Papel |
| --- | --- |
| `nd-core` | Traits `Provider`/`Sink`/`CaptureBackend`, tipos e **construção dos pipelines GStreamer** (sem GUI). |
| `nd-capture` | Captura de tela: portal (`ashpd`) e Mutter direto (`zbus`). |
| `nd-net` | NetworkManager (Wi-Fi Direct) e firewalld via D-Bus (só nativo). |
| `nd-chromecast` | Descoberta mDNS, canal Cast (protobuf/TLS), servidor HTTP do stream. |
| `nd-wfd` | Servidor RTSP + negociação WFD + P2P (Miracast). |
| `nd-gui` | Aplicativo **relm4 + libadwaita** (binário `bignetscreen`). |

## Compilar e executar

Dependências de sistema (já presentes em BigLinux/Manjaro): `gtk4 ≥ 4.10`,
`libadwaita ≥ 1.5`, `gstreamer ≥ 1.20` + plugins, e a toolchain Rust.

```sh
cargo run -p nd-gui          # abre a GUI
cargo test                   # roda os testes
cargo check --workspace      # checagem rápida
```

## Distribuição

- **Nativo (Arch/BigLinux)**: suporte completo, incluindo Miracast/WFD.
- **Flatpak**: Chromecast pleno; Miracast/WFD é limitado pelo sandbox
  (sem NetworkManager/firewalld no barramento de sistema).

## Roteiro

Ver [ARCHITECTURE.md](ARCHITECTURE.md). Resumo: Fase 0 (spike captura→encode) →
Fase 1 (esqueleto/GUI) → Fase 2 (Chromecast) → Fase 3 (WFD) → Fase 4
(paridade + packaging nativo/Flatpak).

## Créditos

- Projeto de referência: [GNOME Network Displays](https://gitlab.gnome.org/GNOME/gnome-network-displays).
- Reescrita Rust e tuning: **[BigCommunity](https://github.com/big-comm)** / Tales A. Mendonça.

## Licença

[GPL-3.0-or-later](COPYING).
