# BigNetScreen

**Leve a tela do Linux para uma TV, um projetor ou um navegador.**

[English](README.md) · [Compilar e desenvolver](docs/development.md) · [Ajuda](SUPPORT.md)

Apresente uma janela sem mostrar o restante da área de trabalho, reproduza arquivos locais em uma tela maior ou use um monitor virtual quando seu ambiente gráfico permitir. O BigNetScreen reúne captura, descoberta de dispositivos e controles de transmissão em uma aplicação nativa GTK4/libadwaita, escrita em Rust.

## Escolha como transmitir

| Destino | O que é necessário |
| --- | --- |
| Chromecast / Google Cast | Computador e receptor na mesma rede local confiável. O programa tenta espelhamento e oferece um caminho HTTP de compatibilidade. |
| Miracast | Instalação nativa, adaptador Wi-Fi Direct compatível e receptor no modo de espelhamento. |
| Navegador | Plugins WebRTC/ICE no computador transmissor. Quem assiste abre o endereço ou QR code e informa o PIN. |
| NDI | Runtime proprietário instalado separadamente, apenas para esse modo. Os demais modos não dependem dele. |

O programa prefere codificação H.264 por hardware quando os drivers e plugins conseguem produzir vídeo; há alternativas por software. Resolução, FPS e atraso variam com a origem, o receptor e a rede. Não prometemos o mesmo desempenho em todos os aparelhos.

## Primeiro uso

Instale o pacote `bignetscreen` pelo gerenciador de programas da sua distribuição, quando disponível. Para compilar, consulte o [guia de desenvolvimento](docs/development.md). O manifesto Flatpak no repositório não significa que já exista um pacote publicado no Flathub.

1. Abra o BigNetScreen e ative o modo Cast ou Espelhamento de Tela no receptor.
2. Escolha uma tela, uma janela ou um monitor virtual disponível e selecione o destino. Autorize a captura na janela do sistema.
3. Para assistir pelo navegador, abra o endereço mostrado pelo aplicativo e informe o PIN.
4. Use **Parar** antes de trocar de destino. Ao fechar a janela, o programa solicita o encerramento e aguarda a limpeza da sessão.

Comece em uma rede doméstica ou de trabalho confiável. Redes de convidados, isolamento entre clientes, VPNs e regras de firewall podem impedir a descoberta ou a conexão do receptor de volta ao computador. Consulte [SUPPORT.md](SUPPORT.md) para coletar informações úteis sem expor senhas e tokens.

## Compatibilidade e segurança

O espelhamento Cast usa RTP, enquanto o fallback HTTP envia H.264/AAC em MPEG-TS para o reprodutor do receptor. Esse reprodutor pode acumular alguns segundos antes de exibir a imagem: fallback é compatibilidade, não garantia de baixa latência.

Miracast depende da integração nativa com a rede; monitores virtuais dependem do ambiente gráfico. No Flatpak, a captura passa pelo portal do desktop. A seleção de janela é solicitada novamente para não transmitir silenciosamente uma janela antiga.

Use apenas redes confiáveis. A autenticação da identidade de um dispositivo Cast ainda não está implementada; a página de PIN/controle do navegador e o fallback HTTP não têm criptografia de ponta a ponta. Não exponha as portas do programa à Internet. Leia [SECURITY.md](SECURITY.md).

**Estado desta branch:** candidata a release, ainda sujeita aos testes automatizados e à homologação em hardware descritos no [checklist](docs/releasing.md). Um teste de pipeline não certifica todos os televisores.

## Participe

Relate o modelo e firmware do seu receptor, ajude nas traduções ou teste ciclos de conectar, parar e reconectar. Para alterar o código, comece por [CONTRIBUTING.md](CONTRIBUTING.md), [ARCHITECTURE.md](ARCHITECTURE.md) e [AGENTS.md](AGENTS.md). A documentação técnica é mantida em inglês para facilitar contribuições internacionais.

O BigNetScreen foi útil? Dê uma estrela ao repositório e apresente o projeto a outras pessoas que usam Linux. Relatos reproduzíveis de compatibilidade ajudam a transformar testes locais em suporte confiável.

Licença [GPL-3.0-or-later](COPYING). O plugin NDI incluído tem licença MPL-2.0; o runtime proprietário NDI não acompanha o programa.
