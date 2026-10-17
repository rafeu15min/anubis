//! Central de estado: o que a página acompanha (pedidos, downloads), mantido
//! pelo servidor — as rotas respondem daqui, sem consultar ninguém na hora.
//!
//! - qBittorrent: uma consulta por diferença (`sync/maindata`) a cada 2 s,
//!   para todos os torrents de uma vez (sem mudança, a resposta tem ~10 bytes);
//! - Radarr/Sonarr: listas inteiras (itens e fila) a cada minuto, ou na hora
//!   em que um webhook avisa (versão pega, importada, item adicionado…);
//! - link do Jellyfin: calculado uma vez por item.
//!
//! Só trabalha enquanto alguém acompanha (acesso no último minuto): com a
//! página fechada, não gera nenhuma chamada.

use crate::orchestrator::Orchestrator;
use crate::search::Kind;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify, broadcast};
use tracing::warn;

/// sem acesso por este tempo, a central para de consultar
const IDLE: Duration = Duration::from_secs(60);
/// listas do Radarr/Sonarr mais velhas que isso não servem (a rota consulta na hora)
const STALE: Duration = Duration::from_secs(120);
const ARR_EVERY: Duration = Duration::from_secs(60);
const QBIT_EVERY: Duration = Duration::from_secs(2);

/// Retrato do Radarr/Sonarr: itens e fila, de uma vez.
#[derive(Default)]
pub struct Snapshot {
    pub movies: Vec<Value>,
    pub series: Vec<Value>,
    pub movie_queue: Vec<Value>,
    pub series_queue: Vec<Value>,
    pub at: Option<Instant>,
}

impl Snapshot {
    pub fn fresh(&self) -> bool {
        self.at.is_some_and(|t| t.elapsed() < STALE)
    }

    /// O item (pelo tmdbId/tvdbId) e a fila dele.
    pub fn item(&self, kind: Kind, id: i64) -> Option<(&Value, Vec<&Value>)> {
        let (list, key, queue, qkey) = match kind {
            Kind::Movie => (&self.movies, "tmdbId", &self.movie_queue, "movieId"),
            _ => (&self.series, "tvdbId", &self.series_queue, "seriesId"),
        };
        let item = list.iter().find(|x| x[key].as_i64() == Some(id))?;
        let lib_id = item["id"].as_i64();
        Some((item, queue.iter().filter(|q| q[qkey].as_i64() == lib_id).collect()))
    }
}

/// Quando se perguntou ao Jellyfin e o link que ele deu (`None`: ainda não tem o item).
pub(crate) type JellyfinLink = (Instant, Option<String>);

pub struct Live {
    pub(crate) arr: Mutex<Snapshot>,
    /// hash (minúsculo) → campos do qBittorrent (progress, size, eta…)
    pub(crate) torrents: Mutex<HashMap<String, Value>>,
    rid: Mutex<u64>,
    /// link do item no Jellyfin; `None` = ainda não está lá (vale por pouco tempo)
    pub(crate) jellyfin: Mutex<HashMap<(Kind, i64), JellyfinLink>>,
    last_access: std::sync::Mutex<Option<Instant>>,
    /// webhook do Radarr/Sonarr (ou um pedido novo): refazer as listas agora
    pub(crate) dirty: Notify,
    /// alguém voltou a acompanhar depois de um tempo parado
    woke: Notify,
    /// a central acabou de se atualizar: as conexões ao vivo conferem na hora
    pub(crate) changed: broadcast::Sender<()>,
}

impl Default for Live {
    fn default() -> Self {
        Self {
            arr: Mutex::new(Snapshot::default()),
            torrents: Mutex::new(HashMap::new()),
            rid: Mutex::new(0),
            jellyfin: Mutex::new(HashMap::new()),
            last_access: std::sync::Mutex::new(None),
            dirty: Notify::new(),
            woke: Notify::new(),
            changed: broadcast::channel(16).0,
        }
    }
}

impl Live {
    /// Alguém está acompanhando: mantém (ou volta a manter) a central viva.
    pub fn touch(&self) {
        let mut last = self.last_access.lock().unwrap();
        let was_idle = !last.is_some_and(|t| t.elapsed() < IDLE);
        *last = Some(Instant::now());
        if was_idle {
            self.woke.notify_waiters();
        }
    }

    fn active(&self) -> bool {
        self.last_access.lock().unwrap().is_some_and(|t| t.elapsed() < IDLE)
    }
}

/// Aplica uma resposta de `sync/maindata` (completa ou só com o que mudou).
fn merge(torrents: &mut HashMap<String, Value>, d: &Value) {
    if d["full_update"] == true {
        torrents.clear();
    }
    if let Some(m) = d["torrents"].as_object() {
        for (hash, fields) in m {
            let e = torrents.entry(hash.to_lowercase()).or_insert_with(|| json!({}));
            if let (Some(e), Some(f)) = (e.as_object_mut(), fields.as_object()) {
                e.extend(f.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
        }
    }
    for h in d["torrents_removed"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        torrents.remove(&h.to_lowercase());
    }
}

/// Mantém a central: qBittorrent a cada 2 s e Radarr/Sonarr a cada minuto (ou
/// no aviso de um webhook), só enquanto alguém acompanha.
pub async fn run(orch: Arc<Orchestrator>) {
    let live = &orch.live;
    let mut last_arr: Option<Instant> = None;
    loop {
        if !live.active() {
            live.woke.notified().await;
            continue;
        }
        let arr_due = last_arr.is_none_or(|t| t.elapsed() >= ARR_EVERY) || !live.arr.lock().await.fresh();
        if arr_due {
            refresh_arr(&orch).await;
            last_arr = Some(Instant::now());
        }
        // o progresso do qBittorrent só importa com algo baixando na fila do
        // Radarr/Sonarr (um item concluído, esperando import, não muda mais)
        let downloading = {
            let s = live.arr.lock().await;
            s.movie_queue.iter().chain(&s.series_queue).any(|q| q["status"] != "completed")
        };
        if downloading {
            refresh_qbit(&orch).await;
        }
        tokio::select! {
            _ = tokio::time::sleep(QBIT_EVERY) => {}
            _ = live.dirty.notified() => { last_arr = None; }
        }
    }
}

pub(crate) async fn get(arr: Option<&crate::services::Arr>, path: &str) -> anyhow::Result<Vec<Value>> {
    match arr {
        Some(a) => Ok(a.get_json(path).await?.as_array().cloned().unwrap_or_default()),
        None => Ok(vec![]),
    }
}

pub(crate) async fn refresh_arr(orch: &Orchestrator) {
    let (movies, series, mq, sq) = tokio::join!(
        get(orch.radarr.as_ref(), "movie"),
        get(orch.sonarr.as_ref(), "series"),
        get(orch.radarr.as_ref(), "queue/details"),
        get(orch.sonarr.as_ref(), "queue/details"),
    );
    match (movies, series, mq, sq) {
        (Ok(movies), Ok(series), Ok(movie_queue), Ok(series_queue)) => {
            *orch.live.arr.lock().await = Snapshot { movies, series, movie_queue, series_queue, at: Some(Instant::now()) };
            let _ = orch.live.changed.send(());
        }
        (m, s, q1, q2) => {
            let why: Vec<String> = [m.err(), s.err(), q1.err(), q2.err()].into_iter().flatten().map(|e| format!("{e:#}")).collect();
            warn!("central: listas do Radarr/Sonarr não atualizadas: {}", why.join("; "));
        }
    }
}

async fn refresh_qbit(orch: &Orchestrator) {
    let rid = *orch.live.rid.lock().await;
    match orch.qbit.maindata(rid).await {
        Ok(d) => {
            merge(&mut *orch.live.torrents.lock().await, &d);
            *orch.live.rid.lock().await = d["rid"].as_u64().unwrap_or(0);
            // resposta só com o rid = nada mudou
            if d.as_object().is_some_and(|o| o.len() > 1) {
                let _ = orch.live.changed.send(());
            }
        }
        Err(e) => warn!("central: qBittorrent não respondeu: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_full_and_partial_updates() {
        let mut t = HashMap::new();
        merge(&mut t, &json!({ "rid": 1, "full_update": true, "torrents": { "ABC": { "progress": 0.1, "size": 100, "eta": 50 }, "def": { "progress": 1.0 } } }));
        assert_eq!(t.len(), 2);
        // só o que mudou vem na resposta seguinte
        merge(&mut t, &json!({ "rid": 2, "torrents": { "abc": { "progress": 0.5 } }, "torrents_removed": ["DEF"] }));
        assert_eq!(t.len(), 1);
        assert_eq!(t["abc"]["progress"], 0.5);
        assert_eq!(t["abc"]["size"], 100);
        // nada mudou
        merge(&mut t, &json!({ "rid": 3 }));
        assert_eq!(t["abc"]["eta"], 50);
    }

    #[test]
    fn finds_the_item_and_its_queue() {
        let s = Snapshot {
            movies: vec![json!({ "id": 6, "tmdbId": 1084244 }), json!({ "id": 7, "tmdbId": 980431 })],
            movie_queue: vec![json!({ "movieId": 7, "size": 10 }), json!({ "movieId": 9 })],
            at: Some(Instant::now()),
            ..Default::default()
        };
        let (item, queue) = s.item(Kind::Movie, 980431).unwrap();
        assert_eq!(item["id"], 7);
        assert_eq!(queue.len(), 1);
        assert!(s.item(Kind::Movie, 1).is_none());
        assert!(s.fresh());
    }
}
