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

#[derive(Debug, Serialize)]
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
}

const LIMIT: usize = 12;

fn found(kind: Kind, v: &Value) -> Option<Found> {
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
        requested: v["id"].as_i64().is_some_and(|i| i > 0),
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
            let cmd = match kind {
                Kind::Movie => json!({ "name": "MoviesSearch", "movieIds": [lib_id] }),
                Kind::Series | Kind::Episode => json!({ "name": "SeriesSearch", "seriesId": lib_id }),
            };
            arr.command(cmd).await?;
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
        self.status(kind, id).await
    }

    pub async fn status(&self, kind: Kind, id: i64) -> Result<Status> {
        if kind == Kind::Episode {
            bail!("o estado de episódios vem do player");
        }
        let arr = self.arr(kind)?;
        let (what, id_param) = if kind == Kind::Movie { ("movie", "tmdbId") } else { ("series", "tvdbId") };
        let Some(item) = arr.find(what, id_param, id).await? else {
            return Ok(Status { state: State::NotRequested, progress: None, eta: None, episodes: None, jellyfin_url: None });
        };
        let lib_id = item["id"].as_i64().context("item sem id")?;

        let (have_any, episodes) = match kind {
            Kind::Movie => (item["hasFile"].as_bool().unwrap_or(false), None),
            Kind::Series | Kind::Episode => {
                let st = &item["statistics"];
                let files = st["episodeFileCount"].as_i64().unwrap_or(0);
                (files > 0, Some((files, st["episodeCount"].as_i64().unwrap_or(0))))
            }
        };

        let queue = arr.queue(&format!("{}={lib_id}", if kind == Kind::Movie { "movieId" } else { "seriesId" })).await?;
        let (mut size, mut left, mut importing) = (0.0, 0.0, false);
        let mut eta = None;
        for q in &queue {
            size += q["size"].as_f64().unwrap_or(0.0);
            left += q["sizeleft"].as_f64().unwrap_or(0.0);
            importing |= matches!(q["trackedDownloadState"].as_str(), Some("importPending" | "importing"));
            eta = eta.or_else(|| q["timeleft"].as_str().map(str::to_owned));
        }
        let mut progress = (size > 0.0).then(|| 1.0 - left / size);

        // a fila do Radarr/Sonarr só se atualiza a cada minuto: o progresso
        // real vem do qBittorrent, pelo hash que a fila registra
        let hashes: Vec<String> = queue.iter().filter_map(|q| q["downloadId"].as_str().map(str::to_owned)).collect();
        if !hashes.is_empty()
            && let Ok(ts) = self.qbit.by_hashes(&hashes).await
            && !ts.is_empty()
        {
            let total: f64 = ts.iter().map(|t| t.size).sum();
            if total > 0.0 {
                progress = Some(ts.iter().map(|t| t.progress * t.size).sum::<f64>() / total);
            }
            let secs = ts.iter().map(|t| t.eta).filter(|e| *e < 8_640_000).max();
            eta = secs.map(|s| format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)).or(eta);
        }

        // disponível ganha dos outros estados: uma série pode ter episódios
        // prontos enquanto outros ainda baixam
        let state = if have_any {
            State::Available
        } else if importing || progress.is_some_and(|p| p >= 1.0) {
            State::Importing
        } else if !queue.is_empty() {
            State::Downloading
        } else {
            State::Searching
        };

        let jellyfin_url = if have_any { self.jellyfin_url(kind, &item).await } else { None };
        Ok(Status { state, progress, eta, episodes, jellyfin_url })
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

    /// Link do item no Jellyfin, se ele já o conhece.
    pub(crate) async fn jellyfin_url(&self, kind: Kind, item: &Value) -> Option<String> {
        let public = self.cfg.jellyfin.public_url.as_deref().unwrap_or(&self.cfg.jellyfin.url).trim_end_matches('/');
        let folder = self.cfg.jellyfin_path_from_container(item["path"].as_str()?)?;
        let jf_kind = if kind == Kind::Movie { "Movie" } else { "Series" };
        let id = self.jellyfin.find_item(jf_kind, item["title"].as_str()?, &folder).await.ok()??;
        Some(format!("{public}/web/#/details?id={id}"))
    }
}
