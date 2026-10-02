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
//! Entram também os "Populares no Brasil" e os populares dos gêneros
//! preferidos. Sai o que já foi visto, baixado ou pedido.
//!
//! Gosto por gênero vale para todos os tipos: muito filme de aventura puxa
//! séries e animes de aventura. Os gêneros de filme e de série do TMDB são
//! unificados (Ação/Aventura = "Ação e Aventura"), e o gosto vem dos gêneros
//! das sementes (quando se sabe — importação) e dos das recomendações.

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
/// peso (contra) de um "não quero ver" — um favorito vale 3
const DISLIKE: f64 = 2.0;
const ROW_LEN: usize = 20;
/// quanto o gosto por gênero pesa perto das recomendações (que valem até 1)
const TASTE_WEIGHT: f64 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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

/// Gênero de filme → o de série equivalente (o TMDB tem listas diferentes).
fn common_genre(id: i64) -> i64 {
    match id {
        28 | 12 => 10759,   // Ação, Aventura → Ação e Aventura
        878 | 14 => 10765,  // Ficção científica, Fantasia → Sci-Fi & Fantasia
        10752 => 10768,     // Guerra → Guerra e Política
        10762 => 10751,     // Kids → Família
        other => other,
    }
}

/// O caminho inverso, para buscar no TMDB os populares de um gênero.
fn native_genres(common: i64, media: Media) -> Vec<i64> {
    match (media, common) {
        (Media::Movie, 10759) => vec![28, 12],
        (Media::Movie, 10765) => vec![878, 14],
        (Media::Movie, 10768) => vec![10752],
        (Media::Movie, 10763 | 10764 | 10766 | 10767) => vec![],
        (Media::Tv, 10751) => vec![10751, 10762],
        // terror, romance, suspense, história, música, filme de TV: séries não têm
        (Media::Tv, 27 | 10749 | 53 | 36 | 10402 | 10770) => vec![],
        (_, g) => vec![g],
    }
}

fn genres_of(v: &Value) -> impl Iterator<Item = i64> + '_ {
    v["genre_ids"].as_array().into_iter().flatten().filter_map(Value::as_i64).map(common_genre)
}

/// Soma por gênero unificado, com o maior valendo 1.
fn normalized(mut g: HashMap<i64, f64>) -> HashMap<i64, f64> {
    let top = g.values().copied().fold(0.0, f64::max);
    if top > 0.0 {
        g.values_mut().for_each(|x| *x /= top);
    }
    g
}

/// Gosto por gênero, sem olhar o tipo: das sementes cujo gênero se sabe e
/// das recomendações (que dizem o gênero das demais). Meio a meio.
fn taste(seed_genres: &[(f64, Vec<i64>)], ranked: &[(Media, Value, f64)]) -> HashMap<i64, f64> {
    let mut from_seeds = HashMap::new();
    for (w, genres) in seed_genres {
        for g in genres {
            *from_seeds.entry(common_genre(*g)).or_insert(0.0) += w;
        }
    }
    let mut from_recs = HashMap::new();
    for (_, v, s) in ranked {
        for g in genres_of(v) {
            *from_recs.entry(g).or_insert(0.0) += s;
        }
    }
    let (a, b) = (normalized(from_seeds), normalized(from_recs));
    let sources = [!a.is_empty(), !b.is_empty()].iter().filter(|x| **x).count().max(1) as f64;
    let mut out = HashMap::new();
    for (g, x) in a.into_iter().chain(b) {
        *out.entry(g).or_insert(0.0) += x / sources;
    }
    out
}

/// Quanto um título combina com o gosto (0–1): a média dos seus gêneros.
/// Animação não conta na fileira de animes (todos são).
pub(crate) fn fit(taste: &HashMap<i64, f64>, v: &Value, cat: Category) -> f64 {
    let genres: HashSet<i64> = genres_of(v).filter(|g| !(cat == Category::Animes && *g == 16)).collect();
    if genres.is_empty() {
        return 0.0;
    }
    genres.iter().map(|g| taste.get(g).copied().unwrap_or(0.0)).sum::<f64>() / genres.len() as f64
}

/// O perfil de quem assiste (ver `viewer_profile`).
pub(crate) struct Profile {
    pub seeds: HashMap<(Media, i64), f64>,
    /// pedido ou na biblioteca
    pub exclude: HashSet<(Media, i64)>,
    /// recomendações do TMDB das sementes, da mais forte para a mais fraca
    pub ranked: Vec<(Media, Value, f64)>,
    /// gosto por gênero unificado (0–1)
    pub taste: HashMap<i64, f64>,
}

#[derive(Default)]
pub struct Cache {
    recs: tokio::sync::Mutex<HashMap<(Media, i64), (Instant, Vec<Value>)>>,
    rows: tokio::sync::Mutex<HashMap<Category, (Instant, Vec<Found>)>>,
    /// populares dos gêneros preferidos, pelo caminho no TMDB
    by_genre: tokio::sync::Mutex<HashMap<String, (Instant, Vec<Value>)>>,
    profile: tokio::sync::Mutex<Option<(Instant, Arc<Profile>)>>,
}

impl Cache {
    /// Histórico mudou (importação da Netflix, AniList): refazer as fileiras.
    pub async fn clear(&self) {
        self.rows.lock().await.clear();
        *self.profile.lock().await = None;
    }
}

impl Orchestrator {
    /// A fileira "Para você" de uma categoria (vazia sem nenhuma semente).
    pub async fn for_you(self: &Arc<Self>, cat: Category) -> Result<Vec<Found>> {
        if let Some((at, row)) = self.for_you_cache.rows.lock().await.get(&cat)
            && at.elapsed() < ROW_TTL
        {
            return Ok(row.clone());
        }
        let Some(p) = self.viewer_profile().await? else { return Ok(vec![]) };
        let Profile { seeds, exclude, ranked, taste } = &*p;
        let media = if cat == Category::Movies { Media::Movie } else { Media::Tv };
        let fits = |m: Media, v: &Value| {
            m == media
                && match cat {
                    Category::Movies => true,
                    Category::Series => !is_anime(v),
                    Category::Animes => is_anime(v),
                }
        };
        // recomendações (0–1, pela mais forte) + os populares, que entram só pelo gosto
        let mut pool: Vec<(Value, f64)> = ranked.iter().filter(|(m, v, _)| fits(*m, v)).map(|(_, v, s)| (v.clone(), *s)).collect();
        let strongest = pool.iter().map(|p| p.1).fold(0.0, f64::max);
        if strongest > 0.0 {
            pool.iter_mut().for_each(|p| p.1 /= strongest);
        }
        let mut seen: HashSet<i64> = pool.iter().filter_map(|p| p.0["id"].as_i64()).collect();
        let mut popular = self.showcase_list(cat, "Populares no Brasil").await.unwrap_or_default();
        popular.extend(self.popular_in(cat, taste).await);
        for v in popular {
            let Some(id) = v["id"].as_i64() else { continue };
            if fits(media, &v) && !seeds.contains_key(&(media, id)) && !exclude.contains(&(media, id)) && seen.insert(id) {
                pool.push((v, 0.0));
            }
        }
        let mut scored: Vec<(Value, f64)> = pool
            .into_iter()
            .filter(|(v, _)| v["poster_path"].is_string())
            .map(|(v, s)| {
                let f = fit(taste, &v, cat);
                (v, s + TASTE_WEIGHT * f)
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        let picked: Vec<Value> = scored.into_iter().take(ROW_LEN).map(|(v, _)| v).collect();
        let row = self.showcase_cards(cat, &picked).await;
        self.for_you_cache.rows.lock().await.insert(cat, (Instant::now(), row.clone()));
        Ok(row)
    }

    /// O perfil de quem assiste: sementes, o que já tem, as recomendações do
    /// TMDB ordenadas e o gosto por gênero (10 min em memória; None sem
    /// histórico nenhum).
    pub(crate) async fn viewer_profile(&self) -> Result<Option<Arc<Profile>>> {
        if let Some((at, p)) = &*self.for_you_cache.profile.lock().await
            && at.elapsed() < ROW_TTL
        {
            return Ok(Some(p.clone()));
        }
        let (seeds, seed_genres) = self.seeds().await?;
        if seeds.is_empty() {
            return Ok(None);
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
        let mut exclude = self.owned().await;
        // "não quero ver": fora, e sementes com peso contra (o que o TMDB
        // recomenda a partir deles desce; os gêneros deles pesam menos)
        let hidden = self.hidden().await;
        let mut weights = seeds.clone();
        let mut known: Vec<(f64, Vec<i64>)> = seed_genres.iter().filter_map(|(k, g)| Some((*seeds.get(k)?, g.clone()))).collect();
        for (i, h) in hidden.iter().rev().enumerate() {
            let key = (h.media, h.tmdb);
            exclude.insert(key);
            weights.insert(key, -DISLIKE);
            known.push((-DISLIKE, h.genres.clone()));
            if i < MAX_SEEDS
                && let Ok(list) = self.recommendations(key).await
            {
                recs.insert(key, list);
            }
        }
        let ranked = rank(&weights, &recs, &exclude);
        let taste = taste(&known, &ranked);
        let p = Arc::new(Profile { seeds, exclude, ranked, taste });
        *self.for_you_cache.profile.lock().await = Some((Instant::now(), p.clone()));
        Ok(Some(p))
    }

    /// Os populares (2 páginas) dos três gêneros preferidos, no tipo da
    /// categoria (24 h em memória). Animes: os populares do Japão, que o
    /// gosto ordena depois.
    async fn popular_in(&self, cat: Category, taste: &HashMap<i64, f64>) -> Vec<Value> {
        let media = if cat == Category::Movies { Media::Movie } else { Media::Tv };
        let mut top: Vec<(&i64, &f64)> = taste.iter().filter(|(g, _)| **g != 16 || cat != Category::Animes).collect();
        top.sort_by(|a, b| b.1.total_cmp(a.1));
        let genres: Vec<String> = top.into_iter().take(3).flat_map(|(g, _)| native_genres(*g, media)).map(|g| g.to_string()).collect();
        let path = match cat {
            Category::Animes => "discover/tv?language=pt-BR&sort_by=popularity.desc&vote_count.gte=50&with_genres=16&with_origin_country=JP".to_owned(),
            _ if genres.is_empty() => return vec![],
            Category::Movies => format!("discover/movie?language=pt-BR&sort_by=popularity.desc&vote_count.gte=200&with_genres={}", genres.join("|")),
            Category::Series => format!("discover/tv?language=pt-BR&sort_by=popularity.desc&vote_count.gte=100&without_genres=10763|10764|10766|10767&with_genres={}", genres.join("|")),
        };
        if let Some((at, v)) = self.for_you_cache.by_genre.lock().await.get(&path)
            && at.elapsed() < RECS_TTL
        {
            return v.clone();
        }
        let Some(tmdb) = self.tmdb.as_ref() else { return vec![] };
        let mut out = vec![];
        for page in 1..=2 {
            match tmdb.get(&format!("{path}&page={page}")).await {
                Ok(v) => out.extend(v["results"].as_array().cloned().unwrap_or_default()),
                Err(_) => return out,
            }
        }
        self.for_you_cache.by_genre.lock().await.insert(path, (Instant::now(), out.clone()));
        out
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
    /// As sementes com peso e, das importadas, os gêneros no TMDB.
    #[allow(clippy::type_complexity)]
    async fn seeds(&self) -> Result<(HashMap<(Media, i64), f64>, HashMap<(Media, i64), Vec<i64>>)> {
        let mut genres = HashMap::new();
        let mut seeds: HashMap<(Media, i64), f64> = HashMap::new();
        let mut add = |k: (Media, i64), w: f64| *seeds.entry(k).or_insert(0.0) += w;

        // Jellyfin: filmes e séries da biblioteca, com o que a pessoa fez com eles
        let user = self.jellyfin_user().await?;
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

        // outros serviços: Netflix (CSV importado) e AniList (Crunchyroll)
        for e in self.external_seeds().await {
            let days = e.last.map(|d| (chrono::Utc::now().date_naive() - d).num_days() as f64).unwrap_or(90.0);
            add((e.media, e.tmdb), weight(e.base, days, e.favorite));
            if !e.genres.is_empty() {
                genres.insert((e.media, e.tmdb), e.genres);
            }
        }

        // "gostei" no Anubis: como um favorito do Jellyfin
        for h in self.liked().await {
            add((h.media, h.tmdb), weight(3.0, 0.0, true));
            if !h.genres.is_empty() {
                genres.insert((h.media, h.tmdb), h.genres);
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
        drop(snap);
        Ok((seeds, genres))
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
    }

    #[test]
    fn a_dislike_pushes_its_genres_down() {
        // aventura vista, drama marcado "não quero ver"
        let t = taste(&[(2.0, vec![12]), (-DISLIKE, vec![18])], &[]);
        let drama = json!({ "genre_ids": [18] });
        let adventure = json!({ "genre_ids": [10759] });
        assert!(fit(&t, &drama, Category::Series) < 0.0);
        assert!(fit(&t, &adventure, Category::Series) > fit(&t, &drama, Category::Series));
        // o que só um oculto recomenda fica com peso negativo
        let r = |id: i64| json!({ "id": id, "genre_ids": [18] });
        let weights = HashMap::from([((Media::Tv, 1), 2.0), ((Media::Tv, 2), -DISLIKE)]);
        let recs = HashMap::from([((Media::Tv, 1), vec![r(10)]), ((Media::Tv, 2), vec![r(11)])]);
        let ranked = rank(&weights, &recs, &HashSet::new());
        assert_eq!(ranked.first().map(|x| x.1["id"].clone()), Some(json!(10)));
        assert!(ranked.iter().any(|x| x.1["id"] == 11 && x.2 < 0.0));
    }

    #[test]
    fn taste_for_genres_crosses_media() {
        // só filmes de aventura vistos: a série de "Ação e Aventura" combina mais que o drama
        let movie = |id: i64, g: i64| (Media::Movie, json!({ "id": id, "genre_ids": [g] }), 1.0);
        let t = taste(&[(2.0, vec![12]), (2.0, vec![28, 12])], &[movie(1, 12), movie(2, 18)]);
        let adventure = json!({ "genre_ids": [10759] });
        let drama = json!({ "genre_ids": [18] });
        assert!(fit(&t, &adventure, Category::Series) > fit(&t, &drama, Category::Series));
        assert_eq!(fit(&t, &adventure, Category::Series), 1.0);
        // na fileira de animes, animação não conta (todos são)
        let anime = json!({ "genre_ids": [16, 10759] });
        assert_eq!(fit(&t, &anime, Category::Animes), 1.0);
        assert_eq!(native_genres(10759, Media::Movie), [28, 12]);
        assert!(native_genres(10749, Media::Tv).is_empty());
    }
}
