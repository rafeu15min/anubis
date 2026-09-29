//! Busca unificada: pesquisa filmes (Radarr/TMDB) e séries (Sonarr/TVDB),
//! faz o pedido e acompanha até o item estar no Jellyfin.
//!
//! O Anubis não baixa nada por conta própria: o pedido entra no Radarr/Sonarr,
//! que buscam no Prowlarr com as regras de qualidade de sempre e mandam para o
//! qBittorrent; o import por hardlink e a limpeza seguem como antes.

use crate::orchestrator::Orchestrator;
use crate::services::Arr;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Movie,
    Series,
    /// episódio de série (id = id do episódio no Sonarr); só no "assistir agora"
    Episode,
}

#[derive(Debug, Clone, Serialize)]
pub struct Found {
    pub kind: Kind,
    /// tmdbId (filme) ou tvdbId (série)
    pub id: i64,
    pub title: String,
    pub year: Option<i64>,
    pub overview: Option<String>,
    pub poster: Option<String>,
    pub genres: Vec<String>,
    /// série com o gênero "Anime" (vai para a pasta de animes)
    pub anime: bool,
    /// já está no Radarr/Sonarr
    pub requested: bool,
    /// já na biblioteca (filme baixado; série com algum episódio)
    pub available: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct Results {
    pub movies: Vec<Found>,
    pub series: Vec<Found>,
    /// serviço que não respondeu (a busca segue com o outro)
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// não está no Radarr/Sonarr
    NotRequested,
    /// pedido feito, sem download em andamento (procurando ou sem versão aceita)
    Searching,
    Downloading,
    /// baixado, entrando na biblioteca
    Importing,
    /// na biblioteca (série: ao menos um episódio)
    Available,
}

#[derive(Debug, Serialize)]
pub struct EpisodeRow {
    /// id do episódio no Sonarr
    pub id: i64,
    pub number: i64,
    pub title: Option<String>,
    pub air_date: Option<String>,
    pub has_file: bool,
}

#[derive(Debug, Serialize)]
pub struct Season {
    pub number: i64,
    pub episodes: Vec<EpisodeRow>,
}

#[derive(Debug, Serialize)]
pub struct Episodes {
    pub title: String,
    pub seasons: Vec<Season>,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub state: State,
    /// 0..1 do que está na fila
    pub progress: Option<f64>,
    /// tempo restante do download, como o Radarr/Sonarr informam ("00:03:12")
    pub eta: Option<String>,
    /// série: episódios na biblioteca / episódios monitorados
    pub episodes: Option<(i64, i64)>,
    pub jellyfin_url: Option<String>,
    /// aviso para a página (ex.: nenhuma fonte tem versão dublada)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// O que a busca nos indexadores achou para um pedido parado em "procurando":
/// quantas versões, quantas aceitas e por que as outras foram recusadas.
#[derive(Debug, Clone, Serialize)]
pub struct Versions {
    pub found: usize,
    pub approved: usize,
    /// com fontes zero (ninguém compartilhando)
    pub no_seeders: usize,
    /// motivo (em português quando conhecido) → quantas versões
    pub reasons: Vec<(String, usize)>,
    /// com fontes e áudio em português (dubladas)
    pub dubbed: usize,
}

/// Os motivos do Radarr/Sonarr que aparecem na prática, em português.
fn reason_pt(r: &str) -> String {
    let l = r.to_lowercase();
    if l.contains("custom format") && l.contains("minimum") {
        // o único formato pontuado é o "Anubis · Português (BR)" das Preferências
        "sem áudio em português".into()
    } else if l.contains("not wanted in profile") {
        "qualidade fora do perfil".into()
    } else if l.contains("upgrade") || l.contains("existing file") {
        "não é melhor que o arquivo atual".into()
    } else if l.contains("seeders") {
        "poucas fontes".into()
    } else if l.contains("size") {
        "tamanho fora do limite".into()
    } else {
        r.to_owned()
    }
}

fn summarize(releases: &[Value]) -> Versions {
    let seeded = |r: &&Value| r["protocol"] != "torrent" || r["seeders"].as_i64().unwrap_or(0) > 0;
    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    for r in releases.iter().filter(seeded).filter(|r| r["rejected"].as_bool() == Some(true)) {
        // uma versão com o mesmo motivo repetido conta uma vez
        let mine: std::collections::BTreeSet<String> = r["rejections"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(reason_pt)).collect();
        for m in mine {
            *counts.entry(m).or_default() += 1;
        }
    }
    let mut reasons: Vec<(String, usize)> = counts.into_iter().collect();
    reasons.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    reasons.truncate(3);
    Versions {
        found: releases.len(),
        approved: releases.iter().filter(seeded).filter(|r| r["rejected"].as_bool() != Some(true)).count(),
        no_seeders: releases.iter().filter(|r| !seeded(r)).count(),
        reasons,
        dubbed: releases
            .iter()
            .filter(seeded)
            .filter(|r| r["languages"].as_array().is_some_and(|l| l.iter().any(|x| x["name"].as_str().is_some_and(|n| n.starts_with("Portuguese")))))
            .count(),
    }
}

/// Estado de um item a partir do que o Radarr/Sonarr dizem (o item e a fila
/// dele) e do progresso de cada torrent no qBittorrent (`torrent(hash)` →
/// progresso 0..1, tamanho, segundos restantes). O link do Jellyfin fica de fora.
fn compute(kind: Kind, item: &Value, queue: &[Value], torrent: impl Fn(&str) -> Option<(f64, f64, i64)>) -> Status {
    let (have_any, episodes) = match kind {
        Kind::Movie => (item["hasFile"].as_bool().unwrap_or(false), None),
        Kind::Series | Kind::Episode => {
            let st = &item["statistics"];
            let files = st["episodeFileCount"].as_i64().unwrap_or(0);
            (files > 0, Some((files, st["episodeCount"].as_i64().unwrap_or(0))))
        }
    };
    let (mut size, mut left, mut importing) = (0.0, 0.0, false);
    let mut eta = None;
    for q in queue {
        size += q["size"].as_f64().unwrap_or(0.0);
        left += q["sizeleft"].as_f64().unwrap_or(0.0);
        importing |= matches!(q["trackedDownloadState"].as_str(), Some("importPending" | "importing"));
        eta = eta.or_else(|| q["timeleft"].as_str().map(str::to_owned));
    }
    let mut progress = (size > 0.0).then(|| 1.0 - left / size);

    // a fila do Radarr/Sonarr só se atualiza a cada minuto: o progresso real
    // vem do qBittorrent, pelo hash que a fila registra
    let ts: Vec<(f64, f64, i64)> = queue.iter().filter_map(|q| q["downloadId"].as_str()).filter_map(&torrent).collect();
    let total: f64 = ts.iter().map(|t| t.1).sum();
    if total > 0.0 {
        progress = Some(ts.iter().map(|t| t.0 * t.1).sum::<f64>() / total);
    }
    if let Some(s) = ts.iter().map(|t| t.2).filter(|e| *e < 8_640_000).max() {
        eta = Some(format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60));
    }

    // disponível ganha dos outros estados: uma série pode ter episódios
    // prontos enquanto outros ainda baixam
    let state = if have_any {
        State::Available
    } else if importing || progress.is_some_and(|p| p >= 1.0) {
        State::Importing
    } else if !queue.is_empty() {
        State::Downloading
    } else if item["monitored"].as_bool() == Some(false) {
        // só passou pelo "assistir agora": ninguém está procurando
        State::NotRequested
    } else {
        State::Searching
    };
    Status { state, progress, eta, episodes, jellyfin_url: None, note: None }
}

/// resultado da busca de versões guardado por este tempo (é uma busca de verdade nos indexadores)
const VERSIONS_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Onde o título passa oficialmente (streaming do país configurado).
#[derive(Debug, Serialize)]
pub struct WhereToWatch {
    /// página do TMDB com os links para cada serviço
    pub link: Option<String>,
    pub offers: Vec<Offer>,
}

#[derive(Debug, Serialize)]
pub struct Offer {
    /// "assinatura", "grátis", "grátis com anúncios", "aluguel", "compra"
    pub kind: &'static str,
    pub providers: Vec<Provider>,
}

#[derive(Debug, Serialize)]
pub struct Provider {
    pub name: String,
    pub logo: Option<String>,
}

/// As ofertas de um país na resposta de `watch/providers`, na ordem que
/// interessa (assinatura antes de comprar), cada uma na ordem do TMDB.
fn offers(country: &Value) -> WhereToWatch {
    let kinds = [("flatrate", "assinatura"), ("free", "grátis"), ("ads", "grátis com anúncios"), ("rent", "aluguel"), ("buy", "compra")];
    let offers = kinds
        .iter()
        .filter_map(|(key, kind)| {
            let providers: Vec<Provider> = country[*key]
                .as_array()?
                .iter()
                .filter_map(|p| {
                    Some(Provider { name: p["provider_name"].as_str()?.to_owned(), logo: p["logo_path"].as_str().map(|l| format!("https://image.tmdb.org/t/p/w92{l}")) })
                })
                .collect();
            (!providers.is_empty()).then_some(Offer { kind, providers })
        })
        .collect();
    WhereToWatch { link: country["link"].as_str().map(str::to_owned), offers }
}

const LIMIT: usize = 12;

pub(crate) fn found(kind: Kind, v: &Value) -> Option<Found> {
    let id = v[if kind == Kind::Movie { "tmdbId" } else { "tvdbId" }].as_i64()?;
    let poster = v["remotePoster"].as_str().map(str::to_owned).or_else(|| {
        v["images"].as_array()?.iter().find(|i| i["coverType"] == "poster")?["remoteUrl"].as_str().map(str::to_owned)
    });
    let genres: Vec<String> = v["genres"].as_array().map(|g| g.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default();
    Some(Found {
        kind,
        id,
        title: v["title"].as_str()?.to_owned(),
        year: v["year"].as_i64().filter(|y| *y > 0),
        overview: v["overview"].as_str().map(str::to_owned),
        poster,
        anime: kind == Kind::Series && genres.iter().any(|g| g == "Anime"),
        genres,
        // no Radarr/Sonarr sem monitorar = só assistido pelo "assistir agora", não pedido
        requested: v["id"].as_i64().is_some_and(|i| i > 0) && v["monitored"].as_bool() != Some(false),
        available: v["hasFile"] == true || v["statistics"]["episodeFileCount"].as_i64().unwrap_or(0) > 0,
    })
}

impl Orchestrator {
    fn arr(&self, kind: Kind) -> Result<&Arr> {
        match kind {
            Kind::Movie => self.radarr.as_ref().context("Radarr não configurado"),
            Kind::Series | Kind::Episode => self.sonarr.as_ref().context("Sonarr não configurado"),
        }
    }

    pub async fn search(&self, term: &str) -> Results {
        let mut out = Results::default();
        let (movies, series) = tokio::join!(
            async { self.arr(Kind::Movie)?.lookup("movie", term).await },
            async { self.arr(Kind::Series)?.lookup("series", term).await },
        );
        match movies {
            Ok(v) => out.movies = v.iter().filter_map(|m| found(Kind::Movie, m)).take(LIMIT).collect(),
            Err(e) => out.errors.push(format!("filmes: {e:#}")),
        }
        match series {
            Ok(v) => out.series = v.iter().filter_map(|s| found(Kind::Series, s)).take(LIMIT).collect(),
            Err(e) => out.errors.push(format!("séries: {e:#}")),
        }
        out
    }

    /// Pedidos (monitorados) e o que já está na biblioteca, mais recentes
    /// primeiro. A lista "Pedidos" da página vem daqui: a mesma em qualquer
    /// aparelho e por qualquer caminho (cabo, rede local, Tailscale).
    pub async fn requests(&self) -> Results {
        const MAX: usize = 30;
        let mut out = Results::default();
        self.live.touch();
        let snap = self.live.arr.lock().await;
        let (movies, series) = if snap.fresh() {
            (Ok(json!(snap.movies)), Ok(json!(snap.series)))
        } else {
            drop(snap);
            tokio::join!(
                async { self.arr(Kind::Movie)?.get_json("movie").await },
                async { self.arr(Kind::Series)?.get_json("series").await },
            )
        };
        let pick = |v: Value, kind: Kind| -> Vec<Found> {
            let mut list: Vec<Value> = v
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|x| {
                    let has_file = if kind == Kind::Movie { x["hasFile"] == true } else { x["statistics"]["episodeFileCount"].as_i64().unwrap_or(0) > 0 };
                    x["monitored"] == true || has_file
                })
                .collect();
            // datas ISO: a ordem do texto é a ordem do tempo
            list.sort_by(|a, b| b["added"].as_str().cmp(&a["added"].as_str()));
            list.iter().filter_map(|x| found(kind, x)).take(MAX).collect()
        };
        match movies {
            Ok(v) => out.movies = pick(v, Kind::Movie),
            Err(e) => out.errors.push(format!("filmes: {e:#}")),
        }
        match series {
            Ok(v) => out.series = pick(v, Kind::Series),
            Err(e) => out.errors.push(format!("séries: {e:#}")),
        }
        out
    }

    /// Pede o item: adiciona ao Radarr/Sonarr e manda buscar. Se já estava lá,
    /// só manda buscar de novo o que falta.
    pub async fn request(&self, kind: Kind, id: i64) -> Result<Status> {
        if kind == Kind::Episode {
            bail!("episódios são pedidos pelo \"assistir agora\"");
        }
        let arr = self.arr(kind)?;
        let (what, id_param) = if kind == Kind::Movie { ("movie", "tmdbId") } else { ("series", "tvdbId") };

        if let Some(existing) = arr.find(what, id_param, id).await? {
            let lib_id = existing["id"].as_i64().context("item sem id")?;
            // entrou pelo "assistir agora" (sem monitorar): sem isto a busca
            // não baixaria nada
            if existing["monitored"].as_bool() == Some(false) {
                match kind {
                    Kind::Movie => arr.set_movie_monitored(lib_id, true).await?,
                    Kind::Series | Kind::Episode => arr.monitor_series_all(lib_id).await?,
                }
            }
            let cmd = match kind {
                Kind::Movie => json!({ "name": "MoviesSearch", "movieIds": [lib_id] }),
                Kind::Series | Kind::Episode => json!({ "name": "SeriesSearch", "seriesId": lib_id }),
            };
            arr.command(cmd).await?;
            crate::live::refresh_arr(self).await;
            return self.status(kind, id).await;
        }

        let term = format!("{}:{id}", if kind == Kind::Movie { "tmdb" } else { "tvdb" });
        let mut item = arr.lookup(what, &term).await?.into_iter().next().with_context(|| format!("{term} não encontrado"))?;
        let cfg = if kind == Kind::Movie { self.cfg.radarr.as_ref() } else { self.cfg.sonarr.as_ref() }.context("serviço não configurado")?;
        let anime = found(kind, &item).is_some_and(|f| f.anime);
        let root = if anime { cfg.anime_root_folder.as_ref().or(cfg.root_folder.as_ref()) } else { cfg.root_folder.as_ref() };
        let Some(root) = root else { bail!("configure root_folder em [{}]", what.replace("movie", "radarr").replace("series", "sonarr")) };

        item["qualityProfileId"] = json!(arr.quality_profile_id(&cfg.quality_profile).await?);
        item["rootFolderPath"] = json!(root);
        item["monitored"] = json!(true);
        match kind {
            Kind::Movie => {
                item["minimumAvailability"] = json!("released");
                item["addOptions"] = json!({ "searchForMovie": true });
            }
            Kind::Series | Kind::Episode => {
                item["seasonFolder"] = json!(true);
                item["seriesType"] = json!(if anime { "anime" } else { "standard" });
                item["addOptions"] = json!({ "monitor": "all", "searchForMissingEpisodes": true });
            }
        }
        arr.add(what, item).await?;
        // a central refaz as listas antes de responder (senão o item novo
        // pareceria ausente até a próxima atualização)
        crate::live::refresh_arr(self).await;
        self.status(kind, id).await
    }

    pub async fn status(&self, kind: Kind, id: i64) -> Result<Status> {
        if kind == Kind::Episode {
            bail!("o estado de episódios vem do player");
        }
        self.live.touch();
        // da central (listas recentes do Radarr/Sonarr + qBittorrent), sem consultar ninguém
        let snap = self.live.arr.lock().await;
        if snap.fresh()
            && let Some((item, queue)) = snap.item(kind, id)
        {
            let (item, queue): (Value, Vec<Value>) = (item.clone(), queue.into_iter().cloned().collect());
            drop(snap);
            let torrents = self.live.torrents.lock().await;
            let mut st = compute(kind, &item, &queue, |h| torrents.get(&h.to_lowercase()).map(|t| (t["progress"].as_f64().unwrap_or(0.0), t["size"].as_f64().unwrap_or(0.0), t["eta"].as_i64().unwrap_or(8_640_000))));
            drop(torrents);
            if st.state == State::Available {
                st.jellyfin_url = self.cached_jellyfin_url(kind, id, &item).await;
            }
            st.note = self.dub_note(kind, &item).await;
            return Ok(st);
        }
        if snap.fresh() {
            // a central está em dia e não o tem: não está no Radarr/Sonarr
            return Ok(Status { state: State::NotRequested, progress: None, eta: None, episodes: None, jellyfin_url: None, note: None });
        }
        drop(snap);

        // central parada (ninguém acompanhava): na hora
        let arr = self.arr(kind)?;
        let (what, id_param) = if kind == Kind::Movie { ("movie", "tmdbId") } else { ("series", "tvdbId") };
        let Some(item) = arr.find(what, id_param, id).await? else {
            return Ok(Status { state: State::NotRequested, progress: None, eta: None, episodes: None, jellyfin_url: None, note: None });
        };
        let lib_id = item["id"].as_i64().context("item sem id")?;
        let queue = arr.queue(&format!("{}={lib_id}", if kind == Kind::Movie { "movieId" } else { "seriesId" })).await?;
        let hashes: Vec<String> = queue.iter().filter_map(|q| q["downloadId"].as_str().map(str::to_owned)).collect();
        let ts = if hashes.is_empty() { vec![] } else { self.qbit.by_hashes(&hashes).await.unwrap_or_default() };
        let mut st = compute(kind, &item, &queue, |h| ts.iter().find(|t| t.hash.eq_ignore_ascii_case(h)).map(|t| (t.progress, t.size, t.eta)));
        if st.state == State::Available {
            st.jellyfin_url = self.cached_jellyfin_url(kind, id, &item).await;
        }
        st.note = self.dub_note(kind, &item).await;
        Ok(st)
    }

    /// Aviso do que foi para o legendado.
    async fn dub_note(&self, kind: Kind, item: &Value) -> Option<String> {
        let ids = (*self.profile_ids.lock().await)?;
        let fallback = match kind {
            Kind::Movie => ids.movies,
            _ => ids.series,
        };
        (fallback.is_some() && item["qualityProfileId"].as_i64() == fallback)
            .then(|| "Sem versão dublada em nenhuma fonte: vem legendada (troca pela dublada se ela surgir)".to_owned())
    }

    /// "Dublagem não encontrada, instalar legendado?" — sim: o filme ou a
    /// série passa para o perfil sem dublagem (baixa legendado e troca pelo
    /// dublado se ele surgir) e o Radarr/Sonarr busca de novo.
    pub async fn accept_subbed(&self, kind: Kind, id: i64) -> Result<Status> {
        let ids = (*self.profile_ids.lock().await).context("perfis ainda não preparados")?;
        let arr = self.arr(kind)?;
        let title = match kind {
            Kind::Movie => {
                let profile = ids.movies.context("Radarr não configurado")?;
                let m = arr.find("movie", "tmdbId", id).await?.context("filme não pedido")?;
                let mid = m["id"].as_i64().context("filme sem id")?;
                arr.set_movie_profile(&[mid], profile).await?;
                arr.command(json!({ "name": "MoviesSearch", "movieIds": [mid] })).await?;
                m["title"].as_str().unwrap_or_default().to_owned()
            }
            _ => {
                let profile = ids.series.context("Sonarr não configurado")?;
                let s = arr.find("series", "tvdbId", id).await?.context("série não pedida")?;
                let sid = s["id"].as_i64().context("série sem id")?;
                arr.set_series_profile(&[sid], profile).await?;
                arr.command(json!({ "name": "SeriesSearch", "seriesId": sid })).await?;
                s["title"].as_str().unwrap_or_default().to_owned()
            }
        };
        tracing::info!("{title}: legendado aceito ({})", crate::prefs::FALLBACK_PROFILE);
        self.versions_cache.lock().await.remove(&(kind, id));
        crate::live::refresh_arr(self).await;
        self.status(kind, id).await
    }

    /// Link do Jellyfin, calculado uma vez por item (é uma busca no Jellyfin).
    async fn cached_jellyfin_url(&self, kind: Kind, id: i64, item: &Value) -> Option<String> {
        if let Some(u) = self.live.jellyfin.lock().await.get(&(kind, id)) {
            return Some(u.clone());
        }
        let url = self.jellyfin_url(kind, item).await?;
        self.live.jellyfin.lock().await.insert((kind, id), url.clone());
        Some(url)
    }

    /// A série no Sonarr, adicionando se preciso — sem monitorar e sem buscar:
    /// quem baixa os episódios é o "assistir agora", um de cada vez.
    pub(crate) async fn ensure_series(&self, tvdb: i64) -> Result<Value> {
        let sonarr = self.arr(Kind::Series)?;
        if let Some(s) = sonarr.find("series", "tvdbId", tvdb).await? {
            return Ok(s);
        }
        let cfg = self.cfg.sonarr.as_ref().context("Sonarr não configurado")?;
        let mut item = sonarr.lookup("series", &format!("tvdb:{tvdb}")).await?.into_iter().next().context("série não encontrada")?;
        let anime = found(Kind::Series, &item).is_some_and(|f| f.anime);
        let root = if anime { cfg.anime_root_folder.as_ref().or(cfg.root_folder.as_ref()) } else { cfg.root_folder.as_ref() };
        item["qualityProfileId"] = json!(sonarr.quality_profile_id(&cfg.quality_profile).await?);
        item["rootFolderPath"] = json!(root.context("configure root_folder em [sonarr]")?);
        item["monitored"] = json!(false);
        item["seasonFolder"] = json!(true);
        item["seriesType"] = json!(if anime { "anime" } else { "standard" });
        item["addOptions"] = json!({ "monitor": "none", "searchForMissingEpisodes": false });
        sonarr.add("series", item).await
    }

    /// Temporadas e episódios de uma série (para escolher o que assistir).
    pub async fn episodes(&self, tvdb: i64) -> Result<Episodes> {
        let series = self.ensure_series(tvdb).await?;
        let series_id = series["id"].as_i64().context("série sem id")?;
        // série recém-adicionada: o Sonarr preenche os episódios logo depois
        let sonarr = self.arr(Kind::Series)?;
        let mut raw = sonarr.episodes_raw(series_id).await?;
        for _ in 0..15 {
            if !raw.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            raw = sonarr.episodes_raw(series_id).await?;
        }
        let mut seasons: std::collections::BTreeMap<i64, Vec<EpisodeRow>> = Default::default();
        for e in raw {
            let season = e["seasonNumber"].as_i64().unwrap_or(0);
            seasons.entry(season).or_default().push(EpisodeRow {
                id: e["id"].as_i64().unwrap_or(0),
                number: e["episodeNumber"].as_i64().unwrap_or(0),
                title: e["title"].as_str().map(str::to_owned),
                air_date: e["airDate"].as_str().map(str::to_owned),
                has_file: e["hasFile"].as_bool().unwrap_or(false),
            });
        }
        let mut seasons: Vec<Season> = seasons
            .into_iter()
            .map(|(number, mut episodes)| {
                episodes.sort_by_key(|e| e.number);
                Season { number, episodes }
            })
            .collect();
        // especiais (temporada 0) por último
        seasons.sort_by_key(|s| (s.number == 0, s.number));
        Ok(Episodes { title: series["title"].as_str().unwrap_or_default().to_owned(), seasons })
    }

    /// Por que um pedido não sai de "procurando": busca as versões (filme; série:
    /// a primeira temporada com episódios faltando) e resume as recusas.
    pub async fn versions(&self, kind: Kind, id: i64) -> Result<Versions> {
        if let Some((at, v)) = self.versions_cache.lock().await.get(&(kind, id))
            && at.elapsed() < VERSIONS_TTL
        {
            return Ok(v.clone());
        }
        let arr = self.arr(kind)?;
        let filter = match kind {
            Kind::Movie => {
                let m = arr.find("movie", "tmdbId", id).await?.context("filme não pedido")?;
                format!("movieId={}", m["id"])
            }
            Kind::Series => {
                let s = arr.find("series", "tvdbId", id).await?.context("série não pedida")?;
                let season = s["seasons"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|x| x["seasonNumber"].as_i64().unwrap_or(0) > 0 && x["monitored"].as_bool() == Some(true))
                    .filter(|x| x["statistics"]["episodeFileCount"].as_i64() < x["statistics"]["episodeCount"].as_i64())
                    .filter_map(|x| x["seasonNumber"].as_i64())
                    .min()
                    .context("nenhuma temporada faltando")?;
                format!("seriesId={}&seasonNumber={season}", s["id"])
            }
            Kind::Episode => bail!("peça pela série"),
        };
        let v = summarize(&arr.releases(&filter).await?);
        self.versions_cache.lock().await.insert((kind, id), (std::time::Instant::now(), v.clone()));
        Ok(v)
    }

    /// Onde assistir oficialmente (filme: tmdbId; série: tvdbId).
    pub async fn where_to_watch(&self, kind: Kind, id: i64) -> Result<WhereToWatch> {
        let (tmdb, cfg) = self.tmdb.as_ref().zip(self.cfg.tmdb.as_ref()).context("configure [tmdb] para ver onde assistir")?;
        let path = match kind {
            Kind::Movie => format!("movie/{id}/watch/providers"),
            Kind::Series => format!("tv/{}/watch/providers", tmdb.tv_id(id).await?.context("série não encontrada no TMDB")?),
            Kind::Episode => bail!("peça pela série"),
        };
        Ok(offers(&tmdb.get(&path).await?["results"][&cfg.region]))
    }

    /// Link do item no Jellyfin, se ele já o conhece.
    pub(crate) async fn jellyfin_url(&self, kind: Kind, item: &Value) -> Option<String> {
        let public = self.cfg.jellyfin.public_url.as_deref().unwrap_or(&self.cfg.jellyfin.url).trim_end_matches('/');
        let folder = self.cfg.jellyfin_path_from_container(item["path"].as_str()?)?;
        let jf_kind = if kind == Kind::Movie { "Movie" } else { "Series" };
        let id = self.jellyfin.find_item(jf_kind, item["title"].as_str()?, &folder).await.ok()??;
        if self.jellyfin_login.is_some() {
            // pelo Anubis, que abre a sessão deste navegador e segue para o Jellyfin
            return Some(format!("/jellyfin/open?next={}", crate::services::url_encode(&format!("/details?id={id}"))));
        }
        Some(format!("{public}/web/#/details?id={id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_offers_subscription_first_and_skips_empty() {
        let br = json!({
            "link": "https://www.themoviedb.org/movie/1/watch?locale=BR",
            "buy": [{ "provider_name": "Apple TV", "logo_path": "/a.jpg" }],
            "rent": [],
            "flatrate": [{ "provider_name": "Netflix", "logo_path": "/n.jpg" }, { "provider_name": "Globoplay" }],
        });
        let w = offers(&br);
        assert_eq!(w.link.as_deref(), Some("https://www.themoviedb.org/movie/1/watch?locale=BR"));
        let kinds: Vec<_> = w.offers.iter().map(|o| o.kind).collect();
        assert_eq!(kinds, ["assinatura", "compra"]);
        assert_eq!(w.offers[0].providers[0].logo.as_deref(), Some("https://image.tmdb.org/t/p/w92/n.jpg"));
        assert_eq!(w.offers[0].providers[1].logo, None);
        // país sem nada
        assert!(offers(&Value::Null).offers.is_empty());
    }

    #[test]
    fn explains_why_nothing_was_accepted() {
        let cf = "Custom Formats have score 0 below Movie's profile minimum 100";
        let rel = |seeders: i64, rejected: bool, why: &[&str]| json!({ "protocol": "torrent", "seeders": seeders, "rejected": rejected, "rejections": why });
        let v = summarize(&[
            rel(10, true, &[cf]),
            rel(3, true, &[cf, "WEBDL-480p is not wanted in profile"]),
            rel(0, true, &[cf]),
            rel(7, true, &["Some new reason"]),
        ]);
        assert_eq!((v.found, v.approved, v.no_seeders), (4, 0, 1));
        assert_eq!(v.reasons[0], ("sem áudio em português".to_owned(), 2));
        assert_eq!(v.reasons.len(), 3);
        assert!(v.reasons.iter().any(|(r, n)| r == "Some new reason" && *n == 1));
        assert_eq!(summarize(&[rel(4, false, &[])]).approved, 1);
        let dub = json!({ "protocol": "torrent", "seeders": 3, "rejected": false, "languages": [{ "name": "Portuguese (Brazil)" }, { "name": "English" }] });
        let dub_sem_fontes = json!({ "protocol": "torrent", "seeders": 0, "rejected": false, "languages": [{ "name": "Portuguese (Brazil)" }] });
        assert_eq!(summarize(&[dub, dub_sem_fontes, rel(4, false, &[])]).dubbed, 1);
    }
}
