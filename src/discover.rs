//! Vitrine da página inicial, por categoria (filmes, séries, animes):
//! "Prontos para assistir" (o que já está na biblioteca) e três listas do
//! TMDB em português — Destaques (tendências da semana), Lançamentos e
//! Populares no Brasil (o que está nos streamings brasileiros).
//!
//! As listas do TMDB ficam 6 h em memória; séries precisam do id do TVDB (o
//! que o Sonarr usa), que o TMDB informa item a item — guardado para sempre.

use crate::avail::{Avail, Want};
use crate::orchestrator::Orchestrator;
use crate::search::{Found, Kind};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const TTL: Duration = Duration::from_secs(6 * 3600);
const POSTER: &str = "https://image.tmdb.org/t/p/w342";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    Movies,
    Series,
    Animes,
}

impl Category {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "filmes" | "movies" => Some(Self::Movies),
            "series" => Some(Self::Series),
            "animes" => Some(Self::Animes),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub title: String,
    pub items: Vec<Found>,
}

#[derive(Debug, Serialize)]
pub struct Showcase {
    /// já na biblioteca, mais recentes primeiro
    pub ready: Vec<Found>,
    pub rows: Vec<Row>,
    /// lista do TMDB que não respondeu (as outras seguem)
    pub errors: Vec<String>,
    /// títulos esperando a busca nos indexadores (de todas as abas)
    pub checking: usize,
    /// quanto falta para a fila acabar, em segundos
    pub checking_eta: u64,
}

#[derive(Default)]
pub struct Cache {
    rows: crate::foryou::Cached<(Category, &'static str), Vec<Value>>,
    tvdb: Mutex<HashMap<i64, Option<i64>>>,
}

/// Animação japonesa = anime (o critério da separação entre séries e animes).
pub(crate) fn is_anime(v: &Value) -> bool {
    v["genre_ids"].as_array().is_some_and(|g| g.iter().any(|x| x == 16)) && v["origin_country"].as_array().is_some_and(|c| c.iter().any(|x| x == "JP"))
}

/// As três listas de cada categoria: (título, caminho no TMDB). `{d60}`/`{hoje}`
/// viram datas; séries e animes são separados depois por `is_anime`.
fn lists(cat: Category) -> [(&'static str, &'static str); 3] {
    match cat {
        Category::Movies => [
            ("Destaques", "trending/movie/week?language=pt-BR"),
            // saíram em digital (4) ou físico (5) no Brasil nos últimos 60 dias: dá para baixar
            ("Lançamentos", "discover/movie?language=pt-BR&region=BR&with_release_type=4|5&release_date.gte={d60}&release_date.lte={hoje}&sort_by=popularity.desc"),
            ("Populares no Brasil", "discover/movie?language=pt-BR&watch_region=BR&with_watch_monetization_types=flatrate&sort_by=popularity.desc"),
        ],
        Category::Series => [
            ("Destaques", "trending/tv/week?language=pt-BR"),
            ("Lançamentos", "discover/tv?language=pt-BR&first_air_date.gte={d60}&first_air_date.lte={hoje}&sort_by=popularity.desc&vote_count.gte=5"),
            ("Populares no Brasil", "discover/tv?language=pt-BR&watch_region=BR&with_watch_monetization_types=flatrate&sort_by=popularity.desc"),
        ],
        Category::Animes => [
            // em exibição e em alta (as tendências gerais quase não trazem anime)
            ("Destaques", "discover/tv?language=pt-BR&with_genres=16&with_origin_country=JP&air_date.gte={d60}&sort_by=popularity.desc"),
            ("Lançamentos", "discover/tv?language=pt-BR&with_genres=16&with_origin_country=JP&first_air_date.gte={d90}&first_air_date.lte={hoje}&sort_by=popularity.desc"),
            ("Populares no Brasil", "discover/tv?language=pt-BR&with_genres=16&with_origin_country=JP&watch_region=BR&with_watch_monetization_types=flatrate&sort_by=popularity.desc"),
        ],
    }
}

fn dated(path: &str) -> String {
    let day = |d: i64| (chrono::Utc::now() - chrono::Duration::days(d)).format("%Y-%m-%d").to_string();
    path.replace("{hoje}", &day(0)).replace("{d60}", &day(60)).replace("{d90}", &day(90))
}

/// Um item do TMDB como cartão da página (sem o estado no Radarr/Sonarr).
fn from_tmdb(kind: Kind, id: i64, v: &Value) -> Option<Found> {
    let title = v["title"].as_str().or(v["name"].as_str())?.to_owned();
    let date = v["release_date"].as_str().or(v["first_air_date"].as_str()).unwrap_or_default();
    Some(Found {
        kind,
        id,
        title,
        year: date.get(..4).and_then(|y| y.parse().ok()),
        overview: v["overview"].as_str().filter(|o| !o.is_empty()).map(str::to_owned),
        poster: v["poster_path"].as_str().map(|p| format!("{POSTER}{p}")),
        genres: vec![],
        anime: kind == Kind::Series && is_anime(v),
        requested: false,
        available: false,
        avail: None,
    })
}

impl Orchestrator {
    /// A vitrine de uma categoria. `focus`: é a aba aberta (os títulos dela
    /// passam na frente da fila de checagem); falso na pré-carga das outras.
    pub async fn showcase(self: &Arc<Self>, cat: Category, focus: bool) -> Result<Showcase> {
        self.live.touch();
        let mut out = Showcase { ready: self.ready(cat).await, rows: vec![], errors: vec![], checking: 0, checking_eta: 0 };
        if self.tmdb.is_none() {
            return Ok(out);
        }
        // "Para você" primeiro (sem histórico nenhum, não aparece)
        match self.for_you(cat).await {
            Ok(items) if !items.is_empty() => out.rows.push(Row { title: "Para você".into(), items }),
            Ok(_) => {}
            Err(e) => out.errors.push(format!("Para você: {e:#}")),
        }
        // animes: os da temporada, pelo gosto
        if cat == Category::Animes {
            match self.season_row().await {
                Ok((title, items)) if !items.is_empty() => out.rows.push(Row { title, items }),
                Ok(_) => {}
                Err(e) => out.errors.push(format!("Temporada: {e:#}")),
            }
        }
        for (title, path) in lists(cat) {
            match self.tmdb_list(cat, title, path).await {
                Ok(list) => {
                    let items = self.cards(cat, &list).await;
                    out.rows.push(Row { title: title.into(), items });
                }
                Err(e) => out.errors.push(format!("{title}: {e:#}")),
            }
        }
        for row in &mut out.rows {
            self.tag_avail(&mut row.items).await;
        }
        if focus {
            // coluna por coluna: o começo de cada fileira (o que está na tela) primeiro
            let longest = out.rows.iter().map(|r| r.items.len()).max().unwrap_or(0);
            let keys: Vec<(Kind, i64)> = (0..longest).flat_map(|i| out.rows.iter().filter_map(move |r| r.items.get(i))).map(|f| (f.kind, f.id)).collect();
            self.availability.prioritize(&keys).await;
        }
        out.checking = self.availability.pending().await;
        out.checking_eta = self.availability.eta().await;
        Ok(out)
    }

    /// Põe em cada cartão o que as fontes têm e ordena a fileira: primeiro o
    /// que dá para baixar (ou já está pedido), depois o que falta conferir,
    /// no fim o que não tem fonte. Na hora de montar (as fileiras do "Para
    /// você" ficam guardadas um tempo, e a checagem anda nesse meio-tempo).
    async fn tag_avail(&self, items: &mut [Found]) {
        for f in items.iter_mut() {
            f.avail = if f.requested || f.available { None } else { Some(self.availability.get(f.kind, f.id).await.unwrap_or(Avail::Unknown)) };
        }
        items.sort_by_key(|f| match f.avail {
            None | Some(Avail::Dubbed) | Some(Avail::Subbed) => 0,
            Some(Avail::Unknown) => 1,
            Some(Avail::Nothing) => 2,
        });
    }

    /// Uma das listas da vitrine, pelo título (para o "Para você" completar).
    pub(crate) async fn showcase_list(&self, cat: Category, title: &str) -> Result<Vec<Value>> {
        let (title, path) = lists(cat).into_iter().find(|(t, _)| *t == title).context("lista desconhecida")?;
        self.tmdb_list(cat, title, path).await
    }

    pub(crate) async fn showcase_cards(self: &Arc<Self>, cat: Category, list: &[Value]) -> Vec<Found> {
        self.cards(cat, list).await
    }

    /// Uma lista do TMDB (6 h em memória), já filtrada pela categoria.
    async fn tmdb_list(&self, cat: Category, title: &'static str, path: &str) -> Result<Vec<Value>> {
        if let Some((at, v)) = self.showcase_cache.rows.lock().await.get(&(cat, title))
            && at.elapsed() < TTL
        {
            return Ok(v.clone());
        }
        let tmdb = self.tmdb.as_ref().context("TMDB não configurado")?;
        let res = tmdb.get(&dated(path)).await?;
        let Some(list) = res["results"].as_array() else { bail!("resposta sem resultados") };
        let list: Vec<Value> = list
            .iter()
            .filter(|v| match cat {
                Category::Movies => true,
                Category::Series => !is_anime(v),
                Category::Animes => is_anime(v),
            })
            .filter(|v| v["poster_path"].is_string())
            .cloned()
            .collect();
        self.showcase_cache.rows.lock().await.insert((cat, title), (Instant::now(), list.clone()));
        Ok(list)
    }

    /// Cartões com o estado no Radarr/Sonarr; séries sem id do TVDB ficam de
    /// fora (o Sonarr não teria como pedi-las). O que não está pedido nem na
    /// biblioteca vai para a fila da checagem nos indexadores (`tag_avail`
    /// põe o resultado).
    async fn cards(self: &Arc<Self>, cat: Category, list: &[Value]) -> Vec<Found> {
        let kind = if cat == Category::Movies { Kind::Movie } else { Kind::Series };
        // ids do TVDB que faltam, buscados juntos
        if kind == Kind::Series {
            let missing: Vec<i64> = {
                let known = self.showcase_cache.tvdb.lock().await;
                list.iter().filter_map(|v| v["id"].as_i64()).filter(|id| !known.contains_key(id)).collect()
            };
            let mut set = tokio::task::JoinSet::new();
            for id in missing {
                let me = self.clone();
                set.spawn(async move {
                    let tvdb = match me.tmdb.as_ref() {
                        Some(t) => t.get(&format!("tv/{id}/external_ids")).await.ok().and_then(|v| v["tvdb_id"].as_i64()),
                        None => None,
                    };
                    (id, tvdb)
                });
            }
            while let Some(Ok((id, tvdb))) = set.join_next().await {
                self.showcase_cache.tvdb.lock().await.insert(id, tvdb);
            }
        }
        let tvdb = self.showcase_cache.tvdb.lock().await.clone();
        let hidden: HashSet<(Kind, i64)> = self.hidden().await.into_iter().map(|h| (h.kind, h.id)).collect();
        let cards: Vec<(Found, &Value)> = {
            let snap = self.live.arr.lock().await;
            list.iter()
                .filter_map(|v| {
                    let tmdb = v["id"].as_i64()?;
                    let id = if kind == Kind::Movie { tmdb } else { (*tvdb.get(&tmdb)?)? };
                    if hidden.contains(&(kind, id)) {
                        return None;
                    }
                    let mut f = from_tmdb(kind, id, v)?;
                    // pedido ou na biblioteca (a central sabe, se estiver em dia)
                    if let Some((x, _)) = snap.item(kind, id) {
                        f.available = x["hasFile"] == true || x["statistics"]["episodeFileCount"].as_i64().unwrap_or(0) > 0;
                        f.requested = x["monitored"] == true || f.available;
                    }
                    Some((f, v))
                })
                .collect()
        };
        let mut shown = vec![];
        for (f, v) in cards {
            // nunca checado ou vencido: para a fila (o que já se sabia continua valendo)
            if !f.requested
                && !f.available
                && let Some(w) = Want::from_tmdb(kind, f.id, v)
            {
                self.availability.want(w).await;
            }
            shown.push(f);
        }
        shown
    }

    /// Já na biblioteca, da categoria, mais recentes primeiro.
    async fn ready(&self, cat: Category) -> Vec<Found> {
        let (movies, series) = {
            let snap = self.live.arr.lock().await;
            if snap.fresh() {
                (snap.movies.clone(), snap.series.clone())
            } else {
                drop(snap);
                let (m, s) = tokio::join!(crate::live::get(self.radarr.as_ref(), "movie"), crate::live::get(self.sonarr.as_ref(), "series"));
                (m.unwrap_or_default(), s.unwrap_or_default())
            }
        };
        let mut list: Vec<Value> = match cat {
            Category::Movies => movies.into_iter().filter(|m| m["hasFile"] == true).collect(),
            _ => series
                .into_iter()
                .filter(|s| s["statistics"]["episodeFileCount"].as_i64().unwrap_or(0) > 0)
                .filter(|s| (s["seriesType"] == "anime") == (cat == Category::Animes))
                .collect(),
        };
        list.sort_by(|a, b| b["added"].as_str().cmp(&a["added"].as_str()));
        let kind = if cat == Category::Movies { Kind::Movie } else { Kind::Series };
        list.iter().filter_map(|v| crate::search::found(kind, v)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn anime_is_japanese_animation() {
        assert!(is_anime(&json!({ "genre_ids": [16, 10759], "origin_country": ["JP"] })));
        assert!(!is_anime(&json!({ "genre_ids": [16], "origin_country": ["US"] })));
        assert!(!is_anime(&json!({ "genre_ids": [18], "origin_country": ["JP"] })));
    }

    #[test]
    fn tmdb_items_become_cards() {
        let v = json!({ "id": 1, "name": "Série", "first_air_date": "2026-08-01", "overview": "", "poster_path": "/p.jpg", "genre_ids": [16], "origin_country": ["JP"] });
        let f = from_tmdb(Kind::Series, 99, &v).unwrap();
        assert_eq!((f.id, f.year, f.anime), (99, Some(2026), true));
        assert_eq!(f.poster.as_deref(), Some("https://image.tmdb.org/t/p/w342/p.jpg"));
        assert_eq!(f.overview, None);
        assert!(dated("x?gte={d60}&lte={hoje}").len() == "x?gte=2026-01-01&lte=2026-01-01".len());
    }
}
