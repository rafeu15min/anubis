//! Leitura (`/leitura`): a estante (o que está em `Meu Drive/Livros`, com o
//! progresso do kosync) por categoria. Separada da parte de vídeo: rotas,
//! estado e página próprios.
//!
//! GET /leitura                 — página
//! GET /leitura/{categoria}     — {"estante": [...], "para_voce": [...],
//!                                "preparando": bool}: estante com lendo primeiro;
//!                                "Para você" = obras dos autores que você lê
//!                                (Open Library), sem as que já estão no Drive
//! PUT /leitura/lido/{doc}      — {"lido": true|false}: marca no site, que
//!                                vale mais que o progresso do Kindle
//! POST /leitura/baixar         — {"title","author"?,"categoria"}: cadeia de
//!                                download (LibGen → Bookshelf → torrent;
//!                                português primeiro; src/leitura_dl.rs) → Drive
//! GET  /leitura/busca?q=        — {"estante": [...], "obras": [...]}: o que
//!                                casa na estante (todas as categorias) e as
//!                                obras do Open Library que ainda não estão lá
//! GET  /leitura/pedidos        — andamento dos pedidos
//! POST /leitura/pedidos/{id}/ingles — sem versão em português: aceita inglês

use crate::drive::{Book, Drive};
use crate::kosync::Kosync;
use crate::leitura_dl::Downloads;
use crate::openlibrary::{OpenLibrary, Work};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;

/// (na URL, pasta no Drive)
pub const CATEGORIES: [(&str, &str); 5] = [
    ("ficcao", "Ficção"),
    ("tecnicos", "Técnicos"),
    ("artigos", "Artigos"),
    ("mangas-hqs", "Mangás e HQs"),
    ("rpg", "RPG"),
];

/// "Para você" pronto, por categoria
const RECS_TTL: Duration = Duration::from_secs(24 * 3600);
const MAX_AUTHORS: usize = 8;
const ROW_LEN: usize = 20;

pub struct Leitura {
    pub drive: Drive,
    pub kosync: Option<Arc<Kosync>>,
    pub ol: OpenLibrary,
    pub downloads: Arc<Downloads>,
    recs: Mutex<HashMap<String, (Instant, Arc<Vec<Suggestion>>)>>,
    building: Mutex<HashSet<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Suggestion {
    /// obra no Open Library ("/works/OL…W")
    pub key: String,
    pub title: String,
    pub author: String,
    pub year: Option<i64>,
    pub cover: Option<String>,
    /// há edição em português
    pub portuguese: bool,
    /// por que apareceu
    pub reason: String,
}

#[derive(Serialize)]
struct Shelved {
    #[serde(flatten)]
    book: Book,
    /// 0.0 a 1.0 (kosync); ausente: nunca aberto
    progress: Option<f64>,
    /// quando o progresso foi gravado (segundos desde 1970)
    read_at: Option<i64>,
    /// terminado: a marca do site, se houver; senão o fim do livro no Kindle
    lido: bool,
}

const MAX_PER_AUTHOR: usize = 3;

/// Título comparável: só letras e números, minúsculas, sem acento.
fn norm(s: &str) -> String {
    s.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            c => c,
        })
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// Já se tem: mesmo título, um contém o outro ("Sprawl #2 Count Zero" tem
/// "Count Zero"), ou começam igual por 10+ letras (coleções: "Arsène Lupin
/// (coleção completa)" cobre "Arsène Lupin, gentleman-cambrioleur").
fn owned(title: &str, owned_titles: &HashSet<String>) -> bool {
    let t = norm(title);
    let prefix = |o: &str| t.chars().zip(o.chars()).take_while(|(a, b)| a == b).count();
    t.len() >= 5 && owned_titles.iter().any(|o| o.len() >= 5 && (o.contains(&t) || t.contains(o.as_str()) || prefix(o) >= 10))
}

/// Autor do nome do arquivo como o Open Library escreve: "Funke, Cornelia"
/// → "Cornelia Funke"; "[...]" e "etc." saem.
fn author_name(raw: &str) -> Option<String> {
    let a = raw.split('[').next().unwrap_or(raw).replace("etc.", "");
    let a = a.trim().trim_end_matches(',').trim();
    let a = match a.split_once(", ") {
        Some((last, first)) if !first.contains(' ') || first.split(' ').count() <= 2 => format!("{first} {last}"),
        _ => a.to_string(),
    };
    (a.chars().filter(|c| c.is_alphabetic()).count() >= 3).then_some(a)
}

impl Leitura {
    pub fn new(drive: Drive, kosync: Option<Arc<Kosync>>, ol: OpenLibrary, downloads: Arc<Downloads>) -> Self {
        Self { drive, kosync, ol, downloads, recs: Mutex::new(HashMap::new()), building: Mutex::new(HashSet::new()) }
    }

    /// O "Para você" da categoria se já está pronto; senão começa a montar.
    async fn for_you(self: &Arc<Self>, folder: &'static str) -> Option<Arc<Vec<Suggestion>>> {
        let cached = self.recs.lock().await.get(folder).cloned();
        if let Some((at, recs)) = &cached
            && at.elapsed() < RECS_TTL
        {
            return Some(recs.clone());
        }
        if self.building.lock().await.insert(folder.to_string()) {
            let me = self.clone();
            tokio::spawn(async move {
                match me.build(folder).await {
                    Ok(recs) => {
                        me.recs.lock().await.insert(folder.to_string(), (Instant::now(), Arc::new(recs)));
                    }
                    Err(e) => tracing::warn!("Para você ({folder}): {e:#}"),
                }
                me.building.lock().await.remove(folder);
            });
        }
        cached.map(|(_, r)| r) // o antigo vale enquanto o novo é montado
    }

    /// Autores da categoria pesados pelo que se lê (lido ou lendo vale 3,
    /// só estar no Drive vale 1); as obras deles que ainda não estão no Drive.
    async fn build(&self, folder: &str) -> anyhow::Result<Vec<Suggestion>> {
        let books = self.drive.books().await?;
        let mut owned_keys = HashSet::new();
        let mut owned_titles = HashSet::new();
        let mut authors: HashMap<String, f64> = HashMap::new();
        for b in books.iter() {
            owned_titles.insert(norm(&b.title));
            if b.category != folder {
                continue;
            }
            let read = match &self.kosync {
                Some(k) => k.get(&b.doc).await.is_some() || k.mark(&b.doc).await.is_some_and(|m| m.lido),
                None => false,
            };
            let author = b.author.as_deref().and_then(author_name);
            let work = self.ol.work_of(&b.path, &b.title, author.as_deref()).await;
            if let Some(w) = &work {
                owned_keys.insert(w.key.clone());
                owned_titles.insert(norm(&w.title));
            }
            if let Some(a) = author.or_else(|| work.and_then(|w| w.author_name.into_iter().next())) {
                *authors.entry(a).or_default() += if read { 3.0 } else { 1.0 };
            }
        }
        let mut top: Vec<(String, f64)> = authors.into_iter().collect();
        top.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut scored: Vec<(f64, Suggestion)> = Vec::new();
        let mut seen = HashSet::new();
        for (author, weight) in top.into_iter().take(MAX_AUTHORS) {
            for (rank, w) in self.ol.works_by(&author).await.into_iter().enumerate() {
                let Work { ref key, ref title, .. } = w;
                let known = owned_keys.contains(key)
                    || owned(title, &owned_titles)
                    || w.alternative_title.iter().any(|t| owned(t, &owned_titles));
                let readable = w.has_portuguese() || w.language.iter().any(|l| l == "eng");
                if known || !readable || !seen.insert(key.clone()) {
                    continue;
                }
                let score = weight * if w.has_portuguese() { 1.5 } else { 1.0 } / (1.0 + rank as f64 * 0.15);
                scored.push((score, Suggestion {
                    key: key.clone(),
                    title: title.clone(),
                    author: author.clone(),
                    year: w.first_publish_year,
                    cover: w.cover(),
                    portuguese: w.has_portuguese(),
                    reason: format!("De {author}, que você lê"),
                }));
            }
        }
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        // variedade: no máximo MAX_PER_AUTHOR de cada autor
        let mut per_author: HashMap<String, usize> = HashMap::new();
        Ok(scored
            .into_iter()
            .map(|(_, s)| s)
            .filter(|s| {
                let n = per_author.entry(s.author.clone()).or_default();
                *n += 1;
                *n <= MAX_PER_AUTHOR
            })
            .take(ROW_LEN)
            .collect())
    }

    /// Um livro do Drive com o progresso do kosync.
    async fn shelved(&self, b: &Book) -> Shelved {
        let (p, m) = match &self.kosync {
            Some(k) => (k.get(&b.doc).await, k.mark(&b.doc).await),
            None => (None, None),
        };
        let progress = p.as_ref().map(|p| p.percentage);
        let lido = m.map(|m| m.lido).unwrap_or(progress.is_some_and(|p| p >= 0.995));
        Shelved { book: b.clone(), progress, read_at: p.map(|p| p.timestamp), lido }
    }

    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/", get(crate::api::app_page))
            .route("/busca", get(search))
            .route("/{categoria}", get(shelf))
            .route("/lido/{doc}", put(mark))
            .route("/baixar", post(download))
            .route("/pedidos", get(|State(l): State<Arc<Leitura>>| async move { Json(l.downloads.list().await) }))
            .route("/pedidos/{id}/ingles", post(|State(l): State<Arc<Leitura>>, Path(id): Path<String>| async move {
                match l.downloads.accept_english(&id).await {
                    Some(j) => (StatusCode::ACCEPTED, Json(json!(j))).into_response(),
                    None => (StatusCode::NOT_FOUND, Json(json!({ "error": "pedido não existe" }))).into_response(),
                }
            }))
            .with_state(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autor_como_o_open_library_escreve() {
        assert_eq!(author_name("Funke, Cornelia").as_deref(), Some("Cornelia Funke"));
        assert_eq!(author_name("Matt Haig").as_deref(), Some("Matt Haig"));
        assert_eq!(author_name("Antoine de Saint-Exupéry [Saint-Exupéry etc.").as_deref(), Some("Antoine de Saint-Exupéry"));
        assert_eq!(author_name("1"), None);
    }

    #[test]
    fn ja_tem_mesmo_com_prefixo_ou_acento() {
        let have: HashSet<String> = ["Sprawl #2 Count Zero", "Drácula - Bram Stoker", "Arsene Lupin (Coleção completa - 21 livros)"].iter().map(|t| norm(t)).collect();
        assert!(owned("Count Zero", &have));
        assert!(owned("Dracula", &have));
        assert!(owned("Arsène Lupin", &have));
        assert!(owned("Arsène Lupin, gentleman-cambrioleur", &have));
        assert!(!owned("Idoru", &have));
    }
}

#[derive(serde::Deserialize)]
struct DownloadBody {
    title: String,
    author: Option<String>,
    categoria: String,
}

async fn download(State(l): State<Arc<Leitura>>, Json(b): Json<DownloadBody>) -> Response {
    let Some(&(_, folder)) = CATEGORIES.iter().find(|(slug, _)| *slug == b.categoria) else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "categoria desconhecida" }))).into_response();
    };
    // o título original ajuda nos indexadores (quase todos em inglês)
    let alt = l.ol.find(&b.title, b.author.as_deref()).await.map(|w| w.title).filter(|t| norm(t) != norm(&b.title));
    let job = l.downloads.request(b.title, alt, b.author, folder.to_string()).await;
    (StatusCode::ACCEPTED, Json(job)).into_response()
}

#[derive(serde::Deserialize)]
struct MarkBody {
    lido: bool,
}

async fn mark(State(l): State<Arc<Leitura>>, Path(doc): Path<String>, Json(body): Json<MarkBody>) -> Response {
    let Some(k) = &l.kosync else {
        return (StatusCode::NOT_IMPLEMENTED, Json(json!({ "error": "sem [kosync] na config" }))).into_response();
    };
    match k.set_mark(&doc, body.lido).await {
        Ok(m) => Json(m).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("{e:#}") }))).into_response(),
    }
}

async fn shelf(State(l): State<Arc<Leitura>>, Path(categoria): Path<String>) -> Response {
    let Some(&(_, folder)) = CATEGORIES.iter().find(|(slug, _)| *slug == categoria) else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "categoria desconhecida" }))).into_response();
    };
    let books = match l.drive.books().await {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_GATEWAY, Json(json!({ "error": format!("{e:#}") }))).into_response(),
    };
    let mut shelf = Vec::new();
    for b in books.iter().filter(|b| b.category == folder) {
        shelf.push(l.shelved(b).await);
    }
    // lendo (com progresso e não terminado), do mais recente; depois o mais novo no Drive
    let reading = |s: &Shelved| s.progress.is_some() && !s.lido;
    shelf.sort_by(|a, b| {
        reading(b)
            .cmp(&reading(a))
            .then(b.read_at.cmp(&a.read_at))
            .then(b.book.modified.cmp(&a.book.modified))
    });
    let recs = l.for_you(folder).await;
    Json(json!({ "estante": shelf, "preparando": recs.is_none(), "para_voce": recs.as_deref().cloned().unwrap_or_default() })).into_response()
}

#[derive(serde::Deserialize)]
struct SearchQuery {
    q: String,
}

async fn search(State(l): State<Arc<Leitura>>, Query(q): Query<SearchQuery>) -> Response {
    let term = q.q.trim();
    let words: Vec<String> = term.split_whitespace().map(norm).filter(|w| !w.is_empty()).collect();
    if term.chars().count() < 2 || words.is_empty() {
        return Json(json!({ "estante": [], "obras": [] })).into_response();
    }
    let books = match l.drive.books().await {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_GATEWAY, Json(json!({ "error": format!("{e:#}") }))).into_response(),
    };
    // estante: todas as palavras no título ou no autor, sem acento
    let mut shelf = Vec::new();
    for b in books.iter() {
        let hay = format!("{}{}", norm(&b.title), norm(b.author.as_deref().unwrap_or("")));
        if words.iter().all(|w| hay.contains(w.as_str())) {
            shelf.push(l.shelved(b).await);
        }
    }
    let owned_titles: HashSet<String> = books.iter().map(|b| norm(&b.title)).collect();
    let works = match l.ol.search_works(term).await {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("Open Library ({term}): {e:#}");
            vec![]
        }
    };
    let works: Vec<Suggestion> = works
        .into_iter()
        .filter(|w| !owned(&w.title, &owned_titles) && !w.alternative_title.iter().any(|t| owned(t, &owned_titles)))
        .map(|w| Suggestion {
            portuguese: w.has_portuguese(),
            cover: w.cover(),
            author: w.author_name.first().cloned().unwrap_or_default(),
            year: w.first_publish_year,
            key: w.key,
            title: w.title,
            reason: String::new(),
        })
        .collect();
    Json(json!({ "estante": shelf, "obras": works })).into_response()
}
