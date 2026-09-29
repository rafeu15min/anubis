//! Histórico de outros serviços, como sementes do "Para você":
//! - Netflix: o CSV da "Atividade de visualização" (Conta → Perfil → Baixar
//!   tudo), enviado pela página de Preferências. Episódios viram a série
//!   ("Série: Temporada 1: Episódio" → "Série"); cada título é achado no TMDB
//!   uma vez (o resultado fica guardado para as próximas importações).
//! - Crunchyroll, de uma vez: um botão de favoritos (na página de
//!   Preferências) lê o histórico da conta logada no crunchyroll.com, pelo
//!   próprio site, e baixa um arquivo que é enviado aqui.
//! - Crunchyroll daqui para a frente, via AniList: a extensão MAL-Sync registra o que se assiste na
//!   Crunchyroll numa conta do AniList, que tem API aberta; o Anubis lê a lista
//!   (status, nota, favoritos) e liga cada anime ao TMDB pela lista pública
//!   github.com/Fribb/anime-lists (AniList → TVDB/TMDB).
//!
//! Tudo fica em `state_dir/externo.json`; a lista de ids, em
//! `state_dir/anime-list-full.json` (renovada a cada 7 dias).

use crate::foryou::Media;
use crate::orchestrator::Orchestrator;
use anyhow::{Context, Result, bail};
use chrono::{NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{info, warn};

const ANILIST: &str = "https://graphql.anilist.co";
const ANIME_MAP_URL: &str = "https://raw.githubusercontent.com/Fribb/anime-lists/master/anime-list-full.json";
const ANIME_MAP_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const ANILIST_TTL: Duration = Duration::from_secs(3600);

/// Uma semente vinda de fora: título no TMDB, peso base e a data mais recente.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Seed {
    pub media: Media,
    pub tmdb: i64,
    pub title: String,
    pub base: f64,
    /// dias atrás (para a meia-vida); favorito não perde peso
    pub last: Option<NaiveDate>,
    #[serde(default)]
    pub favorite: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    netflix: Vec<Seed>,
    netflix_at: Option<String>,
    /// título da Netflix → (tipo, id no TMDB); None = não achado
    #[serde(default)]
    netflix_map: HashMap<String, Option<(Media, i64)>>,
    anilist_user: Option<String>,
    #[serde(default)]
    crunchyroll: Vec<Seed>,
    crunchyroll_at: Option<String>,
    /// título da Crunchyroll → (tipo, id no TMDB); None = não achado
    #[serde(default)]
    crunchyroll_map: HashMap<String, Option<(Media, i64)>>,
    /// id do TVDB → id do TMDB (séries de anime)
    #[serde(default)]
    tvdb_tmdb: HashMap<i64, Option<i64>>,
}

pub struct External {
    path: PathBuf,
    map_path: PathBuf,
    state: Mutex<State>,
    anilist: Mutex<Option<(Instant, Vec<Seed>)>>,
    /// id do AniList → ids no TMDB (série e/ou filme) e no TVDB
    anime_map: Mutex<Option<HashMap<i64, AnimeIds>>>,
}

#[derive(Debug, Clone, Copy, Default)]
struct AnimeIds {
    tmdb_tv: Option<i64>,
    tmdb_movie: Option<i64>,
    tvdb: Option<i64>,
}

/// Uma entrada da lista de ids: `themoviedb_id` é {"tv": N} ou {"movie": N}
/// (em versões antigas, só o número).
fn anime_ids(x: &Value) -> Option<(i64, AnimeIds)> {
    let tmdb = &x["themoviedb_id"];
    let movie_type = x["type"] == "MOVIE";
    let (tmdb_tv, tmdb_movie) = match tmdb.as_i64() {
        Some(n) if movie_type => (None, Some(n)),
        Some(n) => (Some(n), None),
        None => (tmdb["tv"].as_i64(), tmdb["movie"].as_i64()),
    };
    Some((x["anilist_id"].as_i64()?, AnimeIds { tmdb_tv, tmdb_movie, tvdb: x["tvdb_id"].as_i64().or(x["thetvdb_id"].as_i64()) }))
}

#[derive(Debug, Serialize)]
pub struct ImportReport {
    pub lines: usize,
    pub titles: usize,
    pub matched: usize,
    /// alguns títulos não achados no TMDB (para conferir)
    pub unmatched: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub netflix_titles: usize,
    pub netflix_at: Option<String>,
    pub crunchyroll_titles: usize,
    pub crunchyroll_at: Option<String>,
    pub anilist_user: Option<String>,
    /// entradas lidas do AniList na última vez (None: ainda não lido)
    pub anilist_entries: Option<usize>,
}

// ------------------------------------------------------------ Netflix

/// Linhas de um CSV de duas colunas com aspas ("Título","Data").
fn csv_rows(text: &str) -> Vec<(String, String)> {
    let mut out = vec![];
    for line in text.lines().skip(1) {
        let (mut fields, mut cur, mut quoted, mut chars) = (vec![], String::new(), false, line.chars().peekable());
        while let Some(c) = chars.next() {
            match c {
                '"' if quoted && chars.peek() == Some(&'"') => {
                    cur.push('"');
                    chars.next();
                }
                '"' => quoted = !quoted,
                ',' if !quoted => fields.push(std::mem::take(&mut cur)),
                _ => cur.push(c),
            }
        }
        fields.push(cur);
        if fields.len() >= 2 && !fields[0].trim().is_empty() {
            out.push((fields[0].trim().to_owned(), fields[1].trim().to_owned()));
        }
    }
    out
}

/// "29/09/26", "29/09/2026" (Brasil) ou "9/29/26" (EUA).
fn parse_date(s: &str) -> Option<NaiveDate> {
    let p: Vec<i32> = s.split(['/', '-', '.']).filter_map(|x| x.trim().parse().ok()).collect();
    let [a, b, y] = p[..] else { return None };
    let y = if y < 100 { 2000 + y } else { y };
    let (d, m) = if b > 12 { (b, a) } else { (a, b) };
    NaiveDate::from_ymd_opt(y, m as u32, d as u32)
}

const SERIES_MARKS: [&str; 16] = [
    "temporada", "season", "parte ", "part ", "minissérie", "série limitada", "limited series", "volume", "vol.", "livro", "book", "capítulo", "episódio",
    "episode", "coleção", "collection",
];

/// Episódio → série: "Série: Temporada 1: Episódio" e "Série: Série 4: Capítulo"
/// viram "Série"; o resto é filme (ou especial), título inteiro.
fn title_key(t: &str) -> (String, bool) {
    let parts: Vec<&str> = t.split(": ").collect();
    if parts.len() >= 3 {
        return (parts[0].to_owned(), true);
    }
    if parts.len() == 2 {
        let second = parts[1].to_lowercase();
        let numbered = parts[1].starts_with(parts[0]) && parts[1].rsplit(' ').next().is_some_and(|n| n.parse::<u32>().is_ok());
        if SERIES_MARKS.iter().any(|m| second.starts_with(m)) || numbered {
            return (parts[0].to_owned(), true);
        }
    }
    (t.to_owned(), false)
}

/// Títulos únicos: (chave, é série, vezes, data mais recente).
fn group(rows: &[(String, String)]) -> Vec<(String, bool, u32, Option<NaiveDate>)> {
    let mut m: HashMap<String, (bool, u32, Option<NaiveDate>)> = HashMap::new();
    for (t, d) in rows {
        let (key, series) = title_key(t);
        let e = m.entry(key).or_insert((series, 0, None));
        e.0 |= series;
        e.1 += 1;
        e.2 = e.2.max(parse_date(d));
    }
    let mut out: Vec<_> = m.into_iter().map(|(k, (s, n, d))| (k, s, n, d)).collect();
    out.sort_by(|a, b| b.3.cmp(&a.3));
    out
}

// ------------------------------------------------------------ AniList

/// Peso de uma entrada do AniList: status, nota (0–10) e episódios vistos.
fn anilist_weight(status: &str, score: f64, progress: i64) -> Option<f64> {
    let base = match status {
        "COMPLETED" => 2.0 + if score >= 8.0 { 1.0 } else { 0.0 },
        "REPEATING" => 2.5,
        "CURRENT" => 1.5 + (progress as f64 * 0.1).min(1.0),
        "PAUSED" => 1.0,
        "PLANNING" => 0.5,
        _ => return None, // DROPPED
    };
    Some(base)
}

impl External {
    pub fn load(state_dir: PathBuf) -> Self {
        let path = state_dir.join("externo.json");
        let state = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { map_path: state_dir.join("anime-list-full.json"), path, state: Mutex::new(state), anilist: Mutex::new(None), anime_map: Mutex::new(None) }
    }

    async fn save(&self) {
        let st = self.state.lock().await;
        let tmp = self.path.with_extension("tmp");
        let res = serde_json::to_vec_pretty(&*st).map_err(anyhow::Error::from).and_then(|b| Ok(std::fs::write(&tmp, b)?)).and_then(|_| Ok(std::fs::rename(&tmp, &self.path)?));
        if let Err(e) = res {
            warn!("não gravei {}: {e:#}", self.path.display());
        }
    }
}

impl Orchestrator {
    pub async fn history_summary(&self) -> Summary {
        let st = self.external.state.lock().await;
        let entries = self.external.anilist.lock().await.as_ref().map(|(_, s)| s.len());
        Summary {
            netflix_titles: st.netflix.len(),
            netflix_at: st.netflix_at.clone(),
            crunchyroll_titles: st.crunchyroll.len(),
            crunchyroll_at: st.crunchyroll_at.clone(),
            anilist_user: st.anilist_user.clone(),
            anilist_entries: entries,
        }
    }

    /// Importa o CSV da Netflix (substitui a importação anterior).
    pub async fn import_netflix(&self, csv: &str) -> Result<ImportReport> {
        let tmdb = self.tmdb.as_ref().context("configure [tmdb] para importar da Netflix")?;
        let rows = csv_rows(csv);
        if rows.is_empty() {
            bail!("arquivo vazio ou fora do formato (esperado: \"Title\",\"Date\")");
        }
        let titles = group(&rows);
        let mut seeds = vec![];
        let mut unmatched = vec![];
        for (key, series, count, last) in &titles {
            let known = self.external.state.lock().await.netflix_map.get(key).copied();
            let found = match known {
                Some(f) => f,
                None => {
                    let q = crate::services::url_encode(key);
                    let res = tmdb.get(&format!("search/multi?query={q}&language=pt-BR&include_adult=false")).await?;
                    let list = res["results"].as_array().cloned().unwrap_or_default();
                    let pick = |want: &str| list.iter().find(|r| r["media_type"] == want).and_then(|r| r["id"].as_i64());
                    let f = if *series { pick("tv").map(|id| (Media::Tv, id)) } else { pick("movie").map(|id| (Media::Movie, id)).or_else(|| pick("tv").map(|id| (Media::Tv, id))) };
                    self.external.state.lock().await.netflix_map.insert(key.clone(), f);
                    f
                }
            };
            match found {
                Some((media, id)) => {
                    let base = if media == Media::Movie { 2.0 } else { (1.0 + 0.25 * *count as f64).min(3.0) };
                    seeds.push(Seed { media, tmdb: id, title: key.clone(), base, last: *last, favorite: false });
                }
                None if unmatched.len() < 10 => unmatched.push(key.clone()),
                None => {}
            }
        }
        let report = ImportReport { lines: rows.len(), titles: titles.len(), matched: seeds.len(), unmatched };
        {
            let mut st = self.external.state.lock().await;
            st.netflix = seeds;
            st.netflix_at = Some(Utc::now().format("%Y-%m-%d").to_string());
        }
        self.external.save().await;
        self.for_you_cache.clear().await;
        info!("Netflix: {} linhas, {} títulos, {} achados no TMDB", report.lines, report.titles, report.matched);
        Ok(report)
    }

    /// Importa o arquivo do botão da Crunchyroll (substitui o anterior):
    /// `{"items": [{"title", "movie", "episodes", "last"}]}`.
    pub async fn import_crunchyroll(&self, body: &str) -> Result<ImportReport> {
        let tmdb = self.tmdb.as_ref().context("configure [tmdb] para importar da Crunchyroll")?;
        let v: Value = serde_json::from_str(body).context("arquivo fora do formato (é o crunchyroll-historico.json do botão?)")?;
        let items = v["items"].as_array().context("arquivo sem itens")?;
        let mut seeds = vec![];
        let mut unmatched = vec![];
        let mut episodes = 0usize;
        for it in items {
            let Some(title) = it["title"].as_str().map(str::trim).filter(|t| !t.is_empty()) else { continue };
            let movie = it["movie"] == true;
            let count = it["episodes"].as_u64().unwrap_or(1);
            episodes += count as usize;
            let last = it["last"].as_str().and_then(|d| d.get(..10)).and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
            let key = format!("{}{title}", if movie { "filme:" } else { "" });
            let known = self.external.state.lock().await.crunchyroll_map.get(&key).copied();
            let found = match known {
                Some(f) => f,
                None => {
                    let kind = if movie { "movie" } else { "tv" };
                    let res = tmdb.get(&format!("search/{kind}?query={}&language=pt-BR&include_adult=false", crate::services::url_encode(title))).await?;
                    let list = res["results"].as_array().cloned().unwrap_or_default();
                    // na Crunchyroll quase tudo é anime: prefere o resultado que é anime
                    let pick = list.iter().find(|r| movie || crate::discover::is_anime(r)).or(list.first()).and_then(|r| r["id"].as_i64());
                    let f = pick.map(|id| (if movie { Media::Movie } else { Media::Tv }, id));
                    self.external.state.lock().await.crunchyroll_map.insert(key.clone(), f);
                    f
                }
            };
            match found {
                Some((media, id)) => {
                    let base = if media == Media::Movie { 2.0 } else { (1.0 + 0.25 * count as f64).min(3.0) };
                    seeds.push(Seed { media, tmdb: id, title: title.to_owned(), base, last, favorite: false });
                }
                None if unmatched.len() < 10 => unmatched.push(title.to_owned()),
                None => {}
            }
        }
        let report = ImportReport { lines: episodes, titles: items.len(), matched: seeds.len(), unmatched };
        {
            let mut st = self.external.state.lock().await;
            st.crunchyroll = seeds;
            st.crunchyroll_at = Some(Utc::now().format("%Y-%m-%d").to_string());
        }
        self.external.save().await;
        self.for_you_cache.clear().await;
        info!("Crunchyroll: {} episódios, {} títulos, {} achados no TMDB", report.lines, report.titles, report.matched);
        Ok(report)
    }

    pub async fn clear_crunchyroll(&self) {
        let mut st = self.external.state.lock().await;
        st.crunchyroll.clear();
        st.crunchyroll_at = None;
        drop(st);
        self.external.save().await;
        self.for_you_cache.clear().await;
    }

    pub async fn clear_netflix(&self) {
        let mut st = self.external.state.lock().await;
        st.netflix.clear();
        st.netflix_at = None;
        drop(st);
        self.external.save().await;
        self.for_you_cache.clear().await;
    }

    /// Define (ou tira, com vazio) o usuário do AniList e já lê a lista.
    pub async fn set_anilist(&self, user: &str) -> Result<Summary> {
        let user = user.trim();
        let previous = self.external.state.lock().await.anilist_user.clone();
        self.external.state.lock().await.anilist_user = (!user.is_empty()).then(|| user.to_owned());
        *self.external.anilist.lock().await = None;
        // só grava se a lista for lida (usuário inexistente ou lista privada: volta ao anterior)
        if !user.is_empty()
            && let Err(e) = self.anilist_seeds().await
        {
            self.external.state.lock().await.anilist_user = previous;
            return Err(e.context("não li a lista do AniList (confira o nome e se a lista é pública)"));
        }
        self.external.save().await;
        self.for_you_cache.clear().await;
        Ok(self.history_summary().await)
    }

    /// Todas as sementes de fora (Netflix + AniList).
    pub async fn external_seeds(&self) -> Vec<Seed> {
        let mut out = {
            let st = self.external.state.lock().await;
            st.netflix.iter().chain(&st.crunchyroll).cloned().collect::<Vec<_>>()
        };
        match self.anilist_seeds().await {
            Ok(a) => out.extend(a),
            Err(e) => warn!("AniList: {e:#}"),
        }
        out
    }

    /// A lista do AniList como sementes (1 h em memória).
    async fn anilist_seeds(&self) -> Result<Vec<Seed>> {
        let Some(user) = self.external.state.lock().await.anilist_user.clone() else { return Ok(vec![]) };
        if let Some((at, s)) = &*self.external.anilist.lock().await
            && at.elapsed() < ANILIST_TTL
        {
            return Ok(s.clone());
        }
        let query = r#"query($u: String) {
            MediaListCollection(userName: $u, type: ANIME) { lists { entries { status score(format: POINT_10_DECIMAL) progress updatedAt media { id format title { romaji } } } } }
            User(name: $u) { favourites { anime(perPage: 50) { nodes { id } } } }
        }"#;
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
        let res: Value = http.post(ANILIST).json(&json!({ "query": query, "variables": { "u": user } })).send().await?.json().await?;
        if let Some(e) = res["errors"].as_array().and_then(|e| e.first()) {
            bail!("{}", e["message"].as_str().unwrap_or("erro do AniList"));
        }
        let favs: Vec<i64> = res["data"]["User"]["favourites"]["anime"]["nodes"].as_array().into_iter().flatten().filter_map(|n| n["id"].as_i64()).collect();
        let entries: Vec<Value> = res["data"]["MediaListCollection"]["lists"].as_array().into_iter().flatten().flat_map(|l| l["entries"].as_array().cloned().unwrap_or_default()).collect();
        let map = self.anime_map().await?;
        let mut seeds = vec![];
        for e in &entries {
            let Some(aid) = e["media"]["id"].as_i64() else { continue };
            let fav = favs.contains(&aid);
            let Some(base) = anilist_weight(e["status"].as_str().unwrap_or_default(), e["score"].as_f64().unwrap_or(0.0), e["progress"].as_i64().unwrap_or(0)) else {
                continue;
            };
            let Some(ids) = map.get(&aid).copied() else { continue };
            let movie = e["media"]["format"] == "MOVIE";
            let target = match (movie, ids.tmdb_movie, ids.tmdb_tv, ids.tvdb) {
                (true, Some(id), _, _) => Some((Media::Movie, id)),
                (false, _, Some(id), _) => Some((Media::Tv, id)),
                // sem id do TMDB na lista: pelo do TVDB
                (false, _, None, Some(tvdb)) => self.tmdb_of_tvdb(tvdb).await.map(|id| (Media::Tv, id)),
                _ => None,
            };
            let Some((media, id)) = target else { continue };
            let last = e["updatedAt"].as_i64().and_then(|t| chrono::DateTime::from_timestamp(t, 0)).map(|d| d.date_naive());
            let title = e["media"]["title"]["romaji"].as_str().unwrap_or_default().to_owned();
            seeds.push(Seed { media, tmdb: id, title, base: if fav { base.max(3.0) } else { base }, last, favorite: fav });
        }
        info!("AniList {user}: {} entradas, {} sementes", entries.len(), seeds.len());
        self.external.save().await; // guarda os ids do TMDB achados
        *self.external.anilist.lock().await = Some((Instant::now(), seeds.clone()));
        Ok(seeds)
    }

    /// Id do TMDB de uma série pelo do TVDB (guardado).
    async fn tmdb_of_tvdb(&self, tvdb: i64) -> Option<i64> {
        if let Some(v) = self.external.state.lock().await.tvdb_tmdb.get(&tvdb) {
            return *v;
        }
        let id = self.tmdb.as_ref()?.get(&format!("find/{tvdb}?external_source=tvdb_id")).await.ok()?["tv_results"][0]["id"].as_i64();
        self.external.state.lock().await.tvdb_tmdb.insert(tvdb, id);
        id
    }

    /// AniList → TMDB/TVDB, da lista pública (baixada a cada 7 dias).
    async fn anime_map(&self) -> Result<HashMap<i64, AnimeIds>> {
        if let Some(m) = &*self.external.anime_map.lock().await {
            return Ok(m.clone());
        }
        let path = &self.external.map_path;
        let fresh = std::fs::metadata(path).and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().unwrap_or_default() < ANIME_MAP_TTL);
        if !fresh {
            let http = reqwest::Client::builder().timeout(Duration::from_secs(120)).build()?;
            match http.get(ANIME_MAP_URL).send().await.and_then(|r| r.error_for_status()) {
                Ok(r) => {
                    let body = r.bytes().await?;
                    std::fs::write(path, &body).with_context(|| format!("gravando {}", path.display()))?;
                }
                // sem internet: segue com a cópia antiga, se houver
                Err(e) if path.exists() => warn!("lista de ids de anime não renovada: {e}"),
                Err(e) => return Err(e).context("baixando a lista de ids de anime"),
            }
        }
        let raw: Vec<Value> = serde_json::from_slice(&std::fs::read(path)?)?;
        let map: HashMap<i64, AnimeIds> = raw.iter().filter_map(anime_ids).collect();
        *self.external.anime_map.lock().await = Some(map.clone());
        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netflix_csv_becomes_titles() {
        let csv = "Title,Date\n\"Round 6: Temporada 1: Batata Quente\",\"17/09/26\"\n\"Round 6: Temporada 1: Luz Vermelha\",\"16/09/26\"\n\"Glass Onion: Um Mistério Knives Out\",\"05/09/26\"\n\"Stranger Things: Stranger Things 4: Capítulo Um\",\"01/08/26\"\n\"Wandinha: Temporada 2\",\"9/28/26\"\n\"O Poço\",\"\"\"x\"\"\"\n";
        let rows = csv_rows(csv);
        assert_eq!(rows.len(), 6);
        let g = group(&rows);
        let find = |k: &str| g.iter().find(|x| x.0 == k).unwrap();
        assert_eq!((find("Round 6").1, find("Round 6").2), (true, 2));
        assert_eq!(find("Round 6").3, NaiveDate::from_ymd_opt(2026, 9, 17));
        assert!(!find("Glass Onion: Um Mistério Knives Out").1);
        assert!(find("Stranger Things").1);
        assert_eq!(find("Wandinha").3, NaiveDate::from_ymd_opt(2026, 9, 28));
        assert_eq!(find("O Poço").3, None);
    }

    #[test]
    fn reads_both_id_list_formats() {
        let (aid, ids) = anime_ids(&json!({ "anilist_id": 154587, "type": "TV", "themoviedb_id": { "tv": 209867 }, "tvdb_id": 424536 })).unwrap();
        assert_eq!((aid, ids.tmdb_tv, ids.tmdb_movie, ids.tvdb), (154587, Some(209867), None, Some(424536)));
        let (_, ids) = anime_ids(&json!({ "anilist_id": 1, "type": "MOVIE", "themoviedb_id": 129 })).unwrap();
        assert_eq!((ids.tmdb_tv, ids.tmdb_movie), (None, Some(129)));
        assert!(anime_ids(&json!({ "type": "TV" })).is_none());
    }

    #[test]
    fn anilist_status_weights() {
        assert_eq!(anilist_weight("COMPLETED", 9.0, 12), Some(3.0));
        assert_eq!(anilist_weight("COMPLETED", 6.0, 12), Some(2.0));
        assert!((anilist_weight("CURRENT", 0.0, 3).unwrap() - 1.8).abs() < 1e-9);
        assert_eq!(anilist_weight("DROPPED", 10.0, 1), None);
    }
}
