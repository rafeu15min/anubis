//! Leitura (`/leitura`): a estante (o que está em `Meu Drive/Livros`, com o
//! progresso do kosync) por categoria. Separada da parte de vídeo: rotas,
//! estado e página próprios.
//!
//! GET /leitura                 — página
//! GET /leitura/{categoria}     — {"estante": [...], "para_voce": [...],
//!                                "destaques": [...], "lancamentos": [...],
//!                                "populares": [...], "preparando": bool}:
//!                                estante com lendo primeiro; as fileiras como
//!                                as dos filmes (src/leitura_recs.rs); o que já
//!                                está no Drive vem com "estante" (o caminho)
//! PUT /leitura/lido/{doc}      — {"lido": true|false}: marca no site, que
//!                                vale mais que o progresso do Kindle
//! POST /leitura/baixar         — {"title","author"?,"categoria"}: cadeia de
//!                                download (LibGen → Bookshelf → torrent;
//!                                português primeiro; src/leitura_dl.rs) → Drive
//! GET  /leitura/busca?q=        — {"estante": [...], "obras": [...]}: o que
//!                                casa na estante (todas as categorias) e as
//!                                obras do Open Library que ainda não estão lá
//! POST   /leitura/temas/{aba}         — {"nome"}: a pessoa põe um tema de busca
//! DELETE /leitura/temas/{aba}/{nome}  — tira um tema (o do sistema não volta)
//! GET  /leitura/pedidos        — andamento dos pedidos (o registro fica em
//!                                Preferências, não na página da leitura)
//! DELETE /leitura/pedidos              — tira do registro os que já estão no Drive
//! DELETE /leitura/pedidos/{id}         — tira um pedido terminado do registro
//!                                       (o livro, se veio, fica no Drive)
//! POST   /leitura/pedidos/{id}/tentar  — terminado sem o livro: procura de novo
//!
//! O Kindle navega pelas mesmas abas pelo endereço público do kosync (o
//! Tailscale Funnel só publica /kosync), com o usuário e a chave do kosync:
//! GET  /kosync/leitura/{categoria} — as fileiras em seções, com "Na estante"
//! POST /kosync/leitura/baixar      — o mesmo pedido de download da página
//! GET  /kosync/leitura/pedidos     — andamento dos pedidos
//! POST /leitura/pedidos/{id}/ingles — sem versão em português: aceita inglês

use crate::drive::{Book, Drive};
use crate::kosync::Kosync;
use crate::leitura_dl::Downloads;
use crate::leitura_recs::Rows;
use crate::openlibrary::OpenLibrary;
use crate::orchestrator::Orchestrator;
use crate::vitrine_livros::Vitrine;
use std::collections::{HashMap, HashSet};
use std::time::Instant;
use tokio::sync::Mutex;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;

/// As abas: (na URL, nome, pastas do Drive). Livros junta Ficção e Técnicos
/// (cada pedido vai para a pasta do assunto, ou a que a pessoa escolher). A
/// pasta RPG continua no Drive, mas não aparece aqui nem no Kindle
/// (kindle/drivesync.koplugin: HIDDEN).
pub const TABS: [(&str, &str, &[&str]); 3] = [
    ("livros", "Livros", &["Ficção", "Técnicos"]),
    ("artigos", "Artigos", &["Artigos"]),
    ("mangas-hqs", "Mangás e HQs", &["Mangás e HQs"]),
];

/// A aba de um endereço ("ficcao" e "tecnicos", de antes da fusão, são Livros).
pub fn tab_of(slug: &str) -> Option<(&'static str, &'static [&'static str])> {
    let slug = if slug == "ficcao" || slug == "tecnicos" { "livros" } else { slug };
    TABS.iter().find(|t| t.0 == slug).map(|t| (t.1, t.2))
}

fn visible(b: &Book) -> bool {
    TABS.iter().any(|t| t.2.contains(&b.category.as_str()))
}

pub struct Leitura {
    pub drive: Drive,
    pub kosync: Option<Arc<Kosync>>,
    pub ol: OpenLibrary,
    pub downloads: Arc<Downloads>,
    pub(crate) orch: Arc<Orchestrator>,
    pub(crate) vitrine: Vitrine,
    pub(crate) estudo: crate::estudo::Estudo,
    pub(crate) temas: crate::temas::Temas,
    /// os temas que o sistema propôs na última montagem de cada aba (a lista
    /// da página = estes + a edição da pessoa, na hora)
    pub(crate) system_themes: Mutex<HashMap<String, Vec<crate::estudo::Area>>>,
    pub(crate) rows: Mutex<HashMap<String, (Instant, Arc<Rows>)>>,
    pub(crate) building: Mutex<HashSet<String>>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct Suggestion {
    /// a obra na fonte: "/works/OL…W" (Open Library), "amz:ASIN", "gr:ID", "al:ID"
    pub key: String,
    pub title: String,
    pub author: String,
    pub year: Option<i64>,
    pub cover: Option<String>,
    /// há edição em português
    pub portuguese: bool,
    /// por que apareceu
    pub reason: String,
    /// a página da obra na fonte
    pub link: Option<String>,
    /// já está no Drive: o caminho
    pub estante: Option<String>,
    /// artigo de acesso livre: o PDF (baixado direto)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pdf: Option<String>,
    /// a pasta do Drive para onde iria (Livros: Ficção ou Técnicos, pelo assunto)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pasta: Option<String>,
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

/// Título comparável: só letras e números, minúsculas, sem acento.
pub(crate) fn norm(s: &str) -> String {
    crate::gosto::key(s)
}

/// Já se tem: mesmo título, um contém o outro ("Sprawl #2 Count Zero" tem
/// "Count Zero"), ou começam igual por 10+ letras (coleções: "Arsène Lupin
/// (coleção completa)" cobre "Arsène Lupin, gentleman-cambrioleur").
/// `shelf`: título comparável → caminho no Drive; devolve o caminho.
pub(crate) fn owned_path(title: &str, shelf: &HashMap<String, String>) -> Option<String> {
    let t = norm(title);
    let prefix = |o: &str| t.chars().zip(o.chars()).take_while(|(a, b)| a == b).count();
    if t.len() < 5 {
        return None;
    }
    shelf.iter().find(|(o, _)| o.len() >= 5 && (o.contains(&t) || t.contains(o.as_str()) || prefix(o) >= 10)).map(|(_, p)| p.clone())
}

/// Autor do nome do arquivo como o Open Library escreve: "Funke, Cornelia"
/// → "Cornelia Funke"; "[...]" e "etc." saem.
pub(crate) fn author_name(raw: &str) -> Option<String> {
    let a = raw.split('[').next().unwrap_or(raw).replace("etc.", "");
    let a = a.trim().trim_end_matches(',').trim();
    let a = match a.split_once(", ") {
        Some((last, first)) if !first.contains(' ') || first.split(' ').count() <= 2 => format!("{first} {last}"),
        _ => a.to_string(),
    };
    (a.chars().filter(|c| c.is_alphabetic()).count() >= 3).then_some(a)
}

impl Leitura {
    pub fn new(drive: Drive, kosync: Option<Arc<Kosync>>, ol: OpenLibrary, downloads: Arc<Downloads>, orch: Arc<Orchestrator>, vitrine: Vitrine, estudo: crate::estudo::Estudo) -> Self {
        let temas = crate::temas::Temas::load(orch.cfg.state_dir());
        Self { drive, kosync, ol, downloads, orch, vitrine, estudo, temas, system_themes: Mutex::new(HashMap::new()), rows: Mutex::new(HashMap::new()), building: Mutex::new(HashSet::new()) }
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

    /// As rotas do Kindle (montadas dentro de /kosync; ver o topo).
    pub fn kindle_router(self: Arc<Self>, k: Arc<Kosync>) -> Router {
        Router::new()
            .route("/leitura/pedidos", get(|State(l): State<Arc<Leitura>>| async move { Json(l.downloads.list().await) }))
            .route("/leitura/baixar", post(download))
            .route("/leitura/{categoria}", get(kindle_tab))
            .layer(axum::middleware::from_fn(move |req: axum::extract::Request, next: axum::middleware::Next| {
                let k = k.clone();
                async move { if k.authorized(req.headers()) { next.run(req).await } else { crate::kosync::unauthorized() } }
            }))
            .with_state(self)
    }

    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/", get(crate::api::app_page))
            .route("/busca", get(search))
            .route("/temas/{categoria}", post(add_theme))
            .route("/temas/{categoria}/{nome}", axum::routing::delete(remove_theme))
            .route("/{categoria}", get(shelf))
            .route("/lido/{doc}", put(mark))
            .route("/baixar", post(download))
            .route("/pedidos", get(|State(l): State<Arc<Leitura>>| async move { Json(l.downloads.list().await) })
                .delete(|State(l): State<Arc<Leitura>>| async move { Json(json!({ "removidos": l.downloads.clear_delivered().await })) }))
            .route("/pedidos/{id}", axum::routing::delete(|State(l): State<Arc<Leitura>>, Path(id): Path<String>| async move {
                match l.downloads.remove(&id).await {
                    Ok(()) => StatusCode::NO_CONTENT.into_response(),
                    Err(r) => refused(r),
                }
            }))
            .route("/pedidos/{id}/tentar", post(|State(l): State<Arc<Leitura>>, Path(id): Path<String>| async move {
                match l.downloads.retry(&id).await {
                    Ok(j) => (StatusCode::ACCEPTED, Json(json!(j))).into_response(),
                    Err(r) => refused(r),
                }
            }))
            .route("/pedidos/{id}/ingles", post(|State(l): State<Arc<Leitura>>, Path(id): Path<String>| async move {
                match l.downloads.accept_english(&id).await {
                    Some(j) => (StatusCode::ACCEPTED, Json(json!(j))).into_response(),
                    None => (StatusCode::NOT_FOUND, Json(json!({ "error": "pedido não existe" }))).into_response(),
                }
            }))
            .with_state(self)
    }
}

fn refused(r: crate::leitura_dl::Refused) -> Response {
    use crate::leitura_dl::Refused;
    let (code, msg) = match r {
        Refused::NotFound => (StatusCode::NOT_FOUND, "pedido não existe"),
        Refused::Busy => (StatusCode::CONFLICT, "o pedido ainda está procurando ou baixando"),
        Refused::Delivered => (StatusCode::CONFLICT, "o livro já está no Drive"),
    };
    (code, Json(json!({ "error": msg }))).into_response()
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
        let have: HashMap<String, String> = ["Sprawl #2 Count Zero", "Drácula - Bram Stoker", "Arsene Lupin (Coleção completa - 21 livros)"].iter().map(|t| (norm(t), t.to_string())).collect();
        assert_eq!(owned_path("Count Zero", &have).as_deref(), Some("Sprawl #2 Count Zero"));
        assert!(owned_path("Dracula", &have).is_some());
        assert!(owned_path("Arsène Lupin", &have).is_some());
        assert!(owned_path("Arsène Lupin, gentleman-cambrioleur", &have).is_some());
        assert!(owned_path("Idoru", &have).is_none());
    }
}

#[derive(serde::Deserialize)]
struct DownloadBody {
    title: String,
    author: Option<String>,
    categoria: String,
    /// artigo de acesso livre: o PDF
    pdf: Option<String>,
    /// a pasta do Drive (Livros: Ficção ou Técnicos); sem ela, a primeira da aba
    pasta: Option<String>,
}

async fn download(State(l): State<Arc<Leitura>>, Json(b): Json<DownloadBody>) -> Response {
    let Some((_, folders)) = tab_of(&b.categoria) else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "categoria desconhecida" }))).into_response();
    };
    // a pasta escolhida vale se é uma das que aparecem; senão, a da aba
    let all: Vec<&str> = TABS.iter().flat_map(|t| t.2.iter().copied()).collect();
    let folder = b.pasta.as_deref().filter(|p| all.contains(p)).unwrap_or(folders[0]);
    // o título original ajuda nos indexadores (quase todos em inglês)
    let alt = if b.pdf.is_some() { None } else { l.ol.find(&b.title, b.author.as_deref()).await.map(|w| w.title).filter(|t| norm(t) != norm(&b.title)) };
    let job = l.downloads.request(b.title, alt, b.author, folder.to_string(), b.pdf).await;
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
    let Some((tab, folders)) = tab_of(&categoria) else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "categoria desconhecida" }))).into_response();
    };
    let books = match l.drive.books().await {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_GATEWAY, Json(json!({ "error": format!("{e:#}") }))).into_response(),
    };
    let mut shelf = Vec::new();
    for b in books.iter().filter(|b| folders.contains(&b.category.as_str())) {
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
    let rows = l.rows(tab).await;
    let building = l.building.lock().await.contains(tab);
    let r = rows.as_deref().cloned().unwrap_or_default();
    // os temas na hora: os do sistema (última montagem) + a edição da pessoa
    let system = l.system_themes.lock().await.get(tab).cloned().unwrap_or_else(|| r.temas.iter().filter(|t| t.origem != crate::estudo::Origem::Voce).cloned().collect());
    let temas = l.temas.apply(tab, system).await;
    Json(json!({
        "estante": shelf, "preparando": rows.is_none(), "montando": building, "pastas": folders,
        "para_voce": r.para_voce, "destaques": r.destaques, "lancamentos": r.lancamentos, "populares": r.populares, "temas": temas,
    }))
    .into_response()
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
    for b in books.iter().filter(|b| visible(b)) {
        let hay = format!("{}{}", norm(&b.title), norm(b.author.as_deref().unwrap_or("")));
        if words.iter().all(|w| hay.contains(w.as_str())) {
            shelf.push(l.shelved(b).await);
        }
    }
    let owned: HashMap<String, String> = books.iter().map(|b| (norm(&b.title), b.path.clone())).collect();
    let works = match l.ol.search_works(term).await {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("Open Library ({term}): {e:#}");
            vec![]
        }
    };
    let works: Vec<Suggestion> = works
        .into_iter()
        .filter(|w| owned_path(&w.title, &owned).is_none() && !w.alternative_title.iter().any(|t| owned_path(t, &owned).is_some()))
        .map(|w| Suggestion {
            portuguese: w.has_portuguese(),
            cover: w.cover(),
            author: w.author_name.first().cloned().unwrap_or_default(),
            year: w.first_publish_year,
            link: crate::leitura_recs::link_of(&w.key),
            key: w.key,
            title: w.title,
            reason: String::new(),
            estante: None,
            pdf: None,
            pasta: None,
        })
        .collect();
    Json(json!({ "estante": shelf, "obras": works })).into_response()
}

/// Uma aba para o Kindle: as fileiras como seções de uma lista, o que está
/// lendo e a estante. "estante": o caminho no Drive (o plugin abre o livro).
async fn kindle_tab(State(l): State<Arc<Leitura>>, Path(categoria): Path<String>) -> Response {
    let Some((tab, folders)) = tab_of(&categoria) else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "categoria desconhecida" }))).into_response();
    };
    let books = match l.drive.books().await {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_GATEWAY, Json(json!({ "error": format!("{e:#}") }))).into_response(),
    };
    let mut reading = vec![];
    let mut shelf = vec![];
    for b in books.iter().filter(|b| folders.contains(&b.category.as_str())) {
        let s = l.shelved(b).await;
        let item = json!({ "title": b.title, "author": b.author, "estante": b.path, "progress": s.progress, "lido": s.lido });
        if s.progress.is_some() && !s.lido { reading.push(item) } else { shelf.push(item) }
    }
    let rows = l.rows(tab).await;
    let r = rows.as_deref().cloned().unwrap_or_default();
    let section = |title: &str, items: &[Suggestion]| json!({ "titulo": title, "itens": items });
    let mut secoes = vec![];
    if !reading.is_empty() {
        secoes.push(json!({ "titulo": "Lendo agora", "itens": reading }));
    }
    for (t, items) in [("Para você", &r.para_voce), ("Destaques", &r.destaques), ("Lançamentos", &r.lancamentos), ("Populares no Brasil", &r.populares)] {
        if !items.is_empty() {
            secoes.push(section(t, items));
        }
    }
    secoes.push(json!({ "titulo": "Na estante", "itens": shelf }));
    Json(json!({ "categoria": categoria, "pasta": folders[0], "secoes": secoes, "preparando": rows.is_none() })).into_response()
}

#[derive(serde::Deserialize)]
struct ThemeBody {
    nome: String,
}

/// Põe um tema na aba e monta as fileiras de novo (as de antes valem enquanto isso).
async fn add_theme(State(l): State<Arc<Leitura>>, Path(categoria): Path<String>, Json(b): Json<ThemeBody>) -> Response {
    let Some((tab, _)) = tab_of(&categoria) else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "categoria desconhecida" }))).into_response();
    };
    if let Err(e) = l.temas.add(tab, &b.nome).await {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": format!("{e:#}") }))).into_response();
    }
    l.rebuild(tab).await;
    StatusCode::ACCEPTED.into_response()
}

async fn remove_theme(State(l): State<Arc<Leitura>>, Path((categoria, nome)): Path<(String, String)>) -> Response {
    let Some((tab, _)) = tab_of(&categoria) else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "categoria desconhecida" }))).into_response();
    };
    l.temas.remove(tab, &nome).await;
    l.rebuild(tab).await;
    StatusCode::ACCEPTED.into_response()
}
