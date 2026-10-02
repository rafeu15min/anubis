# Player: pesquisa e plano

Pesquisa feita em 2026-10-02 para o item 1 do ROADMAP. Compara os players de referência,
descreve o que o `web/player.html` faz hoje e propõe o que entra, em fases.

## O que cada player faz bem

| Player | Pontos fortes (o que vale copiar) |
|---|---|
| **Netflix** (celular e web) | Toque mostra/esconde controles. Toque duplo nas laterais: −10 s / +10 s. Play no centro entre −10 e +10. Linha de baixo com botões com nome: *Velocidade*, *Bloquear*, *Episódios*, *Áudio e legendas*, *Próximo episódio*. Bloqueio de tela contra toques acidentais. Atalhos: espaço/K, ←/→ 10 s, ↑/↓ volume, M, F, S (pular abertura, só com o aviso na tela), Shift+←/→ episódio anterior/próximo, 0–9 pula para 0–90 %. |
| **YouTube** | Toque duplo nas laterais com toques que se somam (−10, −20, −30…) e o total mostrado no lado tocado. Segurar o dedo na tela = 2× enquanto segura. Miniatura do quadro ao arrastar a barra; deslizar para cima na barra abre a tira de quadros (busca precisa). Atalhos: J/K/L, `<`/`>` velocidade, `,`/`.` quadro a quadro (pausado), C legenda, `+`/`−` tamanho da legenda, 0–9 porcentagem, Ctrl+←/→ capítulo. |
| **Infuse** | Poucos controles na tela, visual limpo. Botões de áudio e legenda direto na sobreposição. Menu de engrenagem com abas: legendas (atraso, tamanho), áudio, velocidade, aumento de volume. Toque duplo à direita +10 s, triplo +20 s. Arrastar na tela avança/volta. |
| **MX Player / VLC** (Android) | Gestos completos: arrastar na horizontal busca, na vertical à esquerda brilho e à direita volume. Pinça para zoom (preencher a tela). Bloqueio. Ajustes de tela e legenda sem sair do vídeo. Intervalo do toque duplo configurável. |
| **Plex** | Qualidade escolhida no player e **ajuste automático** pela velocidade da conexão (começa numa qualidade e muda durante a reprodução). Posição e atraso da legenda em passos de 50/100 ms. |
| **Jellyfin web** | Velocidade de 0,5× a 2× em passos de 0,25. V troca faixa de áudio, C troca legenda. Atraso da legenda no player. Miniaturas (*trickplay*) na barra se a biblioteca as gerar. Botão de pular abertura/créditos vindo dos *media segments* (plugin Intro Skipper, que acha a abertura comparando o áudio entre episódios com chromaprint). |
| **mpv + uosc** | A barra mostra capítulos como trechos. Menus de faixa (vídeo/áudio/legenda) e de qualidade com busca ao digitar. Roda do mouse muda conforme o que está embaixo (barra = busca, volume = volume, velocidade = velocidade). Barra fina sempre visível opcional. |

**Padrão comum a quase todos:**
- toque simples nunca pausa: ele só mostra ou esconde os controles, que somem sozinhos 3 s depois;
- toque duplo nas laterais avança ou volta 10 s, e toques seguidos se somam;
- o toque simples espera ~280 ms antes de agir, para não disparar junto com o duplo;
- áudio, legenda, velocidade e qualidade ficam num menu só (ou em botões com nome no celular);
- a barra mostra o que já carregou e o que já foi assistido, e mostra o tempo (ou uma miniatura) ao arrastar.

## O player hoje (`web/player.html`, 420 linhas)

**Tem:**
- barra com o que está no disco (branco) e o que foi assistido (vermelho), com tempo ao passar o mouse;
- −10 s / +30 s (botões e setas), e toques seguidos se somam;
- volume, mudo, tela cheia, imagem na imagem;
- menu de legendas (Bazarr, torrent e embutidas);
- posição salva neste aparelho;
- botão de próximo episódio;
- aviso de download lento com versão mais leve, e oferta de versão melhor (Bluray);
- correção da dublagem fora de sincronia;
- atalhos espaço/K, J/L, ←/→, ↑/↓, F, M, C.

**Causas dos defeitos relatados:**
- **Toque pausa:** `v.addEventListener('click', toggle)`. Todo toque no vídeo vira play/pausa. No celular, o toque deveria só alternar a visibilidade dos controles.
- **Áudio fora de sincronia ao voltar para a aba:** o Chrome no Android desliga o vídeo de abas em segundo plano para economizar bateria e o religa ao voltar. Há relatos de perda de sincronia na volta (Chromium issue 41337888). O player não trata `visibilitychange`. **Hipótese a confirmar no celular:** ao voltar, reposicionar o vídeo no ponto atual. No vídeo direto, isso é `currentTime = currentTime`. No convertido, é reabrir o stream em `now()`, como já faz a correção da dublagem.

**Servidor já pronto (6b5765a):**
- `stream.mp4?a=N&h=ALTURA` escolhe a faixa de áudio e a qualidade;
- `/info` manda `media.height`, `media.audios` e `transcode_height`.

## Limites do navegador

- **Brilho do aparelho:** a página não consegue mudar. Dá para imitar escurecendo o vídeo (filtro CSS), mas não clarear além do brilho do aparelho.
- **Volume no iPhone:** `video.volume` é só leitura no iOS. Lá o gesto de volume não funciona; no Android funciona.
- **Qualidade automática:** o vídeo convertido é um MP4 contínuo, não HLS/DASH. Não há troca de qualidade sem corte. O jeito é o do Plex: medir se o stream chega mais rápido do que toca e, se não chegar, reabrir na posição atual com `h` menor. Haverá um carregamento curto, igual ao de uma busca.
- **Vídeo direto (arquivo já compatível):** escolher qualidade menor ou outra faixa de áudio obriga a converter, ou seja, a passar para `stream.mp4`.

## Proposta, em fases

**Fase A — defeitos e base (pequena, resolve o que incomoda):** feita em 2026-10-02 (b2d4155).
1. Toque simples mostra ou esconde os controles. No PC, o clique continua pausando, como no YouTube e na Netflix web. No celular, pausa só pelo botão.
2. Botão e seta de avançar passam de 30 s para 10 s. Toque duplo: nas laterais −10/+10 s, com o total somado aparecendo no lado tocado; no centro, tela cheia.
3. Ressincronizar ao voltar para a aba (`visibilitychange`).
4. Barra com três cores: branco = baixado, amarelo = carregado no player (`video.buffered`, somado ao `offset` no vídeo convertido), roxo = assistido (bolinha também roxa).

**Fase B — menu de configurações (usa o servidor já pronto):** feita em 2026-10-02. A qualidade automática só desce (720p → 480p → 360p) e não volta a subir no mesmo vídeo; tamanho e posição da legenda valem para todos os vídeos, o atraso só para o vídeo em que foi ajustado.
5. Botão de engrenagem com seções:
   - Qualidade: Automática (720p), Original e alturas menores que a do arquivo;
   - Áudio: faixas com idioma, título e canais;
   - Legendas: as que existem hoje, mais tamanho, posição e atraso em passos de 0,1 s;
   - Velocidade: 0,5× a 2×.
6. Lembrar escolhas no aparelho: qualidade, idioma do áudio, tamanho da legenda e velocidade.
7. Qualidade automática pela velocidade do stream (ver limites acima).

**Fase C — gestos e atalhos:**
8. Segurar o dedo = 2× enquanto segura.
9. Arrastar na horizontal busca (mostrando o tempo de destino). Arrastar na vertical à direita muda o volume (só Android; brilho e iPhone ficam para o app).
10. Bloqueio de tela.
11. Pinça para alternar entre preencher a tela e mostrar o quadro inteiro (`object-fit`).
12. Atalhos que faltam: `<`/`>` velocidade, 0–9 porcentagem, `,`/`.` quadro a quadro, Shift+N próximo episódio, V faixa de áudio, `+`/`−` tamanho da legenda, S pular abertura.

**Fase D — maiores (cada uma vale uma conversa antes):**
13. Pular abertura/créditos e próximo episódio com contagem. A fonte mais simples são os capítulos do arquivo e os *media segments* do Jellyfin, quando o item já está na biblioteca. Detectar sozinho exigiria comparar o áudio entre episódios.
14. Miniaturas ao arrastar a barra. Para itens já na biblioteca, dá para usar o trickplay do Jellyfin. Durante o download, seria preciso gerar quadros no FeuVault.
15. Posição e preferências entre aparelhos. Hoje ficam no `localStorage`. Seria preciso guardar no Anubis ou usar o progresso do Jellyfin.
16. Chromecast/AirPlay.

## Decisões (2026-10-02)

- Avançar e voltar: 10 s (botões, setas e toque duplo).
- Clique no vídeo pausa só no PC (mouse). No celular, o toque só mostra/esconde os controles.
- Gesto vertical só para volume. Nada de brilho imitado escurecendo o vídeo.
- O que o navegador não permite fica para o app (abaixo).

## Para o app (o navegador não permite)

- Brilho da tela por gesto (arrastar na vertical à esquerda).
- Volume por gesto no iPhone (`video.volume` é só leitura no iOS).
- Volume do sistema, em vez do volume do vídeo.

## Fontes

- Netflix: [controles redesenhados](https://www.androidpolice.com/2018/05/29/netflix-finally-redesigns-player-ui-larger-controls-next-episode-button/), [bloqueio de tela](https://www.phonearena.com/news/Netflix-Android-update-screen-lock-button_id123980), [atalhos](https://help.netflix.com/en/node/24855)
- YouTube: [atalhos](https://support.google.com/youtube/answer/7631406?hl=en), [2× ao segurar](https://x.com/TeamYouTube/status/1714341158212526253), [busca precisa](https://www.androidpolice.com/overlooked-youtube-trick-completely-changed-how-i-watch-videos/)
- Infuse: [controles de reprodução](https://support.firecore.com/hc/en-us/articles/215090987-Playback-Controls), [atraso da legenda](https://x.com/infuse/status/1404872292597911559)
- MX Player / VLC: [gestos MX](https://www.sites.google.com/site/mxvpen/features), [gestos VLC](https://www.vlchelp.com/volume-brightness-seek-gestures-android/)
- Plex: [qualidade automática](https://support.plex.tv/articles/115007570148-automatically-adjust-quality-when-streaming/), [ajustes do web app](https://support.plex.tv/articles/200289408-plex-web-app-settings/)
- Jellyfin: [10.10 e 10.11](https://jellywatch.app/blog/jellyfin-10-10-10-11-upgrade-guide-new-features-2026), [Intro Skipper](https://jellywatch.app/blog/jellyfin-intro-skipper-setup-configuration-troubleshooting-2026), [atraso da legenda](https://jellywatch.app/blog/fix-jellyfin-subtitles-out-of-sync-offset-resync-bazarr-2026)
- mpv/uosc: [README](https://git.uninsane.org/colin/uosc/raw/commit/128c76c3ba1653c9fb78aa9c671447807d4b9aa5/README.md)
- Gestos de toque: [Jularr PR #509](https://github.com/Juloc/Jularr/pull/509), [chewie PR #964](https://github.com/fluttercommunity/chewie/pull/964)
- Chrome em segundo plano: [mudanças do Chrome 61](https://developer.chrome.com/blog/media-updates-in-chrome-61), [Chromium issue 41337888](https://issues.chromium.org/issues/41337888)
