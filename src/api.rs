//! API HTTP do Anubis (no link direto 10.10.10.x).
//!
//! GET  /                        — página de busca
//! GET  /search?q=TERMO          — filmes (Radarr) e séries (Sonarr)
//! POST /request                 — pede um item: {"kind":"movie"|"series","id":N}
//! GET  /status/{kind}/{id}      — procurando / baixando / disponível (+ link do Jellyfin)
//! POST /watch                   — "assistir agora": {"kind","id","guid"?}
//! GET  /watch/{kind}/{id}       — player
//! GET  /watch/{kind}/{id}/video — o vídeo (aceita Range; toca enquanto baixa)
//! GET  /watch/{kind}/{id}/info  — andamento do download e trechos já baixados
//! GET  /health                  — vivo?
//! GET  /storage                 — uso do disco, regras e o último ciclo
//! GET  /storage/plan            — o que seria apagado agora (não apaga)
//! POST /storage/run             — roda um ciclo agora (respeita dry_run)
//! POST /hooks/torrent-finished  — gatilho do qBittorrent ao concluir um download
//!
//! `/storage/run` e o gancho exigem `Authorization: Bearer <token>` quando há
//! token na config. A busca e os pedidos ficam abertos na rede local: pedir um
//! filme não apaga nada, e as regras de qualidade continuam as do Radarr/Sonarr.

use crate::disk;
use crate::orchestrator::{Orchestrator, Report};
use crate::search::Kind;
use crate::stream::{self, Job, Phase, Started};
use axum::body::Body;
use tokio_util::io::ReaderStream;
use axum::extract::{Path, Query, State};
use axum::response::Html;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Notify;

#[derive(Clone)]
pub struct AppState {
    pub orch: Arc<Orchestrator>,
    /// acorda o agendador (gatilho por evento)
    pub wake: Arc<Notify>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { Html(include_str!("../web/index.html")) }))
        .route("/search", get(search))
        .route("/request", post(request))
        .route("/status/{kind}/{id}", get(status))
        .route("/watch", post(watch))
        .route("/watch/{kind}/{id}", get(|| async { Html(include_str!("../web/player.html")) }))
        .route("/watch/{kind}/{id}/video", get(video))
        .route("/watch/{kind}/{id}/info", get(watch_info))
        .route("/health", get(|| async { Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") })) }))
        .route("/storage", get(storage))
        .route("/storage/plan", get(plan))
        .route("/storage/run", post(run))
        .route("/hooks/torrent-finished", post(torrent_finished))
        .with_state(state)
}

fn authorized(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(token) = &state.orch.cfg.server.token else { return true };
    headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")) == Some(token.as_str())
}

fn error(status: StatusCode, msg: impl ToString) -> Response {
    (status, Json(json!({ "error": msg.to_string() }))).into_response()
}

async fn storage(State(s): State<AppState>) -> Response {
    let cfg = &s.orch.cfg.storage;
    let usage = match disk::usage(&cfg.path) {
        Ok(u) => u,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let last = s.orch.last.lock().await;
    Json(json!({
        "path": cfg.path,
        "total": usage.total,
        "avail": usage.avail,
        "used_percent": usage.used_percent(),
        "rules": { "start_percent": cfg.start_percent, "stop_percent": cfg.stop_percent, "min_age_days": cfg.min_age_days },
        "dry_run": cfg.dry_run,
        "last_cycle": *last,
    }))
    .into_response()
}

fn report(r: anyhow::Result<Report>) -> Response {
    match r {
        Ok(r) => Json(r).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn plan(State(s): State<AppState>) -> Response {
    report(s.orch.cycle(false).await)
}

async fn run(State(s): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&s, &headers) {
        return error(StatusCode::UNAUTHORIZED, "token inválido");
    }
    report(s.orch.cycle(true).await)
}

#[derive(serde::Deserialize)]
struct SearchQuery {
    q: String,
}

async fn search(State(s): State<AppState>, Query(q): Query<SearchQuery>) -> Response {
    let term = q.q.trim();
    if term.len() < 2 {
        return error(StatusCode::BAD_REQUEST, "digite ao menos 2 letras");
    }
    Json(s.orch.search(term).await).into_response()
}

#[derive(serde::Deserialize)]
struct RequestBody {
    kind: Kind,
    id: i64,
}

async fn request(State(s): State<AppState>, Json(b): Json<RequestBody>) -> Response {
    match s.orch.request(b.kind, b.id).await {
        Ok(st) => Json(st).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn status(State(s): State<AppState>, Path((kind, id)): Path<(Kind, i64)>) -> Response {
    match s.orch.status(kind, id).await {
        Ok(st) => Json(st).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

#[derive(serde::Deserialize)]
struct WatchBody {
    kind: Kind,
    id: i64,
    guid: Option<String>,
}

async fn watch(State(s): State<AppState>, Json(b): Json<WatchBody>) -> Response {
    match s.orch.watch(b.kind, b.id, b.guid.as_deref()).await {
        Ok(Started::Ready) => Json(json!({ "player": format!("watch/{}/{}", kind_str(b.kind), b.id) })).into_response(),
        Ok(Started::NoApproved(list)) => (StatusCode::CONFLICT, Json(json!({ "no_approved": true, "candidates": list }))).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

fn kind_str(k: Kind) -> &'static str {
    match k {
        Kind::Movie => "movie",
        Kind::Series => "series",
    }
}

async fn job_of(s: &AppState, kind: Kind, id: i64) -> Result<Arc<Job>, Response> {
    let Some(streamer) = s.orch.streamer.as_ref() else { return Err(error(StatusCode::NOT_FOUND, "assistir agora desligado")) };
    streamer.job(kind, id).await.ok_or_else(|| error(StatusCode::NOT_FOUND, "nada sendo assistido aqui — volte à busca"))
}

async fn watch_info(State(s): State<AppState>, Path((kind, id)): Path<(Kind, i64)>) -> Response {
    let job = match job_of(&s, kind, id).await {
        Ok(j) => j,
        Err(r) => return r,
    };
    let mut info = s.orch.streamer.as_ref().map(|st| st.info(&job)).unwrap_or_default();
    info["title"] = json!(job.title);
    info["release"] = json!(job.release);
    info["file"] = json!(job.file_name);
    // link do Jellyfin quando já está na biblioteca
    if job.phase().0 == Phase::Done
        && let Ok(st) = s.orch.status(kind, id).await
    {
        info["jellyfin_url"] = json!(st.jellyfin_url);
    }
    Json(info).into_response()
}

/// Faixa pedida pelo player (`Range: bytes=INÍCIO-FIM`); sem Range, o arquivo todo.
fn byte_range(headers: &HeaderMap, len: u64) -> Option<(u64, u64)> {
    if len == 0 {
        return None;
    }
    let Some(v) = headers.get("range").and_then(|v| v.to_str().ok()) else { return Some((0, len - 1)) };
    // só a primeira faixa: players de vídeo não pedem várias
    let (a, b) = v.strip_prefix("bytes=")?.split(',').next()?.split_once('-')?;
    let (start, end) = match (a.trim(), b.trim()) {
        ("", suffix) => {
            let n: u64 = suffix.parse().ok()?;
            (len.saturating_sub(n), len - 1)
        }
        (a, "") => (a.parse().ok()?, len - 1),
        (a, b) => (a.parse().ok()?, b.parse::<u64>().ok()?.min(len - 1)),
    };
    (start <= end && end < len).then_some((start, end))
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next().map(str::to_lowercase).as_deref() {
        Some("mp4" | "m4v") => "video/mp4",
        // o Chromium toca Matroska com H.264/AAC anunciado como WebM
        Some("mkv" | "webm") => "video/webm",
        Some("mov") => "video/quicktime",
        _ => "application/octet-stream",
    }
}

async fn video(State(s): State<AppState>, Path((kind, id)): Path<(Kind, i64)>, headers: HeaderMap) -> Response {
    let job = match job_of(&s, kind, id).await {
        Ok(j) => j,
        Err(r) => return r,
    };
    let len = job.file_len;
    let Some((start, end)) = byte_range(&headers, len) else {
        return (StatusCode::RANGE_NOT_SATISFIABLE, [("content-range", format!("bytes */{len}"))]).into_response();
    };
    let Some(streamer) = s.orch.streamer.as_ref() else { return error(StatusCode::NOT_FOUND, "assistir agora desligado") };
    let reader = match streamer.open(&job, start).await {
        Ok(r) => r,
        Err(e) => return error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    };
    let body = Body::from_stream(ReaderStream::with_capacity(stream::limit(reader, end - start + 1), 256 * 1024));
    let partial = headers.contains_key("range");
    let mut res = Response::new(body);
    *res.status_mut() = if partial { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK };
    let h = res.headers_mut();
    h.insert("content-type", content_type(&job.file_name).parse().unwrap());
    h.insert("accept-ranges", "bytes".parse().unwrap());
    h.insert("content-length", (end - start + 1).to_string().parse().unwrap());
    if partial {
        h.insert("content-range", format!("bytes {start}-{end}/{len}").parse().unwrap());
    }
    res
}

async fn torrent_finished(State(s): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&s, &headers) {
        return error(StatusCode::UNAUTHORIZED, "token inválido");
    }
    s.wake.notify_one();
    (StatusCode::ACCEPTED, Json(json!({ "status": "agendado" }))).into_response()
}
