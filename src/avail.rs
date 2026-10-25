//! O que as fontes têm de cada título da vitrine: uma busca prévia nos
//! indexadores (Prowlarr) diz, para cada título do TMDB,
//! - dublado: alguma versão com áudio em português;
//! - só legendado: nenhuma em português, mas alguma no idioma original (a
//!   página põe o aviso amarelo);
//! - sem fonte: nada com fontes (a página apaga o cartão e o põe no fim);
//! - "não sei" (nunca checado, indexador fora do ar, busca que falhou): a
//!   conferir, checado de novo em 1 h.
//!
//! Nada some da vitrine por isso: é um resumo do que dá para baixar.
//! Dublagem num terceiro idioma não conta (sem português, só o original).
//! As buscas levam de 6 a 30 s cada, então rodam em segundo plano, uma por
//! vez e com pausa (ver `GAP`), e o resultado fica guardado em
//! `state_dir/disponibilidade.json`. A aba aberta e o título aberto passam
//! na frente da fila.

use crate::orchestrator::Orchestrator;
use crate::search::Kind;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use tracing::{info, warn};

/// Ritmo: um título por vez, com pausa entre eles, e só uma página de cada
/// site — duas checagens juntas com 100 resultados (5 páginas do 1337x por
/// busca) fizeram o Cloudflare do 1337x bloquear o IP do servidor em minutos.
/// (indexador de castigo não precisa de pausa aqui: o Prowlarr já não o consulta)
const GAP: std::time::Duration = std::time::Duration::from_secs(15);
/// O 1337x fica fora da checagem (é o que bloqueia o IP); as outras fontes
/// bastam para saber se o título existe. O Radarr/Sonarr e o "Assistir"
/// continuam usando ele.
pub(crate) fn skipped(indexer: &str) -> bool {
    indexer.to_lowercase().contains("1337x")
}
/// versões de fora do Brasil conferidas pelo `parse` do Radarr/Sonarr, no máximo
const MAX_PARSE: usize = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Avail {
    Dubbed,
    Subbed,
    Nothing,
    Unknown,
}

impl Avail {
    /// até quando o resultado vale
    fn ttl(self) -> Duration {
        match self {
            // dublado raramente some; o legendado pode ganhar dublagem
            Avail::Dubbed => Duration::days(7),
            Avail::Subbed => Duration::days(3),
            Avail::Nothing => Duration::days(1),
            Avail::Unknown => Duration::hours(1),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    avail: Avail,
    until: DateTime<Utc>,
}

/// O que é preciso para checar um título (vem da lista do TMDB).
#[derive(Debug, Clone)]
pub struct Want {
    pub kind: Kind,
    /// tmdbId (filme) ou tvdbId (série), como na vitrine
    pub id: i64,
    pub tmdb: i64,
    /// nome em português e o original
    pub names: Vec<String>,
    pub year: Option<i64>,
    /// código ISO do TMDB ("ja", "en")
    pub original_language: Option<String>,
}

impl Want {
    pub fn from_tmdb(kind: Kind, id: i64, v: &Value) -> Option<Self> {
        let title = v["title"].as_str().or(v["name"].as_str())?;
        let original = v["original_title"].as_str().or(v["original_name"].as_str()).unwrap_or_default();
        let date = v["release_date"].as_str().or(v["first_air_date"].as_str()).unwrap_or_default();
        Some(Want {
            kind,
            id,
            tmdb: v["id"].as_i64()?,
            names: [title, original].into_iter().filter(|n| !n.is_empty()).map(str::to_owned).collect(),
            year: date.get(..4).and_then(|y| y.parse().ok()),
            original_language: v["original_language"].as_str().map(str::to_owned),
        })
    }
}

pub struct Checker {
    path: PathBuf,
    map: Mutex<HashMap<String, Entry>>,
    queue: Mutex<VecDeque<Want>>,
    wake: Notify,
    /// título aberto na página: a pausa entre checagens acaba antes
    urgent: Notify,
    /// cada checagem terminada (quem espera um título acorda e confere)
    done: Notify,
    /// o que está sendo checado agora
    current: Mutex<Option<(Kind, i64)>>,
    /// duração média de uma checagem, em segundos (para a estimativa)
    avg: Mutex<f64>,
}

fn key(kind: Kind, id: i64) -> String {
    format!("{}:{id}", if kind == Kind::Movie { "movie" } else { "series" })
}

impl Checker {
    pub fn load(state_dir: PathBuf) -> Self {
        let path = state_dir.join("disponibilidade.json");
        let map = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { path, map: Mutex::new(map), queue: Mutex::new(VecDeque::new()), wake: Notify::new(), urgent: Notify::new(), done: Notify::new(), current: Mutex::new(None), avg: Mutex::new(10.0) }
    }

    /// O que se sabe do título (mesmo vencido: continua valendo até a nova checagem).
    pub async fn get(&self, kind: Kind, id: i64) -> Option<Avail> {
        self.map.lock().await.get(&key(kind, id)).map(|e| e.avail)
    }

    /// Põe na fila se nunca foi checado ou se o resultado venceu.
    pub async fn want(&self, w: Want) {
        let fresh = self.map.lock().await.get(&key(w.kind, w.id)).is_some_and(|e| e.until > Utc::now());
        if fresh {
            return;
        }
        let mut q = self.queue.lock().await;
        if !q.iter().any(|x| x.kind == w.kind && x.id == w.id) {
            q.push_back(w);
            self.wake.notify_one();
        }
    }

    /// Títulos esperando a checagem.
    pub async fn pending(&self) -> usize {
        self.queue.lock().await.len()
    }

    /// Quanto falta para a fila acabar, em segundos.
    pub async fn eta(&self) -> u64 {
        let per = *self.avg.lock().await + GAP.as_secs_f64();
        (self.pending().await as f64 * per).round() as u64
    }

    /// Tudo o que se sabe ("movie:603" → dublado…), para a página atualizar os cartões.
    pub async fn states(&self) -> HashMap<String, Avail> {
        self.map.lock().await.iter().map(|(k, e)| (k.clone(), e.avail)).collect()
    }

    /// Passa os títulos (os que estão na fila) para a frente, nessa ordem.
    pub async fn prioritize(&self, keys: &[(Kind, i64)]) {
        let mut q = self.queue.lock().await;
        let mut front = vec![];
        for (kind, id) in keys {
            if let Some(i) = q.iter().position(|w| w.kind == *kind && w.id == *id) {
                front.extend(q.remove(i));
            }
        }
        for w in front.into_iter().rev() {
            q.push_front(w);
        }
    }

    /// Checa o título antes dos outros e espera o resultado (até `limit`).
    /// Fora da fila, devolve o que já se sabe.
    pub async fn check_now(&self, kind: Kind, id: i64, limit: std::time::Duration) -> Option<Avail> {
        self.prioritize(&[(kind, id)]).await;
        self.urgent.notify_one();
        let queued = |q: &VecDeque<Want>| q.iter().any(|w| w.kind == kind && w.id == id);
        let wait = async {
            loop {
                let next = self.done.notified();
                tokio::pin!(next);
                next.as_mut().enable();
                if !queued(&*self.queue.lock().await) && !self.checking(kind, id).await {
                    return;
                }
                next.await;
            }
        };
        let _ = tokio::time::timeout(limit, wait).await;
        self.get(kind, id).await
    }

    /// O título está sendo checado agora (já saiu da fila, sem resultado ainda).
    async fn checking(&self, kind: Kind, id: i64) -> bool {
        self.current.lock().await.is_some_and(|c| c == (kind, id))
    }

    async fn record(&self, w: &Want, avail: Avail) {
        self.set(w.kind, w.id, avail).await;
    }

    /// Grava o que se sabe do título (também o resumo dos episódios de uma série).
    pub(crate) async fn set(&self, kind: Kind, id: i64, avail: Avail) {
        let mut map = self.map.lock().await;
        let k = key(kind, id);
        // "não sei" não apaga o que já se sabia: só checa de novo mais cedo
        let kept = match (avail, map.get(&k)) {
            (Avail::Unknown, Some(old)) if old.avail != Avail::Unknown => old.avail,
            _ => avail,
        };
        map.insert(k, Entry { avail: kept, until: Utc::now() + avail.ttl() });
        crate::disk::save_json_or_warn(&self.path, &*map).await;
    }
}

/// Minúsculas, sem acento, em palavras: "Coração.Valente 1995" → ["coracao", "valente", "1995"].
fn words(s: &str) -> Vec<String> {
    let fold = |c: char| match c {
        'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
        'é' | 'è' | 'ê' | 'ë' => 'e',
        'í' | 'ì' | 'î' | 'ï' => 'i',
        'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
        'ú' | 'ù' | 'û' | 'ü' => 'u',
        'ç' => 'c',
        'ñ' => 'n',
        c => c,
    };
    s.to_lowercase().chars().map(fold).collect::<String>().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_owned).collect()
}

/// A versão é deste título? O nome (algum deles) aparece inteiro, palavra por
/// palavra, e — em filme — o ano do nome da versão, se houver, bate (±1).
fn title_matches(names: &[String], year: Option<i64>, release: &str) -> bool {
    let rel = words(release);
    let named = names.iter().map(|n| words(n)).filter(|n| !n.is_empty()).any(|n| rel.windows(n.len()).any(|w| w == n.as_slice()));
    if !named {
        return false;
    }
    let Some(year) = year else { return true };
    let years: Vec<i64> = rel.iter().filter(|w| w.len() == 4).filter_map(|w| w.parse().ok()).filter(|y| (1900..2100).contains(y)).collect();
    years.is_empty() || years.iter().any(|y| (y - year).abs() <= 1)
}

/// O texto da busca: palavras sem pontuação nem acento — a busca dos sites
/// brasileiros não acha "Bob Esponja: O Incrível Resgate 2024", mas acha
/// "Bob Esponja O Incrivel Resgate". O ano só vai no nome original (os sites
/// de fora aceitam); o filtro do ano vale para os resultados de todos.
pub(crate) fn query(name: &str, year: Option<i64>) -> String {
    let q = words(name).join(" ");
    year.map_or(q.clone(), |y| format!("{q} {y}"))
}

/// O nome romaji nos nomes alternativos do TMDB (`results` em séries,
/// `titles` em filmes): "Sousou no Frieren".
pub(crate) fn romaji_in(v: &Value) -> Option<String> {
    let list = v["results"].as_array().or(v["titles"].as_array())?;
    list.iter()
        .filter(|t| t["type"].as_str().is_some_and(|k| k.eq_ignore_ascii_case("romaji")))
        .filter_map(|t| t["title"].as_str())
        .find(|t| latin(t))
        .map(str::to_owned)
}

/// Nome em alfabeto latino (dá para buscar)? "君の名は。" não; "Amélie" sim.
fn latin(s: &str) -> bool {
    s.chars().all(|c| (c as u32) < 0x250)
}

/// Idioma do TMDB (ISO) como o Radarr/Sonarr escrevem.
fn language_name(iso: &str) -> Option<&'static str> {
    Some(match iso {
        "en" => "English",
        "ja" => "Japanese",
        "ko" => "Korean",
        "zh" | "cn" => "Chinese",
        "es" => "Spanish",
        "fr" => "French",
        "de" => "German",
        "it" => "Italian",
        "pt" => "Portuguese",
        "ru" => "Russian",
        "hi" => "Hindi",
        "th" => "Thai",
        "tr" => "Turkish",
        "pl" => "Polish",
        "nl" => "Dutch",
        "sv" => "Swedish",
        "da" => "Danish",
        "no" => "Norwegian",
        "fi" => "Finnish",
        "ar" => "Arabic",
        "he" => "Hebrew",
        "id" => "Indonesian",
        "ta" => "Tamil",
        "te" => "Telugu",
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Audio {
    Portuguese,
    Original,
    Other,
}

/// Sites brasileiros (o torrent-indexer escreve os idiomas no fim do nome,
/// "(brazilian, eng)"): DUAL, DUBLADO e NACIONAL também são áudio em português.
pub(crate) fn br_audio(release: &str) -> Audio {
    let w = words(release);
    if w.iter().any(|x| matches!(x.as_str(), "brazilian" | "portuguese" | "dual" | "dublado" | "dublada" | "nacional")) {
        Audio::Portuguese
    } else {
        Audio::Original
    }
}

/// Os idiomas que o `parse` do Radarr/Sonarr leu no nome; sem idioma no nome
/// é o original (como eles fazem).
pub(crate) fn arr_audio(languages: &[&str], original: Option<&str>) -> Audio {
    if languages.iter().any(|l| l.starts_with("Portuguese")) {
        Audio::Portuguese
    } else if languages.iter().all(|l| *l == "Unknown") || original.is_some_and(|o| languages.contains(&o)) {
        Audio::Original
    } else {
        Audio::Other
    }
}

impl Orchestrator {
    /// Busca o título nos indexadores e diz o que há.
    async fn check_availability(&self, w: &Want) -> Avail {
        let Some(p) = &self.prowlarr else { return Avail::Unknown };
        let original = w.original_language.as_deref().and_then(language_name);
        // nome original fora do alfabeto latino (anime, coreano): o inglês
        let mut names: Vec<String> = w.names.iter().filter(|n| latin(n)).cloned().collect();
        if w.names.iter().any(|n| !latin(n))
            && let Some(t) = &self.tmdb
        {
            let path = if w.kind == Kind::Movie { format!("movie/{}?language=en-US", w.tmdb) } else { format!("tv/{}?language=en-US", w.tmdb) };
            if let Ok(v) = t.get(&path).await
                && let Some(en) = v["title"].as_str().or(v["name"].as_str())
            {
                names.push(en.to_owned());
            }
        }
        // anime: o nome romaji, que é como os grupos de fora chamam
        // ("Sousou no Frieren", não "Frieren and the Journey…")
        let anime = matches!(w.original_language.as_deref(), Some("ja" | "zh"));
        let mut romaji = None;
        if w.original_language.as_deref() == Some("ja")
            && let Some(t) = &self.tmdb
        {
            let path = if w.kind == Kind::Movie { format!("movie/{}/alternative_titles", w.tmdb) } else { format!("tv/{}/alternative_titles", w.tmdb) };
            romaji = t.get(&path).await.ok().and_then(|v| romaji_in(&v));
            names.extend(romaji.clone());
        }
        let mut seen = HashSet::new();
        names.retain(|n| seen.insert(words(n)));
        let cats: &[u32] = if w.kind == Kind::Movie { &[2000] } else { &[5000, 5070] };
        let ids: Vec<i64> = match p.get_json("indexer").await {
            Ok(list) => list
                .as_array()
                .into_iter()
                .flatten()
                .filter(|i| i["enable"] == true && i["protocol"] == "torrent" && !skipped(i["name"].as_str().unwrap_or_default()))
                .filter_map(|i| i["id"].as_i64())
                .collect(),
            Err(e) => {
                warn!("disponibilidade: lista de indexadores: {e:#}");
                return Avail::Unknown;
            }
        };
        // o primeiro nome (português) sem ano; o segundo (ou o mesmo, se só há
        // um) com o ano, em filme
        let Some(first) = names.first() else { return Avail::Unknown };
        let second = romaji.as_ref().filter(|r| words(r) != words(first)).or(names.get(1)).unwrap_or(first);
        let mut queries = vec![query(first, None), query(second, w.year.filter(|_| w.kind == Kind::Movie))];
        queries.dedup();
        let mut found: Vec<Value> = vec![];
        let mut failed = false;
        let (a, b) = tokio::join!(p.search_in(&queries[0], cats, crate::services::ONE_PAGE, &ids), async {
            match queries.get(1) {
                Some(q) => p.search_in(q, cats, crate::services::ONE_PAGE, &ids).await,
                None => Ok(vec![]),
            }
        });
        for r in [a, b] {
            match r {
                Ok(list) => found.extend(list),
                Err(e) => {
                    warn!("disponibilidade de {}: {e:#}", w.names[0]);
                    failed = true;
                }
            }
        }
        let mut matched: Vec<&Value> = found
            .iter()
            .filter(|r| r["protocol"] == "torrent" && r["seeders"].as_i64().unwrap_or(0) > 0)
            .filter(|r| title_matches(&names, w.year, r["title"].as_str().unwrap_or_default()))
            .collect();
        // os brasileiros primeiro (dizem o idioma sem precisar do parse)
        matched.sort_by_key(|r| !is_br(r));
        let arr = if w.kind == Kind::Movie { self.radarr.as_ref() } else { self.sonarr.as_ref() };
        let (mut original_found, mut parsed) = (false, 0);
        for &r in &matched {
            let title = r["title"].as_str().unwrap_or_default();
            let audio = if is_br(r) {
                br_audio(title)
            } else {
                let Some(arr) = arr.filter(|_| parsed < MAX_PARSE) else { continue };
                parsed += 1;
                let Ok(v) = arr.parse(title).await else { continue };
                let info = if w.kind == Kind::Movie { &v["parsedMovieInfo"] } else { &v["parsedEpisodeInfo"] };
                let langs: Vec<&str> = info["languages"].as_array().into_iter().flatten().filter_map(|l| l["name"].as_str()).collect();
                arr_audio(&langs, original)
            };
            match audio {
                Audio::Portuguese => return Avail::Dubbed,
                Audio::Original => original_found = true,
                Audio::Other => {}
            }
        }
        // anime: a dublagem em português que o nome não diz ("Multi-Audio",
        // "Dual Audio"), pelas faixas de verdade das versões de fora
        if anime {
            let foreign = matched.iter().copied().filter(|r| !is_br(r));
            if self.tracks_of(foreign, false).await.values().any(|t| t.portuguese_audio()) {
                return Avail::Dubbed;
            }
        }
        if original_found {
            Avail::Subbed
        } else if failed || self.indexers_down().await.iter().any(|n| !skipped(n)) {
            Avail::Unknown
        } else {
            Avail::Nothing
        }
    }
}

pub(crate) fn is_br(r: &Value) -> bool {
    r["indexer"].as_str().is_some_and(|i| i.ends_with("(BR)"))
}

/// Checagens em segundo plano, uma por vez, na ordem da fila.
pub async fn run(orch: Arc<Orchestrator>) {
    loop {
        // quem espera um título vê ele na fila ou em `current`, nunca em nenhum
        let next = {
            let mut q = orch.availability.queue.lock().await;
            let w = q.pop_front();
            *orch.availability.current.lock().await = w.as_ref().map(|w| (w.kind, w.id));
            w
        };
        let Some(w) = next else {
            orch.availability.wake.notified().await;
            continue;
        };
        let c = &orch.availability;
        let started = std::time::Instant::now();
        let avail = orch.check_availability(&w).await;
        let secs = started.elapsed().as_secs_f64();
        info!("disponibilidade: {} → {avail:?} ({secs:.0} s)", w.names[0]);
        c.record(&w, avail).await;
        *c.current.lock().await = None;
        {
            let mut avg = c.avg.lock().await;
            *avg = *avg * 0.8 + secs * 0.2;
        }
        c.done.notify_waiters();
        tokio::select! {
            _ = tokio::time::sleep(GAP) => {}
            _ = c.urgent.notified() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_release_belongs_to_the_title_by_name_and_year() {
        let names = vec!["Pecadores".to_owned(), "Sinners".to_owned()];
        assert!(title_matches(&names, Some(2025), "Pecadores.2025.1080p.WEB-DL.DUAL.5.1 (brazilian, eng)"));
        assert!(title_matches(&names, Some(2025), "Sinners (2025) [1080p] [YTS.MX] (eng)"));
        // outro filme com o mesmo nome em português
        assert!(!title_matches(&names, Some(2025), "Pecadores.2023.1080p.WEB-DL.x264.DUAL.2.0 (brazilian)"));
        // nome sem ano também serve; palavra solta não ("Sinners" ≠ "Sinnerstown")
        assert!(title_matches(&names, Some(2025), "Sinners 1080p WEB"));
        assert!(!title_matches(&names, Some(2025), "Sinnerstown 2025 1080p"));
        // acentos e pontuação
        assert!(title_matches(&["Coração Valente".to_owned()], Some(1995), "Coracao.Valente.1995.1080p.BluRay"));
        // série: sem ano, nome no meio do título
        assert!(title_matches(&["Frieren".to_owned()], None, "DB Sousou no Frieren 2nd Season Dual Audio"));
    }

    #[test]
    fn which_audio_a_release_has() {
        assert_eq!(br_audio("Pecadores.2025.1080p.WEB-DL.DUAL.5.1 (brazilian, eng)"), Audio::Portuguese);
        assert_eq!(br_audio("Supergirl 2026 WEB-DL DUBLADO"), Audio::Portuguese);
        assert_eq!(br_audio("Sinners (2025) [1080p] [YTS.MX] (eng)"), Audio::Original);
        assert_eq!(br_audio("Frieren S02E01 LEGENDADO"), Audio::Original);
        assert_eq!(arr_audio(&["Portuguese (Brazil)"], Some("English")), Audio::Portuguese);
        assert_eq!(arr_audio(&["Unknown"], Some("Japanese")), Audio::Original);
        assert_eq!(arr_audio(&["Japanese", "English"], Some("Japanese")), Audio::Original);
        // anime dublado em inglês: terceiro idioma
        assert_eq!(arr_audio(&["English"], Some("Japanese")), Audio::Other);
        assert_eq!(arr_audio(&["French"], None), Audio::Other);
        assert!(latin("Amélie") && !latin("君の名は。"));
        assert_eq!(query("Bob Esponja: O Incrível Resgate", None), "bob esponja o incrivel resgate");
        assert_eq!(query("Saving Bikini Bottom: The Sandy Cheeks Movie", Some(2024)), "saving bikini bottom the sandy cheeks movie 2024");
        assert_eq!(language_name("ja"), Some("Japanese"));
        let alt = json!({ "results": [
            { "iso_3166_1": "JP", "title": "そうそうのフリーレン", "type": "Kana" },
            { "iso_3166_1": "JP", "title": "Sōsō no Furīren", "type": "Hepburn Romanization" },
            { "iso_3166_1": "JP", "title": "Sousou no Frieren", "type": "Romaji" } ] });
        assert_eq!(romaji_in(&alt).as_deref(), Some("Sousou no Frieren"));
        assert_eq!(romaji_in(&json!({ "titles": [] })), None);
        // o nome romaji acha as versões dos grupos de anime
        assert!(title_matches(&["Sousou no Frieren".to_owned()], None, "[SubsPlease] Sousou no Frieren S2 - 05 (1080p) [6AAEC79A].mkv"));
    }

    #[test]
    fn a_tmdb_item_becomes_what_to_check() {
        let v = json!({ "id": 1233413, "title": "Pecadores", "original_title": "Sinners", "release_date": "2025-04-16", "original_language": "en" });
        let w = Want::from_tmdb(Kind::Movie, 1233413, &v).unwrap();
        assert_eq!((w.names.clone(), w.year, w.original_language.as_deref()), (vec!["Pecadores".to_owned(), "Sinners".to_owned()], Some(2025), Some("en")));
        let tv = json!({ "id": 209867, "name": "Frieren e a Jornada para o Além", "original_name": "葬送のフリーレン", "first_air_date": "2023-09-29", "original_language": "ja" });
        let w = Want::from_tmdb(Kind::Series, 424536, &tv).unwrap();
        assert_eq!((w.id, w.tmdb, w.names.len()), (424536, 209867, 2));
    }

    #[tokio::test]
    async fn unknown_keeps_what_was_known() {
        let dir = std::env::temp_dir().join(format!("anubis-avail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = Checker::load(dir.clone());
        let w = Want { kind: Kind::Movie, id: 7, tmdb: 7, names: vec!["X".into()], year: None, original_language: None };
        assert_eq!(c.get(Kind::Movie, 7).await, None);
        c.want(w.clone()).await;
        c.want(w.clone()).await;
        assert_eq!(c.pending().await, 1);
        c.record(&w, Avail::Subbed).await;
        c.record(&w, Avail::Unknown).await;
        assert_eq!(c.get(Kind::Movie, 7).await, Some(Avail::Subbed));
        // gravado em disco
        assert_eq!(Checker::load(dir.clone()).get(Kind::Movie, 7).await, Some(Avail::Subbed));
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn want(id: i64) -> Want {
        Want { kind: Kind::Movie, id, tmdb: id, names: vec![format!("T{id}")], year: None, original_language: None }
    }

    #[tokio::test]
    async fn the_open_tab_goes_to_the_front_of_the_queue() {
        let c = Checker::load(std::env::temp_dir().join("anubis-avail-nao-existe"));
        for id in 1..=5 {
            c.want(want(id)).await;
        }
        c.prioritize(&[(Kind::Movie, 4), (Kind::Series, 4), (Kind::Movie, 2)]).await;
        let order: Vec<i64> = c.queue.lock().await.iter().map(|w| w.id).collect();
        assert_eq!(order, [4, 2, 1, 3, 5]);
        // pausa média 10 s + 15 s por título
        assert_eq!(c.eta().await, 5 * 25);
    }

    #[tokio::test]
    async fn opening_a_title_waits_for_its_check() {
        let c = Arc::new(Checker::load(std::env::temp_dir().join(format!("anubis-avail-now-{}", std::process::id()))));
        // fora da fila: devolve na hora o que se sabe
        assert_eq!(c.check_now(Kind::Movie, 9, std::time::Duration::from_secs(5)).await, None);
        c.want(want(1)).await;
        c.want(want(9)).await;
        let worker = {
            let c = c.clone();
            tokio::spawn(async move {
                c.urgent.notified().await;
                let w = c.queue.lock().await.pop_front().unwrap();
                assert_eq!(w.id, 9, "o título aberto passou na frente");
                c.record(&w, Avail::Dubbed).await;
                c.done.notify_waiters();
            })
        };
        assert_eq!(c.check_now(Kind::Movie, 9, std::time::Duration::from_secs(5)).await, Some(Avail::Dubbed));
        worker.await.unwrap();
        std::fs::remove_file(&c.path).ok();
    }

    #[test]
    fn availability_leaves_1337x_out() {
        assert!(skipped("1337x"));
        assert!(skipped("1337x (Prowlarr)"));
        assert!(!skipped("The Pirate Bay"));
    }
}
