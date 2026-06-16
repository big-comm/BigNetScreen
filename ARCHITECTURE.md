# Arquitetura e Roteiro — BigNetScreen (Rust)

Reescrita limpa do BigNetScreen. O projeto C original está em [`bkp/`](bkp/) e é
tratado como **especificação de referência / oráculo de teste** durante o port.

## Princípios

1. **Núcleo sem GUI e testável** (`nd-core`). Protocolo e tuning não dependem de GTK.
2. **Abstrações isolam ambiente e protocolo.** Dois eixos de variação:
   - *Captura*: portal vs Mutter (necessário para Flatpak vs nativo).
   - *Protocolo*: Chromecast vs WFD (traits `Provider`/`Sink`).
3. **Erros por `Result`**, nunca por ponteiro — elimina a classe de bug que
   derrubava o daemon C (`GError` por valor).
4. **Tuning portado, não reinventado.** Os valores de baixa latência vêm do
   HEAD do C (os *bons*, antes da regressão de 500 ms do working tree).

## Mapa de crates

```
nd-core         traits + tipos + pipeline.rs (tuning GStreamer)   ← coração
  ├── nd-capture   PortalBackend (ashpd) | MutterBackend (zbus)
  ├── nd-net       NetworkManager + firewalld (só nativo)
  ├── nd-chromecast  mDNS + Cast(protobuf/TLS) + HTTP(token+allowlist)
  ├── nd-wfd       RTSP(7236) + negociação WFD + P2P (usa nd-net)
  └── nd-gui       relm4 + libadwaita  → binário `bignetscreen`
```

### Abstrações-chave (em `nd-core`)

- `capture::CaptureBackend` — `start(SourceType) -> CaptureSource{fd, node_id}`.
  Seleção em runtime via `nd_capture::select_backend()`.
- `provider::Provider` — `start_discovery()` + stream de `DiscoveryEvent`.
- `sink::Sink` — máquina de estados explícita (`SinkState`), `start/stop_stream`.
  `Error` é **terminal** (corrige o bug do C que sobrescrevia erro com
  `Disconnected`).
- `pipeline` — `select_encoder()` (VAAPI > x264 > openh264, com guarda do driver
  Intel `xe`), `StreamConfig::scaled_bitrate_kbps()` (teto CBR por resolução),
  e descrições de pipeline WFD/Chromecast com filas curtas e `videorate`
  obrigatório.

## Modelo de concorrência

- GStreamer + relm4 no glib main context.
- Runtime tokio (worker thread) para o canal Cast (TLS/protobuf) e sockets;
  ponte com a GUI via mensagens relm4. Sem callback-soup, sem race manual.

## Decisões travadas

| Tema | Decisão |
| --- | --- |
| GUI | relm4 + libadwaita 1.9 (AdwToolbarView/StatusPage; ToggleGroup quando bindings ≥ 1.7) |
| Protocolos | Chromecast **e** WFD (ambos no escopo) |
| Distribuição | Nativo (PKGBUILD) **e** Flatpak |

### Caveat Flatpak × Miracast

WFD/Miracast precisa de NetworkManager (Wi-Fi Direct) + firewalld no barramento
de **sistema**, bloqueados no sandbox Flatpak. Portanto: **Chromecast roda pleno
em Flatpak; WFD pleno só no build nativo.** O app detecta o ambiente
(`nd_capture::select_backend`, `nd_net`) e degrada para "apenas Chromecast" sob
sandbox. Por isso `nd-net`/WFD ficam isolados em crates próprios.

## Roteiro

### Fase 0 — Spike (validar a stack)
- `nd-capture::PortalBackend` (ashpd ScreenCast) → `nd-core::pipeline` (encode)
  → arquivo/UDP. Prova captura+encode em Rust ponta-a-ponta.

### Fase 1 — Esqueleto compartilhado ✅ (em andamento)
- [x] Workspace Cargo + 6 crates compilando.
- [x] Traits `Provider`/`Sink`/`CaptureBackend` + `pipeline` com tuning + testes.
- [x] Shell relm4/libadwaita (janela + StatusPage).
- [ ] `MetaProvider` agregando providers numa lista observável.
- [ ] Lista de sinks na GUI ligada ao `MetaProvider`.

### Fase 2 — Chromecast (primeiro cast utilizável)
- `mdns-sd`: descoberta `_googlecast._tcp`.
- `tokio-rustls` + `prost`: canal Cast 8009 — **validar certificado**
  (aceitar só `UNKNOWN_CA`/`BAD_IDENTITY`; corrige o `return TRUE` cego do C).
- `hyper`: servidor HTTP com path-token UUID + allowlist de IP (mantém a boa
  prática do C).
- Pipeline `chromecast_pipeline_description` → `multisocketsink`.

### Fase 3 — WFD / Miracast (build nativo)
- `nd-net`: P2P via NetworkManager + zona firewalld.
- `gstreamer-rtsp-server`: source RTSP 7236 + negociação WFD M1–M7.
- **Reabilitar seleção de resolução/60 Hz** (o C deixou em `#if 0`).
- Portar quirks: encoder Intel `xe`, stall do monitor virtual, caps/`videorate`.

### Fase 4 — Paridade + distribuição
- Captura de janela, monitor virtual, áudio (PulseAudio/PipeWire), instalar codec.
- i18n: reaproveitar os `.po` de `bkp/po/` (28 idiomas).
- Packaging: `PKGBUILD` nativo + manifesto Flatpak (`build-aux/flatpak/`).
- CI (build + testes + lint) — ausente no projeto C.

## Dívidas/quirks a portar do C (não esquecer)

- `videorate` obrigatório entre fonte (taxa variável) e capsfilter fixo.
- Driver Intel `xe`: VAAPI trava → cair para software.
- Monitor virtual do Mutter não emite frames até negociar caps (bridge
  `intervideosink`/`intervideosrc` com `timeout` alto).
- Latência de 500 ms só faz sentido para `openh264`; x264/VAAPI usam ~20 ms.
- TLS do Chromecast precisa de validação real (o C aceitava qualquer cert).
- Nome vindo de mDNS deve ser sanitizado antes de virar argumento (injeção no
  módulo do PulseAudio no C).
