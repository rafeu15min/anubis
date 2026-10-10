//! Open Library: a obra de cada livro do Drive (para saber o que já se tem,
//! mesmo com título em outra língua) e as obras de um autor (recomendações).
//! Sem chave. Um pedido por vez, com pausa, e a obra de cada livro do Drive
//! guardada em `state_dir/openlibrary.json` (só se busca uma vez).

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

const FIELDS: &str = "key,title,author_name,first_publish_year,cover_i,language,ratings_count,alternative_title";

pub struct OpenLibrary {
    http: reqwest::Client,
    path: PathBuf,
    /// caminho no Drive → obra (None: o Open Library não achou)
    owned: Mutex<HashMap<String, Option<Work>>>,
    authors: Mutex<HashMap<String, Vec<Work>>>,
    turn: Mutex<()>,
}

impl OpenLibrary {
    pub fn load(state_dir: PathBuf) -> Result<Self> {
        let path = state_dir.join("openlibrary.json");
        let owned = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("Anubis/0.1 (leitura pessoal)")
            .build()?;
        Ok(Self { http, path, owned: Mutex::new(owned), authors: Mutex::new(HashMap::new()), turn: Mutex::new(()) })
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
        if let Some(w) = self.owned.lock().await.get(drive_path) {
            return w.clone();
        }
        // texto livre acha também os títulos das edições (em português); o
        // título sozinho fica de reserva
        let q = format!("{title} {}", author.unwrap_or(""));
        let found = match self.search(&[("q", q.trim()), ("limit", "1")]).await {
            Ok(docs) if !docs.is_empty() => Ok(docs),
            _ => self.search(&[("title", title), ("limit", "1")]).await,
        };
        let work = match found {
            Ok(docs) => docs.into_iter().next(),
            Err(e) => {
                tracing::warn!("Open Library ({title}): {e:#}");
                return None; // sem guardar: tenta de novo depois
            }
        };
        let mut owned = self.owned.lock().await;
        owned.insert(drive_path.to_string(), work.clone());
        if let Ok(bytes) = serde_json::to_vec(&*owned) {
            let tmp = self.path.with_extension("json.tmp");
            if tokio::fs::write(&tmp, bytes).await.is_ok() {
                let _ = tokio::fs::rename(&tmp, &self.path).await;
            }
        }
        work
    }

    /// A obra de um título qualquer (sem guardar): o título original.
    pub async fn find(&self, title: &str, author: Option<&str>) -> Option<Work> {
        let q = format!("{title} {}", author.unwrap_or(""));
        self.search(&[("q", q.trim()), ("limit", "1")]).await.ok()?.into_iter().next()
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
