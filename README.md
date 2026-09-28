# Anubis

Orquestrador da stack de mídia do FeuVault (Sonarr, Radarr, Prowlarr,
qBittorrent e Jellyfin). Escrito em Rust (Tokio + Axum), roda como serviço
systemd no host.

## O que faz hoje

**Gerência de armazenamento.** Quando o RAID0 passa de 85% de uso, apaga os
vídeos mais antigos (pela data em que entraram na biblioteca) até voltar a 75%.
Pula o que chegou há menos de 7 dias e as séries ou filmes que alguém está
assistindo ou deixou pela metade no Jellyfin.

Download e biblioteca são hardlinks do mesmo dado: apagar só o vídeo da
biblioteca não libera espaço enquanto o torrent continua semeando. Por isso o
Anubis identifica o torrent pelo inode, apaga o vídeo pelo Sonarr/Radarr
(que também desmarca o item, para não ser baixado de novo), remove o torrent
do qBittorrent junto com os dados e avisa o Jellyfin do arquivo apagado
(o Sonarr/Radarr não avisam quando a exclusão vem pela API deles). Só conta
como espaço liberado o que perde todos os nomes no disco; um dado com algum
nome fora do Sonarr/Radarr/qBittorrent é pulado.

O Jellyfin aplica o aviso depois do próprio `LibraryMonitorDelay` (60 s por
padrão).

A checagem roda a cada 30 minutos e também na hora em que um download termina
(o qBittorrent chama `POST /hooks/torrent-finished`). Com `dry_run = true` ele
só registra no log o que apagaria.

**Busca unificada.** Página em `/` que pesquisa filmes (Radarr/TMDB) e séries
(Sonarr/TVDB) ao mesmo tempo. "Pedir" adiciona o item ao Radarr/Sonarr com o
perfil de qualidade configurado (séries com o gênero Anime vão para a pasta de
animes) e manda buscar. A página acompanha o pedido — procurando, baixando
(progresso lido direto do qBittorrent), entrando na biblioteca, disponível — e
abre o item no Jellyfin quando ele fica pronto.

**Assistir agora (padrão para filmes e episódios).** "Assistir" escolhe a versão entre as
que o Radarr aprova (preferindo H.264/AV1, que o navegador toca direto), baixa
com o motor próprio do Anubis (librqbit) e abre o player da página enquanto
baixa. Sem ninguém assistindo, o download vai em ordem; com o player aberto, as
peças da posição dele vêm primeiro — pular para o meio do filme reprioriza o
download. A barra do player mostra em branco o que já está no disco e em
vermelho o que já foi assistido. Ao terminar, o filme entra no Radarr por
hardlink e, quando ninguém assiste há 5 minutos, o torrent passa ao qBittorrent
para semear. Se nenhuma versão passa no perfil de qualidade, a página lista as
recusadas com o motivo e deixa escolher uma mesmo assim.

Séries: "Episódios" lista as temporadas; cada episódio tem "Assistir" (usa a
versão do episódio ou, sem ela, o pacote da temporada baixando só aquele
arquivo) e o player oferece o próximo. O que o navegador não toca (H.265,
áudio AC3/DTS, MKV) é convertido na hora pelo ffmpeg do container do Jellyfin
(`docker exec`, com um vigia que o encerra quando o player fecha). Pedidos em
andamento sobrevivem a reinícios (`/var/lib/anubis`).

Legendas: o botão CC do player lista as legendas ao lado do vídeo (as que o
Bazarr baixa em PT-BR, as que vêm no torrent) e, com o arquivo completo, as
embutidas no MKV; tudo vira WebVTT (SRT em Windows-1252 incluso), acompanha os
pulos no modo convertido e o idioma escolhido fica lembrado. O Bazarr pula
legenda quando o áudio já é português.

**Preferências** (`/preferencias`, ícone de engrenagem na busca): idioma do
áudio para filmes e séries — obrigatório (só dublado), preferido (baixa o que
houver e troca pela dublada quando surgir) ou tanto faz — e os idiomas de
legenda do Bazarr. O Anubis grava direto no Radarr/Sonarr (formato
personalizado "Anubis · Português (BR)" pontuado no perfil) e no Bazarr.

## API

Escuta no link direto PC ↔ FeuVault (`10.10.10.2:7070`), fora da rede da casa.

| Rota | O que faz |
|---|---|
| `GET /` | página de busca |
| `GET /search?q=TERMO` | filmes e séries encontrados, e se já foram pedidos |
| `POST /request` | pede um item: `{"kind":"movie"\|"series","id":N}` (tmdbId/tvdbId) |
| `GET /status/{kind}/{id}` | estado do pedido, progresso e link do Jellyfin |
| `POST /watch` | assistir agora: `{"kind":"movie","id":N,"guid"?:…}` |
| `GET /watch/{kind}/{id}` | player |
| `GET /watch/{kind}/{id}/video` | o vídeo, com Range (toca enquanto baixa) |
| `GET /watch/{kind}/{id}/info` | andamento do download e trechos já no disco |
| `GET /preferencias` | página de preferências |
| `GET /prefs` · `PUT /prefs` | idioma do áudio (filmes/séries) e legendas |
| `GET /health` | versão e estado |
| `GET /storage` | uso do disco, regras e o último ciclo executado |
| `GET /storage/plan` | o que seria apagado agora (não apaga nada) |
| `POST /storage/run` | roda um ciclo agora (respeita `dry_run`) |
| `POST /hooks/torrent-finished` | gatilho do qBittorrent ao concluir um download |

`POST /storage/run` e o gancho do qBittorrent exigem
`Authorization: Bearer <token>`. A busca e os pedidos ficam abertos na rede
local: pedir não apaga nada e as regras de qualidade são as do Radarr/Sonarr.

## Uso

```fish
anubis plan                  # mostra o plano de limpeza e sai
anubis serve                 # daemon: API + agendador
anubis --config ARQ serve    # outra configuração
```

Configuração em `~/.config/anubis/anubis.toml` (ou `$ANUBIS_CONFIG`); modelo
em [`anubis.example.toml`](anubis.example.toml).

## Instalação no FeuVault

Compilar no PC (o FeuVault não tem Rust; mesma glibc):

```fish
cargo build --release
scp target/release/anubis deploy/anubis.service meuserver:anubis/
ssh -t meuserver "sudo install -m 755 ~/anubis/anubis /usr/local/bin/anubis; and sudo install -m 644 ~/anubis/anubis.service /etc/systemd/system/; and sudo systemctl daemon-reload; and sudo systemctl enable --now anubis"
```

Logs: `journalctl -u anubis -f`.

## Estrutura

```
src/planner.rs       decide o que apagar (função pura, testada)
src/orchestrator.rs  inventário (APIs + disco) → plano → execução
src/services.rs      clientes de Sonarr, Radarr, qBittorrent e Jellyfin
src/disk.rs          uso do sistema de arquivos e inodes
src/search.rs        busca, pedido e acompanhamento
src/stream.rs        assistir agora (librqbit, import, entrega ao qBittorrent)
src/api.rs           rotas HTTP
web/index.html       página de busca (embutida no binário)
web/player.html      player
vendor/              librqbit-dualstack-sockets com fallback para IPv4 (o
                     FeuVault roda com ipv6.disable=1)
deploy/              unidade systemd
infra/feuvault/      composes das stacks de mídia (fonte de verdade)
```

## Testado de ponta a ponta (2026-09-27)

Com um filme de domínio público baixado pelo Radarr: import por hardlink
(2 nomes, 0 bytes a mais), proteção de item tocando e de item pela metade,
recusa de apagar dado com nome desconhecido, e exclusão real (Radarr sem
arquivo e desmonitorado, torrent removido, espaço devolvido, item fora do
Jellyfin).

## Próximos passos

- Acesso pelos apps Android/TV na rede local (por IP).
- Testes de carga (muitos gatilhos simultâneos) e medição das metas de
  consumo sob carga.
