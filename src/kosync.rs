//! Progresso de leitura único: servidor do "Progress sync" do KOReader
//! (kosync). O Kindle, o KOReader do celular e a página de leitura do Anubis
//! leem e gravam aqui, em `state_dir/progresso.json`.
//!
//! O KOReader identifica o livro por `md5(nome do arquivo)` (opção "nome do
//! arquivo" no plugin), o mesmo nome que o arquivo tem no Drive. Um usuário
//! só, sem cadastro aberto: `x-auth-key` é o md5 da senha, guardado no
//! `key_file` da config.
//!
//! Rotas (sob /kosync, publicado na internet pelo Tailscale Funnel):
//! GET  /healthcheck
//! POST /users/create            — sempre 402 (cadastro fechado)
//! GET  /users/auth              — confere x-auth-user / x-auth-key
//! PUT  /syncs/progress          — {document, progress, percentage, device, device_id}
//! GET  /syncs/progress/{document}

use crate::config::KosyncCfg;
use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    pub document: String,
    /// xpointer (EPUB) ou número da página (PDF), como o KOReader mandou
    pub progress: Value,
    /// 0.0 a 1.0
    pub percentage: f64,
    pub device: String,
    pub device_id: String,
    /// segundos desde 1970 (o KOReader compara com a última virada de página)
    #[serde(default)]
    pub timestamp: i64,
}

pub struct Kosync {
    user: String,
    key: String,
    path: PathBuf,
    map: Mutex<HashMap<String, Progress>>,
}

impl Kosync {
    pub fn load(cfg: &KosyncCfg, state_dir: PathBuf) -> Result<Self> {
        let key = std::fs::read_to_string(&cfg.key_file)
            .with_context(|| format!("lendo {}", cfg.key_file.display()))?
            .trim()
            .to_lowercase();
        let path = state_dir.join("progresso.json");
        let map = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Ok(Self { user: cfg.user.clone(), key, path, map: Mutex::new(map) })
    }

    /// Onde o livro parou (documento = md5 do nome do arquivo).
    pub async fn get(&self, document: &str) -> Option<Progress> {
        self.map.lock().await.get(document).cloned()
    }

    async fn put(&self, mut p: Progress) -> Result<i64> {
        p.timestamp = chrono::Utc::now().timestamp();
        let ts = p.timestamp;
        let mut map = self.map.lock().await;
        map.insert(p.document.clone(), p);
        // grava inteiro e troca de uma vez: um desligamento no meio não corrompe
        let tmp = self.path.with_extension("json.tmp");
        tokio::fs::write(&tmp, serde_json::to_vec(&*map)?).await?;
        tokio::fs::rename(&tmp, &self.path).await?;
        Ok(ts)
    }

    fn authorized(&self, h: &HeaderMap) -> bool {
        let get = |k| h.get(k).and_then(|v| v.to_str().ok());
        get("x-auth-user") == Some(self.user.as_str()) && get("x-auth-key").map(str::to_lowercase).as_deref() == Some(&self.key)
    }

    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/healthcheck", get(|| async { Json(json!({ "state": "OK" })) }))
            .route("/users/create", post(|| async {
                (StatusCode::PAYMENT_REQUIRED, Json(json!({ "code": 2005, "message": "Cadastro fechado: use o usuário do Anubis." })))
            }))
            .route("/users/auth", get(auth))
            .route("/syncs/progress", put(update))
            .route("/syncs/progress/{document}", get(fetch))
            .with_state(self)
    }
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({ "code": 2001, "message": "Unauthorized" }))).into_response()
}

async fn auth(State(k): State<Arc<Kosync>>, h: HeaderMap) -> Response {
    if !k.authorized(&h) {
        return unauthorized();
    }
    Json(json!({ "authorized": "OK" })).into_response()
}

async fn update(State(k): State<Arc<Kosync>>, h: HeaderMap, Json(p): Json<Progress>) -> Response {
    if !k.authorized(&h) {
        return unauthorized();
    }
    let document = p.document.clone();
    match k.put(p).await {
        Ok(timestamp) => Json(json!({ "document": document, "timestamp": timestamp })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "message": format!("{e:#}") }))).into_response(),
    }
}

async fn fetch(State(k): State<Arc<Kosync>>, h: HeaderMap, Path(document): Path<String>) -> Response {
    if !k.authorized(&h) {
        return unauthorized();
    }
    // sem registro: objeto vazio, como o servidor oficial ("nada para sincronizar")
    match k.get(&document).await {
        Some(p) => Json(p).into_response(),
        None => Json(json!({})).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kosync(dir: &std::path::Path) -> Kosync {
        let key_file = dir.join("key");
        std::fs::write(&key_file, "5F4DCC3B5AA765D61D8327DEB882CF99\n").unwrap();
        Kosync::load(&KosyncCfg { user: "rafeu".into(), key_file }, dir.to_path_buf()).unwrap()
    }

    #[test]
    fn autoriza_so_o_usuario_e_a_chave_certos() {
        let dir = std::env::temp_dir().join(format!("kosync-auth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let k = kosync(&dir);
        let headers = |u: &str, key: &str| {
            let mut h = HeaderMap::new();
            h.insert("x-auth-user", u.parse().unwrap());
            h.insert("x-auth-key", key.parse().unwrap());
            h
        };
        assert!(k.authorized(&headers("rafeu", "5f4dcc3b5aa765d61d8327deb882cf99")));
        assert!(!k.authorized(&headers("rafeu", "errada")));
        assert!(!k.authorized(&headers("outro", "5f4dcc3b5aa765d61d8327deb882cf99")));
        assert!(!k.authorized(&HeaderMap::new()));
    }

    #[tokio::test]
    async fn progresso_sobrevive_a_reinicio() {
        let dir = std::env::temp_dir().join(format!("kosync-put-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let k = kosync(&dir);
        let p = Progress {
            document: "abc".into(),
            progress: json!(28),
            percentage: 0.158,
            device: "Kindle".into(),
            device_id: "K1".into(),
            timestamp: 0,
        };
        let ts = k.put(p.clone()).await.unwrap();
        assert!(ts > 0);
        let again = kosync(&dir);
        assert_eq!(again.get("abc").await, Some(Progress { timestamp: ts, ..p }));
        assert_eq!(again.get("outro").await, None);
    }
}
