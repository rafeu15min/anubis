//! "Temporada de outubro de 2026 para você": os animes da temporada (lista
//! do AniList), na ordem do gosto de quem assiste — o mesmo perfil do "Para
//! você". Continuação de algo já visto (Netflix, Crunchyroll, Jellyfin) sobe;
//! o que já está pedido ou na biblioteca sai (vem sozinho).

use crate::discover::{Category, is_anime};
use crate::external::ANILIST;
use crate::foryou::{Media, fit};
use crate::orchestrator::Orchestrator;
use crate::search::Found;
use crate::services::url_encode;
use anyhow::{Context, Result, bail};
use chrono::{Datelike, NaiveDate};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// a lista da temporada muda pouco (estreias entram aos poucos)
const LIST_TTL: Duration = Duration::from_secs(12 * 3600);
const ROW_LEN: usize = 20;

/// A temporada de anime de uma data: (nome no AniList, ano, mês em que ela
/// começa). Nas duas últimas semanas de uma temporada já é a próxima — as
/// estreias que vêm aí.
pub(crate) fn season_of(d: NaiveDate) -> (&'static str, i32, &'static str) {
    const SEASONS: [(&str, &str); 4] = [("WINTER", "janeiro"), ("SPRING", "abril"), ("SUMMER", "julho"), ("FALL", "outubro")];
    let mut i = (d.month0() / 3) as usize;
    let mut year = d.year();
    if d.month() == i as u32 * 3 + 3 && d.day() >= 16 {
        i += 1;
        if i == 4 {
            i = 0;
            year += 1;
        }
    }
    (SEASONS[i].0, year, SEASONS[i].1)
}

/// Os animes da temporada no TMDB, na ordem de popularidade do AniList.
type List = Vec<Value>;
static CACHE: OnceLock<Mutex<Option<(Instant, &'static str, i32, List)>>> = OnceLock::new();
static REFRESHING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

impl Orchestrator {
    /// A fileira da temporada (título, cartões), para a aba Animes.
    pub async fn season_row(self: &Arc<Self>) -> Result<(String, Vec<Found>)> {
        let (season, year, month) = season_of(chrono::Local::now().date_naive());
        let list = self.season_list(season, year).await?;
        let profile = self.viewer_profile().await?;
        let n = list.len().max(1) as f64;
        // popularidade no AniList: só desempata (0–0,3)
        let popular = |i: usize| 0.3 * (1.0 - i as f64 / n);
        let mut scored: Vec<(f64, &Value)> = match &profile {
            Some(p) => {
                let recs: HashMap<i64, f64> =
                    p.ranked.iter().filter(|(m, _, _)| *m == Media::Tv).filter_map(|(_, v, s)| Some((v["id"].as_i64()?, *s))).collect();
                let top = recs.values().copied().fold(0.0, f64::max).max(1e-9);
                list.iter()
                    .enumerate()
                    .filter_map(|(i, v)| {
                        let id = v["id"].as_i64()?;
                        if p.exclude.contains(&(Media::Tv, id)) {
                            return None;
                        }
                        // já visto (outra temporada): a continuação interessa
                        let watched = if p.seeds.contains_key(&(Media::Tv, id)) { 1.0 } else { 0.0 };
                        let recommended = recs.get(&id).map_or(0.0, |s| s / top);
                        Some((watched + recommended + fit(&p.taste, v, Category::Animes) + popular(i), v))
                    })
                    .collect()
            }
            None => list.iter().enumerate().map(|(i, v)| (popular(i), v)).collect(),
        };
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        let picked: Vec<Value> = scored.into_iter().take(ROW_LEN).map(|(_, v)| v.clone()).collect();
        let title = match profile {
            Some(_) => format!("Temporada de {month} de {year} para você"),
            None => format!("Temporada de {month} de {year}"),
        };
        Ok((title, self.showcase_cards(Category::Animes, &picked).await))
    }

    /// A lista da temporada. Passadas 12 h, a antiga segue valendo enquanto
    /// a nova é montada em segundo plano (leva ~15 s: AniList + TMDB).
    async fn season_list(self: &Arc<Self>, season: &'static str, year: i32) -> Result<List> {
        let cache = CACHE.get_or_init(Default::default);
        if let Some((at, s, y, list)) = &*cache.lock().await
            && (*s, *y) == (season, year)
        {
            if at.elapsed() >= LIST_TTL && !REFRESHING.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let me = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = me.build_season_list(season, year).await {
                        tracing::warn!("temporada: {e:#}");
                    }
                    REFRESHING.store(false, std::sync::atomic::Ordering::SeqCst);
                });
            }
            return Ok(list.clone());
        }
        self.build_season_list(season, year).await
    }

    async fn build_season_list(self: &Arc<Self>, season: &'static str, year: i32) -> Result<List> {
        let cache = CACHE.get_or_init(Default::default);
        let anilist = self.anilist_season(season, year).await?;
        let map = self.anime_map().await.unwrap_or_default();
        let sem = Arc::new(tokio::sync::Semaphore::new(8));
        let mut set = tokio::task::JoinSet::new();
        for (pos, (aid, titles)) in anilist.into_iter().enumerate() {
            let known = map.get(&aid).and_then(|m| m.tmdb_tv);
            let (me, sem) = (self.clone(), sem.clone());
            set.spawn(async move {
                let _slot = sem.acquire_owned().await;
                (pos, me.season_show(known, &titles).await)
            });
        }
        let mut found: Vec<(usize, Value)> = vec![];
        while let Some(r) = set.join_next().await {
            if let Ok((pos, Some(v))) = r {
                found.push((pos, v));
            }
        }
        found.sort_by_key(|(pos, _)| *pos);
        // duas partes da mesma série no AniList são uma só no TMDB
        let mut seen = HashSet::new();
        let list: List = found.into_iter().map(|(_, v)| v).filter(|v| seen.insert(v["id"].as_i64())).collect();
        if list.is_empty() {
            bail!("nenhum anime da temporada achado no TMDB");
        }
        *cache.lock().await = Some((Instant::now(), season, year, list.clone()));
        Ok(list)
    }

    /// O anime no TMDB, como vem nas listas (com genre_ids e origin_country,
    /// que o gosto e a aba usam). Estreia nova, ainda fora da lista de ids:
    /// pelo título.
    async fn season_show(&self, known: Option<i64>, titles: &[String]) -> Option<Value> {
        let tmdb = self.tmdb.as_ref()?;
        let id = match known {
            Some(id) => id,
            None => {
                let mut found = None;
                for t in titles {
                    let res = tmdb.get(&format!("search/tv?query={}&language=pt-BR", url_encode(t))).await.ok()?;
                    found = res["results"].as_array().into_iter().flatten().find(|x| is_anime(x)).and_then(|x| x["id"].as_i64());
                    if found.is_some() {
                        break;
                    }
                }
                found?
            }
        };
        let mut v = tmdb.get(&format!("tv/{id}?language=pt-BR")).await.ok()?;
        let genres: Vec<i64> = v["genres"].as_array().into_iter().flatten().filter_map(|g| g["id"].as_i64()).collect();
        v["genre_ids"] = json!(genres);
        // estreia sem nome traduzido no TMDB ("デモンズ・クレスト"): o do AniList
        let latin = v["name"].as_str().is_some_and(|n| n.chars().any(|c| c.is_ascii_alphabetic()));
        if !latin && let Some(t) = titles.first() {
            v["name"] = json!(t);
        }
        v["poster_path"].is_string().then_some(v)
    }

    /// Os animes da temporada no AniList (TV, curtos e ONA do Japão), dos
    /// mais populares: (id no AniList, títulos em inglês e romaji).
    async fn anilist_season(&self, season: &str, year: i32) -> Result<Vec<(i64, Vec<String>)>> {
        let query = r#"query($s: MediaSeason, $y: Int, $p: Int) {
            Page(page: $p, perPage: 50) {
                pageInfo { hasNextPage }
                media(season: $s, seasonYear: $y, type: ANIME, isAdult: false, countryOfOrigin: JP, format_in: [TV, TV_SHORT, ONA], sort: POPULARITY_DESC) {
                    id title { english romaji }
                }
            }
        }"#;
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
        let mut out = vec![];
        for page in 1..=2 {
            let body = json!({ "query": query, "variables": { "s": season, "y": year, "p": page } });
            let res: Value = http.post(ANILIST).json(&body).send().await.context("AniList: sem resposta")?.json().await?;
            if let Some(e) = res["errors"].as_array().and_then(|e| e.first()) {
                bail!("AniList: {}", e["message"].as_str().unwrap_or("erro"));
            }
            let data = &res["data"]["Page"];
            for m in data["media"].as_array().into_iter().flatten() {
                let Some(id) = m["id"].as_i64() else { continue };
                let titles = ["english", "romaji"].iter().filter_map(|k| m["title"][*k].as_str().map(str::to_owned)).collect();
                out.push((id, titles));
            }
            if data["pageInfo"]["hasNextPage"] != true {
                break;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn season_turns_to_the_next_premieres_in_its_last_two_weeks() {
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        assert_eq!(season_of(d(2026, 8, 10)), ("SUMMER", 2026, "julho"));
        assert_eq!(season_of(d(2026, 9, 15)), ("SUMMER", 2026, "julho"));
        assert_eq!(season_of(d(2026, 9, 29)), ("FALL", 2026, "outubro"));
        assert_eq!(season_of(d(2026, 12, 20)), ("WINTER", 2027, "janeiro"));
        assert_eq!(season_of(d(2027, 1, 5)), ("WINTER", 2027, "janeiro"));
        assert_eq!(season_of(d(2027, 5, 1)), ("SPRING", 2027, "abril"));
    }
}
