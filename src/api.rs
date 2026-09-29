//! API HTTP do Anubis (no link direto 10.10.10.x).
//!
//! GET  /                        — página de busca
//! GET  /search?q=TERMO          — filmes (Radarr) e séries (Sonarr)
//! POST /request                 — pede um item: {"kind":"movie"|"series","id":N}
//! GET  /status/{kind}/{id}      — procurando / baixando / disponível (+ link do Jellyfin)
//! GET  /status/{kind}/{id}/versions — pedido parado: o que os indexadores têm e por que foi recusado
//! POST /watch                   — "assistir agora": {"kind","id","guid"?}
//! GET  /watch/{kind}/{id}       — player
//! GET  /watch/{kind}/{id}/video — o vídeo (aceita Range; toca enquanto baixa)
//! GET  /watch/{kind}/{id}/info  — andamento do download, trechos já baixados e formato
//! GET  /watch/{kind}/{id}/lighter — versões mais leves (o download não acompanha o vídeo)
//! GET  /watch/{kind}/{id}/stream.mp4?t=S — convertido na hora (H.265, AC3…), a partir de S s
//! GET  /series/{tvdb}/episodes  — temporadas e episódios (para escolher o que assistir)
//! GET  /where/{kind}/{id}       — onde assistir oficialmente (streaming no Brasil, pelo TMDB)
//! GET  /mux/{tmdb}/options     — versões para juntar: vídeo e dublagem PT-BR
//! POST /mux                     — juntar dublagem: {"id":tmdb,"video":guid,"audio":guid}
//! GET  /mux/{tmdb}  ·  DELETE   — andamento · desistir (apaga os downloads)
//! GET  /preferencias            — página de preferências
//! GET  /prefs  ·  PUT /prefs     — idioma do áudio (filmes/séries) e legendas
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
        .route("/preferencias", get(|| async { Html(include_str!("../web/prefs.html")) }))
        .route("/prefs", get(get_prefs).put(put_prefs))
        .route("/request", post(request))
        .route("/status/{kind}/{id}", get(status))
        .route("/status/{kind}/{id}/versions", get(versions))
        .route("/watch", post(watch))
        .route("/watch/{kind}/{id}", get(|| async { Html(include_str!("../web/player.html")) }))
        .route("/watch/{kind}/{id}/video", get(video))
        .route("/watch/{kind}/{id}/info", get(watch_info))
        .route("/watch/{kind}/{id}/lighter", get(lighter))
        .route("/watch/{kind}/{id}/stream.mp4", get(converted))
        .route("/watch/{kind}/{id}/subs", get(subs_list))
        .route("/watch/{kind}/{id}/subs/{n}", get(sub_vtt))
        .route("/watch/{kind}/{id}/subs/{n}/raw", get(sub_raw))
        .route("/series/{tvdb}/episodes", get(episodes))
        .route("/where/{kind}/{id}", get(where_to_watch))
        .route("/mux", post(start_mux))
        .route("/mux/{tmdb}", get(mux_status).delete(cancel_mux))
        .route("/mux/{tmdb}/options", get(mux_options))
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

async fn versions(State(s): State<AppState>, Path((kind, id)): Path<(Kind, i64)>) -> Response {
    match s.orch.versions(kind, id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

#[derive(serde::Deserialize)]
struct WatchBody {
    kind: Kind,
    id: i64,
    guid: Option<String>,
    /// tocar o que está na biblioteca sem oferecer troca
    #[serde(default)]
    keep: bool,
}

async fn watch(State(s): State<AppState>, Json(b): Json<WatchBody>) -> Response {
    match s.orch.watch(b.kind, b.id, b.guid.as_deref(), b.keep).await {
        Ok(Started::Ready) => Json(json!({ "player": format!("watch/{}/{}", kind_str(b.kind), b.id) })).into_response(),
        Ok(Started::NoApproved(list)) => (StatusCode::CONFLICT, Json(json!({ "no_approved": true, "candidates": list }))).into_response(),
        Ok(Started::Better { current, candidates }) => (StatusCode::CONFLICT, Json(json!({ "better": true, "current": current, "candidates": candidates }))).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

fn kind_str(k: Kind) -> &'static str {
    match k {
        Kind::Movie => "movie",
        Kind::Series => "series",
        Kind::Episode => "episode",
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
    info["title"] = json!(job.info.title);
    info["release"] = json!(job.info.release);
    info["file"] = json!(job.info.file_name);
    // com a duração, dá quanto o vídeo consome por segundo (o player compara com a velocidade)
    info["size"] = json!(job.info.file_len);
    // link do Jellyfin quando já está na biblioteca
    if job.phase().0 == Phase::Done {
        info["jellyfin_url"] = json!(s.orch.watch_jellyfin_url(&job).await);
    }
    Json(info).into_response()
}

/// Versões mais leves que a que está baixando (o download não acompanha).
async fn lighter(State(s): State<AppState>, Path((kind, id)): Path<(Kind, i64)>) -> Response {
    let job = match job_of(&s, kind, id).await {
        Ok(j) => j,
        Err(r) => return r,
    };
    match s.orch.lighter(&job).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
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
    let len = job.info.file_len;
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
    h.insert("content-type", content_type(&job.info.file_name).parse().unwrap());
    h.insert("accept-ranges", "bytes".parse().unwrap());
    h.insert("content-length", (end - start + 1).to_string().parse().unwrap());
    if partial {
        h.insert("content-range", format!("bytes {start}-{end}/{len}").parse().unwrap());
    }
    res
}

#[derive(serde::Deserialize)]
struct StartAt {
    #[serde(default)]
    t: f64,
}

/// O vídeo convertido na hora, a partir de `t` segundos (o player pula
/// recomeçando daqui: vídeo convertido não tem "faixa de bytes").
async fn converted(State(s): State<AppState>, Path((kind, id)): Path<(Kind, i64)>, Query(q): Query<StartAt>) -> Response {
    let job = match job_of(&s, kind, id).await {
        Ok(j) => j,
        Err(r) => return r,
    };
    let Some(streamer) = s.orch.streamer.as_ref() else { return error(StatusCode::NOT_FOUND, "assistir agora desligado") };
    let (child, out) = match streamer.transcode(&job, q.t.max(0.0)) {
        Ok(x) => x,
        Err(e) => return error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    };
    // o ffmpeg vive enquanto o navegador lê; fechou a conexão, ele é encerrado
    let body = Body::from_stream(WithChild { inner: ReaderStream::with_capacity(out, 256 * 1024), _child: child });
    let mut res = Response::new(body);
    res.headers_mut().insert("content-type", "video/mp4".parse().unwrap());
    res.headers_mut().insert("cache-control", "no-store".parse().unwrap());
    res
}

/// Stream que mantém o processo (ffmpeg) vivo enquanto é lido.
struct WithChild {
    inner: ReaderStream<tokio::process::ChildStdout>,
    _child: tokio::process::Child,
}

impl futures_core::Stream for WithChild {
    type Item = std::io::Result<bytes::Bytes>;
    fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}

async fn subs_list(State(s): State<AppState>, Path((kind, id)): Path<(Kind, i64)>) -> Response {
    let job = match job_of(&s, kind, id).await {
        Ok(j) => j,
        Err(r) => return r,
    };
    let list = s.orch.streamer.as_ref().map(|st| st.subtitles(&job)).unwrap_or_default();
    Json(list).into_response()
}

#[derive(serde::Deserialize)]
struct Shift {
    #[serde(default)]
    shift: f64,
}

async fn sub_vtt(State(s): State<AppState>, Path((kind, id, n)): Path<(Kind, i64, String)>, Query(q): Query<Shift>) -> Response {
    let job = match job_of(&s, kind, id).await {
        Ok(j) => j,
        Err(r) => return r,
    };
    let Some(n) = n.trim_end_matches(".vtt").parse().ok() else { return error(StatusCode::NOT_FOUND, "legenda inválida") };
    let Some(streamer) = s.orch.streamer.as_ref() else { return error(StatusCode::NOT_FOUND, "assistir agora desligado") };
    match streamer.subtitle_vtt(&job, n, q.shift).await {
        Ok(vtt) => ([("content-type", "text/vtt; charset=utf-8")], vtt).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn sub_raw(State(s): State<AppState>, Path((kind, id, n)): Path<(Kind, i64, usize)>) -> Response {
    let job = match job_of(&s, kind, id).await {
        Ok(j) => j,
        Err(r) => return r,
    };
    let Some(streamer) = s.orch.streamer.as_ref() else { return error(StatusCode::NOT_FOUND, "assistir agora desligado") };
    match streamer.subtitle_raw(&job, n).await {
        Ok(b) => ([("content-type", "text/plain; charset=utf-8")], b).into_response(),
        Err(e) => error(StatusCode::NOT_FOUND, format!("{e:#}")),
    }
}

async fn get_prefs(State(s): State<AppState>) -> Response {
    match s.orch.prefs().await {
        Ok(p) => Json(p).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn put_prefs(State(s): State<AppState>, Json(p): Json<crate::prefs::Prefs>) -> Response {
    match s.orch.set_prefs(&p).await {
        Ok(p) => Json(p).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn episodes(State(s): State<AppState>, Path(tvdb): Path<i64>) -> Response {
    match s.orch.episodes(tvdb).await {
        Ok(e) => Json(e).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn where_to_watch(State(s): State<AppState>, Path((kind, id)): Path<(Kind, i64)>) -> Response {
    if s.orch.tmdb.is_none() {
        return error(StatusCode::NOT_FOUND, "configure [tmdb] para ver onde assistir");
    }
    match s.orch.where_to_watch(kind, id).await {
        Ok(w) => Json(w).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn mux_options(State(s): State<AppState>, Path(tmdb): Path<i64>) -> Response {
    match s.orch.mux_options(tmdb).await {
        Ok(o) => Json(o).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

#[derive(serde::Deserialize)]
struct MuxBody {
    id: i64,
    video: String,
    audio: String,
}

async fn start_mux(State(s): State<AppState>, Json(b): Json<MuxBody>) -> Response {
    match s.orch.start_mux(b.id, &b.video, &b.audio).await {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({ "status": "baixando" }))).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn mux_status(State(s): State<AppState>, Path(tmdb): Path<i64>) -> Response {
    match s.orch.mux_status(tmdb).await {
        Some(v) => Json(v).into_response(),
        None => error(StatusCode::NOT_FOUND, "nada sendo juntado aqui"),
    }
}

async fn cancel_mux(State(s): State<AppState>, Path(tmdb): Path<i64>) -> Response {
    match s.orch.cancel_mux(tmdb).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => error(StatusCode::CONFLICT, format!("{e:#}")),
    }
}

async fn torrent_finished(State(s): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&s, &headers) {
        return error(StatusCode::UNAUTHORIZED, "token inválido");
    }
    s.wake.notify_one();
    (StatusCode::ACCEPTED, Json(json!({ "status": "agendado" }))).into_response()
}
