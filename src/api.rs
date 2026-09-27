//! API HTTP do Anubis (no link direto 10.10.10.x).
//!
//! GET  /health                  — vivo?
//! GET  /storage                 — uso do disco, regras e o último ciclo
//! GET  /storage/plan            — o que seria apagado agora (não apaga)
//! POST /storage/run             — roda um ciclo agora (respeita dry_run)
//! POST /hooks/torrent-finished  — gatilho do qBittorrent ao concluir um download
//!
//! As rotas POST exigem `Authorization: Bearer <token>` quando há token na config.

use crate::disk;
use crate::orchestrator::{Orchestrator, Report};
use axum::extract::State;
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

async fn torrent_finished(State(s): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&s, &headers) {
        return error(StatusCode::UNAUTHORIZED, "token inválido");
    }
    s.wake.notify_one();
    (StatusCode::ACCEPTED, Json(json!({ "status": "agendado" }))).into_response()
}
