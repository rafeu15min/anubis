//! "Para você": recomendações a partir do que a pessoa assiste.
//!
//! Sementes (com peso, que cai com o tempo — meia-vida de 30 dias):
//! - favoritos no Jellyfin (o coração): 3, sem perder peso com o tempo —
//!   favoritar é o jeito mais forte de ensinar o Anubis;
//! - assistidos até o fim: 2 (+0,5 por vez a mais, até +1);
//! - em andamento: 1,5; séries: 1 + 0,25 por episódio visto (até 3);
//! - pedidos ou baixados no Radarr/Sonarr: 1 (intenção, não gosto).
//!
//! Candidatos: as recomendações do TMDB ("quem viu isto também viu") das
//! sementes mais fortes; quem aparece em várias listas, bem colocado, sobe.
//! Sai o que já foi visto, baixado ou pedido. Para completar a fileira desde
//! o primeiro dia, entram os "Populares no Brasil" dos gêneros das sementes.

use crate::discover::{Category, is_anime};
use crate::orchestrator::Orchestrator;
use crate::search::Found;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// recomendações do TMDB de cada semente
const RECS_TTL: Duration = Duration::from_secs(24 * 3600);
/// a fileira pronta, por categoria (o histórico muda devagar)
const ROW_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_SEEDS: usize = 10;
const ROW_LEN: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Media {
    Movie,
    Tv,
}

impl Media {
    fn path(self) -> &'static str {
        match self {
            Media::Movie => "movie",
            Media::Tv => "tv",
        }
    }
}

/// Peso de um sinal, com a meia-vida de 30 dias (favorito não perde peso).
fn weight(base: f64, days_ago: f64, favorite: bool) -> f64 {
    if favorite { base } else { base * 0.5f64.powf(days_ago.max(0.0) / 30.0) }
}

fn days_since(date: Option<&str>) -> f64 {
    date.and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
        .map(|d| (chrono::Utc::now() - d.with_timezone(&chrono::Utc)).num_hours() as f64 / 24.0)
        .unwrap_or(90.0)
}

/// Ordena os candidatos: cada semente soma `peso × (1 − posição/25)` aos
/// itens que recomenda. Tira as sementes e o que está em `exclude`.
fn rank(seeds: &HashMap<(Media, i64), f64>, recs: &HashMap<(Media, i64), Vec<Value>>, exclude: &HashSet<(Media, i64)>) -> Vec<(Media, Value, f64)> {
    let mut score: HashMap<(Media, i64), (Value, f64)> = HashMap::new();
    for (seed, w) in seeds {
        for (pos, v) in recs.get(seed).into_iter().flatten().enumerate() {
            let Some(id) = v["id"].as_i64() else { continue };
            let key = (seed.0, id);
            if seeds.contains_key(&key) || exclude.contains(&key) {
                continue;
            }
            let e = score.entry(key).or_insert_with(|| (v.clone(), 0.0));
            e.1 += w * (1.0 - pos as f64 / 25.0);
        }
    }
    let mut out: Vec<(Media, Value, f64)> = score.into_iter().map(|((m, _), (v, s))| (m, v, s)).collect();
    out.sort_by(|a, b| b.2.total_cmp(&a.2));
    out
}

/// Peso de cada gênero entre os candidatos (para completar com os populares).
fn genre_affinity(ranked: &[(Media, Value, f64)]) -> HashMap<i64, f64> {
    let mut g = HashMap::new();
    for (_, v, s) in ranked {
        for id in v["genre_ids"].as_array().into_iter().flatten().filter_map(Value::as_i64) {
            *g.entry(id).or_insert(0.0) += s;
        }
    }
    g
}

#[derive(Default)]
pub struct Cache {
    recs: tokio::sync::Mutex<HashMap<(Media, i64), (Instant, Vec<Value>)>>,
    rows: tokio::sync::Mutex<HashMap<Category, (Instant, Vec<Found>)>>,
}

impl Orchestrator {
    /// A fileira "Para você" de uma categoria (vazia sem nenhuma semente).
    pub async fn for_you(self: &Arc<Self>, cat: Category) -> Result<Vec<Found>> {
        if let Some((at, row)) = self.for_you_cache.rows.lock().await.get(&cat)
            && at.elapsed() < ROW_TTL
        {
            return Ok(row.clone());
        }
        let seeds = self.seeds().await?;
        if seeds.is_empty() {
            return Ok(vec![]);
        }
        // as sementes mais fortes de cada tipo
        let mut strongest: Vec<(&(Media, i64), &f64)> = seeds.iter().collect();
        strongest.sort_by(|a, b| b.1.total_cmp(a.1));
        let mut recs = HashMap::new();
        for (seed, _) in strongest.into_iter().take(MAX_SEEDS * 2) {
            if let Ok(list) = self.recommendations(*seed).await {
                recs.insert(*seed, list);
            }
        }
        let exclude = self.owned().await;
        let ranked = rank(&seeds, &recs, &exclude);
        let fits = |m: Media, v: &Value| match cat {
            Category::Movies => m == Media::Movie,
            Category::Series => m == Media::Tv && !is_anime(v),
            Category::Animes => m == Media::Tv && is_anime(v),
        };
        let mut picked: Vec<Value> = ranked.iter().filter(|(m, v, _)| fits(*m, v) && v["poster_path"].is_string()).map(|(_, v, _)| v.clone()).take(ROW_LEN).collect();
        // completa com os populares no Brasil, os dos gêneros das sementes primeiro
        if picked.len() < ROW_LEN {
            let affinity = genre_affinity(&ranked);
            let media = if cat == Category::Movies { Media::Movie } else { Media::Tv };
            let seen: HashSet<i64> = picked.iter().filter_map(|v| v["id"].as_i64()).collect();
            let mut popular: Vec<Value> = self
                .showcase_list(cat, "Populares no Brasil")
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|v| v["id"].as_i64().is_some_and(|id| !seen.contains(&id) && !seeds.contains_key(&(media, id)) && !exclude.contains(&(media, id))))
                .collect();
            let fit = |v: &Value| v["genre_ids"].as_array().into_iter().flatten().filter_map(Value::as_i64).map(|g| affinity.get(&g).copied().unwrap_or(0.0)).sum::<f64>();
            popular.sort_by(|a, b| fit(b).total_cmp(&fit(a)));
            picked.extend(popular.into_iter().take(ROW_LEN - picked.len()));
        }
        let row = self.showcase_cards(cat, &picked).await;
        self.for_you_cache.rows.lock().await.insert(cat, (Instant::now(), row.clone()));
        Ok(row)
    }

    /// Recomendações do TMDB de uma semente (24 h em memória).
    async fn recommendations(&self, seed: (Media, i64)) -> Result<Vec<Value>> {
        if let Some((at, v)) = self.for_you_cache.recs.lock().await.get(&seed)
            && at.elapsed() < RECS_TTL
        {
            return Ok(v.clone());
        }
        let tmdb = self.tmdb.as_ref().context("TMDB não configurado")?;
        let v = tmdb.get(&format!("{}/{}/recommendations?language=pt-BR", seed.0.path(), seed.1)).await?;
        let list = v["results"].as_array().cloned().unwrap_or_default();
        self.for_you_cache.recs.lock().await.insert(seed, (Instant::now(), list.clone()));
        Ok(list)
    }

    /// O que já está pedido ou na biblioteca (pelo id do TMDB).
    async fn owned(&self) -> HashSet<(Media, i64)> {
        let snap = self.live.arr.lock().await;
        let mut out = HashSet::new();
        for m in &snap.movies {
            if let Some(id) = m["tmdbId"].as_i64() {
                out.insert((Media::Movie, id));
            }
        }
        for s in &snap.series {
            if let Some(id) = s["tmdbId"].as_i64().filter(|i| *i > 0) {
                out.insert((Media::Tv, id));
            }
        }
        out
    }

    /// As sementes: histórico e favoritos no Jellyfin + pedidos no Radarr/Sonarr.
    async fn seeds(&self) -> Result<HashMap<(Media, i64), f64>> {
        let mut seeds: HashMap<(Media, i64), f64> = HashMap::new();
        let mut add = |k: (Media, i64), w: f64| *seeds.entry(k).or_insert(0.0) += w;

        // Jellyfin: filmes e séries da biblioteca, com o que a pessoa fez com eles
        let users = self.jellyfin.get_json("Users").await?;
        let user = users
            .as_array()
            .and_then(|u| {
                let by_name = self.jellyfin_login.as_ref().and_then(|(name, _)| u.iter().find(|x| x["Name"] == name.as_str()));
                by_name.or_else(|| u.iter().find(|x| x["Policy"]["IsAdministrator"] == true)).or(u.first())
            })
            .and_then(|u| u["Id"].as_str())
            .context("Jellyfin sem usuário")?
            .to_owned();
        let lib = self.jellyfin.get_json(&format!("Users/{user}/Items?Recursive=true&IncludeItemTypes=Movie,Series&Fields=ProviderIds")).await?;
        let mut series_tmdb: HashMap<String, i64> = HashMap::new();
        for it in lib["Items"].as_array().into_iter().flatten() {
            let Some(tmdb) = it["ProviderIds"]["Tmdb"].as_str().and_then(|t| t.parse().ok()) else { continue };
            let media = if it["Type"] == "Movie" { Media::Movie } else { Media::Tv };
            if media == Media::Tv
                && let Some(id) = it["Id"].as_str()
            {
                series_tmdb.insert(id.to_owned(), tmdb);
            }
            let u = &it["UserData"];
            let days = days_since(u["LastPlayedDate"].as_str());
            if u["IsFavorite"] == true {
                add((media, tmdb), weight(3.0, days, true));
            }
            if media == Media::Movie {
                if u["Played"] == true {
                    let extra = (u["PlayCount"].as_f64().unwrap_or(1.0) - 1.0).clamp(0.0, 2.0) * 0.5;
                    add((media, tmdb), weight(2.0 + extra, days, false));
                } else if u["PlaybackPositionTicks"].as_i64().unwrap_or(0) > 0 {
                    add((media, tmdb), weight(1.5, days, false));
                }
            }
        }
        // episódios vistos ou em andamento contam para a série
        let eps = self
            .jellyfin
            .get_json(&format!("Users/{user}/Items?Recursive=true&IncludeItemTypes=Episode&SortBy=DatePlayed&SortOrder=Descending&Limit=300"))
            .await?;
        let mut per_series: HashMap<String, (f64, f64)> = HashMap::new(); // episódios, dias do mais recente
        for e in eps["Items"].as_array().into_iter().flatten() {
            let u = &e["UserData"];
            if u["Played"] != true && u["PlaybackPositionTicks"].as_i64().unwrap_or(0) == 0 {
                continue;
            }
            let Some(sid) = e["SeriesId"].as_str() else { continue };
            let entry = per_series.entry(sid.to_owned()).or_insert((0.0, f64::MAX));
            entry.0 += 1.0;
            entry.1 = entry.1.min(days_since(u["LastPlayedDate"].as_str()));
        }
        for (sid, (n, days)) in per_series {
            if let Some(tmdb) = series_tmdb.get(&sid) {
                add((Media::Tv, *tmdb), weight((1.0 + 0.25 * n).min(3.0), days, false));
            }
        }

        // pedidos e baixados: intenção, peso baixo (pela data em que entraram)
        let snap = self.live.arr.lock().await;
        let arr = snap.movies.iter().map(|m| (Media::Movie, m)).chain(snap.series.iter().map(|s| (Media::Tv, s)));
        for (media, x) in arr {
            let owned = x["monitored"] == true || x["hasFile"] == true || x["statistics"]["episodeFileCount"].as_i64().unwrap_or(0) > 0;
            if let (true, Some(tmdb)) = (owned, x["tmdbId"].as_i64().filter(|i| *i > 0)) {
                add((media, tmdb), weight(1.0, days_since(x["added"].as_str()), false));
            }
        }
        Ok(seeds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn weights_fade_except_favorites() {
        assert!((weight(2.0, 30.0, false) - 1.0).abs() < 1e-9);
        assert!((weight(2.0, 0.0, false) - 2.0).abs() < 1e-9);
        assert_eq!(weight(3.0, 365.0, true), 3.0);
    }

    #[test]
    fn items_recommended_by_several_strong_seeds_rank_first() {
        let r = |id: i64| json!({ "id": id, "genre_ids": [16] });
        let seeds = HashMap::from([((Media::Movie, 1), 3.0), ((Media::Movie, 2), 1.0)]);
        let recs = HashMap::from([
            ((Media::Movie, 1), vec![r(10), r(11), r(2)]),
            ((Media::Movie, 2), vec![r(12), r(11)]),
        ]);
        let exclude = HashSet::from([(Media::Movie, 12)]);
        let ids: Vec<i64> = rank(&seeds, &recs, &exclude).iter().map(|(_, v, _)| v["id"].as_i64().unwrap()).collect();
        // 11 vem das duas sementes; 2 é semente e 12 já está na biblioteca
        assert_eq!(ids, [11, 10]);
        assert!(genre_affinity(&rank(&seeds, &recs, &exclude))[&16] > 0.0);
    }
}
