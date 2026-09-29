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
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
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
    progress: Mutex<Progress>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Netflix,
    Crunchyroll,
}

/// Como procurar um título no TMDB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Want {
    Series,
    /// título sem marca de episódio: filme, ou série se não houver filme
    MovieOrSeries,
    /// Crunchyroll: série que é quase sempre anime
    Anime,
    AnimeMovie,
}

/// Um título a achar no TMDB, com o quanto foi assistido.
#[derive(Debug, Clone)]
struct Wanted {
    key: String,
    title: String,
    want: Want,
    count: u32,
    last: Option<NaiveDate>,
}

/// Andamento da importação em segundo plano (a página acompanha).
#[derive(Debug, Clone, Default, Serialize)]
pub struct Progress {
    pub source: Option<Source>,
    pub running: bool,
    pub done: usize,
    pub total: usize,
    pub report: Option<ImportReport>,
}

#[derive(Debug, Clone, Serialize)]
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
    /// importação em andamento ou a última (Netflix/Crunchyroll)
    pub import: Progress,
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

fn date_parts(s: &str) -> Option<[i32; 3]> {
    let p: Vec<i32> = s.split(['/', '-', '.']).filter_map(|x| x.trim().parse().ok()).collect();
    p.try_into().ok()
}

/// O arquivo inteiro diz o formato: algum primeiro número > 12 = dia/mês
/// (Brasil); algum segundo > 12 = mês/dia (EUA, "9/28/26"). Sem pista, Brasil.
fn day_first(rows: &[(String, String)]) -> bool {
    let parts: Vec<[i32; 3]> = rows.iter().filter_map(|(_, d)| date_parts(d)).collect();
    if parts.iter().any(|p| p[0] > 12) {
        true
    } else {
        !parts.iter().any(|p| p[1] > 12)
    }
}

fn parse_date(s: &str, day_first: bool) -> Option<NaiveDate> {
    let [a, b, y] = date_parts(s)?;
    let y = if y < 100 { 2000 + y } else { y };
    let (d, m) = if day_first { (a, b) } else { (b, a) };
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
    let dmy = day_first(rows);
    // "Fullmetal Alchemist: Os motivos dela": a Netflix nem sempre marca o
    // episódio; o mesmo prefixo em dois títulos diferentes é série
    let mut prefixes: HashMap<&str, HashSet<&str>> = HashMap::new();
    for (t, _) in rows {
        if let Some((p, _)) = t.split_once(": ") {
            prefixes.entry(p).or_default().insert(t);
        }
    }
    let mut m: HashMap<String, (bool, u32, Option<NaiveDate>)> = HashMap::new();
    for (t, d) in rows {
        let (key, series) = match t.split_once(": ") {
            Some((p, _)) if prefixes[p].len() >= 2 => (p.to_owned(), true),
            _ => title_key(t),
        };
        // título vazio ou só pontuação: não dá para achar
        if key.trim().chars().filter(|c| c.is_alphanumeric()).count() < 2 {
            continue;
        }
        let e = m.entry(key.trim().to_owned()).or_insert((series, 0, None));
        e.0 |= series;
        e.1 += 1;
        e.2 = e.2.max(parse_date(d, dmy));
    }
    let mut out: Vec<_> = m.into_iter().map(|(k, (s, n, d))| (k, s, n, d)).collect();
    out.sort_by(|a, b| b.3.cmp(&a.3));
    out
}

/// A série buscada entre os resultados: a de nome exato; com várias (o anime
/// de One Piece e a de 2023), a mais votada. A busca do TMDB às vezes põe
/// na frente outra de nome parecido ("Hilda Furacão" para "Hilda").
fn series_named<'a>(list: &'a [Value], title: &str) -> Option<&'a Value> {
    let tv: Vec<&Value> = list.iter().filter(|r| r["media_type"] == "tv").collect();
    let same_name = |r: &Value| [&r["name"], &r["original_name"]].iter().any(|n| n.as_str().is_some_and(|n| n.eq_ignore_ascii_case(title)));
    tv.iter()
        .take(5)
        .filter(|r| same_name(r))
        .max_by_key(|r| r["vote_count"].as_u64().unwrap_or(0))
        .or(tv.first())
        .copied()
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
        Self { map_path: state_dir.join("anime-list-full.json"), path, state: Mutex::new(state), anilist: Mutex::new(None), anime_map: Mutex::new(None), progress: Mutex::new(Progress::default()) }
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
            import: self.external.progress.lock().await.clone(),
        }
    }

    /// Importa o CSV da Netflix (substitui a importação anterior). Responde
    /// logo com quantos títulos há; a busca no TMDB segue em segundo plano.
    pub async fn import_netflix(self: &Arc<Self>, csv: &str) -> Result<usize> {
        let rows = csv_rows(csv);
        if rows.is_empty() {
            bail!("arquivo vazio ou fora do formato (esperado: \"Title\",\"Date\")");
        }
        let wanted = group(&rows)
            .into_iter()
            .map(|(key, series, count, last)| Wanted { title: key.clone(), key, want: if series { Want::Series } else { Want::MovieOrSeries }, count, last })
            .collect();
        self.start_import(Source::Netflix, wanted, rows.len()).await
    }

    /// Importa o arquivo do botão da Crunchyroll (substitui o anterior):
    /// `{"items": [{"title", "movie", "episodes", "last"}]}`.
    pub async fn import_crunchyroll(self: &Arc<Self>, body: &str) -> Result<usize> {
        let v: Value = serde_json::from_str(body).context("arquivo fora do formato (é o crunchyroll-historico.json do botão?)")?;
        let items = v["items"].as_array().context("arquivo sem itens")?;
        let mut episodes = 0usize;
        let wanted: Vec<Wanted> = items
            .iter()
            .filter_map(|it| {
                let title = it["title"].as_str().map(str::trim).filter(|t| !t.is_empty())?;
                let movie = it["movie"] == true;
                let count = it["episodes"].as_u64().unwrap_or(1) as u32;
                episodes += count as usize;
                let last = it["last"].as_str().and_then(|d| d.get(..10)).and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
                let key = format!("{}{title}", if movie { "filme:" } else { "" });
                Some(Wanted { key, title: title.to_owned(), want: if movie { Want::AnimeMovie } else { Want::Anime }, count, last })
            })
            .collect();
        self.start_import(Source::Crunchyroll, wanted, episodes).await
    }

    async fn start_import(self: &Arc<Self>, source: Source, wanted: Vec<Wanted>, lines: usize) -> Result<usize> {
        self.tmdb.as_ref().context("configure [tmdb] para importar históricos")?;
        {
            let mut p = self.external.progress.lock().await;
            if p.running {
                bail!("já há uma importação em andamento — espere terminar");
            }
            *p = Progress { source: Some(source), running: true, done: 0, total: wanted.len(), report: None };
        }
        let total = wanted.len();
        let me = self.clone();
        tokio::spawn(async move { me.run_import(source, wanted, lines).await });
        Ok(total)
    }

    /// Acha os títulos no TMDB (8 de cada vez; o que já foi achado antes vem
    /// da memória) e grava as sementes.
    async fn run_import(self: Arc<Self>, source: Source, wanted: Vec<Wanted>, lines: usize) {
        let sem = Arc::new(tokio::sync::Semaphore::new(8));
        let mut set = tokio::task::JoinSet::new();
        for w in wanted {
            let (me, sem) = (self.clone(), sem.clone());
            set.spawn(async move {
                let _slot = sem.acquire_owned().await;
                let found = me.resolve(source, &w).await;
                me.external.progress.lock().await.done += 1;
                (w, found)
            });
        }
        let (mut seeds, mut unmatched, mut titles) = (vec![], vec![], 0usize);
        while let Some(res) = set.join_next().await {
            let Ok((w, found)) = res else { continue };
            titles += 1;
            match found {
                Some((media, id)) => {
                    let base = if media == Media::Movie { 2.0 } else { (1.0 + 0.25 * w.count as f64).min(3.0) };
                    seeds.push(Seed { media, tmdb: id, title: w.title, base, last: w.last, favorite: false });
                }
                None if unmatched.len() < 10 => unmatched.push(w.title),
                None => {}
            }
        }
        let report = ImportReport { lines, titles, matched: seeds.len(), unmatched };
        {
            let mut st = self.external.state.lock().await;
            let today = Some(Utc::now().format("%Y-%m-%d").to_string());
            match source {
                Source::Netflix => (st.netflix, st.netflix_at) = (seeds, today),
                Source::Crunchyroll => (st.crunchyroll, st.crunchyroll_at) = (seeds, today),
            }
        }
        self.external.save().await;
        self.for_you_cache.clear().await;
        info!("{source:?}: {} linhas, {} títulos, {} achados no TMDB", report.lines, report.titles, report.matched);
        let mut p = self.external.progress.lock().await;
        p.running = false;
        p.report = Some(report);
    }

    /// Um título no TMDB (guardado por fonte; falha de rede tenta de novo uma
    /// vez e, persistindo, fica como não achado sem ser guardado).
    async fn resolve(&self, source: Source, w: &Wanted) -> Option<(Media, i64)> {
        let cached = {
            let st = self.external.state.lock().await;
            match source {
                Source::Netflix => st.netflix_map.get(&w.key).copied(),
                Source::Crunchyroll => st.crunchyroll_map.get(&w.key).copied(),
            }
        };
        if let Some(f) = cached {
            return f;
        }
        let tmdb = self.tmdb.as_ref()?;
        let q = crate::services::url_encode(&w.title);
        let path = match w.want {
            Want::Series | Want::MovieOrSeries => format!("search/multi?query={q}&language=pt-BR&include_adult=false"),
            Want::Anime => format!("search/tv?query={q}&language=pt-BR&include_adult=false"),
            Want::AnimeMovie => format!("search/movie?query={q}&language=pt-BR&include_adult=false"),
        };
        let mut res = tmdb.get(&path).await;
        if res.is_err() {
            tokio::time::sleep(Duration::from_secs(2)).await;
            res = tmdb.get(&path).await;
        }
        let res = res.ok()?;
        let mut list = res["results"].as_array().cloned().unwrap_or_default();
        let mut asked = w.title.as_str();
        // "Série: episódio" visto uma vez só: sem filme com esse nome, tenta a série
        if list.is_empty()
            && w.want == Want::MovieOrSeries
            && let Some((prefix, _)) = w.title.split_once(": ")
        {
            asked = prefix;
            let q = crate::services::url_encode(prefix);
            let res = tmdb.get(&format!("search/tv?query={q}&language=pt-BR&include_adult=false")).await.ok()?;
            list = res["results"].as_array().cloned().unwrap_or_default();
            for r in &mut list {
                r["media_type"] = "tv".into();
            }
        }
        let of = |want: &str| list.iter().find(|r| r["media_type"] == want).and_then(|r| r["id"].as_i64());
        let tv = || series_named(&list, asked).and_then(|r| r["id"].as_i64()).map(|id| (Media::Tv, id));
        let found = match w.want {
            Want::Series => tv(),
            Want::MovieOrSeries => of("movie").map(|id| (Media::Movie, id)).or_else(tv),
            // na Crunchyroll quase tudo é anime: prefere o resultado que é anime
            Want::Anime => list.iter().find(|r| crate::discover::is_anime(r)).or(list.first()).and_then(|r| r["id"].as_i64()).map(|id| (Media::Tv, id)),
            Want::AnimeMovie => list.first().and_then(|r| r["id"].as_i64()).map(|id| (Media::Movie, id)),
        };
        let mut st = self.external.state.lock().await;
        match source {
            Source::Netflix => st.netflix_map.insert(w.key.clone(), found),
            Source::Crunchyroll => st.crunchyroll_map.insert(w.key.clone(), found),
        };
        found
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
        let csv = "Title,Date\n\"Round 6: Temporada 1: Batata Quente\",\"17/09/26\"\n\"Round 6: Temporada 1: Luz Vermelha\",\"16/09/26\"\n\"Glass Onion: Um Mistério Knives Out\",\"05/09/26\"\n\"Stranger Things: Stranger Things 4: Capítulo Um\",\"01/08/26\"\n\"Wandinha: Temporada 2\",\"28/09/26\"\n\"O Poço\",\"\"\"x\"\"\"\n\": \",\"01/01/26\"\n";
        let rows = csv_rows(csv);
        assert_eq!(rows.len(), 7);
        let g = group(&rows);
        let find = |k: &str| g.iter().find(|x| x.0 == k).unwrap();
        assert_eq!((find("Round 6").1, find("Round 6").2), (true, 2));
        assert_eq!(find("Round 6").3, NaiveDate::from_ymd_opt(2026, 9, 17));
        assert!(!find("Glass Onion: Um Mistério Knives Out").1);
        assert!(find("Stranger Things").1);
        assert_eq!(find("Wandinha").3, NaiveDate::from_ymd_opt(2026, 9, 28));
        assert_eq!(find("O Poço").3, None);
        // título vazio fica de fora
        assert_eq!(g.len(), 5);
        let eps = vec![
            ("Fullmetal Alchemist: Os motivos dela".to_owned(), "01/08/26".to_owned()),
            ("Fullmetal Alchemist: Pecado interior".to_owned(), "02/08/26".to_owned()),
            ("Glass Onion: Um Mistério Knives Out".to_owned(), "05/09/26".to_owned()),
        ];
        let g = group(&eps);
        assert!(g.contains(&("Fullmetal Alchemist".to_owned(), true, 2, NaiveDate::from_ymd_opt(2026, 8, 2))));
        assert!(g.iter().any(|x| x.0 == "Glass Onion: Um Mistério Knives Out" && !x.1));
    }

    #[test]
    fn series_by_exact_name_then_votes() {
        let list = vec![
            json!({"media_type": "tv", "id": 111110, "name": "ONE PIECE: A Série", "original_name": "ONE PIECE", "vote_count": 1949}),
            json!({"media_type": "movie", "id": 900667, "title": "One Piece Film: Red", "vote_count": 1157}),
            json!({"media_type": "tv", "id": 37854, "name": "One Piece", "original_name": "ONE PIECE", "vote_count": 5567}),
        ];
        assert_eq!(series_named(&list, "One Piece").unwrap()["id"], 37854);
        let hilda = vec![
            json!({"media_type": "tv", "id": 72351, "name": "Hilda Furacão", "vote_count": 37}),
            json!({"media_type": "tv", "id": 82456, "name": "Hilda", "vote_count": 253}),
        ];
        assert_eq!(series_named(&hilda, "Hilda").unwrap()["id"], 82456);
        assert_eq!(series_named(&hilda, "Outra").unwrap()["id"], 72351);
    }

    #[test]
    fn date_format_comes_from_the_whole_file() {
        let us = vec![("a".to_owned(), "9/11/26".to_owned()), ("b".to_owned(), "9/28/26".to_owned())];
        assert!(!day_first(&us));
        assert_eq!(parse_date("9/11/26", false), NaiveDate::from_ymd_opt(2026, 9, 11));
        let br = vec![("a".to_owned(), "9/11/26".to_owned()), ("b".to_owned(), "28/09/26".to_owned())];
        assert!(day_first(&br));
        assert_eq!(parse_date("9/11/26", true), NaiveDate::from_ymd_opt(2026, 11, 9));
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
