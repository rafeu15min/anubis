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
(que também desmarca o item, para não ser baixado de novo, e avisa o Jellyfin)
e remove o torrent do qBittorrent junto com os dados. Só conta como espaço
liberado o que perde todos os nomes no disco.

A checagem roda a cada 30 minutos e também na hora em que um download termina
(o qBittorrent chama `POST /hooks/torrent-finished`). Com `dry_run = true` ele
só registra no log o que apagaria.

## API

Escuta no link direto PC ↔ FeuVault (`10.10.10.2:7070`), fora da rede da casa.

| Rota | O que faz |
|---|---|
| `GET /health` | versão e estado |
| `GET /storage` | uso do disco, regras e o último ciclo executado |
| `GET /storage/plan` | o que seria apagado agora (não apaga nada) |
| `POST /storage/run` | roda um ciclo agora (respeita `dry_run`) |
| `POST /hooks/torrent-finished` | gatilho do qBittorrent ao concluir um download |

As rotas POST exigem `Authorization: Bearer <token>`.

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
src/api.rs           rotas HTTP
deploy/              unidade systemd
infra/feuvault/      composes das stacks de mídia (fonte de verdade)
```

## Próximos passos

- API de busca unificada (Prowlarr → Sonarr/Radarr/qBittorrent, progresso e
  link do Jellyfin ao terminar), usada pela página na Homepage e pelos apps
  Android/TV na rede local.
- Testes de carga (muitos gatilhos simultâneos) e medição das metas de
  consumo sob carga.
