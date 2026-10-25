//! API HTTP do Anubis (no link direto 10.10.10.x).
//!
//! GET  /                        — o app (vídeos; /leitura, /preferencias e /watch/… também)
//! GET  /search?q=TERMO          — filmes (Radarr) e séries (Sonarr)
//! POST /request                 — pede um item: {"kind":"movie"|"series","id":N}
//! GET  /jellyfin/open?next=/details?id=X — abre o Jellyfin já logado (sessão deste navegador)
//! GET  /discover/{filmes|series|animes}[?bg=1] — vitrine: prontos para assistir, destaques, lançamentos, populares no Brasil
//!                                 (bg=1: pré-carga, sem passar a aba na frente da checagem)
//! GET  /availability            — o que as fontes têm de cada título checado + fila e estimativa
//! POST /availability/{kind}/{id} — checa esse título antes dos outros e devolve o resultado (até 90 s)
//! GET  /events?items=movie:ID,series:ID — estado ao vivo (SSE) desses itens
//! GET  /requests                — pedidos e biblioteca (a lista "Pedidos" da página)
//! GET  /status/{kind}/{id}      — procurando / baixando / disponível (+ link do Jellyfin)
//! GET  /status/{kind}/{id}/versions — pedido parado: o que os indexadores têm e por que foi recusado
//! POST /watch                   — "assistir agora": {"kind","id","guid"?}
//! GET  /icones.woff2           — ícones das páginas
//! GET  /assets/{arquivo}         — o front-end (web/app, Vue), embutido
//! GET  /watch/{kind}/{id}       — player
//! GET  /watch/{kind}/{id}/video — o vídeo (aceita Range; toca enquanto baixa)
//! GET  /watch/{kind}/{id}/info  — andamento do download, trechos já baixados e formato
//! GET  /watch/{kind}/{id}/lighter — versões mais leves (o download não acompanha o vídeo)
//! GET  /watch/{kind}/{id}/stream.mp4?t=S&a=N&h=A — convertido na hora (H.265, AC3…), a partir de S s, faixa de áudio N, altura máxima A
//! GET  /watch/{kind}/{id}/progress — onde o usuário parou no Jellyfin
//! GET  /watch/{kind}/{id}/candidates — as versões que o "assistir" consideraria, na ordem (sem baixar)
//! POST /watch/{kind}/{id}/progress {"t", "duration"} — o player avisa onde está; grava no Jellyfin
//! GET  /series/{tvdb}/episodes  — temporadas e episódios (para escolher o que assistir)
//! GET  /series/{tvdb}/availability — o que as fontes têm de cada episódio que falta (202 enquanto avalia)
//! POST /series/{tvdb}/subbed    — sem dublado em nenhuma fonte: aceita o legendado
//! POST /movie/{tmdb}/subbed      — o mesmo para filmes
//! GET /hidden · POST/DELETE /hidden/{kind}/{id} — "não quero ver" (fora da vitrine)
//! GET /liked · POST/DELETE /liked/{kind}/{id} — "gostei" (semente do "Para você")
//! GET/DELETE /library/{kind}/{id} — o que sai ao desinstalar o título / desinstala (filme, série ou episódio)
//! GET  /indexers · POST /indexers/test — indexadores fora do ar · testar de novo (tira do castigo)
//! GET  /where/{kind}/{id}       — onde assistir oficialmente (streaming no Brasil, pelo TMDB)
//! GET  /autodub                — dublagem automática: o que sabe de cada título
//! POST /autodub/{kind}/{id}    — dublagem na hora: movie (tmdb), episode (Sonarr), series (tvdb)
//! GET  /preferencias            — página de preferências
//! GET  /prefs  ·  PUT /prefs     — idioma do áudio (filmes/séries) e legendas
//! GET  /history                 — histórico de outros serviços (Netflix, AniList)
//! POST /history/netflix  · DELETE — importa o CSV da Netflix (corpo = o arquivo) · apaga
//! POST /history/crunchyroll · DELETE — importa o arquivo do botão da Crunchyroll · apaga
//! PUT  /history/anilist         — {"user": "nome"} (vazio tira)
//! GET  /health                  — vivo?
//! GET  /storage                 — uso do disco, regras e o último ciclo
//! GET  /storage/plan            — o que seria apagado agora (não apaga)
//! POST /storage/run             — roda um ciclo agora (respeita dry_run)
//! POST /hooks/torrent-finished  — gatilho do qBittorrent ao concluir um download
//! POST /hooks/arr?token=T       — webhook do Radarr/Sonarr: a central refaz as listas
//! /kosync/…                     — progresso de leitura do KOReader (src/kosync.rs)
//! /leitura/…                    — página de leitura, separada da de vídeo (src/leitura.rs)
//!
//! `/storage/run` e o gancho exigem `Authorization: Bearer <token>` quando há
//! token na config. A busca e os pedidos ficam abertos na rede local: pedir um
//! filme não apaga nada, e as regras de qualidade continuam as do Radarr/Sonarr.

use tracing::warn;
use crate::disk;
use crate::orchestrator::{Orchestrator, Report};
use crate::search::Kind;
use crate::stream::{self, Job, Phase, Started};
use axum::body::Body;
use tokio_util::io::ReaderStream;
use axum::extract::{Path, Query, State};
use axum::response::Html;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
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
    /// progresso de leitura do KOReader (com `[kosync]` na config)
    pub kosync: Option<Arc<crate::kosync::Kosync>>,
    /// página de leitura (com `[leitura]` na config)
    pub leitura: Option<Arc<crate::leitura::Leitura>>,
}

pub fn router(state: AppState) -> Router {
    let kosync = state.kosync.clone();
    let leitura = state.leitura.clone();
    let app = Router::new()
        .route("/", get(app_page))
        .route("/search", get(search))
        .route("/preferencias", get(app_page))
        .route("/icones.woff2", get(icons))
        .route("/assets/{file}", get(asset))
        .route("/prefs", get(get_prefs).put(put_prefs))
        .route("/history", get(|State(s): State<AppState>| async move { Json(s.orch.history_summary().await) }))
        .route("/history/netflix", post(import_netflix).delete(clear_netflix).layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024)))
        .route("/history/anilist", axum::routing::put(set_anilist))
        .route("/hidden", get(|State(s): State<AppState>| async move { Json(s.orch.hidden().await) }))
        .route("/hidden/{kind}/{id}", post(hide).delete(unhide))
        .route("/library/{kind}/{id}", get(uninstall_preview).delete(uninstall))
        .route("/liked", get(|State(s): State<AppState>| async move { Json(s.orch.liked().await) }))
        .route("/liked/{kind}/{id}", post(like).delete(unlike))
        .route("/history/crunchyroll", post(import_crunchyroll).delete(clear_crunchyroll).layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024)))
        .route("/request", post(request))
        .route("/events", get(events))
        .route("/discover/{category}", get(discover))
        .route("/availability", get(availability))
        .route("/availability/{kind}/{id}", post(availability_now))
        .route("/indexers", get(indexers))
        .route("/indexers/test", post(indexers_test))
        .route("/jellyfin/open", get(jellyfin_open))
        .route("/requests", get(|State(s): State<AppState>| async move { Json(s.orch.requests().await) }))
        .route("/status/{kind}/{id}", get(status))
        .route("/status/{kind}/{id}/versions", get(versions))
        .route("/watch", post(watch))
        .route("/watch/{kind}/{id}", get(app_page))
        .route("/watch/{kind}/{id}/video", get(video))
        .route("/watch/{kind}/{id}/info", get(watch_info))
        .route("/watch/{kind}/{id}/lighter", get(lighter))
        .route("/watch/{kind}/{id}/better", get(better))
        .route("/watch/{kind}/{id}/stream.mp4", get(converted))
        .route("/watch/{kind}/{id}/progress", get(get_progress).post(post_progress))
        .route("/watch/{kind}/{id}/candidates", get(watch_candidates))
        .route("/watch/{kind}/{id}/subs", get(subs_list))
        .route("/watch/{kind}/{id}/subs/{n}", get(sub_vtt))
        .route("/watch/{kind}/{id}/subs/{n}/raw", get(sub_raw))
        .route("/series/{tvdb}/episodes", get(episodes))
        .route("/series/{tvdb}/availability", get(episode_availability))
        .route("/series/{tvdb}/subbed", post(series_subbed))
        .route("/movie/{tmdb}/subbed", post(movie_subbed))
        .route("/where/{kind}/{id}", get(where_to_watch))
        .route("/autodub", get(|State(s): State<AppState>| async move { Json(s.orch.autodub_state().await) }))
        .route("/autodub/{kind}/{id}", post(autodub_now))
        .route("/health", get(health))
        .route("/storage", get(storage))
        .route("/storage/plan", get(plan))
        .route("/storage/run", post(run))
        .route("/hooks/torrent-finished", post(torrent_finished))
        .route("/hooks/arr", post(arr_event))
        // qualquer acesso ao site (fora os avisos do Radarr/Sonarr/qBittorrent)
        // pausa a varredura da DHT
        .layer(axum::middleware::from_fn_with_state(state.clone(), activity))
        .with_state(state);
    // fora do middleware acima: o Kindle sincronizando não é alguém no site
    let app = match kosync {
        // o Kindle navega pelas abas da Leitura pelo mesmo endereço público
        Some(k) => app.nest_service("/kosync", match &leitura {
            Some(l) => k.clone().router().merge(l.clone().kindle_router(k)),
            None => k.router(),
        }),
        None => app,
    };
    match leitura {
        Some(l) => app.nest_service("/leitura", l.router()),
        None => app,
    }
}

async fn activity(State(s): State<AppState>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    if !req.uri().path().starts_with("/hooks/") {
        s.orch.dht_touch();
    }
    next.run(req).await
}

fn authorized(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(token) = &state.orch.cfg.server.token else { return true };
    headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")) == Some(token.as_str())
}

fn error(status: StatusCode, msg: impl ToString) -> Response {
    (status, Json(json!({ "error": msg.to_string() }))).into_response()
}

/// Vivo? E os endereços em que o Anubis escuta, do caminho mais rápido ao
/// mais lento (a página troca para o melhor que o aparelho alcança).
async fn health(State(s): State<AppState>) -> Response {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION"), "addresses": s.orch.cfg.server.listen, "upstream_calls": crate::services::UPSTREAM_CALLS.load(std::sync::atomic::Ordering::Relaxed) })).into_response()
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
        // os fora do ar agora (as versões ficam guardadas 15 min; eles, 1 min)
        Ok(v) => {
            let mut v = json!(v);
            v["indexers_down"] = json!(s.orch.indexers_down().await);
            Json(v).into_response()
        }
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
        Ok(Started::NoApproved(list, dead)) => {
            // consultados agora: a busca que acabou pode ter derrubado algum
            let down = s.orch.refresh_indexers().await;
            (StatusCode::CONFLICT, Json(json!({ "no_approved": true, "candidates": list, "dead": dead, "indexers_down": down }))).into_response()
        }
        Err(e) => {
            warn!("assistir {}/{}: {e:#}", kind_str(b.kind), b.id);
            error(StatusCode::BAD_GATEWAY, format!("{e:#}"))
        }
    }
}

async fn indexers(State(s): State<AppState>) -> Response {
    Json(json!({ "down": s.orch.indexers_down().await })).into_response()
}

async fn indexers_test(State(s): State<AppState>) -> Response {
    Json(json!({ "down": s.orch.retest_indexers(&Default::default()).await })).into_response()
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
    // altura em que o vídeo convertido sai sem escolha no player (o menu de
    // qualidade mostra "Automática (720p)")
    info["transcode_height"] = json!(s.orch.streamer.as_ref().map(|st| st.transcode.max_height));
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

async fn better(State(s): State<AppState>, Path((kind, id)): Path<(Kind, i64)>) -> Response {
    let job = match job_of(&s, kind, id).await {
        Ok(j) => j,
        Err(r) => return r,
    };
    match s.orch.better(&job).await {
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

async fn watch_candidates(State(s): State<AppState>, Path((kind, id)): Path<(Kind, i64)>) -> Response {
    match s.orch.watch_candidates(kind, id).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

/// Onde o usuário parou no Jellyfin (`t` em s; `null` sem progresso).
async fn get_progress(State(s): State<AppState>, Path((kind, id)): Path<(Kind, i64)>) -> Response {
    let job = match job_of(&s, kind, id).await {
        Ok(j) => j,
        Err(r) => return r,
    };
    Json(json!({ "t": s.orch.saved_position(&job).await })).into_response()
}

#[derive(serde::Deserialize)]
struct ProgressBody {
    t: f64,
    duration: f64,
}

/// O player avisa onde está; o Anubis grava no Jellyfin.
async fn post_progress(State(s): State<AppState>, Path((kind, id)): Path<(Kind, i64)>, Json(b): Json<ProgressBody>) -> Response {
    let job = match job_of(&s, kind, id).await {
        Ok(j) => j,
        Err(r) => return r,
    };
    match s.orch.save_progress(&job, b.t, b.duration).await {
        Ok(saved) => Json(json!({ "saved": saved })).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

#[derive(serde::Deserialize)]
struct StartAt {
    #[serde(default)]
    t: f64,
    /// faixa de áudio (0:a:N)
    a: Option<usize>,
    /// altura máxima do vídeo (qualidade escolhida no player)
    h: Option<u32>,
}

/// O vídeo convertido na hora, a partir de `t` segundos (o player pula
/// recomeçando daqui: vídeo convertido não tem "faixa de bytes"), com a faixa
/// de áudio `a` e a altura `h` escolhidas no player.
async fn converted(State(s): State<AppState>, Path((kind, id)): Path<(Kind, i64)>, Query(q): Query<StartAt>) -> Response {
    let job = match job_of(&s, kind, id).await {
        Ok(j) => j,
        Err(r) => return r,
    };
    let Some(streamer) = s.orch.streamer.as_ref() else { return error(StatusCode::NOT_FOUND, "assistir agora desligado") };
    let (child, out) = match streamer.transcode(&job, q.t.max(0.0), q.a, q.h) {
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

async fn import_netflix(State(s): State<AppState>, body: String) -> Response {
    match s.orch.import_netflix(&body).await {
        // a busca no TMDB segue em segundo plano; GET /history mostra o andamento
        Ok(total) => (StatusCode::ACCEPTED, Json(json!({ "total": total }))).into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

async fn import_crunchyroll(State(s): State<AppState>, body: String) -> Response {
    match s.orch.import_crunchyroll(&body).await {
        // a busca no TMDB segue em segundo plano; GET /history mostra o andamento
        Ok(total) => (StatusCode::ACCEPTED, Json(json!({ "total": total }))).into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

async fn clear_crunchyroll(State(s): State<AppState>) -> Response {
    s.orch.clear_crunchyroll().await;
    StatusCode::NO_CONTENT.into_response()
}

async fn hide(State(s): State<AppState>, Path((kind, id)): Path<(Kind, i64)>) -> Response {
    match s.orch.hide(kind, id).await {
        Ok(h) => Json(h).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn unhide(State(s): State<AppState>, Path((kind, id)): Path<(Kind, i64)>) -> Response {
    s.orch.unhide(kind, id).await;
    StatusCode::NO_CONTENT.into_response()
}

/// O front-end (web/app, em Vue; montado por web/app/build.sh em web/dist),
/// embutido no binário. Um app de página única: `/`, `/leitura`,
/// `/preferencias` e `/watch/…` devolvem a mesma página, e ele mostra a tela
/// certa sem recarregar ao navegar.
static DIST: include_dir::Dir = include_dir::include_dir!("$CARGO_MANIFEST_DIR/web/dist");

pub async fn app_page() -> Response {
    match DIST.get_file("index.html").and_then(|f| f.contents_utf8()) {
        Some(html) => page(html),
        None => (StatusCode::INTERNAL_SERVER_ERROR, "front-end não montado (web/app/build.sh)").into_response(),
    }
}

/// Arquivos do build: o nome muda com o conteúdo, então podem ficar em cache para sempre.
/// O build.sh deixa um `.gz` ao lado de cada .js e .css: se o navegador aceita
/// gzip, vai esse (pronto, sem gastar CPU na hora).
async fn asset(Path(file): Path<String>, headers: HeaderMap) -> Response {
    if file.ends_with(".gz") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let gz = accepts_gzip(&headers).then(|| DIST.get_file(format!("assets/{file}.gz"))).flatten();
    let Some(f) = gz.or_else(|| DIST.get_file(format!("assets/{file}"))) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let kind = match file.rsplit('.').next() {
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("woff2") => "font/woff2",
        Some("png") => "image/png",
        _ => "application/octet-stream",
    };
    let mut res = ([("content-type", kind), ("cache-control", "public, max-age=31536000, immutable"), ("vary", "accept-encoding")], f.contents()).into_response();
    if gz.is_some() {
        res.headers_mut().insert("content-encoding", HeaderValue::from_static("gzip"));
    }
    res
}

/// O navegador aceita gzip? (`Accept-Encoding: gzip, deflate, br`; `gzip;q=0` é recusa)
fn accepts_gzip(headers: &HeaderMap) -> bool {
    let accepted = headers.get("accept-encoding").and_then(|v| v.to_str().ok()).unwrap_or_default();
    accepted.split(',').any(|e| {
        let mut parts = e.trim().split(';');
        parts.next() == Some("gzip") && parts.next().map(str::trim) != Some("q=0")
    })
}

/// Página embutida no binário: o navegador confere a cada carga (uma versão
/// nova do Anubis aparece na hora, com os ícones e a fonte novos).
fn page(html: &'static str) -> Response {
    ([("cache-control", "no-cache")], Html(html)).into_response()
}

/// Ícones das páginas (Nerd Font reduzida; web/icones.py). Sem prazo de
/// cache: o navegador confere a versão (ETag) a cada carga, e um ícone novo
/// aparece na hora (com `max-age`, ele seguia com a fonte velha por um dia).
async fn icons(headers: HeaderMap) -> Response {
    const FONT: &[u8] = include_bytes!("../web/icones.woff2");
    // FNV-1a do arquivo: muda quando a fonte muda
    let tag = format!("\"{:016x}\"", FONT.iter().fold(0xcbf29ce484222325_u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3)));
    let headers_out = [("content-type", "font/woff2".to_owned()), ("cache-control", "no-cache".to_owned()), ("etag", tag.clone())];
    if headers.get("if-none-match").and_then(|v| v.to_str().ok()) == Some(tag.as_str()) {
        return (StatusCode::NOT_MODIFIED, headers_out).into_response();
    }
    (headers_out, FONT).into_response()
}

async fn uninstall_preview(State(s): State<AppState>, Path((kind, id)): Path<(Kind, i64)>) -> Response {
    match s.orch.uninstall_preview(kind, id).await {
        Ok(p) => Json(p).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn uninstall(State(s): State<AppState>, Path((kind, id)): Path<(Kind, i64)>) -> Response {
    match s.orch.uninstall(kind, id).await {
        Ok(u) => Json(u).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn like(State(s): State<AppState>, Path((kind, id)): Path<(Kind, i64)>) -> Response {
    match s.orch.like(kind, id).await {
        Ok(h) => Json(h).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn unlike(State(s): State<AppState>, Path((kind, id)): Path<(Kind, i64)>) -> Response {
    s.orch.unlike(kind, id).await;
    StatusCode::NO_CONTENT.into_response()
}

async fn clear_netflix(State(s): State<AppState>) -> Response {
    s.orch.clear_netflix().await;
    StatusCode::NO_CONTENT.into_response()
}

#[derive(serde::Deserialize)]
struct AnilistBody {
    #[serde(default)]
    user: String,
}

async fn set_anilist(State(s): State<AppState>, Json(b): Json<AnilistBody>) -> Response {
    match s.orch.set_anilist(&b.user).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
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

async fn series_subbed(State(s): State<AppState>, Path(tvdb): Path<i64>) -> Response {
    accept_subbed(s, Kind::Series, tvdb).await
}

async fn movie_subbed(State(s): State<AppState>, Path(tmdb): Path<i64>) -> Response {
    accept_subbed(s, Kind::Movie, tmdb).await
}

async fn accept_subbed(s: AppState, kind: Kind, id: i64) -> Response {
    match s.orch.accept_subbed(kind, id).await {
        Ok(st) => Json(st).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn episodes(State(s): State<AppState>, Path(tvdb): Path<i64>) -> Response {
    match s.orch.episodes(tvdb).await {
        Ok(e) => Json(e).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn episode_availability(State(s): State<AppState>, Path(tvdb): Path<i64>) -> Response {
    match s.orch.episode_availability(tvdb).await {
        Some(eps) => Json(json!({ "episodes": eps.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<std::collections::HashMap<_, _>>() })).into_response(),
        None => (StatusCode::ACCEPTED, Json(json!({ "checking": true }))).into_response(),
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

async fn autodub_now(State(s): State<AppState>, Path((kind, id)): Path<(Kind, i64)>) -> Response {
    match s.orch.autodub_now(kind, id).await {
        Ok(keys) => (StatusCode::ACCEPTED, Json(json!({ "queued": keys }))).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

#[derive(serde::Deserialize)]
struct EventsQuery {
    /// "movie:TMDB,series:TVDB,…"
    #[serde(default)]
    items: String,
}

/// Canal do SSE como Stream (a conexão fecha quando o navegador sai).
struct Rx(tokio::sync::mpsc::Receiver<Event>);

impl futures_core::Stream for Rx {
    type Item = Result<Event, std::convert::Infallible>;
    fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        self.0.poll_recv(cx).map(|e| e.map(Ok))
    }
}

/// Estado ao vivo (SSE) dos itens pedidos: tudo ao conectar, depois só o que
/// muda — conferido na memória da central a cada 2 s e na hora em que ela se
/// atualiza (webhook do Radarr/Sonarr, qBittorrent). Inclui o "juntar dublagem".
async fn events(State(s): State<AppState>, Query(q): Query<EventsQuery>) -> Response {
    let keys: Vec<(Kind, i64)> = q
        .items
        .split(',')
        .filter_map(|k| {
            let (kind, id) = k.trim().split_once(':')?;
            let kind = match kind {
                "movie" => Kind::Movie,
                "series" => Kind::Series,
                "episode" => Kind::Episode,
                _ => return None,
            };
            Some((kind, id.parse().ok()?))
        })
        .take(200)
        .collect();
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    tokio::spawn(async move {
        let mut sent: std::collections::HashMap<(Kind, i64), serde_json::Value> = Default::default();
        let mut changed = s.orch.live.changed.subscribe();
        loop {
            // conexão aberta = alguém acompanhando: a central segue viva
            s.orch.live.touch();
            for &(kind, id) in &keys {
                let mut v = match s.orch.status(kind, id).await {
                    Ok(st) => json!(st),
                    Err(e) => json!({ "error": format!("{e:#}") }),
                };
                if kind == Kind::Movie
                    && let Some(m) = s.orch.mux_status(Kind::Movie, id).await
                {
                    v["mux"] = m;
                }
                if sent.get(&(kind, id)) == Some(&v) {
                    continue;
                }
                let data = json!({ "key": format!("{}:{id}", kind_str(kind)), "status": v });
                if tx.send(Event::default().event("status").data(data.to_string())).await.is_err() {
                    return;
                }
                sent.insert((kind, id), v);
            }
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
                _ = changed.recv() => {}
                _ = tx.closed() => return,
            }
        }
    });
    Sse::new(Rx(rx)).keep_alive(KeepAlive::default()).into_response()
}

#[derive(serde::Deserialize)]
struct DiscoverQuery {
    #[serde(default)]
    bg: u8,
}

async fn discover(State(s): State<AppState>, Path(category): Path<String>, Query(q): Query<DiscoverQuery>) -> Response {
    let Some(cat) = crate::discover::Category::parse(&category) else { return error(StatusCode::NOT_FOUND, "categoria: filmes, series ou animes") };
    match s.orch.showcase(cat, q.bg == 0).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn availability(State(s): State<AppState>) -> Response {
    let a = &s.orch.availability;
    Json(json!({ "states": a.states().await, "pending": a.pending().await, "eta": a.eta().await })).into_response()
}

async fn availability_now(State(s): State<AppState>, Path((kind, id)): Path<(Kind, i64)>) -> Response {
    let avail = s.orch.availability.check_now(kind, id, std::time::Duration::from_secs(90)).await;
    Json(json!({ "avail": avail.unwrap_or(crate::avail::Avail::Unknown) })).into_response()
}

#[derive(serde::Deserialize)]
struct OpenQuery {
    /// tela do Jellyfin web (ex.: "/details?id=…")
    #[serde(default)]
    next: String,
}

/// Identificador deste navegador (cookie de um ano): o mesmo aparelho no
/// Jellyfin, então um novo clique substitui a sessão em vez de somar outra.
fn device_id(headers: &HeaderMap) -> (String, bool) {
    let from_cookie = headers
        .get_all("cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().strip_prefix("anubis_jf="))
        .find(|v| v.len() == 32 && v.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_owned);
    match from_cookie {
        Some(id) => (id, false),
        None => {
            let mut b = [0u8; 16];
            let _ = std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom").expect("/dev/urandom"), &mut b);
            (b.iter().map(|x| format!("{x:02x}")).collect(), true)
        }
    }
}

/// Nome do aparelho na lista do Jellyfin, pelo navegador que pediu.
fn device_name(headers: &HeaderMap) -> String {
    let ua = headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or_default();
    let os = ["Android", "iPhone", "iPad", "Windows", "Mac OS", "Linux"].into_iter().find(|o| ua.contains(o)).unwrap_or("navegador");
    format!("Anubis · {os}")
}

/// "Abrir no Jellyfin" já logado: abre (ou renova) a sessão deste navegador
/// e segue para a página-ponte do Jellyfin pelo mesmo endereço que o
/// navegador usou para chegar aqui (cabo, rede local ou Tailscale).
async fn jellyfin_open(State(s): State<AppState>, Query(q): Query<OpenQuery>, headers: HeaderMap) -> Response {
    let Some((user, password)) = &s.orch.jellyfin_login else { return error(StatusCode::NOT_FOUND, "login automático do Jellyfin não configurado") };
    let (device, new) = device_id(&headers);
    let (token, user_id, server_id) = match s.orch.jellyfin.authenticate(user, password, &device, &device_name(&headers)).await {
        Ok(x) => x,
        Err(e) => return error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    };
    let host = headers.get("host").and_then(|v| v.to_str().ok()).and_then(|h| h.rsplit_once(':').map_or(Some(h), |(a, _)| Some(a))).unwrap_or("10.10.10.2");
    let port = reqwest::Url::parse(&s.orch.cfg.jellyfin.url).ok().and_then(|u| u.port_or_known_default()).unwrap_or(8096);
    let next = if q.next.starts_with('/') { q.next.as_str() } else { "/home" };
    let enc = crate::services::url_encode;
    let to = format!("http://{host}:{port}/web/anubis-login.html#t={}&u={}&s={}&d={device}&next={}", enc(&token), enc(&user_id), enc(&server_id), enc(next));
    let mut res = (StatusCode::FOUND, [("location", to), ("cache-control", "no-store".to_owned())]).into_response();
    if new {
        res.headers_mut().insert("set-cookie", format!("anubis_jf={device}; Max-Age=31536000; Path=/; SameSite=Lax; HttpOnly").parse().unwrap());
    }
    res
}

#[derive(serde::Deserialize)]
struct HookToken {
    token: Option<String>,
}

/// Webhook do Radarr/Sonarr (versão pega, importada, item adicionado…): a
/// central refaz as listas agora. O token vai na URL (`?token=`), que é o
/// que o webhook deles permite configurar.
async fn arr_event(State(s): State<AppState>, Query(q): Query<HookToken>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let by_query = s.orch.cfg.server.token.is_some() && q.token == s.orch.cfg.server.token;
    if !by_query && !authorized(&s, &headers) {
        return error(StatusCode::UNAUTHORIZED, "token inválido");
    }
    s.orch.live.dirty.notify_one();
    // importou (pelo "Baixar" ou pelo "Assistir"): o Jellyfin varre a biblioteca
    let ev: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    if ev["eventType"] == "Download" {
        // entrou sem português: a dublagem é procurada na hora
        let orch = s.orch.clone();
        let ev = ev.clone();
        tokio::spawn(async move { orch.autodub_after_import(&ev).await });
    }
    if ev["eventType"] == "Download"
        && let Some(path) = ev["movie"]["folderPath"].as_str().or(ev["series"]["path"].as_str())
    {
        let (orch, path) = (s.orch.clone(), path.to_owned());
        tokio::spawn(async move {
            // um pacote de temporada chega como vários avisos: uma varredura por minuto basta
            if !first_in_a_while(&path).await {
                return;
            }
            if let Err(e) = orch.jellyfin.refresh_library_of(&path).await {
                warn!("{e:#}");
            }
        });
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Verdadeiro na primeira vez em 60 s para a mesma pasta.
async fn first_in_a_while(key: &str) -> bool {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};
    static SEEN: std::sync::OnceLock<tokio::sync::Mutex<HashMap<String, Instant>>> = std::sync::OnceLock::new();
    let mut seen = SEEN.get_or_init(Default::default).lock().await;
    seen.retain(|_, at| at.elapsed() < Duration::from_secs(60));
    seen.insert(key.to_owned(), Instant::now()).is_none()
}

async fn torrent_finished(State(s): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&s, &headers) {
        return error(StatusCode::UNAUTHORIZED, "token inválido");
    }
    s.wake.notify_one();
    (StatusCode::ACCEPTED, Json(json!({ "status": "agendado" }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_encoding(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("accept-encoding", HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn gzip_is_accepted_only_when_the_browser_says_so() {
        assert!(accepts_gzip(&with_encoding("gzip, deflate, br")));
        assert!(accepts_gzip(&with_encoding("br;q=1.0, gzip;q=0.8")));
        assert!(!accepts_gzip(&with_encoding("br, deflate")));
        assert!(!accepts_gzip(&with_encoding("gzip;q=0")));
        assert!(!accepts_gzip(&HeaderMap::new()));
    }

    /// Todo .js/.css do build tem seu `.gz` (o build.sh gera) e o servidor entrega o certo.
    #[tokio::test]
    async fn every_script_and_style_has_a_gzip_twin_that_is_served() {
        let assets = DIST.get_dir("assets").expect("web/dist/assets (rode web/app/build.sh)");
        let names: Vec<&str> = assets.files().filter_map(|f| f.path().file_name()?.to_str()).filter(|n| n.ends_with(".js") || n.ends_with(".css")).collect();
        assert!(!names.is_empty());
        for name in names {
            let plain = asset(Path(name.to_owned()), HeaderMap::new()).await;
            assert_eq!(plain.status(), StatusCode::OK);
            assert!(plain.headers().get("content-encoding").is_none());
            let gz = asset(Path(name.to_owned()), with_encoding("gzip")).await;
            assert_eq!(gz.status(), StatusCode::OK, "{name}.gz não existe: rode web/app/build.sh");
            assert_eq!(gz.headers()["content-encoding"], "gzip");
            assert_eq!(gz.headers()["content-type"], plain.headers()["content-type"]);
            let body = axum::body::to_bytes(gz.into_body(), usize::MAX).await.unwrap();
            assert_eq!(&body[..2], &[0x1f, 0x8b], "{name}.gz não é gzip");
        }
        assert_eq!(asset(Path("qualquer.js.gz".to_owned()), with_encoding("gzip")).await.status(), StatusCode::NOT_FOUND);
    }
}

