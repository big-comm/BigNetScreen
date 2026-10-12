# BigNetScreen

**Leve a tela do Linux para uma TV, um projetor ou um navegador.**

[English](README.md) · [Compilar e desenvolver](docs/development.md) · [Ajuda](SUPPORT.md)

Apresente uma janela sem mostrar o restante da área de trabalho, envie vídeos e músicas para uma televisão ou use uma tela virtual quando seu ambiente gráfico permitir. O BigNetScreen reúne captura, descoberta de dispositivos e controles de transmissão em uma aplicação nativa GTK4/libadwaita, escrita em Rust.

## Escolha como transmitir

| Destino | O que é necessário |
| --- | --- |
| Chromecast / Google Cast | Computador e receptor na mesma rede local confiável. O programa tenta espelhamento e oferece um caminho HTTP de compatibilidade. Também envia arquivos e músicas. |
| Miracast | Instalação nativa, adaptador Wi-Fi Direct compatível e receptor no modo de espelhamento. |
| Televisão DLNA | Computador e televisão na mesma rede local confiável. O reprodutor da televisão acumula mais atraso que o espelhamento. Também recebe arquivos e músicas. |
| Navegador | Plugin WebRTC do gst-plugins-rs e plugin libnice no computador transmissor. Quem assiste abre o endereço ou QR code e informa o PIN. |
| NDI | Runtime proprietário instalado separadamente, apenas para esse modo. Os demais modos não dependem dele. |

O programa prefere codificação H.264 por hardware quando os drivers e plugins conseguem produzir vídeo; há alternativas por software. Resolução, FPS e atraso variam com a origem, o receptor e a rede. Não prometemos o mesmo desempenho em todos os aparelhos.

A versão atual é candidata a release. O que falta para declarar uma versão estável, incluindo testes com receptores reais, está no [checklist de release](docs/releasing.md).

## Primeiro uso

Instale o pacote `bignetscreen` pelo gerenciador de programas da sua distribuição, quando disponível. Para compilar, consulte o [guia de desenvolvimento](docs/development.md). O repositório tem um manifesto Flatpak, mas não há pacote publicado no Flathub.

1. Abra o BigNetScreen e ative o modo Cast ou Espelhamento de Tela no receptor.
2. Escolha uma tela, uma janela ou uma tela virtual e selecione o destino. Autorize a captura na janela do sistema.
3. Para assistir pelo navegador, abra no outro aparelho o endereço ou QR code mostrado pelo aplicativo e informe o PIN.
4. Use **Parar** antes de trocar de destino. Fechar a janela não interrompe o compartilhamento: um serviço em segundo plano mantém a transmissão até você pará-la.

Comece em uma rede doméstica ou de trabalho confiável. Redes de convidados, isolamento entre clientes, VPNs e regras de firewall podem impedir a descoberta ou a conexão do receptor de volta ao computador. Consulte [SUPPORT.md](SUPPORT.md) para coletar informações úteis sem expor senhas e tokens.

## Compatibilidade e segurança

O espelhamento Cast usa RTP, enquanto o fallback HTTP envia H.264/AAC em MPEG-TS para o reprodutor do receptor. Esse reprodutor pode acumular alguns segundos antes de exibir a imagem: fallback é compatibilidade, não garantia de baixa latência.

No Flatpak, a captura passa pelo portal da área de trabalho e o Miracast não está disponível, porque depende do NetworkManager no barramento do sistema. Telas virtuais dependem do ambiente gráfico. A seleção de janela é solicitada a cada vez, para não transmitir silenciosamente uma janela antiga.

Use apenas redes confiáveis. Cada Chromecast é lembrado pela chave de dispositivo que apresenta no primeiro uso, mas essa chave não é verificada junto à autoridade certificadora do Google, então a primeira conexão não é autenticada. A página do navegador, o PIN e o fallback HTTP não são criptografados. Não exponha as portas do programa à Internet. Leia [SECURITY.md](SECURITY.md).

## Participe

Relate o modelo e o firmware do seu receptor, ajude nas traduções ou teste ciclos de conectar, parar e reconectar. Para alterar o código, comece por [CONTRIBUTING.md](CONTRIBUTING.md) e [ARCHITECTURE.md](ARCHITECTURE.md). A documentação técnica é mantida em inglês para facilitar contribuições internacionais.

O BigNetScreen foi útil? Dê uma estrela ao repositório e apresente o projeto a outras pessoas que usam Linux.

Licença [GPL-3.0-or-later](COPYING). O plugin NDI incluído tem licença MPL-2.0; o runtime proprietário NDI não acompanha o programa.
