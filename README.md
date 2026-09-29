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
abre o item no Jellyfin quando ele fica pronto. Pedido parado em
"procurando" diz por quê: a página busca as versões nos indexadores (uma vez
a cada 15 min) e mostra, por exemplo, "Nenhuma versão aceita: 11 encontradas
— 3 sem áudio em português, 8 sem fontes". Item que só passou pelo "assistir
agora" (no Radarr/Sonarr sem monitorar) aparece como não pedido, e pedi-lo
liga o monitoramento.

**Assistir agora (padrão para filmes e episódios).** "Assistir" escolhe a versão entre as
que o Radarr aprova (preferindo a dublada e, depois, H.264/AV1, que o
navegador toca direto), baixa
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

**Juntar dublagem** (filmes, botão no cartão): a imagem de uma versão com o
áudio em português de outra — para quando a dublada só existe em qualidade
pior. A página lista as versões do Radarr em duas colunas (vídeo, melhor
imagem primeiro; dublagem, as com áudio em português) e o Anubis baixa as
duas (só o vídeo de cada torrent). Antes de juntar, confere a sincronia: em
três pontos do filme (20%, 50%, 80%) procura onde o áudio da dublagem cai no
do vídeo, pelos "ataques" do som (música e efeitos são os mesmos na
dublagem). Mesmo atraso nos pontos: corta ou acrescenta esse tempo. Duração
indicando outra velocidade (25 × 23,976 quadros/s): ajusta e confere de
novo. Nada bate (versão estendida, cena a mais): não junta e mostra o que
mediu. O arquivo final tem o vídeo copiado, a dublagem como primeira faixa
(AAC) e os áudios e legendas originais; entra no Radarr pelo import manual,
com o idioma marcado como PT-BR, e os dois downloads são apagados. O ffmpeg
roda numa cópia descartável da imagem do Jellyfin (`docker run`) com o
`/data` montado como o Radarr o vê.

**Onde assistir** (opcional, com `[tmdb]` na config): quando não há versão
para baixar — no "Assistir" sem versão aceita e no "Juntar dublagem" sem
áudio em português —, a página mostra em quais serviços de streaming do
Brasil o título está (assinatura, grátis, aluguel, compra), com os dados do
JustWatch via TMDB. O TMDB não informa o idioma do áudio.

**Central de estado.** O que a página acompanha (pedidos, downloads) é
mantido pelo servidor e as rotas respondem da memória: uma consulta por
diferença ao qBittorrent a cada 2 s (só com algo na fila), as listas do
Radarr/Sonarr a cada minuto ou na hora de um webhook (versão pega, importada,
item adicionado), e o link do Jellyfin calculado uma vez por item. Só trabalha
enquanto alguém acompanha. A página recebe o estado ao vivo por uma conexão
SSE (`/events`, uma por aba): tudo ao conectar, depois só o que muda — um
pedido aparece em todas as abas junto com a resposta (medido: ~0,3 s, o tempo
do próprio Radarr); sem a conexão, volta a consultar a cada 3 s. Duas abas
abertas por um minuto custam ~12 chamadas externas (as listas do
Radarr/Sonarr), independente de quantos itens acompanham. Medido com 2 abas × 2 itens por 60 s: de 240 para
46 chamadas ao Radarr/Sonarr/Jellyfin (em regime, zero por consulta) e de
22 ms para 1,4 ms por `/status`. O `/health` mostra o total de chamadas.

**Preferências** (`/preferencias`, ícone de engrenagem na busca): idioma do
áudio para filmes e séries — obrigatório (só dublado), preferido (baixa o que
houver e troca pela dublada quando surgir) ou tanto faz — e os idiomas de
legenda do Bazarr. O Anubis grava direto no Radarr/Sonarr (formato
personalizado "Anubis · Português (BR)" pontuado no perfil) e no Bazarr.
Qualidade: Bluray 1080p primeiro e WEB 1080p como segunda opção (o alvo do
perfil fica no WEB, então um WEB baixado não é trocado sozinho) e, nos
filmes, recusar gravações de cinema (CAM, Telesync, Telecine, Workprint).

**Tocar enquanto baixa sem travar.** No "Assistir", uma versão com menos de
5 fontes perde para uma bem semeada (um Bluray mal semeado dá lugar ao WEB).
Se o download ficar ~30 s abaixo do que o vídeo consome, com pouco já
baixado à frente, o player oferece versões mais leves; trocar cancela o
download atual e continua do mesmo ponto. Item na biblioteca em WEB com um
Bluray disponível (e bem semeado): o "Assistir" pergunta se troca — a nova
versão entra no Radarr/Sonarr como melhoria e o torrent da antiga sai do
qBittorrent, se nenhum arquivo dele continuar na biblioteca. Essa conferência
tem limite de 8 s; passado isso, toca o que já está no disco.

## API

Escuta em três caminhos, do mais rápido ao mais lento: cabo direto PC ↔
FeuVault (`10.10.10.2`), rede da casa (`192.168.1.80`) e Tailscale
(`100.121.241.44` / `feuvault`), porta 7070. A página de busca testa os
caminhos mais rápidos que o atual (em paralelo, 1,5 s) e passa para o melhor
que o aparelho alcança; o "Abrir no Jellyfin" usa o mesmo caminho (o
Jellyfin publica a 8096 nos mesmos três endereços e trata essas faixas como
rede local). Para uso fora de casa, o endereço fixo é o do Tailscale:
em casa, a página sobe sozinha para a rede local ou o cabo. O cabo só fica de
pé com o PC desligado com `ignore-carrier` no NetworkManager do FeuVault
(sem isso, o 10.10.10.2 some e o Anubis perde o Radarr/Sonarr).

| Rota | O que faz |
|---|---|
| `GET /` | página de busca |
| `GET /search?q=TERMO` | filmes e séries encontrados, e se já foram pedidos |
| `POST /request` | pede um item: `{"kind":"movie"\|"series","id":N}` (tmdbId/tvdbId) |
| `GET /requests` | pedidos e biblioteca (a lista "Seus filmes e séries", igual em qualquer aparelho) |
| `GET /status/{kind}/{id}` | estado do pedido, progresso e link do Jellyfin |
| `GET /status/{kind}/{id}/versions` | pedido parado: versões encontradas e motivos das recusas |
| `POST /watch` | assistir agora: `{"kind":"movie","id":N,"guid"?:…}` |
| `GET /watch/{kind}/{id}` | player |
| `GET /watch/{kind}/{id}/video` | o vídeo, com Range (toca enquanto baixa) |
| `GET /watch/{kind}/{id}/info` | andamento do download e trechos já no disco |
| `GET /watch/{kind}/{id}/lighter` | versões mais leves (o download não acompanha o vídeo) |
| `GET /where/{kind}/{id}` | onde assistir no Brasil (filme: tmdbId; série: tvdbId) |
| `GET /mux/{tmdb}/options` | versões para juntar: vídeo e dublagem PT-BR |
| `POST /mux` | juntar dublagem: `{"id":tmdb,"video":guid,"audio":guid}` |
| `GET /mux/{tmdb}` · `DELETE` | andamento · desistir (apaga os downloads) |
| `GET /preferencias` | página de preferências |
| `GET /prefs` · `PUT /prefs` | idioma do áudio (filmes/séries) e legendas |
| `GET /health` | versão e estado |
| `GET /storage` | uso do disco, regras e o último ciclo executado |
| `GET /storage/plan` | o que seria apagado agora (não apaga nada) |
| `POST /storage/run` | roda um ciclo agora (respeita `dry_run`) |
| `POST /hooks/torrent-finished` | gatilho do qBittorrent ao concluir um download |
| `POST /hooks/arr?token=T` | webhook do Radarr/Sonarr: a central de estado refaz as listas na hora |

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
src/mux.rs           juntar dublagem (sincronia pelo áudio, ffmpeg, import manual)
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
