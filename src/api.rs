//! API HTTP do Anubis (no link direto 10.10.10.x).
//!
//! GET  /                        — página de busca
//! GET  /search?q=TERMO          — filmes (Radarr) e séries (Sonarr)
//! POST /request                 — pede um item: {"kind":"movie"|"series","id":N}
//! GET  /status/{kind}/{id}      — procurando / baixando / disponível (+ link do Jellyfin)
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

async fn torrent_finished(State(s): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&s, &headers) {
        return error(StatusCode::UNAUTHORIZED, "token inválido");
    }
    s.wake.notify_one();
    (StatusCode::ACCEPTED, Json(json!({ "status": "agendado" }))).into_response()
}
