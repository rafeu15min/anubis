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

**Vitrine.** A página abre em abas Filmes · Séries · Animes (a escolhida
fica lembrada). Cada uma tem, em carrossel de pôsteres: Prontos para
assistir (o que já está na biblioteca), Destaques (tendências da semana no
TMDB; em animes, os em exibição mais populares), Lançamentos (filmes lançados
em digital no Brasil nos últimos 60 dias; séries e animes estreados nos
últimos 60/90 dias) e Populares no Brasil (o que está nos streamings
brasileiros). Anime = animação japonesa. Um toque no pôster abre o cartão
completo (assistir, pedir, episódios). As listas ficam 6 h em
memória; a primeira abertura de séries/animes leva alguns segundos (o id do
TVDB de cada série vem do TMDB), e a página já pré-carrega as outras abas.

**Para você** (primeira fileira de cada aba, depois de Prontos para
assistir): recomendações a partir do que você assiste. Sementes: favoritos no
Jellyfin (o coração — o jeito mais forte de ensinar o Anubis; não perdem peso
com o tempo), assistidos até o fim, em andamento, episódios vistos de cada
série e, com peso baixo, os pedidos; o peso cai à metade a cada 30 dias. Os
candidatos são as recomendações do TMDB das sementes mais fortes (quem aparece
em várias listas sobe); sai o que já foi visto, baixado ou pedido. Para
funcionar desde o primeiro dia, completa com os Populares no Brasil dos
gêneros das sementes. Recomendações 24 h e a fileira 10 min em memória.
Também entra o histórico de fora (Preferências → Histórico de outros
serviços): o histórico completo da Crunchyroll (um botão de favoritos, usado
no crunchyroll.com logado, lê o histórico da conta pelo próprio site e baixa
um arquivo que é enviado ali), o CSV da Netflix ("Atividade de visualização" → Baixar tudo;
episódios viram a série, cada título é achado no TMDB uma vez) e a lista do
AniList — com a extensão MAL-Sync, o que se assiste na Crunchyroll vai sozinho
para o AniList (status, nota e favoritos pesam; ids pela lista pública
Fribb/anime-lists). Fica em `state_dir/externo.json`.

**Assistir um episódio, com o Prowlarr** (`[prowlarr]` na config): a busca
vai direto aos indexadores — o episódio, a temporada e o nome em português
da série (do TMDB; é como os sites brasileiros publicam) ao mesmo tempo — e
cada nome passa pelo `parse` do Sonarr (temporada/episódios, qualidade,
formatos), julgado pelo perfil da série. Numa série sem dublado, a resposta
caiu de ~76 s para ~21 s; e "A Lenda de Vox Machina S03E01-02-03 … DUAL",
que o Sonarr não reconhecia, passa a ser achado. Na importação, se o Sonarr
não reconhece o nome em 30 s, o Anubis faz a importação manual dizendo a
série. Sem `[prowlarr]`, a busca é a do Sonarr.

**Animes: nomes, legendas e dublagem** (`src/tosho.rs`). Os nomes vão às
buscas sem pontuação, em toda série: "Frieren: Beyond Journey's End S02E05"
não achava nada nos indexadores, "frieren beyond journey s end s02e05" acha
57 versões. Num anime (série com o gênero Anime), a busca do "Assistir" usa
também o nome romaji (o do TMDB, tipo "Romaji"; senão, o nome alternativo do
Sonarr) com o número do episódio, e o absoluto quando é outro — é como os
grupos publicam: "[SubsPlease] Sousou no Frieren S2 - 05", "[Erai-raws]
Sousou no Frieren 2nd Season - 05". A lista de episódios busca o romaji e o
nome de cada temporada que falta ("Sousou no Frieren 2nd Season"), e os
cartões da vitrine buscam o romaji no lugar do inglês. As buscas pelo romaji
vão só aos indexadores que não são brasileiros (o `language` do Prowlarr): os
sites brasileiros não usam esse nome, e o Prowlarr enfileira as consultas a
cada site (~2 s cada, mais com o FlareSolverr). O "Assistir" registra no log
quanto levou cada etapa (buscas, nomes, faixas).

O nome da versão não diz ao certo o que ela tem: "MULTi" ou "Multi-Audio"
pode vir sem português (japonês, inglês, hindi, tâmil…), e "Dual Audio" pode
trazer legenda em português sem dizer. O Anime Tosho publica o MediaInfo dos
torrents de anime (Nyaa, nekoBT, AniDex); o Anubis consulta pelo hash
(`feed.animetosho.net/json?show=torrent&btih=`) as 24 versões mais semeadas
de cada busca, por até 6 s, e as faixas de verdade decidem: áudio em
português conta como dublado (passa até na dublagem obrigatória e vem
primeiro), legenda em português soma o formato "Legenda PT-BR", e sem ela o
formato sai mesmo que o nome diga "Multi-Subs"; sem áudio no idioma original,
o formato do original também sai. Nos cartões (originais em japonês ou
chinês), uma dublagem em português achada assim marca o título como dublado.
Torrent que o Tosho não tem continua julgado pelo nome. As respostas ficam em
memória (sem faixas: pergunta de novo em 6 h).

A regex do formato "Anubis · Legenda PT-BR" também reconhece "Multiple
Subtitle" e "[POR-BR]" (a lista de legendas dos grupos de anime) e é
regravada no Radarr/Sonarr quando muda.

**Não quero ver** (no cartão de um título da vitrine): ele sai de todas as
fileiras da vitrine e vira sinal contra no Para você — o que o TMDB recomenda
a partir dele desce, e os gêneros dele pesam menos (cada um vale −2; um
favorito vale 3). A busca continua achando. Preferências → Ocultos da vitrine
lista os ocultos, com "Mostrar de novo".

**Temporada de … para você** (aba Animes, depois do Para você): os animes da
temporada (lista do AniList: TV, curtos e ONA do Japão; nas duas últimas
semanas de uma temporada, já as estreias da próxima), na ordem do mesmo
perfil do Para você — continuação de algo já visto sobe, depois recomendado
pelas sementes e o gosto por gênero; a popularidade no AniList só desempata.
O que já está pedido ou na biblioteca sai. A lista fica 12 h em memória (a
antiga vale enquanto a nova é montada).

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

**Botões (ícones, iguais em filmes, séries, animes e episódios):** ▶ sempre
(o "Assistir" acima); ⬇ no que não está no servidor (pede ao Radarr/Sonarr,
que baixa pelo qBittorrent; 🔔 quando nenhuma fonte tem agora: pede e fica de
olho); 🗑 no que está; 🎙, discreto, no que está sem português (procura a
dublagem na hora). Baixando, o ⬇ vira a porcentagem. Séries e animes abrem
numa janela só, com as temporadas embutidas; no topo, "▶ T2·E3" toca o
próximo episódio não assistido (tudo visto: T1·E1), e o ⬇ baixa os episódios
já exibidos que faltam. O ⬇ de um episódio, e o da série por temporada, usa
a busca do "Assistir" (nome em português, sites brasileiros, nomes como "1ª
Temporada" convertidos para o formato de cena) e entrega a versão escolhida
ao Sonarr (`release/push`), com o nome da série no Sonarr quando o português
não é reconhecido; o Sonarr baixa e importa como sempre. Nada aceito por ele:
o Sonarr busca sozinho e o episódio fica monitorado.

Séries: o ▶ de cada episódio usa a versão do episódio ou, sem ela, o pacote
da temporada baixando só aquele arquivo, e o player oferece o próximo. O que o navegador não toca (H.265,
áudio AC3/DTS, MKV) é convertido na hora pelo ffmpeg do container do Jellyfin
(`docker exec`, com um vigia que o encerra quando o player fecha). Pedidos em
andamento sobrevivem a reinícios (`/var/lib/anubis`).

Legendas: o botão CC do player lista as legendas ao lado do vídeo (as que o
Bazarr baixa em PT-BR, as que vêm no torrent) e, com o arquivo completo, as
embutidas no MKV; tudo vira WebVTT (SRT em Windows-1252 incluso), acompanha os
pulos no modo convertido e o idioma escolhido fica lembrado. O Bazarr pula
legenda quando o áudio já é português.

**Dublagem automática** (`src/autodub.rs`): filmes e episódios da
biblioteca sem áudio em português (nem no nome nem nas faixas que o mediainfo
leu; fora os de idioma original português e os especiais) ganham a faixa PT-BR
quando uma dublagem aparece nas fontes. Um arquivo que entra sem português
(webhook "Download" do Radarr/Sonarr) é procurado na hora, como o 🎙 da
página; o resto, a cada 6 h, um título por vez, com o
site ocioso há 10 min e 30 s entre as buscas (no máximo 30 por passada); a
busca é a do "Assistir" com a DHT sempre consultada, só versões com áudio em
português, inclusive as que dizem ter zero fontes (os sites brasileiros
informam 0 em quase tudo) — sem ninguém esperando, dá para tentar. Só a
dublagem é baixada (num pacote de temporada, só o episódio); parada 20 min,
desiste daquela versão. Antes de embutir, confere a sincronia contra o arquivo
da biblioteca: em três pontos (20%, 50%, 80%) procura onde o áudio da dublagem
cai no do vídeo, pelos "ataques" do som (música e efeitos são os mesmos na
dublagem). Mesmo atraso nos pontos: corta ou acrescenta esse tempo. Duração
indicando outra velocidade (25 × 23,976 quadros/s): ajusta e confere de novo.
Nada bate (versão estendida, cena a mais): tenta a próxima dublagem (até 3).
O arquivo final tem o vídeo copiado, a dublagem como primeira faixa (AAC) e os
áudios e legendas originais; entra no Radarr/Sonarr pelo import manual no
lugar do antigo, com o idioma marcado como PT-BR, e o torrent que semeava o
antigo sai do qBittorrent se não semeia mais nada da biblioteca. Sem dublagem
nas fontes, tenta de novo em 2 dias; uma dublagem que não serviu fica de fora
por 30 dias (`state_dir/dublagem.json`). `POST /autodub/{kind}/{id}` pede na
hora (filme, episódio ou a série inteira). O ffmpeg roda numa cópia
descartável da imagem do Jellyfin (`docker run`) com o `/data` montado como o
Radarr o vê. O mesmo mecanismo junta duas versões baixadas de um filme
(`POST /mux`, só pela API).

**Onde assistir** (opcional, com `[tmdb]` na config): quando não há versão
para baixar — no "Assistir" sem versão aceita —, a página mostra em quais serviços de streaming do
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

**Jellyfin já logado** (opcional, `login_user` + `login_password_file` em
`[jellyfin]`): o "Abrir no Jellyfin" passa por `/jellyfin/open`, que abre uma
sessão no Jellyfin para aquele navegador (um identificador fixo num cookie:
novos cliques renovam a mesma sessão em vez de somar aparelhos) e segue para
uma página-ponte servida pelo próprio Jellyfin (`/web/anubis-login.html`,
montada pelo compose). Ela grava a sessão no formato do Jellyfin web e abre o
item — pelo mesmo caminho (cabo, rede local, Tailscale) que o navegador usou.
A senha fica só no servidor. Quem alcança o Anubis entra no Jellyfin como esse
usuário.

**Vitrine com o que as fontes têm** (`src/avail.rs`): todo título do TMDB
aparece na vitrine (todas as fileiras, inclusive "Para você" e a temporada),
com uma etiqueta do que uma busca prévia no Prowlarr achou: dublado (sem
etiqueta), "só legendado" (amarelo), "sem fonte" (cartão apagado, no fim da
fileira) ou "a conferir". Pedidos e a biblioteca não são checados. No topo da
aba, o resumo ("Filmes: 80 dublados · 25 só legendados · …") e a fila com a
estimativa; as etiquetas se atualizam sozinhas a cada 30 s
(`GET /availability`), sem reordenar a fileira.

Busca pelo nome em português (sem pontuação nem ano — é o que a busca dos
sites brasileiros acha) e pelo original com o ano (o inglês, se o original
não é em alfabeto latino); o ano filtra os resultados (±1). Áudio: sites
brasileiros pelo nome ("brazilian", DUAL, DUBLADO, NACIONAL); os outros pelo
`parse` do Radarr/Sonarr. Português = dublado; só o idioma original = "só
legendado"; terceiro idioma não conta. Nada achado com indexador fora do ar =
"não sei" (a conferir, checa de novo em 1 h, e o que se sabia antes continua
valendo). Vale 7 dias (dublado), 3 (legendado), 1 (nada); fica em
`state_dir/disponibilidade.json`.

Ritmo: um título por vez, 15 s entre eles, uma página de cada site
(`limit=20`), e **sem o 1337x** — com duas checagens juntas e 100 resultados,
o Cloudflare dele bloqueou o IP do servidor (erro 1006) em minutos; as outras
fontes bastam para saber se o título existe, e o Radarr/Sonarr e o "Assistir"
continuam usando o 1337x. Ordem da fila: a aba aberta passa na frente (coluna
por coluna, o começo de cada fileira primeiro; a pré-carga das outras abas usa
`?bg=1` e não passa); o título aberto passa na frente de tudo
(`POST /availability/{kind}/{id}`, sem esperar a pausa) e o cartão mostra o
resultado em segundos.

**Indexadores fora do ar**: o Radarr, o Sonarr e o Prowlarr põem de castigo
o indexador que falha e param de consultá-lo por horas. O Anubis junta os que
estão fora (Prowlarr `indexerstatus` + `/health` do Radarr/Sonarr, 1 min de
cache) e avisa no "nenhuma versão" do "Assistir" e no cartão parado em
"procurando" — a busca pode ter deixado versões de fora. "Testar de novo"
(`POST /indexers/test`) testa esses indexadores em todos os programas; o que
responde sai do castigo, e a busca roda de novo.

**Preferências** (`/preferencias`, ícone de engrenagem na busca): idioma do
áudio para filmes e séries — obrigatório (só dublado), preferido (baixa o que
houver e troca pela dublada quando surgir) ou tanto faz — e os idiomas de
legenda do Bazarr. O Anubis grava direto no Radarr/Sonarr (formato
personalizado "Anubis · Português (BR)" pontuado no perfil) e no Bazarr.
Sem português, só o idioma original: o formato "Anubis · Idioma original"
(idioma "Original" do Radarr/Sonarr, que tomam o nome sem idioma como
original) vale 50, o dublado 100, e o mínimo do perfil fica em 50 — dublagem
num terceiro idioma (anime em inglês) é recusada. O "Assistir" também tira
essas versões da lista, e na busca direta pelo Prowlarr (série com nome em
português, que o Sonarr não reconhece) o Anubis marca o original sozinho. Na
inicialização a regra é regravada nos perfis (os antigos ganham o formato).
Série ou anime em "obrigatório" sem versão dublada em nenhuma fonte (a busca
acha versões com fontes, nenhuma em português): o cartão avisa "Dublagem não
encontrada em nenhuma fonte" e oferece "Instalar legendado" — aquela série
passa para o perfil "Anubis · Sem dublagem" do Sonarr (preferido: baixa
legendada e troca pela dublada se ela surgir; entre as legendadas, as com
legenda em português vêm primeiro) e o cartão passa a avisar que vem legendada.
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
| `POST /request` | pede um item: `{"kind":"movie"\|"series"\|"episode","id":N}` (tmdbId/tvdbId/id do episódio no Sonarr) |
| `GET /requests` | pedidos e biblioteca (a lista "Seus filmes e séries", igual em qualquer aparelho) |
| `GET /discover/{filmes\|series\|animes}` | vitrine: prontos para assistir, destaques, lançamentos, populares no Brasil |
| `GET /jellyfin/open?next=…` | abre o Jellyfin já logado (sessão deste navegador, pela página-ponte) |
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
| `GET /autodub` | dublagem automática: o que sabe de cada título e o que está embutindo |
| `POST /autodub/{kind}/{id}` | dublagem na hora: `movie` (tmdb), `episode` (id no Sonarr) ou `series` (tvdb) |
| `GET /preferencias` | página de preferências |
| `GET /history` · `POST`/`DELETE /history/netflix` e `/history/crunchyroll` · `PUT /history/anilist` | histórico de outros serviços (Netflix, Crunchyroll, AniList); o envio responde 202 e a busca no TMDB segue em segundo plano, com o andamento em `GET /history` |
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
src/discover.rs      vitrine por categoria (TMDB + biblioteca)
src/foryou.rs        "Para você": sementes do Jellyfin/Radarr/Sonarr + recomendações do TMDB
src/season.rs        "Temporada de … para você": animes da temporada (AniList) pelo gosto
src/external.rs      histórico de fora: CSV da Netflix e lista do AniList (Crunchyroll)
src/live.rs          central de estado (qBittorrent, Radarr/Sonarr, webhooks)
src/mux.rs           juntar dublagem (sincronia pelo áudio, ffmpeg, import manual)
src/autodub.rs       dublagem automática da biblioteca sem português
src/tosho.rs         faixas de áudio e legenda de torrents de anime (Anime Tosho)
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
