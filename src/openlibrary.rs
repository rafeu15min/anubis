//! Open Library: a obra de cada livro do Drive (para saber o que já se tem,
//! mesmo com título em outra língua), as obras de um autor e de um assunto
//! (recomendações) e os assuntos de cada obra (o gosto: src/gosto.rs). Sem
//! chave. Um pedido por vez, com pausa. A obra de cada livro do Drive e as
//! dos títulos das vitrines ficam em `state_dir/openlibrary-v2.json` (só se
//! busca uma vez; o v1 não tinha os assuntos).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::Mutex;

const BASE: &str = "https://openlibrary.org";
const GAP: Duration = Duration::from_millis(700);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Work {
    /// "/works/OL…W"
    pub key: String,
    pub title: String,
    #[serde(default)]
    pub author_name: Vec<String>,
    pub first_publish_year: Option<i64>,
    pub cover_i: Option<i64>,
    #[serde(default)]
    pub language: Vec<String>,
    pub ratings_count: Option<i64>,
    #[serde(default)]
    pub alternative_title: Vec<String>,
    #[serde(default)]
    pub subject: Vec<String>,
}

impl Work {
    pub fn has_portuguese(&self) -> bool {
        self.language.iter().any(|l| l == "por")
    }
    pub fn cover(&self) -> Option<String> {
        self.cover_i.map(|id| format!("https://covers.openlibrary.org/b/id/{id}-M.jpg"))
    }
}

#[derive(Deserialize)]
struct Search {
    docs: Vec<Work>,
}

const FIELDS: &str = "key,title,author_name,first_publish_year,cover_i,language,ratings_count,alternative_title,subject";

/// Versão da busca da obra de cada livro do Drive: mudou, busca de novo.
const OWNED_VERSION: u32 = 3;

/// Título comparável: só letras e números, minúsculas, sem acento.
fn norm(s: &str) -> String {
    crate::gosto::fold(s).chars().filter(|c| c.is_alphanumeric()).collect()
}

/// O nome do arquivo como título de livro: sem "Sprawl #1", "(Z-Library)",
/// "(1)", "(Coleção completa - 21 livros)" e "_".
pub fn clean_title(t: &str) -> String {
    let mut s = String::new();
    let mut depth = 0;
    for c in t.replace('_', " ").chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = (depth - 1).max(0),
            c if depth == 0 => s.push(c),
            _ => {}
        }
    }
    // "Sprawl #1 Neuromancer" → "Neuromancer"
    let words: Vec<&str> = s.split_whitespace().collect();
    let start = words.iter().position(|w| w.starts_with('#')).map_or(0, |i| i + 1);
    let out = words[start.min(words.len())..].join(" ");
    if out.len() >= 3 { out } else { words.join(" ") }
}

/// A obra é a do livro: o título bate (um contém o outro, ou um título
/// alternativo dela), ou o autor bate — "O Senhor dos Ladrões" de Cornelia
/// Funke é "The Thief Lord".
pub fn same_work(title: &str, author: Option<&str>, w: &Work) -> bool {
    let t = norm(&clean_title(title));
    let by_title = std::iter::once(&w.title).chain(&w.alternative_title).any(|x| {
        let x = norm(x);
        x.len() >= 4 && t.len() >= 4 && (x.contains(&t) || t.contains(&x) || t.chars().zip(x.chars()).take_while(|(a, b)| a == b).count() >= 10)
    });
    // autor: o sobrenome (a palavra mais longa) aparece num dos autores da obra
    let by_author = author.is_some_and(|a| {
        let last = a.split_whitespace().max_by_key(|p| p.len()).map(norm).unwrap_or_default();
        last.len() >= 4 && w.author_name.iter().any(|n| norm(n).contains(&last))
    });
    by_title || by_author
}

#[derive(Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    v: u32,
    /// caminho no Drive → obra (None: o Open Library não achou)
    owned: HashMap<String, Option<Work>>,
    /// "título|autor" de uma vitrine → obra
    #[serde(default)]
    found: HashMap<String, Option<Work>>,
}

pub struct OpenLibrary {
    http: reqwest::Client,
    path: PathBuf,
    store: Mutex<Store>,
    authors: Mutex<HashMap<String, Vec<Work>>>,
    subjects: Mutex<HashMap<String, Vec<Work>>>,
    turn: Mutex<()>,
}

impl OpenLibrary {
    pub fn load(state_dir: PathBuf) -> Result<Self> {
        let path = state_dir.join("openlibrary-v2.json");
        let mut store: Store = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        if store.v < OWNED_VERSION {
            store.owned.clear();
            store.v = OWNED_VERSION;
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("Anubis/0.1 (leitura pessoal)")
            .build()?;
        Ok(Self { http, path, store: Mutex::new(store), authors: Mutex::new(HashMap::new()), subjects: Mutex::new(HashMap::new()), turn: Mutex::new(()) })
    }

    async fn search(&self, params: &[(&str, &str)]) -> Result<Vec<Work>> {
        let _turn = self.turn.lock().await; // um por vez
        let url = reqwest::Url::parse_with_params(&format!("{BASE}/search.json"), params.iter().copied().chain([("fields", FIELDS)]))?;
        let res = self
            .http
            .get(url)
            .send()
            .await
            .context("Open Library: sem resposta")?
            .error_for_status()?;
        let s: Search = res.json().await.context("Open Library: resposta inesperada")?;
        tokio::time::sleep(GAP).await;
        Ok(s.docs)
    }

    /// A obra de um livro do Drive (busca uma vez e guarda).
    pub async fn work_of(&self, drive_path: &str, title: &str, author: Option<&str>) -> Option<Work> {
        if let Some(w) = self.store.lock().await.owned.get(drive_path) {
            return w.clone();
        }
        // título e autor separados; depois texto livre (acha os títulos das
        // edições em português); depois só o título. Dos 5 primeiros, o que é
        // mesmo o livro, de preferência com assuntos
        let clean = clean_title(title);
        let mut tries: Vec<Vec<(&str, &str)>> = vec![];
        if let Some(a) = author {
            tries.push(vec![("title", clean.as_str()), ("author", a)]);
        }
        let q = format!("{clean} {}", author.unwrap_or(""));
        tries.push(vec![("q", q.trim())]);
        tries.push(vec![("title", clean.as_str())]);
        let mut work = None;
        let mut failed = false;
        for params in tries {
            let params: Vec<(&str, &str)> = params.into_iter().chain([("limit", "5")]).collect();
            match self.search(&params).await {
                Ok(docs) => {
                    let mine: Vec<Work> = docs.into_iter().filter(|w| same_work(title, author, w)).collect();
                    let best = mine.iter().find(|w| !w.subject.is_empty()).or(mine.first()).cloned();
                    if best.as_ref().is_some_and(|w| !w.subject.is_empty()) {
                        work = best;
                        break;
                    }
                    work = work.or(best);
                }
                Err(e) => {
                    tracing::warn!("Open Library ({title}): {e:#}");
                    failed = true;
                }
            }
        }
        if work.is_none() && failed {
            return None; // sem guardar: tenta de novo depois
        }
        self.store.lock().await.owned.insert(drive_path.to_string(), work.clone());
        self.save().await;
        work
    }

    /// A obra já guardada de um livro do Drive, sem buscar (None: nunca buscada).
    pub async fn known(&self, drive_path: &str) -> Option<Option<Work>> {
        self.store.lock().await.owned.get(drive_path).cloned()
    }

    async fn save(&self) {
        let bytes = serde_json::to_vec(&*self.store.lock().await);
        if let Ok(bytes) = bytes {
            let tmp = self.path.with_extension("json.tmp");
            if tokio::fs::write(&tmp, bytes).await.is_ok() {
                let _ = tokio::fs::rename(&tmp, &self.path).await;
            }
        }
    }

    /// A obra de um título qualquer (sem guardar): o título original.
    pub async fn find(&self, title: &str, author: Option<&str>) -> Option<Work> {
        let q = format!("{title} {}", author.unwrap_or(""));
        self.search(&[("q", q.trim()), ("limit", "1")]).await.ok()?.into_iter().next()
    }

    /// Como `find`, mas guardado: os títulos das vitrines (Amazon, Goodreads)
    /// se repetem de um dia para o outro.
    pub async fn find_cached(&self, title: &str, author: Option<&str>) -> Option<Work> {
        let key = format!("{title}|{}", author.unwrap_or(""));
        if let Some(w) = self.store.lock().await.found.get(&key) {
            return w.clone();
        }
        let q = format!("{title} {}", author.unwrap_or(""));
        let work = match self.search(&[("q", q.trim()), ("limit", "1")]).await {
            Ok(docs) => docs.into_iter().next(),
            Err(e) => {
                tracing::info!("Open Library ({title}): {e:#}");
                return None; // sem guardar: tenta de novo depois
            }
        };
        self.store.lock().await.found.insert(key, work.clone());
        self.save().await;
        work
    }

    /// As obras mais lidas de um assunto ("cyberpunk"; em memória enquanto o Anubis roda).
    pub async fn works_about(&self, subject: &str) -> Vec<Work> {
        if let Some(w) = self.subjects.lock().await.get(subject) {
            return w.clone();
        }
        let q = format!("subject:\"{subject}\"");
        let works = match self.search(&[("q", q.as_str()), ("sort", "readinglog"), ("limit", "25")]).await {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("Open Library (assunto {subject}): {e:#}");
                return vec![];
            }
        };
        self.subjects.lock().await.insert(subject.to_string(), works.clone());
        works
    }

    /// Busca livre (título, autor, assunto): as obras mais relevantes.
    pub async fn search_works(&self, q: &str) -> Result<Vec<Work>> {
        self.search(&[("q", q), ("limit", "24")]).await
    }

    /// As obras mais bem avaliadas de um autor (em memória enquanto o Anubis roda).
    pub async fn works_by(&self, author: &str) -> Vec<Work> {
        if let Some(w) = self.authors.lock().await.get(author) {
            return w.clone();
        }
        let works = match self.search(&[("author", author), ("sort", "rating"), ("limit", "30")]).await {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("Open Library ({author}): {e:#}");
                return vec![];
            }
        };
        self.authors.lock().await.insert(author.to_string(), works.clone());
        works
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titulo_do_arquivo() {
        assert_eq!(clean_title("Sprawl #1 Neuromancer"), "Neuromancer");
        assert_eq!(clean_title("Suzana_Kasey_Garota_Interrompida (1)"), "Suzana Kasey Garota Interrompida");
        assert_eq!(clean_title("Arsene Lupin (Coleção completa - 21 livros)"), "Arsene Lupin");
        assert_eq!(clean_title("Fahrenheit 451"), "Fahrenheit 451");
    }
}
