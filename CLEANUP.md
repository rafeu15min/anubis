# Anubis — Plano de lapidação

> Auditoria de 2026-10-17, com o projeto funcionalmente completo. Objetivo:
> app mais leve, rápido e bem estruturado, **sem tocar em nada ligado a
> incidente real** (lista em "Protegido"). Baseline: tag `baseline-lapidacao`
> = `a4a28d1` (árvore limpa, 17 commits ainda não enviados).
>
> **Status: só auditoria. Nada do código foi alterado.** Nenhum número abaixo
> foi medido: os ganhos são estimativas feitas lendo o código. Antes de cada
> item de requisição ou RAM, medir com o contador `UPSTREAM_CALLS` de
> `GET /health` (services.rs:17-19) e o RSS do processo.

## Como foi levantado

Cinco auditorias de leitura em paralelo, uma por área de arquivos: (ST)
streaming e mux; (BU) busca, disponibilidade e indexadores; (RC) recomendações
e histórico; (LE) leitura (LibGen, Telegram, livros); (FW) API, config e
front-end. O que cada uma achou de mais importante foi **conferido de novo no
código** antes de entrar aqui (marcado ✔). Itens sem ✔ vêm só do relatório e
precisam ser relidos antes de mexer. Também rodei `cargo clippy`: 7 avisos
leves (5 tipos complexos demais em season.rs:43, foryou.rs:184/190,
external.rs:406, discover.rs:62; stream.rs:568 referência desnecessária;
search.rs:640 `contains()`), sem `allow(dead_code)` e sem TODO/FIXME.

**Resultado geral:** não há código morto relevante (todo símbolo suspeito tem
chamador). O ganho está em requisições repetidas, duplicação e na estrutura
de arquivos grandes (`stream.rs` 2744 linhas).

## Como aplicar (protocolo)

1. `cargo test` e `cargo clippy` verdes **antes** (rodar com
   `--test-threads=1` se aparecer flakiness de ETXTBSY como no vault-sync).
2. Uma camada por vez, um commit por item, `cargo test` entre eles.
3. Testar em instância própria (porta 7071, config de teste), **nunca** o
   serviço de produção do FeuVault; binário em produção é instalado por você
   com `sudo`. Em teste, **nenhum pedido real** ao Radarr/Sonarr (eles baixam
   minutos depois).
4. Itens de requisição/RAM: medir antes e depois (UPSTREAM_CALLS, RSS).
5. UI: botões só com ícone.

## Camada 1 — morto ou risco zero

- **L1-1** Comentários soltos: services.rs:317-318 (doc do TMDB sobre
  `ONE_PAGE`), avail.rs:38, orchestrator.rs:33 (doc de `versions_cache` sobre
  o campo `live`), search.rs:507 (descreve `dub_note`, linha 524).
- **L1-2** stream.rs:2551 — `use super::*;` do `mod tests` está no meio do
  módulo; mover para o topo.
- **L1-3** style.css:41/43 — `.gear` declarado duas vezes.
- **L1-4** external.rs:682-684 — dois braços do `match` com a mesma busca.
- **L1-5** ✔ openlibrary.rs:29 e 50 — `ratings_count` é pedido à API e
  gravado no struct, mas nunca lido; o arquivo não usa `deny_unknown_fields`.
  Seguro tirar do `FIELDS` e do struct.
- **L1-6** Os 7 avisos do clippy acima.
- Falso positivo descartado: classes `i-s1…i-s8` (style.css:29-30) são
  montadas em Episodes.vue:27. **Não remover.**
- **Não remover sem migrar:** `Seed.title` (external.rs:45) — gravado em
  `externo.json`, leitura não confirmada; mexer muda o formato do arquivo.

## Camada 2 — duplicação mecânica, risco baixo

Estimativa: ~250-350 linhas a menos.

- **L2-1** ✔ Gravação atômica de JSON (tmp + rename) em **12 lugares**:
  estudo.rs, avail.rs, autodub.rs, external.rs, leitura_recs.rs, kosync.rs,
  openlibrary.rs, leitura_dl.rs, temas.rs, telegram.rs (modo 0600!), mux.rs,
  vitrine_livros.rs, mais stream.rs:755. Sete deles fazem `let _ =` e **engolem
  o erro** (a gravação falha em silêncio). Um `gravar_json(path, &valor) ->
  Result<()>` em `disk.rs` (telegram mantém 0600). `dubsync.rs:80` hoje grava
  sem tmp+rename e passaria a ser atômico.
- **L2-2** api.rs: ~27 ocorrências de `error(BAD_GATEWAY, format!("{e:#}"))` e
  ~20 blocos `match … Ok(x)=>Json(x)…`; molde já existe em `report()`
  (api.rs:206-211). Também `job_of(...)` com `match` repetido 10 vezes
  (api.rs:316…523) e closures `|State(s)| async move { Json(s.orch.X()…) }`
  (api.rs:98-138). ~80-90 linhas. Manter os códigos 404/409/202/204.
- **L2-3** services.rs: qBittorrent `by_hashes`/`maindata`/`add_seeding`
  repetem o ciclo FORBIDDEN→login→reenvio (473-528); `test_indexer` igual em
  Arr (219) e Prowlarr (358); `monitor_episodes` = `set_episodes_monitored(ids,
  true)` (139/145); vários `PUT series|movie/editor`.
- **L2-4** stream.rs/mux.rs: persistir a fonte torrent (stream.rs:1432 =
  mux.rs:608); `add_source` ≈ `readd` (stream.rs:1131 / mux.rs:721);
  `import_joined` e `import_episode` com o mesmo bloco de idiomas e o mesmo
  laço de espera (mux.rs:786-859; stream.rs:2264); varredura de inodes do
  qBittorrent em `uninstall.rs:211` e `stream.rs:1703`; atrim/adelay em
  `dubsync.rs:128` e `mux.rs:473` (só o trecho comum).
- **L2-5** Renomear colisões `Target` (stream.rs:2130 struct × mux.rs:91 enum)
  e `Library`/`LibraryVideo`. Compilador confere.
- **L2-6** search.rs: cálculo de anime/root/perfil duplicado
  (427-433 / 582-586), `replace("movie","radarr")` como gambiarra (430), literal
  `Status{NotRequested…}` duas vezes (485/493), pós-processamento repetido
  (474-481 / 500-503). Filtro "indexador torrent ligado e não 1337x" em três
  lugares (avail.rs:372, episode_avail.rs:97, stream.rs:393) → um helper.
- **L2-7** Recomendações: `anime_map()` devolve **clone do HashMap inteiro** a
  cada chamada (external.rs:1049, 1068) → `Arc<HashMap>` ✔; conversão
  `Category→Media/Kind` repetida 4× (foryou.rs:227/348, discover.rs:219/297);
  regra "Movies/Series/Animes" duplicada (discover.rs:202 / foryou.rs:228);
  `tmdb_of_tvdb` (com cache) e `Tmdb::tv_id` (sem) fazem a mesma chamada.
- **L2-8** Leitura: normalizadores de título em 5 lugares (leitura.rs:128,
  gosto.rs:77, openlibrary.rs:56, telegram.rs:192, temas.rs:33);
  `names_title`/`score_release` com a mesma regra de 60% (leitura_dl.rs:222 /
  244); nome do arquivo montado 3× (leitura_dl.rs:607/684/805); laço de
  progresso duplicado com `unreachable!()` (leitura_dl.rs:692 / 811);
  percent-encoding manual em 2 lugares (libgen.rs já usa `Url::parse_with_params`);
  User-Agent do Chrome copiado 3×.
  ⚠ `leitura_dl.rs:201` (`fold`) **não trata `ñ`** e os outros 4 tratam: antes
  de unificar é uma decisão (ver "Decisões").
- **L2-9** Front: `fetch` cru onde existe `api()` (Player.vue:100/146/393/480/
  529/535/555, Prefs.vue:149) — atenção: fetch cru não lança em erro HTTP,
  trocar muda o comportamento; `router.push('/'+d.player.replace(...))` 3×.
- **L2-10** `bitmagnet.rs:190` e `quality.rs:107` repetem as entidades HTML.

## Camada 3 — recursos e requisições (muda comportamento mensurável)

Ordenada pelo ganho estimado. ✔ = conferido no código.

**Alto**
- **L3-1** Compressão HTTP ausente ✔ (nenhuma referência no Cargo.toml/
  api.rs). Medido com `gzip -9` nos arquivos de `web/dist`: JS principal
  152.751→55.242 B, CSS 14.657→3.905, Player 27.675→10.758, Prefs 26.383→9.824;
  ~100 KB a menos na primeira carga. Opção A: gerar `.gz` no `build.sh` e
  servir com `content-encoding` quando o cliente aceita (~25 linhas, sem
  crate nova). Opção B: `tower-http::CompressionLayer` (dependência nova).
  → recomendo A (YAGNI). Os assets já têm cache `immutable` (api.rs:593).
- **L3-2** Player faz `GET /info` a cada 2 s durante toda a sessão ✔
  (Player.vue:660), sem parar ao concluir; com `phase Done` cada `/info` chama
  `watch_jellyfin_url` (api.rs:330), que em série vai ao Sonarr. Até 30
  req/min por aba de episódio. Após `done`: espaçar para 10 s ou parar,
  guardando o `jellyfin_url`; preservar `have` (alimenta a barra).
- **L3-3** Jellyfin sem cache negativo ✔ (search.rs:565-572, 736-745): quando
  `find_item` não acha, a busca se repete a **cada** `status()`, e o SSE chama
  `status()` a cada 2 s por item disponível sem link. Cache negativo de 1-5
  min (o link demora até o TTL a aparecer).
- **L3-4** Bug: o cache do Anime Tosho nunca recebe as consultas em voo ✔
  (tosho.rs:111). O `timeout` descarta o futuro e aborta o `JoinSet`; o
  `insert` está dentro das tarefas abortadas. Resultado: cada busca de anime
  gasta até 6 s e refaz as mesmas consultas; o comentário de tosho.rs:74-75
  está errado. Corrigir gravando no cache fora da tarefa.

**Médio**
- **L3-5** `releases_of` sem cache (stream.rs:1535): `watch`, `better`,
  `lighter` e `watch_candidates` disparam até 5 buscas no Prowlarr + `/parse`
  por resultado. Cache curto (5-10 min) por (kind,id). Reduz pressão nos
  indexadores (alinha com a proteção do 1337x). Também ~3 GETs repetidos de
  episódio/série por "assistir" (stream.rs:1354/1546/1813/1816) e nomes TMDB
  sem cache (stream.rs:1772/1883/1999).
- **L3-6** `sub_cache` sem limite ✔ (stream.rs:226; só `get`/`insert`, nunca
  remove). ~50-150 KB por legenda embutida (estimativa): 100 títulos ≈ 10 MB,
  perto do ocioso inteiro (12,5 MB). Limpar no `cancel` e limitar a N entradas.
- **L3-7** Jobs que nunca saem da memória (stream.rs:220 `jobs`, mux.rs
  `muxes`): cada Job é pequeno (KB), não é vazamento grave, mas `autodub_state`
  e `mux_status` percorrem tudo a cada GET `/autodub`. Varredura periódica de
  jobs sem handle, em `Done`/`Failed` e ociosos > 1 h; **não** tocar em job
  com handle nem em job que o player segura.
- **L3-8** PDF inteiro em RAM ✔ (leitura_dl.rs:622 `res.bytes()`) + `Client`
  novo por PDF (613). Usar streaming para arquivo como `libgen.rs:144-186`.
- **L3-9** `leitura-pedidos.json` regravado inteiro a cada 3 s de progresso
  (e a cada 10 s no torrent), com o lock de `jobs` preso (leitura_dl.rs:447,
  697, 816, 934). Gravar só na troca de estágio ou com debounce (reinício no
  meio perde só o progresso exibido).
- **L3-10** Concorrência sem limite nos downloads de leitura
  (leitura_dl.rs:339-367): LibGen derruba conexões acima do limite
  (libgen.rs:10 documenta). Semáforo por fonte.
- **L3-11** Busca no Telegram sem cache (telegram.rs:274-310): percorre todos
  os canais em sequência, até 4 variantes de título e de novo a cada retry.
  Cache por consulta com TTL de horas.
- **L3-12** OpenLibrary: `save()` do arquivo inteiro a cada entrada nova
  (openlibrary.rs:186/227), `found` sem expiração; `/baixar` usa `find` sem
  cache (leitura.rs:285) e a busca livre não tem cache (leitura.rs:362).
- **L3-13** kosync: cada virada de página regrava `progresso.json` inteiro
  com o lock preso (kosync.rs:86-97). Serializar fora do lock.
- **L3-14** Bug: TTL de 7 dias do mapa de anime só vale na primeira carga ✔
  (external.rs:1048-1052 devolve o mapa da memória sem checar a idade): em
  servidor de longa duração a lista só renova ao reiniciar. Guardar a hora da
  carga junto com o mapa.
- **L3-15** Perfil "Para você" no caminho frio: até ~40 chamadas TMDB em série
  (foryou.rs:305-337) → paralelo limitado (Semaphore 8 como no import);
  `tmdb_episodes` refaz temporadas já trazidas (external.rs:766-775);
  `JoinSet` sem semáforo em discover.rs:226; entradas de cache vencidas nunca
  removidas (foryou.rs:184-195).
- **L3-16** Import: laços de espera de 5 s (stream.rs:2264 × 60; mux.rs:814/851
  × 120) → backoff (5→10→15 s) ou webhook (−2/3 de GETs, detecção até ~15 s
  mais tarde).
- **L3-17** qBittorrent N+1: `files()` de **todos** os torrents para achar
  inode compartilhado (uninstall.rs:214, stream.rs:1705). Pular antes os que
  são menores que o arquivo.
- **L3-18** `episode_avail.rs:140`: `parse` do Sonarr para cada versão, sem
  teto e em série (avail.rs tem `MAX_PARSE = 30`). Teto muda quais versões
  entram → ordenar por semeadores.

**Baixo (só se medir)**
- **L3-19** `docker inspect` repetido a cada `media_tool` (mux.rs:345; ~18
  chamadas numa conferência de sincronia que falha); referência de áudio
  recalculada por velocidade (mux.rs:423) — método de sincronia é PROTEGIDO,
  só reaproveitar o cálculo.
- **L3-20** tosho cache sem teto; `avail.rs` `states()` clona o mapa a cada
  `/availability`; indexers refresca duas vezes no mesmo ciclo (indexers.rs:31
  e 133); `reqwest::Client::new()` por busca em bitmagnet.rs:75;
  `have_ranges` aloca o bitfield inteiro a cada `/info` (stream.rs:806);
  autodub 2 GETs por série a cada 6 h; sem timeout em `docker`/`pdfinfo`/
  `pdftotext` (bitmagnet.rs:146, quality.rs:165/176).

## Camada 4 — estrutura (movimento puro, o compilador confere)

- **L4-1** `stream.rs` (2744 linhas; `impl Orchestrator` sozinho ~1030).
  Divisão em ordem de risco crescente, com `pub use` no `mod.rs` para não
  mudar os caminhos que mux/dubsync/autodub/api importam: (1) funções puras
  sem IO (`playable`, `normalize_magnet`, `judge`, `classify`…) → `julgar.rs`
  (~450); (2) player/ffmpeg → `player.rs` (~400); (3) torrent → `torrent.rs`
  (~250); (4) busca → `busca.rs` (~550); (5) pedido (`watch`, `adopt_download`,
  `start_torrent` com struct no lugar de 10 parâmetros) → `pedido.rs` (~330);
  (6) import/entrega → `importar.rs` (~230); resto menor. Campos de `Job`/
  `Streamer` passam a `pub(super)`. Começar pelo (1).
- **L4-2** `mux.rs`: primeiro corte `sync.rs` (envelope, locate, check_sync,
  measure). As constantes `FPS`, `RATE`, `MIN_SHARPNESS`, `TOLERANCE`,
  `POINTS` são PROTEGIDAS: mover sem alterar.
- **L4-3** `api.rs` (963 linhas, ~60 handlers) por área (watch/stream, subs,
  storage/hooks, history), sem mudar rotas. `services.rs` um módulo por
  cliente. `search.rs` por assunto (check_availability e evaluate_episodes
  ~110-125 linhas cada: `names_for`/`queries_for`/`classify`).
- **L4-4** Leitura: dependência circular `leitura.rs`↔`leitura_recs.rs`
  (leitura.rs:39 × leitura_recs.rs:16) → módulo pequeno com `Suggestion`,
  `norm`, `owned_path`, `author_name`, `link_of`; `build_rows` (~200 linhas,
  leitura_recs.rs:408-604) em funções nomeadas; Bookshelf para arquivo próprio
  (leitura_dl.rs:123-189, o corte mais seguro).
- **L4-5** `external.rs` (1204 linhas): funções puras de CSV/Netflix →
  `external/netflix.rs`, AniList → `external/anilist.rs`; `for_you`/`seeds`
  (foryou.rs, 76 e 88 linhas) extrair as partes puras (regra das posições 2,
  6 e 10 dos livros é de design: não mexer).
- **Não recomendo agora:** quebrar `Player.vue` (859 linhas; regressão no
  player é cara e exige teste ao vivo), unificar os 8 `StdMutex` de `Job`
  (ganho desprezível), dividir a struct `Orchestrator` (~25 campos).

## Decisões suas (não aplico sem resposta)

1. **Rotas sem chamador no front** (api.rs): `GET /autodub`, `GET /indexers`,
   `GET /watch/{kind}/{id}/candidates`, `GET …/subs/{n}/raw`, `POST /mux` +
   `GET|DELETE /mux/{tmdb}` + `GET /mux/{tmdb}/options`, `DELETE /history/
   netflix|crunchyroll`. Atenção: **o "juntar dublagem" (mux) ficou sem botão
   no front**; só dispara por curl/CLI. Remover, ou religar a UI? Não remover:
   `/storage*`, `/hooks/*`, `/jellyfin/open`, `/indexers/test`.
2. **Compressão:** opção A (`.gz` no build, sem crate) ou B (`tower-http`).
3. **`ñ` nos títulos:** unificar os normalizadores muda como títulos com `ñ`
   casam nos torrents (hoje só `leitura_dl.rs` ignora o `ñ`).
4. **Teto de tamanho** nos downloads de HQ (cbz/cbr), hoje sem limite
   (leitura_dl.rs:669/791 só filtra `Kind::Book`, 300 MB) — decisão de produto.
5. **Fileiras da Amazon** (20 páginas por montagem via FlareSolverr, 90 s cada,
   em série): medir antes; cortar categorias muda o conteúdo.
6. **Mudar `anime_map` TTL** (L3-14) e **cache negativo do Jellyfin** (L3-3)
   alteram quando um dado aparece; confirmar o TTL aceitável.

## Achados fora da lapidação (verificar à parte)

- **A verificar:** a legenda por índice usa a chave `(kind,id,n)` mas a lista
  de legendas muda durante o download (stream.rs:983-997 vs 1018-1047); se
  mudar, o cache pode guardar a faixa errada. Ler `subtitles()` primeiro.
- `live.rs:88-129`: `touch()` usa `notify_waiters`, que não guarda permissão;
  janela mínima em que a central dorme até o próximo ciclo ocioso.
- `episode_avail.rs:47`: se a tarefa der panic, o item fica em `Checking` até
  reiniciar; em erro, refaz a avaliação a cada consulta sem backoff.
- `episode_avail.rs:50/125`: spawn por série aberta, sem limite e sem o GAP do
  worker — possível **lacuna na regra "uma checagem por vez"** (proteção do
  ban do 1337x). É da área PROTEGIDA: só verificar, não "otimizar".
- Testes: `uninstall.rs` e `dubsync.rs` não têm testes; `unmonitor_series`
  (uninstall.rs:105) é o caminho que a memória do projeto diz nunca ter sido
  exercitado ao vivo.
- Segurança: `kosync.rs:115` compara a chave com `==` (não em tempo
  constante) e a rota fica exposta pelo Tailscale Funnel.
- Nome "Anubis" (para o rename futuro: só a API/backend deve manter): título
  em `index.html:6`, `main.js:11-14`, `Marca.vue:17`, `Prefs.vue` (144, 242,
  255, 261, 319), `Player.vue:559`, `crunchyroll.js:62/78`, fonte
  `'Anubis Icones'` (style.css:21); chaves de localStorage `anubis.*` (trocar
  zera as preferências de cada aparelho); perfis do Radarr/Sonarr "Anubis · …"
  (prefs.rs:20-36; renomear cria duplicata); nome do aparelho no Jellyfin
  (api.rs:883). **PROTEGIDOS:** cookie `anubis_jf` e `anubis-login.html`.

## Protegido (nada disto entra em nenhuma camada)

Incidentes reais e decisões já tomadas. A lista completa está na memória do
projeto; os pontos que a auditoria conferiu no código:

- `vendor/librqbit-dualstack-sockets` (fallback 0.0.0.0 sem IPv6).
- `normalize_magnet` (btih base32 minúsculo), `ANNOUNCE_PORT=443`, listener
  `[::]:6882` TCP+uTP, 5 trackers UDP públicos, troca de release após 40 s
  (dedup por infohash), `adopt_download`.
- Import manual do Sonarr/Radarr consultando **só a pasta** (nunca com
  `seriesId` na listagem).
- Busca anti-ban do 1337x: uma checagem por vez, `GAP`, `ONE_PAGE=20`, 1337x
  fora das checagens de fundo, "desconhecido" mantém o estado, TTLs 7d/3d/1d/1h,
  queries PT limpa + original com ano, filtro ano ±1, cache de 15 min em
  versões, `indexers_down` 1 min, reteste um por vez. `bitmagnet.rs` é
  dormente por decisão (ISP derruba TLS IPv4 com DHT): não mexer.
- Sincronia de dublagem: `FPS=100`, `RATE=8000`, `MIN_SHARPNESS=10`,
  `sincronia.json` (chave `path|size`); ffmpeg em `docker run` descartável;
  watchdog de stdin do ffmpeg.
- Perfis/Custom Formats "Anubis · …" reaplicados a cada start
  (`setup_profiles`); `judge()` próprio na busca de episódio.
- Netflix em segundo plano (202 + progresso, 8 em paralelo, falha de rede não
  vira "não achado" cacheado), `best_named`, formato atual do Fribb, "Não
  quero ver", gosto cruzado entre mídias, "Para você" desde o dia 1, troca de
  temporada nas últimas 2 semanas.
- Leitura: cadeia LibGen PT → Telegram → torrent PT, canais públicos sem
  entrar, abas Livros/Artigos/Mangás, temas editáveis, fileiras em disco,
  pedidos como registro em Preferências, kosync, segredos fora do log.
- SSE `/events` + fallback de polling de 3 s, `listen` com vários endereços e
  probe de 1,5 s, webhook `/hooks/arr?token=`, `/jellyfin/open`, localStorage
  por origem, sem autenticação própria (aceito), botões só com ícone, sem
  scrollbars e setas ‹ › só com ponteiro fino.

## Ordem sugerida

1. Camada 1 (um commit, mecânico) e os bugs pequenos e certos: L3-4
   (tosho), L3-14 (TTL do anime), L2-1 (helper de JSON atômico).
2. Camada 2 por área (um commit por helper).
3. Camada 3 pelos itens ✔ de maior ganho: L3-1, L3-2, L3-3, depois L3-6, L3-8,
   L3-5, medindo `UPSTREAM_CALLS` e RSS antes/depois.
4. Camada 4 começando pelo corte puro de `stream.rs` (L4-1 passo 1) e pelo
   Bookshelf (L4-4).
