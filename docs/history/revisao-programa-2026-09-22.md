# Revisão do BigNetScreen — 22 de setembro de 2026

## Implementação após a revisão

As correções abaixo foram aplicadas ao checkout, sem novas dependências. A revisão inicial, preservada depois desta seção, descreve o comportamento anterior; suas referências de linha não representam o código modificado.

| Área | Resultado implementado |
| --- | --- |
| Estado e encerramento | Erros sobrevivem à atualização periódica; `stopping` permanece até terminar a limpeza. Cancelar arquivos mantém a sessão até o encerramento real. |
| Preferências e áudio | Gravação serializada e fora da thread GTK, publicação atômica com sincronização do diretório, falhas visíveis e gravação aguardada antes de transmitir. Fechar também grava mudanças pendentes e notifica o serviço. Sem alterações, a GUI não regrava a configuração. Áudio seletivo cria/remove a saída, não depende dela quando o áudio do sistema está desligado e adia remoções enquanto há transmissão. |
| Serviço e GUI | Perda do dono D-Bus limpa disponibilidade antiga e inicia reconexão. A janela mantém o serviço disponível com uma renovação por minuto; depois que ela fecha e não há transmissão, o serviço volta a poder sair. |
| Descoberta | Provedores inicializam independentemente, com prazo. Miracast tenta novamente mesmo quando a primeira abertura falha. DLNA entrega resultados progressivos, consulta quatro descrições por vez, cancela ao perder o consumidor e sinaliza recuperação. |
| Várias redes | SSDP usa um endereço de origem por conexão LAN ativa do NetworkManager, reavaliada a cada busca. Enumera até 64 conexões; exclui tipos de túnel/VPN e loopback. Sem NetworkManager ou sem resultado em dois segundos, conserva a rota escolhida pelo kernel. Isso não oferece enumeração de todas as redes em instalações que usam outro gerenciador. |
| Memória e processamento | Filas mDNS/Miracast limitadas; registros de receptores limitados a 256; fila de arquivos limitada a 1000. Cache de pixels limitado a 16 MiB; texturas da fila usam até 48 px, em vez de 360 px. Pré-visualizações pendentes da fila são canceladas quando ela muda. A GUI consome propriedades D-Bus alteradas e atualiza somente os campos/páginas necessários. |
| Interface | Destino antes de conteúdo; ajuda recolhida; navegação adaptativa; escolhas avançadas recolhidas; orientação próxima dos controles de mídia; resumo de áudio e acesso às configurações de som. Erros em linguagem simples, com detalhes técnicos separados. Seleção e contagem acompanham o filtro. Capacidades de envio são compartilhadas pelas páginas; AirPlay informa que só é detectado. |
| Clareza | A medição diferencia pendente, sem resposta e resposta válida de zero milissegundos. Ela é apresentada como resposta da rede, sem prometer atraso da imagem. Foram removidas as promessas fixas de resposta instantânea/rápida por protocolo. POT e PT-BR atualizados; arquitetura corrigida para dez crates e serviço proprietário das sessões. |

A seleção de origem multicast segue o comportamento do [kernel Linux](https://raw.githubusercontent.com/torvalds/linux/master/net/ipv4/route.c); a enumeração usa as propriedades documentadas de [conexões ativas do NetworkManager](https://networkmanager.dev/docs/api/latest/gdbus-org.freedesktop.NetworkManager.Connection.Active.html). Não houve alteração nas interfaces, rotas ou configurações da rede pessoal.

### Evidência da implementação

Artefatos: [diretório de implementação](/home/bruno/.cache/bns-implementation-20260922). Compilações e testes usaram Rust 1.98.1, `--offline --locked`, um alvo de build e um lock, jobs=1, incremental=0 e debug info desabilitado. Os testes foram executados com diretório temporário em filesystem executável.

| Verificação | Resultado |
| --- | --- |
| Workspace | **390 aprovados, zero falhas, 11 ignorados**, em 22 resultados de suites, incluindo doctests. [Log](/home/bruno/.cache/bns-implementation-20260922/tests-complete.log). |
| GTK adicional | Cinco testes normalmente ignorados executados separadamente: descoberta/filtro, biblioteca de mídia, edição da fila, capturas de layout e capa musical. Todos passaram. [Filtro final](/home/bruno/.cache/bns-implementation-20260922/devices-gtk-final.log), [layouts](/home/bruno/.cache/bns-implementation-20260922/layouts-final.log). |
| Áudio adicional | Teste de criar, encontrar, repetir e remover a saída passou em PipeWire/Pulse isolado. [Log](/home/bruno/.cache/bns-implementation-20260922/audio/virtual-card-test.log). |
| Descoberta com duas origens | Teste UDP em loopback confirmou as duas origens, uma identidade deduplicada e entrega em menos de um segundo, antes de acabar a busca de 5,4 s. Consulta somente de leitura ao NetworkManager conferiu os endereços contra `ip address`; nenhum anúncio foi enviado à rede física por essa consulta. [Teste](/home/bruno/.cache/bns-implementation-20260922/tests-dlna-multisource.log). |
| Check, Clippy e formatação | Workspace com todos os targets verificado; Clippy com `-D warnings`; formatação e `git diff --check`. [Check](/home/bruno/.cache/bns-implementation-20260922/check-complete.log), [Clippy](/home/bruno/.cache/bns-implementation-20260922/clippy-delivery.log). |
| Catálogo e ferramentas | `msgfmt --check`, `msgcmp`, verificador de projeto e quatro testes das ferramentas de desenvolvimento aprovados. As 217 mensagens ativas têm tradução PT-BR, sem entradas fuzzy. |

Os seis testes adicionais não mudam a configuração `ignored` da suite padrão. Permaneceram sem execução os quatro testes/benchmarks NDI e o teste de firewall.

O efeito GUI → arquivo → serviço → áudio foi observado: ligar o controle criou `bignetscreen`; desligá-lo removeu a saída. Com permissão de escrita retirada apenas da configuração isolada, o programa mostrou erro, conservou o arquivo e ofereceu manter a janela aberta. Após restaurar a permissão, fechar gravou a preferência e aplicou a saída no serviço. [Criação](/home/bruno/.cache/bns-implementation-20260922/audio-gui-created.txt), [remoção](/home/bruno/.cache/bns-implementation-20260922/audio-gui-off.txt), [recuperação no fechamento](/home/bruno/.cache/bns-implementation-20260922/save-recovery-close.json).

Também foram observadas perda/reconexão do serviço, ativação pelo descritor D-Bus em barramento privado e saída após fechar o último cliente. Executáveis carregados foram conferidos por `/proc/PID/exe` e hashes. A ativação privada não substitui instalar e iniciar o pacote no sistema. As sessões gráficas e servidores de áudio da validação foram encerrados.

O GTK foi examinado em português e tema claro, com navegação, teclado e AT-SPI. O teste de layout produziu 11 capturas em claro/escuro, incluindo largura de 480 px. A captura do harness usa inglês; as capturas do programa completo usam PT-BR. Os valores das ComboRows, antes cortados em janela pequena, agora aparecem como subtítulos. Isso não equivale a uma auditoria completa com leitor de tela, escala elevada ou todos os temas.

![Tela inicial após as correções](/home/bruno/.cache/bns-implementation-20260922/home-final-pt.png)

### Ganhos medidos e limites

Durante três atualizações da descoberta, a captura D-Bus registrou **zero releituras Get/GetAll de propriedades**; a GUI aplicou os valores recebidos nos sinais. Na região de instrução medida em tema claro, o contraste foi **12,56:1**, contra 3,17:1 na região problemática da revisão inicial. São medições dos percursos e regiões indicados, não certificações globais. [Tráfego](/home/bruno/.cache/bns-implementation-20260922/dbus-call-count.log), [contraste](/home/bruno/.cache/bns-implementation-20260922/contrast.json).

Em três amostras de cinco segundos, sem transmissão, a GUI ficou em aproximadamente **150,5 MiB PSS** e **0–0,4% de um núcleo**; o serviço, em **25,2 MiB PSS** e 0% no intervalo amostrado. A GUI anterior estava perto de 149 MiB: **não há redução significativa comprovada de memória em repouso**. Os limites de cache, filas e texturas reduzem o crescimento permitido, mas não medem o pico em uma biblioteca real. [Amostras](/home/bruno/.cache/bns-implementation-20260922/resources-idle.json).

Houve uma falha intermitente de preparação de dois arquivos temporários (`ENOENT`) na rodada `tests-closure.log`. Não se confirmou causa no código do produto. O diagnóstico do teste agora inclui o caminho; a repetição focada passou com 137 testes e a rodada completa posterior passou com 390, usando o diretório persistente de evidências como `TMPDIR`. A causa original permanece **UNKNOWN**, sem assertivas removidas. O primeiro teste de layout também falhou por o cliente de captura ultrapassar seu prazo; a execução com captura sincronizada passou. Tentativas GTK com backend X11 herdado foram corrigidas para o Wayland isolado e passaram. Esses resultados falhos foram preservados nos logs.

Continuam dependentes de validação externa: atraso ponta a ponta, CPU/GPU e memória durante transmissão prolongada; Ethernet + Wi-Fi com VPN e aparelhos reais; estabilidade de cada receptor; MSRV 1.93, build release e pacote instalado. Não foram criados perfis novos de uso com valores não medidos nem alteradas bibliotecas do sistema. O scanner GStreamer reportou falha no plugin LADSPA do ambiente, e o teste WebRTC reportou ausência de `rtpgccbwe`; os testes de vídeo passaram, mas esses avisos não são garantia de controle de congestionamento no ambiente.

Nenhuma ação do usuário é necessária para usar as alterações no checkout. Elas não foram publicadas, instaladas no sistema ou enviadas ao remoto.

## Revisão inicial — antes das alterações

Revisão do commit `28d75ac97c58dd4df2ad0a198047a00c931df8b1`. O programa tem uma base funcional e já incorpora várias otimizações importantes. As prioridades são corrigir estados que enganam a pessoa, completar a integração das configurações de áudio e tornar a descoberta cancelável e independente por protocolo. Reduzir buffers indiscriminadamente não é a primeira correção indicada.

**Escopo e evidência**

Foram examinados a arquitetura, os caminhos principais dos 10 crates, captura/identidade do portal, construção das pipelines, descoberta, controle de sessões, comunicação entre serviço e GUI, configurações, áudio virtual e páginas GTK. A revisão é ampla, com aprofundamento nos problemas abaixo; não representa inspeção linha a linha de todas as dependências ou certificação dos protocolos.

A GUI e o serviço foram compilados a partir deste checkout e executados em KWin isolado, com configuração própria e receptores fictícios do provedor de testes existente. O barramento de sistema e o servidor de áudio reais ficaram inacessíveis aos processos de teste. Não houve transmissão para aparelhos reais, alteração de firewall ou criação de placa virtual no ambiente pessoal. O encerramento do serviço foi simulado somente no processo criado pela revisão.

Interface examinada em português, Adwaita claro, desktop de 1280 × 720 e janela compacta de 800 × 650. Estados observados: descoberta desligada, lista preenchida, erro de provedor, escolha do conteúdo, configurações de imagem e áudio, mídia vazia, filtro de dispositivos e perda do serviço. Controles foram inspecionados por AT-SPI com profundidade até 35. Houve interação real por ponteiro, rolagem, Tab/espaço e Escape; o diálogo Sobre abriu por teclado e fechou com Escape. Isso não cobre um percurso completo com leitor de tela.

Artefatos locais: [diretório de evidências](/home/bruno/.cache/bns-review-20260922). Os arquivos estão no cache desta máquina, fora do código do produto; devem acompanhar o relatório se ele for compartilhado. Os hashes dos executáveis estão em [binaries.sha256](/home/bruno/.cache/bns-review-20260922/binaries.sha256). Os caminhos em `/proc/PID/exe` foram conferidos durante a execução. Todos os processos da sessão isolada foram encerrados.

**Problemas funcionais prioritários**

| Prioridade | Achado e consequência | Evidência e correção indicada |
| --- | --- | --- |
| P1 | **Erros de transmissão desaparecem quase imediatamente.** A pessoa pode ver a conexão falhar e logo receber novamente uma contagem de dispositivos, sem tempo de entender o motivo. | `CastFinished` publica `error`, mas o polling de 400 ms chama `refresh_status`, que o substitui. Foi observada a sequência real `connecting → error → found` ao simular indisponibilidade do áudio na sessão isolada. Preservar o erro até nova ação ou dispensa explícita; preservar também `stopping` enquanto a limpeza estiver em curso. [engine.rs:258](/note/BigNetScreen/crates/nd-service/src/engine.rs:258), [engine.rs:383](/note/BigNetScreen/crates/nd-service/src/engine.rs:383), [sinais capturados](/home/bruno/.cache/bns-review-20260922/error-signals.txt). |
| P1 | **O controle de áudio seletivo não aciona o ciclo de criação/remoção que o serviço implementa.** O texto promete adicionar a saída BigNetScreen, mas a alteração isolada desse controle não chama o serviço. | A GUI só solicita `reload_settings` quando protocolo ou descoberta mudam. A criação/remoção está em `ApplySettings`; a criação adicional no início do cast não executa a remoção. Notificar o serviço após a gravação efetivamente terminar e mostrar sucesso/falha real. [app.rs:541](/note/BigNetScreen/crates/nd-gui/src/app.rs:541), [engine.rs:301](/note/BigNetScreen/crates/nd-service/src/engine.rs:301), [cast.rs:49](/note/BigNetScreen/crates/nd-service/src/cast.rs:49). |
| P2 | **A janela mantém disponibilidade antiga quando o serviço sai.** Os botões continuam habilitados mesmo sem dono do nome D-Bus. | Após encerrar o serviço isolado, `GetNameOwner` confirmou ausência e a imagem da janela permaneceu idêntica pixel a pixel. O serviço normal pode sair após 180 s sem transmissão, inclusive com uma janela ociosa aberta. Observar perda/retorno do dono, invalidar disponibilidade e definir como manter descoberta enquanto a janela estiver aberta. Não foi testada a ativação automática instalada nem o prazo real de 180 s. [app.rs:895](/note/BigNetScreen/crates/nd-gui/src/app.rs:895), [bignetscreend.rs:14](/note/BigNetScreen/crates/nd-service/src/bin/bignetscreend.rs:14), [evidência](/home/bruno/.cache/bns-review-20260922/service-stopped.txt). |
| P2 | **Mudar uma preferência e iniciar rapidamente pode usar o valor anterior.** A memória da GUI muda antes do arquivo que o serviço lê. | A gravação é adiada 400 ms e despachada sem aguardar sua conclusão; `begin_cast` e `send_media` recarregam do disco. A janela de corrida é verificável no código, mas não foi medida por interação cronometrada. Aguardar a publicação da preferência antes de iniciar uma sessão, mantendo a escrita fora da thread GTK. [settings.rs:478](/note/BigNetScreen/crates/nd-gui/src/pages/settings.rs:478), [engine.rs:706](/note/BigNetScreen/crates/nd-service/src/engine.rs:706), [engine.rs:778](/note/BigNetScreen/crates/nd-service/src/engine.rs:778). |
| P2 | **Filtro e dispositivo selecionado discordam.** Com Miracast selecionado, a lista mostra apenas Miracast, mas o painel lateral continua oferecendo ações para o Chromecast anterior. | Reproduzido visualmente. `SetFilter` apenas refaz a lista. Limpar a seleção que deixa de pertencer ao filtro ou indicar expressamente sua permanência; a primeira opção é mais simples. Distinguir também total encontrado de total visível. [devices.rs:383](/note/BigNetScreen/crates/nd-gui/src/pages/devices.rs:383), [captura](/home/bruno/.cache/bns-review-20260922/devices-filter-mismatch.png). |
| P2 | **A medição de conexão perde a diferença entre “ainda não medido” e “não respondeu”.** Pode ficar em “Medindo…” quando deveria informar indisponibilidade. | O serviço usa `Option<Option<Duration>>`, mas publica ambos como zero. A GUI aceita apenas valores maiores que zero e nunca produz `Quality::Unreachable` nesse caminho. Uma resposta válida menor que 1 ms também vira zero. Preservar o estado da medição na interface do serviço. [engine.rs:503](/note/BigNetScreen/crates/nd-service/src/engine.rs:503), [app.rs:752](/note/BigNetScreen/crates/nd-gui/src/app.rs:752), [pages/mod.rs:263](/note/BigNetScreen/crates/nd-gui/src/pages/mod.rs:263). |

No áudio há duas correções complementares. O preflight verifica somente `virtual_audio`, mesmo que `system_audio` tenha sido desligado e a opção virtual tenha ficado marcada, porém insensível. Uma sessão sem áudio do sistema pode, portanto, depender desnecessariamente da criação da placa. Além disso, falhas de criação/remoção em `ApplySettings` ficam apenas no log. O controle deve refletir o estado real e explicar como direcionar um aplicativo para a saída criada. O comportamento de não recorrer silenciosamente ao áudio global quando o modo seletivo falha deve ser preservado.

**Detecção de dispositivos**

1. **Cancelar de fato a busca DLNA — P2, confirmado no código.** `DlnaProvider::discover` solta uma tarefa com `tokio::spawn` e não mantém seu cancelamento. A tarefa só encerra por falha de envio no ramo de um dispositivo novo; com rede vazia ou aparelhos já conhecidos, pode continuar buscando após o stream ser descartado. Cada atualização pode deixar outra busca ativa até o processo sair. Verificar fechamento do consumidor e ligar o tempo de vida da tarefa ao stream, inclusive durante espera e consulta de descrição. [ssdp.rs:157](/note/BigNetScreen/crates/nd-dlna/src/ssdp.rs:157), [ssdp.rs:216](/note/BigNetScreen/crates/nd-dlna/src/ssdp.rs:216).

2. **Publicar resultados sem esperar todos os provedores — P2, confirmado no código.** `MetaProvider` aguarda cada `discover()` antes de entregar o stream combinado. O primeiro `P2pDevice::open()` faz chamadas D-Bus e pode segurar resultados mDNS já disponíveis. Inicializar provedores de forma independente, com prazo e erro por provedor. Um erro no primeiro `open()` do Miracast também impede entrar no laço de recuperação existente; considerar retorno da conectividade/hardware sem exigir atualização manual. [meta.rs:43](/note/BigNetScreen/crates/nd-core/src/meta.rs:43), [nd-wfd/lib.rs:49](/note/BigNetScreen/crates/nd-wfd/src/lib.rs:49), [p2p.rs:282](/note/BigNetScreen/crates/nd-net/src/p2p.rs:282).

3. **Evitar que um DLNA lento atrase os demais — P2, confirmado no código.** Primeiro se coleta a janela SSDP inteira, de cerca de 5,4 s; depois se buscam descrições uma a uma, cada qual com prazo de 8 s. Aparelhos lentos anteriores na lista atrasam os seguintes. Entregar resultados progressivamente e buscar descrições com concorrência pequena e limitada. Preservar a identidade USN e os limites de tamanho já existentes. [ssdp.rs:92](/note/BigNetScreen/crates/nd-dlna/src/ssdp.rs:92), [ssdp.rs:168](/note/BigNetScreen/crates/nd-dlna/src/ssdp.rs:168), [upnp.rs:28](/note/BigNetScreen/crates/nd-dlna/src/upnp.rs:28).

4. **Cobrir máquinas com várias interfaces — P2, limitação de implementação, efeito dependente da rede.** O SSDP abre `0.0.0.0` e envia a um único destino multicast sem escolher interfaces. Isso não equivale a transmitir em todas as redes: a interface de saída segue a seleção de multicast/roteamento do sistema. Descobrir nas interfaces locais elegíveis, deduplicar por USN e reavaliar quando a rede muda. Validar em Ethernet + Wi-Fi e com VPN. O controle pertinente é documentado em [Linux ip(7)](https://man7.org/linux/man-pages/man7/ip.7.html). [ssdp.rs:96](/note/BigNetScreen/crates/nd-dlna/src/ssdp.rs:96).

5. **Limitar eventos e registros retidos — P2, prevenção de consumo excessivo.** mDNS e Miracast usam canais `unbounded`; a agregação posterior ter canal limitado não limita os anteriores. Preferir filas limitadas e consolidar atualizações repetidas por identidade, preservando remoções e erros. Definir limites também para registros anunciados. Não foi gerado tráfego de estresse na rede. [nd-chromecast/lib.rs:126](/note/BigNetScreen/crates/nd-chromecast/src/lib.rs:126), [nd-wfd/lib.rs:54](/note/BigNetScreen/crates/nd-wfd/src/lib.rs:54).

6. **Representar recuperação DLNA.** O provedor publica indisponibilidade em falhas de busca, mas não publica novo `ProviderReady` ao recuperar, salvo na inicialização. O aviso pode sobreviver ao retorno dos aparelhos. Manter prontidão por provedor e limpar o aviso apenas quando houver recuperação comprovada. [ssdp.rs:157](/note/BigNetScreen/crates/nd-dlna/src/ssdp.rs:157).

Já estão presentes e devem ser preservados: identidade USN no DLNA, deduplicação, tolerância a duas varreduras perdidas, renovação da busca Wi-Fi Direct, tratamento de falhas após a abertura inicial do P2P, preservação do receptor ativo e encerramento explícito do daemon mDNS. Não é necessário reconstruir a descoberta inteira.

**Lag, CPU e memória**

O código já possui filas curtas, encoder sem B-frames nas rotas pertinentes, bitrate adaptativo no Cast, pacing, recuperação por keyframe, appsinks limitados, GPU em partes da conversão, perfis de atraso e instrumentação opcional de FPS/latência. Uma revisão baseada apenas na arquitetura antiga concluiria incorretamente que não existe adaptação de bitrate. [rate.rs](/note/BigNetScreen/crates/nd-chromecast/src/rate.rs), [pipeline.rs:727](/note/BigNetScreen/crates/nd-core/src/pipeline.rs:727).

| Oportunidade | O que foi verificado | Próximo passo e prova necessária |
| --- | --- | --- |
| Reduzir trabalho entre serviço e GUI | Cada sinal de propriedade faz a GUI reler seis propriedades em sequência. Cada getter clona o snapshot inteiro. Uma mudança de várias propriedades gera várias releituras; mídia inclui a fila de arquivos. O serviço já evita publicar snapshots idênticos. | Consumir as propriedades efetivamente alteradas ou uma atualização coerente agregada; evitar clonar campos não solicitados. Medir mensagens D-Bus, alocações e CPU durante mídia e atualização de receptores. [dbus.rs:122](/note/BigNetScreen/crates/nd-service/src/dbus.rs:122), [app.rs:954](/note/BigNetScreen/crates/nd-gui/src/app.rs:954). |
| Evitar trabalho GTK sem mudança | Qualquer snapshot dispara mensagens para várias páginas; `HomeMsg::Searching` constrói outro placeholder mesmo com o mesmo valor. As linhas dos dispositivos já são sincronizadas sem reconstrução completa. | Comparar os campos antes de emitir e substituir widgets somente quando o estado mudar. Manter a sincronização incremental existente. [app.rs:654](/note/BigNetScreen/crates/nd-gui/src/app.rs:654), [home.rs:780](/note/BigNetScreen/crates/nd-gui/src/pages/home.rs:780). |
| Reduzir retenção das miniaturas | Cache global de até 96 previews, tamanho máximo usual 360 × 360; imagens RGBA quadradas nesse limite representam cerca de 47,5 MiB só de pixels. O valor é estimativa do armazenamento, não medição de vazamento. Referências nas páginas/texturas podem continuar vivas após remoção do cache. Já há limite de duas decodificações simultâneas e grade de 60 itens. | Medir pasta vazia, 20 e 60 itens e mudanças sucessivas de categoria. Definir orçamento por bytes e descartar resultados sem consumidor; reduzir miniaturas da fila ao tamanho necessário. [preview.rs:18](/note/BigNetScreen/crates/nd-gui/src/pages/preview.rs:18), [preview.rs:106](/note/BigNetScreen/crates/nd-gui/src/pages/preview.rs:106). |
| Reagir antes que o atraso cresça | O controlador Cast reduz bitrate por retransmissão/quadro descartado. O limite de 120 quadros sem ACK protege os IDs do protocolo; não é um orçamento de atraso percebido. | Medir idade de quadros, atraso dos ACKs e tempo no sender; avaliar sinal temporal para adaptação antes de esgotar a janela. Não trocar o limite por um número menor sem testar wrap e recuperação. [flow.rs:7](/note/BigNetScreen/crates/nd-chromecast/src/flow.rs:7), [rate.rs:90](/note/BigNetScreen/crates/nd-chromecast/src/rate.rs:90). |
| Reduzir custo da conversão de vídeo | VA usa `vapostproc`; outros caminhos ainda podem converter/escalar em CPU. Há caminho GPU opt-in, acompanhado de relato histórico de interop mais lento. | Comparar por GPU/driver e formato realmente negociado; medir CPU, memória de vídeo, cópias e FPS. Não ativar DMA-BUF/GL globalmente só por parecer mais rápido. [pipeline.rs:343](/note/BigNetScreen/crates/nd-core/src/pipeline.rs:343), [pipeline.rs:2038](/note/BigNetScreen/crates/nd-core/src/pipeline.rs:2038). |
| Reduzir custo de tela parada e inicialização | `videorate` regulariza a captura; GUI e serviço fazem preflight de encoder. São candidatos a perfil, não desperdícios quantificados nesta revisão. | Comparar tela parada/movimento e inicialização fria/quente. Preservar timestamps, áudio e heartbeat. A remoção de `videorate` exige provar o mapeamento de tempo; não é uma correção de uma linha segura. [pipeline.rs:1996](/note/BigNetScreen/crates/nd-core/src/pipeline.rs:1996), [main.rs:65](/note/BigNetScreen/crates/nd-gui/src/main.rs:65). |

Filas têm compromissos reais: o GStreamer bloqueia ao atingir seus limites, salvo quando configurado para descarte. Limites de tempo, bytes e buffers e sua ocupação devem ser observados conjuntamente. Consultados os documentos primários de [queue](https://gstreamer.freedesktop.org/documentation/coreelements/queue.html) e [appsink](https://gstreamer.freedesktop.org/documentation/app/appsink.html). Não recomendo introduzir descarte indiscriminado de quadros H.264 dependentes nem aplicar as mesmas opções a Miracast, Cast RTP e HTTP/DLNA.

**Medição de repouso realizada**

Três janelas consecutivas de 5 s, sem percorrer a árvore AT-SPI durante a coleta, sem captura/transmissão e sem biblioteca de mídia carregada. Dois receptores fictícios; descoberta automática desligada. Build `dev` sem debug info, renderizador padrão, na mesma sessão/máquina. CPU: i5-13400, 16 CPUs lógicas; host compartilhado com outras tarefas. O valor serve como referência desta execução, não orçamento de release.

| Processo | PSS observado | CPU de um núcleo, por janela | Threads |
| --- | --- | --- | --- |
| GUI | 149,1–149,3 MiB | 0,0%; 0,2%; 0,2% | 12 |
| Serviço | 26,2 MiB | 0,0%; 0,0%; 0,0% | 17 |

Total aproximado de 175,5 MiB de PSS. “0,0%” é o resultado na resolução do contador/amostragem, não ausência absoluta de trabalho. RSS não foi usado como consumo exclusivo, pois contabiliza bibliotecas compartilhadas. Não há evidência aqui de crescimento contínuo em repouso nem de economia alcançada: o código não foi otimizado durante a revisão. [Amostras](/home/bruno/.cache/bns-review-20260922/resources-idle.json).

Para lag real, falta medir captura → codificação → envio → imagem na TV, incluindo p50/p95, perda, FPS e sincronismo de áudio. Handshake TCP rápido não mede atraso da imagem. A UI desenha barras e pode escrever “sinal fraco” a partir desse handshake; recomendar “tempo de resposta do dispositivo” e usar métricas do fluxo para avaliar a transmissão. Também substituir “resposta imediata” do Miracast por uma expectativa menos absoluta. [probe.rs:54](/note/BigNetScreen/crates/nd-net/src/probe.rs:54), [pages/mod.rs:222](/note/BigNetScreen/crates/nd-gui/src/pages/mod.rs:222).

**Avaliação visual e facilidade de uso**

O caminho “escolher aparelho → escolher o que compartilhar” é compreensível e merece ser mantido. Os alvos são grandes, os nomes essenciais aparecem no AT-SPI e o português cobre as páginas principais. O principal problema é a distribuição de espaço e informação, somada a estados pouco confiáveis.

![Tela inicial em português, sem busca ativa](/home/bruno/.cache/bns-review-20260922/home-empty-pt.png)

- **A ajuda compete com a tarefa.** A coluna Dicas repete a instrução que já está no estado vazio e continua ocupando grande área depois de escolher o receptor. O compartilhamento por navegador começa abaixo da primeira tela de 720 px. Recolher a ajuda em “Não encontrei minha TV” e colocar os caminhos TV/navegador em posição visível.
- **“Pronto para compartilhar” e “Nenhum receptor encontrado” não explicam que a busca está desligada.** Essa informação aparece apenas pequena no cabeçalho. Dar ao estado vazio uma causa local e uma ação: “A busca está desligada” / “Procurar aparelhos”. Diferenciar busca em curso, nenhum resultado e falha de um protocolo.
- **A composição compacta exige muita rolagem.** Em 800 × 650, a barra lateral conserva 250 px; apenas uma opção inteira e parte da segunda cabem na área inicial. Usar navegação recolhível e opções mais compactas nessa largura/altura. Os quatro tipos de compartilhamento devem continuar reconhecíveis. O suporte nativo está em [layouts adaptativos do libadwaita](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/main/adaptive-layouts.html). [Captura compacta](/home/bruno/.cache/bns-review-20260922/share-compact-800x650.png), [app.rs:158](/note/BigNetScreen/crates/nd-gui/src/app.rs:158), [style.css:77](/note/BigNetScreen/crates/nd-gui/src/style.css:77).
- **Contraste insuficiente em texto útil.** No bloco de dicas da captura inicial, texto `RGB(136,137,142)` sobre fundo predominante `RGB(242,244,247)` resultou em aproximadamente **3,17:1**. Foi medida a tinta mais escura recorrente numa região só de texto, não a borda suavizada dos caracteres. Para texto normal, a referência AA é [4,5:1](https://www.w3.org/WAI/WCAG21/Understanding/contrast-minimum). Aumentar contraste das instruções essenciais; não aplicar opacidade de conteúdo secundário ao que ensina a tarefa. Esse resultado vale para a região e o tema medidos, não para todos os textos/temas. [Medição](/home/bruno/.cache/bns-review-20260922/contrast.json).
- **Os erros falam a língua da implementação.** A indisponibilidade induzida do barramento mostrou `unsupported`, `system bus`, `os error 2` e `native build` em um banner em português. Traduzir categorias conhecidas em causa e ação, mantendo a mensagem técnica em detalhes copiáveis. “Entendi” somente esconde o banner e não ajuda a recuperar. [Captura](/home/bruno/.cache/bns-review-20260922/home-receivers-error-pt.png), [app.rs:1061](/note/BigNetScreen/crates/nd-gui/src/app.rs:1061).
- **As configurações começam pelo que um iniciante não sabe escolher.** Protocolo, resolução, FPS e buffers aparecem antes de intenções simples. Propor “Uso do computador”, “Vídeos e filmes” e “Economizar recursos”, com explicação de uma linha, mantendo os valores exatos em Avançado. São propostas de UX; os valores e efeitos de cada perfil precisam de medição. [Configurações](/home/bruno/.cache/bns-review-20260922/settings-pt.png).
- **Áudio seletivo precisa orientar uma ação concreta.** “Apenas um aplicativo escolhido” não oferece um seletor de aplicativo: exige roteamento externo. Preferir “Som enviado à saída BigNetScreen”, indicar se a saída já existe e disponibilizar “Abrir configurações de som”. Explicar que a opção não faz monitoramento local e que o microfone, se habilitado, é acrescentado separadamente. [Áudio](/home/bruno/.cache/bns-review-20260922/audio-pt.png).
- **As ações variam entre páginas sem explicar por quê.** A página Dispositivos mostra “Enviar mídia” só para Chromecast, embora o serviço ofereça arquivos também para Miracast. Usar a mesma política de capacidade que já existe no serviço. O filtro “Chromecast / AirPlay” também inclui DLNA no código sem nomeá-lo. AirPlay é apenas descoberta; isso deve ser claro antes de parecer um destino utilizável. [devices.rs:34](/note/BigNetScreen/crates/nd-gui/src/pages/devices.rs:34), [devices.rs:243](/note/BigNetScreen/crates/nd-gui/src/pages/devices.rs:243), [engine.rs:824](/note/BigNetScreen/crates/nd-service/src/engine.rs:824).
- **Na mídia vazia, a orientação fica distante das ações.** A mensagem aparece no fim de uma grande região vazia. Aproximar a instrução de “Selecionar arquivos” e usar “Adicionar pasta” quando a ação ainda não inicia envio. Não alterar o estado seguro do botão de envio, que aparece desabilitado sem seleção. [Mídia compacta](/home/bruno/.cache/bns-review-20260922/media-empty-800x650.png).

![Filtro Miracast com painel do Chromecast ainda selecionado](/home/bruno/.cache/bns-review-20260922/devices-filter-mismatch.png)

**Fluxo recomendado para pessoas sem conhecimento técnico**

1. Tela inicial: **“Onde você quer mostrar?”**, lista de aparelhos pelo nome e alternativa **“Em outro computador ou celular”**. Endereço IP e protocolo ficam em detalhes, salvo quando necessários para distinguir aparelhos iguais.
2. Após a escolha: **“O que você quer mostrar?”** com Tela inteira, Uma janela, Tela extra e Arquivos. Explicar Tela extra como “Mova para ela o que deseja mostrar”. Mostrar apenas opções realmente disponíveis, com motivo curto para limitações relevantes.
3. Antes de iniciar: escolhas de áudio compreensíveis — Sem som, Som do computador, Som enviado à saída BigNetScreen — e Microfone separado. Mostrar o destino e o conteúdo escolhidos de forma persistente.
4. Durante a transmissão: **“Compartilhando com Sala”**, botão **“Parar de compartilhar”**, estado de áudio e aviso acionável se a transmissão piorar. Detalhes técnicos acessíveis sem dominar a tela.
5. Na falha: manter causa e recuperação visíveis. Exemplo: “Não foi possível criar a saída de som BigNetScreen” com “Tentar novamente” e “Ver detalhes”. Nunca substituir silenciosamente áudio seletivo por áudio global.

**Qualidade do código e cobertura que falta**

Priorizar contratos entre componentes. Os testes atuais cobrem diversas invariantes internas, mas não impediram erros entre GUI, arquivo de preferências, serviço e dispositivo. Os próximos testes devem verificar: erro visível após vários ticks; estado `stopping` preservado até cleanup; preferências efetivamente aplicadas antes do cast; criação/remoção da placa acionada pela GUI; cancelamento DLNA com zero resultados; um provedor lento sem bloquear outro; perda e retorno do dono D-Bus; filtro e seleção coerentes. São comportamentos distintos, não testes extras de getters ou de constantes.

Manter política de capacidades num dono compartilhado e evitar repetir listas de protocolos na GUI. Preferir estados internos tipados a comparações dispersas de strings, preservando o contrato externo do D-Bus. Essa melhoria deve acompanhar as correções reais; não justifica reescrever o programa ou introduzir um novo framework.

Atualizar [ARCHITECTURE.md](/note/BigNetScreen/ARCHITECTURE.md): descreve oito crates, GUI proprietária da sessão e limitações de congestionamento que já não representam toda a implementação. Há dez membros, serviço separado, DLNA e controle de bitrate no Cast. Também há comentários locais desatualizados sobre quais protocolos enviam arquivos.

Limites de segurança documentados continuam relevantes: identidade Cast não plenamente autenticada e front doors HTTP sem confidencialidade. Não foi feita certificação de segurança nem consulta a uma base atual de avisos de dependências. Recomendações de descoberta não devem enfraquecer autorização do portal, escopo do firewall, validação de entrada, tokens ou limites de memória.

**Validação executada**

Rust/cargo 1.98.1, rustfmt 1.9.0, clippy 0.1.98. Bibliotecas nativas: GTK 4.22.4, libadwaita 1.9.3, GStreamer 1.28.6. Um alvo de build e um lock; jobs=1, incremental=0, debug info dev/test=0; prazos de 240 s nas etapas pesadas. `TMPDIR` executável no cache para os testes. Os comandos pesados tiveram PID/grupo controlado e terminaram com exit 0.

| Verificação | Resultado |
| --- | --- |
| `cargo build --offline --locked -p nd-gui -p nd-service` | Exit 0; GUI e serviço usados na inspeção. [Log](/home/bruno/.cache/bns-review-20260922/build.log). |
| `cargo fmt --all -- --check` | Exit 0. |
| `cargo test --offline --locked --workspace` | Exit 0; **383 aprovados, 0 falhas, 11 ignorados**, em 22 resultados de suites, incluindo a etapa de doctests. [Log](/home/bruno/.cache/bns-review-20260922/tests.log). |
| `cargo clippy --offline --locked --workspace --all-targets -- -D warnings` | Exit 0. [Log](/home/bruno/.cache/bns-review-20260922/clippy.log). |
| `python3 -S scripts/check-project.py` | Exit 0; 239 caminhos-fonte e 20 documentos mantidos, zero erros. Isso não valida a atualidade semântica da arquitetura. [Log](/home/bruno/.cache/bns-review-20260922/project-check.log). |
| `msgfmt po/pt_BR.po` para catálogo isolado | Exit 0; catálogo do checkout usado na inspeção. |
| GTK manual e AT-SPI | Executados nos estados e tamanhos descritos; [controles observados](/home/bruno/.cache/bns-review-20260922/a11y-selected-controls.txt). |
| `git diff --check` | Executado antes da entrega. |

Os 11 ignorados: placa virtual real; quatro testes GTK; capa musical com sandbox de imagem; quatro testes/benchmarks NDI; teste de firewall. A inspeção manual não transforma esses testes ignorados em aprovados.

Houve ruído de ambiente na primeira inicialização: o scanner GStreamer relatou problema no plugin LADSPA do sistema, mas a aplicação abriu; libadwaita avisou sobre uma preferência GTK de tema herdada. O primeiro lançamento também herdou `LC_ALL=C.UTF-8`; foi corrigido no ambiente isolado antes das capturas em português. Esses fatos não foram classificados como defeitos de tradução do produto.

Não executados: build release de desempenho, MSRV 1.93, instalação/ativação pelo sistema, validação de pacote, todos os cenários de temas/escala/leitor de tela, transmissão real, testes de 30 minutos e reconexões físicas, perda de rede controlada, stress de descoberta, nem perfis de CPU/GPU durante captura. O serviço da revisão foi iniciado manualmente; isso não prova a ativação D-Bus do pacote instalado.

**Ordem de execução recomendada**

1. Corrigir permanência de erros/parada, áudio seletivo e sincronização de preferências; provar o caminho GUI → serviço → efeito.
2. Corrigir cancelamento e inicialização da descoberta, recuperação do serviço e coerência de filtro/seleção.
3. Simplificar a composição GTK, elevar contraste e tornar erros/áudio compreensíveis.
4. Reduzir releituras/clones e medir miniaturas; depois perfilar captura, conversão e sender em aparelhos reais.

Entregue: diagnóstico, prioridades e evidências, sem alteração funcional do programa. Nenhuma decisão do usuário é necessária para concluir esta revisão. Ganhos de lag/CPU em transmissão permanecem dependentes das medições de hardware descritas acima.
