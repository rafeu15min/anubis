//! Vitrine pela disponibilidade: um título do TMDB só aparece depois de uma
//! busca prévia nos indexadores (Prowlarr) achar torrent dele com fontes.
//! - dublado: alguma versão com áudio em português;
//! - só legendado: nenhuma em português, mas alguma no idioma original (a
//!   página põe o aviso amarelo);
//! - nada: some da vitrine; "não sei" (indexador fora do ar, busca que falhou)
//!   também não aparece, mas volta a ser checado em 1 h.
//!
//! Dublagem num terceiro idioma não conta (sem português, só o original).
//! As buscas levam de 6 a 30 s cada, então rodam em segundo plano, uma por
//! vez e com pausa (ver `GAP`), e o resultado fica guardado em
//! `state_dir/disponibilidade.json`.

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
const PAGE: u32 = 20;
const GAP: std::time::Duration = std::time::Duration::from_secs(45);
/// com indexador fora do ar, espera antes de continuar (não insiste num site
/// que acabou de bloquear; e as checagens dariam "não sei")
const BACKOFF: std::time::Duration = std::time::Duration::from_secs(15 * 60);
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
}

fn key(kind: Kind, id: i64) -> String {
    format!("{}:{id}", if kind == Kind::Movie { "movie" } else { "series" })
}

impl Checker {
    pub fn load(state_dir: PathBuf) -> Self {
        let path = state_dir.join("disponibilidade.json");
        let map = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { path, map: Mutex::new(map), queue: Mutex::new(VecDeque::new()), wake: Notify::new() }
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

    async fn record(&self, w: &Want, avail: Avail) {
        let mut map = self.map.lock().await;
        let k = key(w.kind, w.id);
        // "não sei" não apaga o que já se sabia: só checa de novo mais cedo
        let kept = match (avail, map.get(&k)) {
            (Avail::Unknown, Some(old)) if old.avail != Avail::Unknown => old.avail,
            _ => avail,
        };
        map.insert(k, Entry { avail: kept, until: Utc::now() + avail.ttl() });
        let tmp = self.path.with_extension("tmp");
        let res = serde_json::to_vec(&*map).map_err(anyhow::Error::from).and_then(|b| Ok(std::fs::write(&tmp, b)?)).and_then(|_| Ok(std::fs::rename(&tmp, &self.path)?));
        if let Err(e) = res {
            warn!("não gravei {}: {e:#}", self.path.display());
        }
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
fn query(name: &str, year: Option<i64>) -> String {
    let q = words(name).join(" ");
    year.map_or(q.clone(), |y| format!("{q} {y}"))
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

#[derive(Debug, PartialEq, Eq)]
enum Audio {
    Portuguese,
    Original,
    Other,
}

/// Sites brasileiros (o torrent-indexer escreve os idiomas no fim do nome,
/// "(brazilian, eng)"): DUAL, DUBLADO e NACIONAL também são áudio em português.
fn br_audio(release: &str) -> Audio {
    let w = words(release);
    if w.iter().any(|x| matches!(x.as_str(), "brazilian" | "portuguese" | "dual" | "dublado" | "dublada" | "nacional")) {
        Audio::Portuguese
    } else {
        Audio::Original
    }
}

/// Os idiomas que o `parse` do Radarr/Sonarr leu no nome; sem idioma no nome
/// é o original (como eles fazem).
fn arr_audio(languages: &[&str], original: Option<&str>) -> Audio {
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
        let mut seen = HashSet::new();
        names.retain(|n| seen.insert(words(n)));
        let cats: &[u32] = if w.kind == Kind::Movie { &[2000] } else { &[5000, 5070] };
        // o primeiro nome (português) sem ano; o segundo (ou o mesmo, se só há
        // um) com o ano, em filme
        let Some(first) = names.first() else { return Avail::Unknown };
        let second = names.get(1).unwrap_or(first);
        let mut queries = vec![query(first, None), query(second, w.year.filter(|_| w.kind == Kind::Movie))];
        queries.dedup();
        let mut found: Vec<Value> = vec![];
        let mut failed = false;
        let (a, b) = tokio::join!(p.search(&queries[0], cats, PAGE), async {
            match queries.get(1) {
                Some(q) => p.search(q, cats, PAGE).await,
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
        for r in matched {
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
        if original_found {
            Avail::Subbed
        } else if failed || !self.indexers_down().await.is_empty() {
            Avail::Unknown
        } else {
            Avail::Nothing
        }
    }
}

fn is_br(r: &Value) -> bool {
    r["indexer"].as_str().is_some_and(|i| i.ends_with("(BR)"))
}

/// Checagens em segundo plano, uma por vez, na ordem em que a vitrine pediu.
pub async fn run(orch: Arc<Orchestrator>) {
    loop {
        if !orch.indexers_down().await.is_empty() {
            tokio::time::sleep(BACKOFF).await;
            orch.refresh_indexers().await;
            continue;
        }
        let next = orch.availability.queue.lock().await.pop_front();
        let Some(w) = next else {
            orch.availability.wake.notified().await;
            continue;
        };
        let started = std::time::Instant::now();
        let avail = orch.check_availability(&w).await;
        info!("disponibilidade: {} → {avail:?} ({:.0} s)", w.names[0], started.elapsed().as_secs_f64());
        orch.availability.record(&w, avail).await;
        tokio::time::sleep(GAP).await;
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
}
