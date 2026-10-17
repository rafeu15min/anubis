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
//! Os episódios vistos lá (Netflix: "Série: Temporada 1: Nome"; Crunchyroll:
//! temporada, número e nome) são casados com os do TMDB pelo nome (em
//! português ou inglês) ou pelo número, e a janela da série mostra
//! "assistido (Netflix)" (search.rs: `episodes`).
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

pub(crate) const ANILIST: &str = "https://graphql.anilist.co";
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
    /// gêneros no TMDB (ids do tipo: filme ou série); vazio se não se sabe
    #[serde(default)]
    pub genres: Vec<i64>,
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
    /// "Movie:123"/"Tv:123" → gêneros, vindos da busca na importação
    #[serde(default)]
    genres: HashMap<String, Vec<i64>>,
    /// "não quero ver": fora da vitrine, e o que se parece com eles desce
    #[serde(default)]
    hidden: Vec<Hidden>,
    /// "gostei": sementes do "Para você" com peso de favorito
    #[serde(default)]
    liked: Vec<Hidden>,
    /// episódios vistos fora: fonte ("netflix"/"crunchyroll") → série no
    /// TMDB → episódios
    #[serde(default)]
    watched: HashMap<String, HashMap<i64, Vec<WatchedEp>>>,
}

/// Um episódio visto fora, como veio da fonte.
#[derive(Debug, Clone, Default, PartialEq)]
struct RawEp {
    season: Option<i64>,
    number: Option<i64>,
    title: Option<String>,
    /// outro nome possível (Netflix: tudo depois da temporada, "Capítulo
    /// oito: Papai", quando `title` é só "Papai")
    alt: Option<String>,
    /// visto até o fim (a Netflix não diz: conta como visto)
    full: bool,
    /// parado no meio: quanto (Crunchyroll)
    percent: Option<u8>,
}

/// Um episódio visto fora, já com o número do TMDB e os nomes (para casar
/// com o Sonarr, que pode numerar diferente).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchedEp {
    pub season: i64,
    pub number: i64,
    /// os nomes no TMDB (português e inglês), comparáveis (`ep_norm`)
    #[serde(default)]
    pub names: Vec<String>,
    pub full: bool,
    #[serde(default)]
    pub percent: Option<u8>,
}

/// Nome de episódio comparável: só letras e números, minúsculas, sem acento.
pub fn ep_norm(s: &str) -> String {
    crate::gosto::fold(s).chars().filter(|c| c.is_alphanumeric()).collect()
}

/// Mesmo nome, ou um contém o outro sendo quase do mesmo tamanho ("Episódio
/// 2" não casa com "Episódio 29").
fn same_name(a: &str, b: &str) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if a == b {
        return short.len() >= 3;
    }
    // o que sobra do mais longo: só número é outro episódio ("2" e "29")
    let rest = long.replacen(short, "", 1);
    short.len() >= 8 && short.len() * 10 >= long.len() * 7 && long.contains(short) && !rest.chars().all(|c| c.is_ascii_digit())
}

/// Um "gostei" na página: marcado no Anubis, favorito no Jellyfin, ou os dois.
#[derive(Debug, Clone, Serialize)]
pub struct Liked {
    pub kind: crate::search::Kind,
    pub id: i64,
    pub title: String,
    /// quando foi marcado no Anubis
    pub at: Option<String>,
    pub anubis: bool,
    pub jellyfin: bool,
}

/// Tipo e id da página de um item do Jellyfin (filme: TMDB; série: TVDB).
fn page_id(item: &Value) -> Option<(crate::search::Kind, i64, String)> {
    let (kind, key) = match item["Type"].as_str()? {
        "Movie" => (crate::search::Kind::Movie, "Tmdb"),
        "Series" => (crate::search::Kind::Series, "Tvdb"),
        _ => return None,
    };
    let id = item["ProviderIds"][key].as_str()?.parse().ok()?;
    Some((kind, id, item["Name"].as_str().unwrap_or_default().to_owned()))
}

/// Um título marcado pela pessoa: "não quero ver" ou "gostei".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hidden {
    /// como a página o identifica (filme: id do TMDB; série: do TVDB)
    pub kind: crate::search::Kind,
    pub id: i64,
    pub media: Media,
    pub tmdb: i64,
    pub title: String,
    #[serde(default)]
    pub genres: Vec<i64>,
    pub at: String,
}

fn genre_key(media: Media, id: i64) -> String {
    format!("{media:?}:{id}")
}

pub struct External {
    path: PathBuf,
    map_path: PathBuf,
    state: Mutex<State>,
    anilist: Mutex<Option<(Instant, Vec<Seed>)>>,
    /// id do AniList → ids no TMDB (série e/ou filme) e no TVDB
    anime_map: Mutex<Option<(Instant, Arc<AnimeMap>)>>,
    progress: Mutex<Progress>,
}

/// AniList → ids no TMDB/TVDB (da lista pública Fribb).
type AnimeMap = HashMap<i64, AnimeIds>;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AnimeIds {
    pub(crate) tmdb_tv: Option<i64>,
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

impl Source {
    pub fn key(self) -> &'static str {
        match self {
            Source::Netflix => "netflix",
            Source::Crunchyroll => "crunchyroll",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Source::Netflix => "Netflix",
            Source::Crunchyroll => "Crunchyroll",
        }
    }
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
    /// os episódios vistos (séries)
    eps: Vec<RawEp>,
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

/// Número arábico ou romano ("3", "III").
fn number_word(w: &str) -> Option<i64> {
    w.parse().ok().or_else(|| {
        let r = ["i", "ii", "iii", "iv", "v", "vi", "vii", "viii", "ix", "x"];
        r.iter().position(|x| *x == w.to_lowercase()).map(|i| i as i64 + 1)
    })
}

/// "Temporada 2", "Season 2", "Livro 3", "Book III", "Parte 1": a temporada.
fn season_part(p: &str) -> Option<i64> {
    let lower = p.trim().to_lowercase();
    let mut w = lower.split_whitespace();
    let (mark, n) = (w.next()?, w.next()?);
    ["temporada", "season", "livro", "book", "parte", "part", "volume"].contains(&mark).then_some(())?;
    number_word(n.trim_end_matches(':'))
}

/// "Episódio 14", "Capítulo 02", "Episode 119": o número.
fn number_part(p: &str) -> Option<i64> {
    let lower = crate::gosto::fold(p);
    let words: Vec<&str> = lower.split_whitespace().collect();
    words.windows(2).find_map(|w| ["episodio", "episode", "capitulo", "chapter", "ep", "ep."].contains(&w[0]).then(|| w[1].trim_matches(|c: char| !c.is_ascii_digit()).parse().ok()).flatten())
}

/// O episódio de uma linha da Netflix: "Série: Temporada 2: Nome", "Série:
/// The Fighting!: Temporada 1: Episódio 29", "Série: Livro 3: Arco: Nome",
/// "Série: Alabasta: Episódio 119", "Série: Temporada 2: Capítulo 02: Nome".
fn netflix_episode(title: &str, key: &str) -> Option<RawEp> {
    let rest = title.strip_prefix(key)?.strip_prefix(": ")?;
    let parts: Vec<&str> = rest.split(": ").map(str::trim).filter(|p| !p.is_empty()).collect();
    // "Stranger Things 4": a temporada escrita com o nome da série
    let named_season = |p: &str| p.strip_prefix(key).and_then(|n| n.trim().parse::<i64>().ok());
    let season_at = parts.iter().position(|p| season_part(p).is_some() || named_season(p).is_some());
    let season = season_at.and_then(|i| season_part(parts[i]).or_else(|| named_season(parts[i])));
    let number = parts.iter().find_map(|p| number_part(p));
    // o nome: a última parte que não é temporada nem só o número
    let name = parts.iter().rev().find(|p| season_part(p).is_none() && named_season(p).is_none() && !(number_part(p).is_some() && p.split_whitespace().count() <= 2)).copied();
    // nome genérico ("Episódio 29"): só o número vale
    (name.is_some() || number.is_some()).then_some(())?;
    // tudo depois da temporada, quando são várias partes
    let after: Vec<&str> = parts[season_at.map_or(0, |i| i + 1)..].to_vec();
    let alt = (after.len() >= 2).then(|| after.join(": "));
    Some(RawEp { season, number, title: name.map(str::to_owned), alt, full: true, percent: None })
}

/// Títulos únicos: (chave, é série, vezes, data mais recente).
#[cfg(test)]
fn group(rows: &[(String, String)]) -> Vec<(String, bool, u32, Option<NaiveDate>)> {
    group_eps(rows).into_iter().map(|(k, s, n, d, _)| (k, s, n, d)).collect()
}

/// Um título único: (chave, é série, vezes, data mais recente, episódios vistos).
type Titulo = (String, bool, u32, Option<NaiveDate>, Vec<RawEp>);

/// Como `group`, com os episódios vistos de cada série.
fn group_eps(rows: &[(String, String)]) -> Vec<Titulo> {
    let dmy = day_first(rows);
    // "Fullmetal Alchemist: Os motivos dela": a Netflix nem sempre marca o
    // episódio; o mesmo prefixo em dois títulos diferentes é série
    let mut prefixes: HashMap<&str, HashSet<&str>> = HashMap::new();
    for (t, _) in rows {
        if let Some((p, _)) = t.split_once(": ") {
            prefixes.entry(p).or_default().insert(t);
        }
    }
    let mut m: HashMap<String, (bool, u32, Option<NaiveDate>, Vec<RawEp>)> = HashMap::new();
    for (t, d) in rows {
        let (key, series) = match t.split_once(": ") {
            Some((p, _)) if prefixes[p].len() >= 2 => (p.to_owned(), true),
            _ => title_key(t),
        };
        // título vazio ou só pontuação: não dá para achar
        if key.trim().chars().filter(|c| c.is_alphanumeric()).count() < 2 {
            continue;
        }
        let ep = if series { netflix_episode(t, &key) } else { None };
        let e = m.entry(key.trim().to_owned()).or_insert((series, 0, None, vec![]));
        e.0 |= series;
        e.1 += 1;
        e.2 = e.2.max(parse_date(d, dmy));
        if let Some(ep) = ep
            && !e.3.contains(&ep)
        {
            e.3.push(ep);
        }
    }
    let mut out: Vec<_> = m.into_iter().map(|(k, (s, n, d, eps))| (k, s, n, d, eps)).collect();
    out.sort_by(|a, b| b.3.cmp(&a.3));
    out
}

/// O título buscado entre os resultados (`media`: "tv" ou "movie"): o de nome
/// igual, sem contar pontuação; com vários (o anime de One Piece e a série de
/// 2023, o "Belle" do Hosoda e um de 2025), o mais votado. A busca do TMDB às
/// vezes põe na frente outro de nome parecido ("Hilda Furacão" para "Hilda").
fn best_named<'a>(list: &'a [Value], title: &str, media: &str) -> Option<&'a Value> {
    let norm = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect::<String>();
    let want = norm(title);
    let found: Vec<&Value> = list.iter().filter(|r| r["media_type"] == media).collect();
    let fields = if media == "tv" { ["name", "original_name"] } else { ["title", "original_title"] };
    let same_name = |r: &Value| fields.iter().any(|f| r[*f].as_str().is_some_and(|n| norm(n) == want));
    found
        .iter()
        .take(5)
        .filter(|r| same_name(r))
        .max_by_key(|r| r["vote_count"].as_u64().unwrap_or(0))
        .or(found.first())
        .copied()
}

/// Um título da Crunchyroll, série ou filme: o de nome igual (sem contar
/// pontuação), anime primeiro e o mais votado. Sem nome igual, o primeiro
/// anime com votos: com 3 episódios vistos ou mais, só série (o filme-resumo
/// de Kabaneri tem o nome em inglês e a série não); com 1 ou 2, filme também
/// ("Sword Art Online Progressive" → o filme Aria).
fn anime_named(list: &[Value], title: &str, episodes: u32) -> Option<(Media, i64)> {
    let norm = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect::<String>();
    let want = norm(title);
    let media = |r: &Value| match r["media_type"].as_str() {
        Some("tv") => Some(Media::Tv),
        Some("movie") => Some(Media::Movie),
        _ => None,
    };
    let anime = |r: &Value| {
        let animation = r["genre_ids"].as_array().is_some_and(|g| g.iter().any(|x| x == 16));
        animation && (r["original_language"] == "ja" || r["origin_country"].as_array().is_some_and(|c| c.iter().any(|x| x == "JP")))
    };
    let same_name = |r: &Value| ["name", "original_name", "title", "original_title"].iter().any(|f| r[*f].as_str().is_some_and(|n| norm(n) == want));
    let found: Vec<&Value> = list.iter().filter(|r| media(r).is_some()).collect();
    found
        .iter()
        .take(8)
        .filter(|r| same_name(r))
        .max_by_key(|r| (anime(r), r["vote_count"].as_u64().unwrap_or(0)))
        .or_else(|| {
            let allowed = |r: &&&Value| episodes <= 2 || media(r) == Some(Media::Tv);
            let voted = |r: &&&Value| r["vote_count"].as_u64().unwrap_or(0) >= 10;
            let anime = |r: &&&Value| anime(r);
            found
                .iter()
                .find(|r| allowed(r) && anime(r) && voted(r))
                .or_else(|| found.iter().find(|r| allowed(r) && anime(r)))
                .or_else(|| found.iter().find(|r| allowed(r)))
        })
        .and_then(|r| Some((media(r)?, r["id"].as_i64()?)))
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
        crate::disk::save_json_or_warn(&self.path, &*st).await;
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
        let wanted = group_eps(&rows)
            .into_iter()
            .map(|(key, series, count, last, eps)| Wanted { title: key.clone(), key, want: if series { Want::Series } else { Want::MovieOrSeries }, count, last, eps })
            .collect();
        self.start_import(Source::Netflix, wanted, rows.len()).await
    }

    /// Importa o arquivo do botão da Crunchyroll (substitui o anterior):
    /// `{"items": [{"title", "movie", "episodes", "last", "eps"}]}`; "eps"
    /// (o botão novo): `[{"s": temporada, "e": número, "t": nome, "done", "pct"}]`.
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
                let eps = it["eps"].as_array().into_iter().flatten().map(|e| RawEp {
                    season: e["s"].as_i64(),
                    number: e["e"].as_i64().or_else(|| e["e"].as_str().and_then(|n| n.parse().ok())),
                    title: e["t"].as_str().map(str::to_owned),
                    alt: None,
                    full: e["done"] == true,
                    percent: e["pct"].as_u64().map(|p| p.min(99) as u8),
                }).collect();
                Some(Wanted { key, title: title.to_owned(), want: if movie { Want::AnimeMovie } else { Want::Anime }, count, last, eps })
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
        let mut series_eps: Vec<(i64, Vec<RawEp>)> = vec![];
        while let Some(res) = set.join_next().await {
            let Ok((w, found)) = res else { continue };
            titles += 1;
            if let Some((Media::Tv, id)) = found
                && !w.eps.is_empty()
            {
                series_eps.push((id, w.eps.clone()));
            }
            match found {
                Some((media, id)) => {
                    let base = if media == Media::Movie { 2.0 } else { (1.0 + 0.25 * w.count as f64).min(3.0) };
                    let genres = self.external.state.lock().await.genres.get(&genre_key(media, id)).cloned().unwrap_or_default();
                    seeds.push(Seed { media, tmdb: id, title: w.title, base, last: w.last, favorite: false, genres });
                }
                None if unmatched.len() < 10 => unmatched.push(w.title),
                None => {}
            }
        }
        // os episódios vistos de cada série, com o número no TMDB
        let mut watched: HashMap<i64, Vec<WatchedEp>> = HashMap::new();
        let total_eps: usize = series_eps.iter().map(|(_, e)| e.len()).sum();
        for (id, eps) in series_eps {
            let found = self.match_episodes(id, &eps).await;
            watched.entry(id).or_default().extend(found);
        }
        let matched_eps: usize = watched.values().map(Vec::len).sum();
        info!("{source:?}: {matched_eps} de {total_eps} episódios casados com o TMDB");
        let report = ImportReport { lines, titles, matched: seeds.len(), unmatched };
        {
            let mut st = self.external.state.lock().await;
            let today = Some(Utc::now().format("%Y-%m-%d").to_string());
            match source {
                Source::Netflix => (st.netflix, st.netflix_at) = (seeds, today),
                Source::Crunchyroll => (st.crunchyroll, st.crunchyroll_at) = (seeds, today),
            }
            st.watched.insert(source.key().to_owned(), watched);
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
        // não achado da outra vez: tenta de novo (a busca pode ter melhorado)
        if let Some(Some(f)) = cached {
            return Some(f);
        }
        let tmdb = self.tmdb.as_ref()?;
        let q = crate::services::url_encode(&w.title);
        let path = match w.want {
            // (anime: a Crunchyroll lista filme como série de um episódio, então busca os dois)
            Want::Series | Want::MovieOrSeries | Want::Anime => format!("search/multi?query={q}&language=pt-BR&include_adult=false"),
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
        let named = |media| best_named(&list, asked, media).and_then(|r| r["id"].as_i64());
        let tv = || named("tv").map(|id| (Media::Tv, id));
        let found = match w.want {
            Want::Series => tv(),
            Want::MovieOrSeries => named("movie").map(|id| (Media::Movie, id)).or_else(tv),
            // na Crunchyroll quase tudo é anime: prefere o resultado que é anime
            Want::Anime => anime_named(&list, asked, w.count),
            Want::AnimeMovie => list.first().and_then(|r| r["id"].as_i64()).map(|id| (Media::Movie, id)),
        };
        let mut st = self.external.state.lock().await;
        if let Some((media, id)) = found {
            let kind = if media == Media::Movie { "movie" } else { "tv" };
            let hit = list.iter().find(|r| r["id"].as_i64() == Some(id) && (r["media_type"].is_null() || r["media_type"] == kind));
            let genres: Vec<i64> = hit.map(|r| r["genre_ids"].as_array().into_iter().flatten().filter_map(Value::as_i64).collect()).unwrap_or_default();
            st.genres.insert(genre_key(media, id), genres);
        }
        match source {
            Source::Netflix => st.netflix_map.insert(w.key.clone(), found),
            Source::Crunchyroll => st.crunchyroll_map.insert(w.key.clone(), found),
        };
        found
    }

    /// Os episódios de uma série no TMDB (temporada, número, nomes em
    /// português e inglês), das temporadas pedidas (None: todas).
    async fn tmdb_episodes(&self, id: i64, seasons: Option<&HashSet<i64>>) -> Vec<(i64, i64, Vec<String>)> {
        let Some(tmdb) = self.tmdb.as_ref() else { return vec![] };
        let all: Vec<i64> = match tmdb.get(&format!("tv/{id}")).await {
            Ok(v) => v["seasons"].as_array().into_iter().flatten().filter_map(|s| s["season_number"].as_i64()).collect(),
            Err(_) => return vec![],
        };
        let mut out = vec![];
        for s in all.into_iter().filter(|s| seasons.is_none_or(|w| w.contains(s))) {
            let mut names: HashMap<i64, Vec<String>> = HashMap::new();
            for lang in ["pt-BR", "en-US"] {
                if let Ok(v) = tmdb.get(&format!("tv/{id}/season/{s}?language={lang}")).await {
                    for e in v["episodes"].as_array().into_iter().flatten() {
                        let (Some(n), Some(name)) = (e["episode_number"].as_i64(), e["name"].as_str()) else { continue };
                        names.entry(n).or_default().push(ep_norm(name));
                    }
                }
            }
            let mut eps: Vec<(i64, Vec<String>)> = names.into_iter().collect();
            eps.sort_by_key(|e| e.0);
            out.extend(eps.into_iter().map(|(n, names)| (s, n, names)));
        }
        out
    }

    /// Casa os episódios vistos com os do TMDB: pelo nome (na temporada
    /// dita, se houver; senão em qualquer uma); sem nome que case, pelo número
    /// (e, número maior que a temporada, pelo número absoluto — anime).
    async fn match_episodes(&self, id: i64, eps: &[RawEp]) -> Vec<WatchedEp> {
        let need: Option<HashSet<i64>> = if eps.iter().all(|e| e.season.is_some() && e.title.is_some()) {
            Some(eps.iter().filter_map(|e| e.season).collect())
        } else {
            None
        };
        let mut tm = self.tmdb_episodes(id, need.as_ref()).await;
        // nomes que não acharam na temporada dita podem estar em outra
        let missing = need.is_some() && eps.iter().any(|e| {
            let t = e.title.as_deref().map(ep_norm).unwrap_or_default();
            let a = e.alt.as_deref().map(ep_norm).unwrap_or_default();
            !tm.iter().any(|(_, _, names)| names.iter().any(|n| same_name(n, &t) || same_name(n, &a)))
        });
        if missing {
            tm = self.tmdb_episodes(id, None).await;
        }
        let regular: Vec<&(i64, i64, Vec<String>)> = tm.iter().filter(|e| e.0 > 0).collect();
        let mut out: Vec<WatchedEp> = vec![];
        for e in eps {
            let t = e.title.as_deref().map(ep_norm).unwrap_or_default();
            let a = e.alt.as_deref().map(ep_norm).unwrap_or_default();
            let by_name = |season: Option<i64>| tm.iter().find(|(s, _, names)| season.is_none_or(|w| w == *s) && names.iter().any(|n| same_name(n, &t) || same_name(n, &a)));
            let hit = by_name(e.season)
                .or_else(|| by_name(None))
                .or_else(|| {
                    let n = e.number?;
                    // na temporada dita; sem ela (ou além do fim dela), o número absoluto
                    e.season.and_then(|s| tm.iter().find(|(ts, tn, _)| *ts == s && *tn == n)).or_else(|| regular.get((n - 1).max(0) as usize).copied())
                });
            let Some((s, n, names)) = hit else { continue };
            match out.iter_mut().find(|w| w.season == *s && w.number == *n) {
                Some(w) => {
                    w.full |= e.full;
                    w.percent = w.percent.max(e.percent);
                }
                None => out.push(WatchedEp { season: *s, number: *n, names: names.clone(), full: e.full, percent: e.percent }),
            }
        }
        out
    }

    /// Os episódios de uma série (id no TMDB) vistos na Netflix ou na Crunchyroll.
    pub async fn watched_outside(&self, tmdb: i64) -> Vec<(Source, WatchedEp)> {
        let st = self.external.state.lock().await;
        [Source::Netflix, Source::Crunchyroll]
            .into_iter()
            .flat_map(|src| st.watched.get(src.key()).and_then(|m| m.get(&tmdb)).cloned().unwrap_or_default().into_iter().map(move |w| (src, w)))
            .collect()
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
            seeds.push(Seed { media, tmdb: id, title, base: if fav { base.max(3.0) } else { base }, last, favorite: fav, genres: vec![] });
        }
        info!("AniList {user}: {} entradas, {} sementes", entries.len(), seeds.len());
        self.external.save().await; // guarda os ids do TMDB achados
        *self.external.anilist.lock().await = Some((Instant::now(), seeds.clone()));
        Ok(seeds)
    }

    /// Um título marcado ("gostei" ou "não quero ver"), com os gêneros do TMDB.
    async fn mark(&self, kind: crate::search::Kind, id: i64) -> Result<Hidden> {
        use crate::search::Kind;
        let (media, tmdb_id) = match kind {
            Kind::Movie => (Media::Movie, id),
            _ => (Media::Tv, self.tmdb_of_tvdb(id).await.context("série sem id no TMDB")?),
        };
        let tmdb = self.tmdb.as_ref().context("TMDB não configurado")?;
        let path = if media == Media::Movie { "movie" } else { "tv" };
        let v = tmdb.get(&format!("{path}/{tmdb_id}?language=pt-BR")).await?;
        Ok(Hidden {
            kind,
            id,
            media,
            tmdb: tmdb_id,
            title: v["title"].as_str().or(v["name"].as_str()).unwrap_or_default().to_owned(),
            genres: v["genres"].as_array().into_iter().flatten().filter_map(|g| g["id"].as_i64()).collect(),
            at: chrono::Local::now().format("%Y-%m-%d").to_string(),
        })
    }

    /// "Não quero ver": sai da vitrine e vira sinal contra (o que o TMDB
    /// recomenda a partir dele desce, e os gêneros dele pesam menos).
    pub async fn hide(&self, kind: crate::search::Kind, id: i64) -> Result<Hidden> {
        let h = self.mark(kind, id).await?;
        // não é mais favorito (no Anubis, abaixo, e no Jellyfin)
        self.jellyfin_favorite(kind, id, false).await;
        {
            let mut st = self.external.state.lock().await;
            st.hidden.retain(|x| (x.kind, x.id) != (kind, id));
            st.liked.retain(|x| (x.kind, x.id) != (kind, id));
            st.hidden.push(h.clone());
        }
        self.external.save().await;
        self.for_you_cache.clear().await;
        info!("não quero ver: {}", h.title);
        Ok(h)
    }

    pub async fn unhide(&self, kind: crate::search::Kind, id: i64) {
        self.external.state.lock().await.hidden.retain(|x| (x.kind, x.id) != (kind, id));
        self.external.save().await;
        self.for_you_cache.clear().await;
    }

    /// Os ocultos, do mais antigo ao mais recente.
    pub async fn hidden(&self) -> Vec<Hidden> {
        self.external.state.lock().await.hidden.clone()
    }

    /// "Gostei": vira semente do "Para você" com o peso de um favorito do
    /// Jellyfin (o que o TMDB recomenda a partir dele sobe, e os gêneros
    /// dele pesam mais). Sai dos ocultos, se estava lá. No Jellyfin, vira
    /// favorito (se ele tem o título).
    pub async fn like(&self, kind: crate::search::Kind, id: i64) -> Result<Hidden> {
        let h = self.mark(kind, id).await?;
        {
            let mut st = self.external.state.lock().await;
            st.liked.retain(|x| (x.kind, x.id) != (kind, id));
            st.hidden.retain(|x| (x.kind, x.id) != (kind, id));
            st.liked.push(h.clone());
        }
        self.external.save().await;
        self.for_you_cache.clear().await;
        self.jellyfin_favorite(kind, id, true).await;
        info!("gostei: {}", h.title);
        Ok(h)
    }

    /// Tira o "gostei" do Anubis e o favorito do Jellyfin.
    pub async fn unlike(&self, kind: crate::search::Kind, id: i64) {
        self.external.state.lock().await.liked.retain(|x| (x.kind, x.id) != (kind, id));
        self.external.save().await;
        self.for_you_cache.clear().await;
        self.jellyfin_favorite(kind, id, false).await;
    }

    /// Os "gostei" do Anubis (com gêneros, para o "Para você").
    pub async fn liked_marks(&self) -> Vec<Hidden> {
        self.external.state.lock().await.liked.clone()
    }

    /// Os "gostei" da página: os do Anubis e os favoritos do Jellyfin, juntos.
    pub async fn liked(&self) -> Vec<Liked> {
        let mut out: Vec<Liked> = self
            .liked_marks()
            .await
            .into_iter()
            .map(|h| Liked { kind: h.kind, id: h.id, title: h.title, at: Some(h.at), anubis: true, jellyfin: false })
            .collect();
        match self.jellyfin_favorites().await {
            Ok(favs) => {
                for (kind, id, title) in favs {
                    match out.iter_mut().find(|l| (l.kind, l.id) == (kind, id)) {
                        Some(l) => l.jellyfin = true,
                        None => out.push(Liked { kind, id, title, at: None, anubis: false, jellyfin: true }),
                    }
                }
            }
            Err(e) => warn!("favoritos do Jellyfin: {e:#}"),
        }
        out
    }

    /// Filmes e séries que o usuário marcou como favorito no Jellyfin, pelo id
    /// da página (filme: TMDB; série: TVDB).
    async fn jellyfin_favorites(&self) -> Result<Vec<(crate::search::Kind, i64, String)>> {
        let user = self.jellyfin_user().await?;
        let v = self.jellyfin.get_json(&format!("Users/{user}/Items?Recursive=true&IncludeItemTypes=Movie,Series&Filters=IsFavorite&Fields=ProviderIds")).await?;
        Ok(v["Items"].as_array().into_iter().flatten().filter_map(page_id).collect())
    }

    /// Liga ou desliga o favorito do título no Jellyfin (achado pelo id do
    /// TMDB/TVDB). Fora do Jellyfin ou com erro, só registra no log: o
    /// "gostei" do Anubis vale de qualquer jeito.
    async fn jellyfin_favorite(&self, kind: crate::search::Kind, id: i64, on: bool) {
        let res = async {
            let user = self.jellyfin_user().await?;
            let jf_kind = if kind == crate::search::Kind::Movie { "Movie" } else { "Series" };
            let v = self.jellyfin.get_json(&format!("Users/{user}/Items?Recursive=true&IncludeItemTypes={jf_kind}&Fields=ProviderIds")).await?;
            let Some(item) = v["Items"].as_array().into_iter().flatten().find(|i| page_id(i).is_some_and(|(k, x, _)| (k, x) == (kind, id))) else {
                return Ok(());
            };
            self.jellyfin.set_favorite(item["Id"].as_str().context("item sem id")?, &user, on).await
        };
        if let Err(e) = res.await {
            warn!("favorito no Jellyfin ({kind:?} {id}): {e:#}");
        }
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

    /// AniList → TMDB/TVDB, da lista pública (baixada a cada 7 dias, também
    /// no servidor que fica semanas ligado).
    pub(crate) async fn anime_map(&self) -> Result<Arc<AnimeMap>> {
        let loaded = self.external.anime_map.lock().await.as_ref().filter(|(at, _)| at.elapsed() < ANIME_MAP_TTL).map(|(_, m)| m.clone());
        if let Some(map) = loaded {
            return Ok(map);
        }
        self.refresh_anime_file().await?;
        let raw: Vec<Value> = serde_json::from_slice(&std::fs::read(&self.external.map_path)?)?;
        let map: Arc<AnimeMap> = Arc::new(raw.iter().filter_map(anime_ids).collect());
        *self.external.anime_map.lock().await = Some((Instant::now(), map.clone()));
        Ok(map)
    }

    /// Baixa a lista de ids se a cópia em disco venceu; sem internet, segue com a antiga, se houver.
    async fn refresh_anime_file(&self) -> Result<()> {
        let path = &self.external.map_path;
        let fresh = std::fs::metadata(path).and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().unwrap_or_default() < ANIME_MAP_TTL);
        if fresh {
            return Ok(());
        }
        let http = reqwest::Client::builder().timeout(Duration::from_secs(120)).build()?;
        let res = http.get(ANIME_MAP_URL).send().await.and_then(|r| r.error_for_status());
        let body = match res {
            Ok(r) => r.bytes().await?,
            Err(e) if path.exists() => {
                warn!("lista de ids de anime não renovada: {e}");
                return Ok(());
            }
            Err(e) => return Err(e).context("baixando a lista de ids de anime"),
        };
        std::fs::write(path, &body).with_context(|| format!("gravando {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn episodio_da_netflix() {
        let ep = |t: &str| netflix_episode(t, "Rick and Morty");
        let one_title = |t: &str| netflix_episode(t, "Vinland Saga").and_then(|e| e.title);
        assert_eq!(ep("Rick and Morty: Temporada 8: Hot Rick"), Some(RawEp { season: Some(8), title: Some("Hot Rick".into()), full: true, ..Default::default() }));
        let st = netflix_episode("Stranger Things: Stranger Things 4: Capítulo oito: Papai", "Stranger Things").unwrap();
        assert_eq!((st.season, st.title.as_deref(), st.alt.as_deref()), (Some(4), Some("Papai"), Some("Capítulo oito: Papai")));
        assert_eq!(ep("Rick and Morty: Parte 1: O começo").and_then(|e| e.title), Some("O começo".into()));
        let one = |t: &str, k: &str| netflix_episode(t, k).unwrap();
        let e = one("Hajime no Ippo: The Fighting!: Temporada 1: Episódio 29", "Hajime no Ippo");
        assert_eq!((e.season, e.number), (Some(1), Some(29)));
        let e = one("One Piece: Alabasta: Episódio 119", "One Piece");
        assert_eq!((e.season, e.number), (None, Some(119)));
        let e = one("Avatar: Livro 3: Sozin's Comet: Parte 4", "Avatar");
        assert_eq!(e.season, Some(3));
        let e = one("Wakfu: The Quest: Livro I : O começo", "Wakfu");
        assert_eq!(e.season, Some(1));
        let e = one("3%: Temporada 2: Capítulo 02: Torradeira", "3%");
        assert_eq!((e.season, e.number, e.title.as_deref()), (Some(2), Some(2), Some("Torradeira")));
        let rows: Vec<(String, String)> = [("Rick and Morty: Temporada 1: Piloto", "9/11/26"), ("Rick and Morty: Temporada 8: Hot Rick", "9/11/26"), ("Rick and Morty: Temporada 8: Hot Rick", "9/12/26")]
            .iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        let g = group_eps(&rows);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].4.len(), 2, "o mesmo episódio duas vezes conta uma");
        assert!(same_name(&ep_norm("Hot Rick"), &ep_norm("hot rick")));
        assert!(!same_name(&ep_norm("Rick"), &ep_norm("Hot Rick")));
        assert!(!same_name(&ep_norm("Episódio 2"), &ep_norm("Episódio 29")));
        assert_eq!(one_title("Vinland Saga: Temporada 1: Episódio 14"), None);
    }

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
    fn title_by_exact_name_then_votes() {
        let list = vec![
            json!({"media_type": "tv", "id": 111110, "name": "ONE PIECE: A Série", "original_name": "ONE PIECE", "vote_count": 1949}),
            json!({"media_type": "movie", "id": 900667, "title": "One Piece Film: Red", "vote_count": 1157}),
            json!({"media_type": "tv", "id": 37854, "name": "One Piece", "original_name": "ONE PIECE", "vote_count": 5567}),
        ];
        assert_eq!(best_named(&list, "One Piece", "tv").unwrap()["id"], 37854);
        let hilda = vec![
            json!({"media_type": "tv", "id": 72351, "name": "Hilda Furacão", "vote_count": 37}),
            json!({"media_type": "tv", "id": 82456, "name": "Hilda", "vote_count": 253}),
        ];
        assert_eq!(best_named(&hilda, "Hilda", "tv").unwrap()["id"], 82456);
        assert_eq!(best_named(&hilda, "Outra", "tv").unwrap()["id"], 72351);
        let belle = vec![
            json!({"media_type": "movie", "id": 1130863, "title": "Belle", "original_title": "Belle", "vote_count": 38}),
            json!({"media_type": "movie", "id": 776305, "title": "Belle", "original_title": "竜とそばかすの姫", "vote_count": 918}),
        ];
        assert_eq!(best_named(&belle, "Belle", "movie").unwrap()["id"], 776305);
        let your_name = vec![
            json!({"media_type": "movie", "id": 372058, "title": "Your Name.", "original_title": "君の名は。", "vote_count": 11000}),
            json!({"media_type": "movie", "id": 553301, "title": "Your Name", "original_title": "Your Name", "vote_count": 0}),
        ];
        assert_eq!(best_named(&your_name, "Your Name", "movie").unwrap()["id"], 372058);
    }

    #[test]
    fn crunchyroll_movie_listed_as_series_is_a_movie() {
        let list = vec![
            json!({"media_type": "tv", "id": 95479, "name": "Jujutsu Kaisen", "genre_ids": [16], "origin_country": ["JP"], "vote_count": 4000}),
            json!({"media_type": "movie", "id": 810693, "title": "Jujutsu Kaisen 0", "genre_ids": [16, 28], "original_language": "ja", "vote_count": 1500}),
        ];
        assert_eq!(anime_named(&list, "JUJUTSU KAISEN 0", 1), Some((Media::Movie, 810693)));
        assert_eq!(anime_named(&list, "JUJUTSU KAISEN", 20), Some((Media::Tv, 95479)));
        // série com nome em japonês e filme-resumo com o nome em inglês
        let kabaneri = vec![
            json!({"media_type": "movie", "id": 431803, "title": "Kabaneri of the Iron Fortress: Life That Burns", "genre_ids": [16], "original_language": "ja", "vote_count": 41}),
            json!({"media_type": "tv", "id": 65945, "name": "Kabaneri da Fortaleza de Ferro", "genre_ids": [16], "origin_country": ["JP"], "vote_count": 900}),
        ];
        assert_eq!(anime_named(&kabaneri, "Kabaneri of the Iron Fortress", 5), Some((Media::Tv, 65945)));
        let sao = vec![
            json!({"media_type": "person", "id": 1, "name": "Alguém"}),
            json!({"media_type": "movie", "id": 776655, "title": "Sword Art Online Progressive: Aria of a Starless Night", "genre_ids": [16], "original_language": "ja"}),
        ];
        assert_eq!(anime_named(&sao, "Sword Art Online Progressive", 1), Some((Media::Movie, 776655)));
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
