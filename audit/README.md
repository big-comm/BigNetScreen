> Historical R1/R2 audit tooling and evidence, not a current release approval. Use [the maintained test plan](../docs/testing.md) and logs for the exact candidate commit.

# Entrega de revisão — 20/09/2026

**Código candidato, não compilado e não homologado nesta revisão.** Leia `REVIEW-20260920.md` antes de aplicar ou testar. Os testes do agente anterior não aprovam estas alterações.

## Rodada R2 — ambiente e toolchain obrigatório

Leia `REVIEW-R2-20260920.md`. Rust 1.98.1 foi exigido no validador, mas o
arquivo não pôde ser materializado nesta sessão; **nenhum Rust foi instalado
ou executado**. O núcleo passou no probe nativo, enquanto faltam bibliotecas
reais de GTK4/libadwaita/Graphene para o link da interface.

`tools/check-rust-toolchain.py` verifica um prefixo já instalado, sem baixar
ou instalar. `tools/probe-buildenv.py` distingue metadados pkg-config de
link/execução reais. `tools/test-toolchain-preflight.py` testa apenas o checker
com mocks, não o compilador. Resultados reais: `evidence/r2/`.

## Arquivos

- `REVIEW-20260920.md`: achados, mudanças, evidências, cobertura, riscos e fontes.
- `validate-offline.sh`: gates Rust sequenciais, limitados e offline; não instala nada.
- `tools/probe-gst-clock.py`: reproduz o deslocamento de PTS usando o GStreamer instalado (ABI Linux amd64); não executa BigNetScreen.
- `tools/validate-models.py`: modelos Python e verificações estáticas; não substitui compilação.
- `tools/analyse-evidence.py`: analisa os arquivos H.264 originais, sem afirmar FPS ponta a ponta ou qualidade perceptual.
- `evidence/`: resultados realmente executados nesta revisão.

## Preparar o build

Extraia o buildenv original e o novo checkpoint **num diretório novo**, ambos contendo `bignetscreen-audit/`. Não extraia o source antigo por cima do novo. O checkpoint não duplica o vendor externo de 390 crates, nem inclui caches/target/toolchain.

```sh
tar -xzf bignetscreen-buildenv-2026-09-20.tar.gz
tar -xzf BIGNETSCREEN-REVIEW-20260920.tar.gz
cd bignetscreen-audit
./setup.sh
source ./build-env.sh
cd repo
./audit/validate-offline.sh
```

O Rust **1.98.1** precisa estar previamente instalado em
`/mnt/data/toolchains/rust-1.98.1` (ou definir `BIGNETSCREEN_RUST_PREFIX`). Não usar rustup/downloads para tentar reparar este sandbox. `setup.sh` do buildenv grava caminhos absolutos; usar sysroot recém-extraído e regenerar a configuração em cada local, sem transportar `.cargo` ou `build-env.sh` de outro diretório. A existência do vendor não prova que o compiler ou todos os headers estejam disponíveis.

## Reproduzir evidências sem Rust

```sh
cd bignetscreen-audit/repo
python3 audit/tools/validate-models.py
# ctypes ABI probe: Linux amd64, GStreamer + x264; rodar com timeout.
timeout --signal=TERM --kill-after=5s 20s python3 audit/tools/probe-gst-clock.py
python3 audit/tools/analyse-evidence.py ../evidence
```

Pygments é opcional: sua ausência marca apenas o check lexical como não executado. Os dois primeiros checks não demonstram interoperabilidade com Chromecast.

## Patches

O patch **incremental** e a série partem de `7addb67`: o snapshot com as mudanças não commitadas recebidas já aplicadas. Não aplicá-los sobre o HEAD antigo limpo.

O patch **completo** parte de `a762d33`, incluindo as alterações do agente anterior. Não aplicá-lo sobre a árvore suja original. Preferir o checkpoint com `.git`, que preserva ambas as bases e os commits da revisão.

Exemplo, com a base incremental exata e árvore limpa:

```sh
git apply --check /caminho/BIGNETSCREEN-REVIEW-incremental.patch
git apply /caminho/BIGNETSCREEN-REVIEW-incremental.patch
```

`CHECKPOINT-VALIDATION.json` e `SHA256SUMS` acompanham o pacote externamente para evitar hash autorreferente. A restauração e aplicação dos patches são verificações de integridade, não substituem testes Rust ou de hardware.
