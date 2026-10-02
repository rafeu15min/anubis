# Fila do Anubis

## 1. Player próprio aceitável como player principal (pedido em 2026-10-01)

O player (`web/player.html`) é a única parte do Anubis que ainda incomoda. Objetivo: dar para
usá-lo no lugar do Jellyfin no dia a dia, principalmente no celular.

**Antes de codar:** pesquisar a fundo os melhores players do mercado (Netflix, YouTube, Disney+,
Prime Video, Plex, Jellyfin, Infuse, VLC, mpv, MX Player…) e anotar o que cada um faz bem em
gestos, controles e aparência. Escolher o que entra com base nisso.

**Defeitos relatados:**
- Tocar na tela pausa o vídeo. O toque deveria só mostrar/esconder os controles (como nos
  players de celular); pausar só pelo botão ou com toque duplo no centro.
- Ao sair do app/aba e voltar, o áudio fica fora de sincronia com o vídeo.

**Aparência:**
- Barra de progresso amarela (hoje vermelha).

**Funções que faltam (o Jellyfin já tem várias):**
- Escolha de qualidade/resolução do vídeo (e automática pela velocidade).
- Escolha de faixa de áudio.
- Legendas: tamanho, posição, atraso.
- Velocidade de reprodução.
- Pular abertura/créditos, próximo episódio com contagem.
- Gestos no celular: toque duplo nas laterais para voltar/avançar, arrastar para brilho/volume.
- Prévia (miniatura) ao arrastar a barra.
- Lembrar posição, volume e preferências entre sessões e aparelhos.
- Atalhos de teclado completos no PC; Chromecast/AirPlay se viável.

Já feito em 2026-10-01: tempo atualiza na hora do toque e toques seguidos em voltar/avançar
se somam (bf144ae).

## Depois
- Renomear o site/apps (Anubis = só a API).
- Sistema de login que cria usuários no Jellyfin.
- bitmagnet (parado).
