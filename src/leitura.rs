//! Leitura (`/leitura`): a estante (o que está em `Meu Drive/Livros`, com o
//! progresso do kosync) por categoria. Separada da parte de vídeo: rotas,
//! estado e página próprios.
//!
//! GET /leitura                 — página
//! GET /leitura/{categoria}     — {"estante": [...]}: lendo primeiro (o mais
//!                                recente), depois o que entrou por último
//! PUT /leitura/lido/{doc}      — {"lido": true|false}: marca no site, que
//!                                vale mais que o progresso do Kindle

use crate::drive::{Book, Drive};
use crate::kosync::Kosync;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, put};
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

pub struct Leitura {
    pub drive: Drive,
    pub kosync: Option<Arc<Kosync>>,
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

impl Leitura {
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/", get(|| async { ([("cache-control", "no-cache")], Html(include_str!("../web/leitura.html"))) }))
            .route("/{categoria}", get(shelf))
            .route("/lido/{doc}", put(mark))
            .with_state(self)
    }
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
    let Some((_, folder)) = CATEGORIES.iter().find(|(slug, _)| *slug == categoria) else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "categoria desconhecida" }))).into_response();
    };
    let books = match l.drive.books().await {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_GATEWAY, Json(json!({ "error": format!("{e:#}") }))).into_response(),
    };
    let mut shelf = Vec::new();
    for b in books.iter().filter(|b| b.category == *folder) {
        let (p, m) = match &l.kosync {
            Some(k) => (k.get(&b.doc).await, k.mark(&b.doc).await),
            None => (None, None),
        };
        let progress = p.as_ref().map(|p| p.percentage);
        let lido = m.map(|m| m.lido).unwrap_or(progress.is_some_and(|p| p >= 0.995));
        shelf.push(Shelved { book: b.clone(), progress, read_at: p.map(|p| p.timestamp), lido });
    }
    // lendo (com progresso e não terminado), do mais recente; depois o mais novo no Drive
    let reading = |s: &Shelved| s.progress.is_some() && !s.lido;
    shelf.sort_by(|a, b| {
        reading(b)
            .cmp(&reading(a))
            .then(b.read_at.cmp(&a.read_at))
            .then(b.book.modified.cmp(&a.book.modified))
    });
    Json(json!({ "estante": shelf })).into_response()
}
