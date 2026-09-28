//! Clientes das APIs que o Anubis orquestra: Sonarr, Radarr, qBittorrent e
//! Jellyfin — o necessário para a limpeza de armazenamento e para a busca.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use reqwest::{Client, RequestBuilder};
pub use reqwest::Method;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::time::Duration;

fn http() -> Result<Client> {
    Ok(Client::builder().timeout(Duration::from_secs(30)).cookie_store(true).build()?)
}

async fn send<T: DeserializeOwned>(req: RequestBuilder, what: &str) -> Result<T> {
    let res = req.send().await.with_context(|| format!("{what}: sem resposta"))?;
    let status = res.status();
    if !status.is_success() {
        bail!("{what}: HTTP {status}: {}", res.text().await.unwrap_or_default());
    }
    let body = res.text().await?;
    serde_json::from_str(if body.trim().is_empty() { "null" } else { &body }).with_context(|| format!("{what}: resposta inesperada"))
}

// ---------------------------------------------------------------- Sonarr/Radarr

/// Sonarr e Radarr falam o mesmo "dialeto" (API v3, chave no cabeçalho).
pub struct Arr {
    name: &'static str,
    base: String,
    key: String,
    http: Client,
}

impl Arr {
    pub fn new(name: &'static str, base: &str, key: &str) -> Result<Self> {
        Ok(Self { name, base: base.trim_end_matches('/').into(), key: key.into(), http: http()? })
    }

    fn req(&self, method: Method, path: &str) -> RequestBuilder {
        self.http.request(method, format!("{}/api/v3/{path}", self.base)).header("X-Api-Key", &self.key)
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        send(self.req(Method::GET, path), &format!("{} GET {path}", self.name)).await
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Series {
    pub id: i64,
    pub title: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpisodeFile {
    pub id: i64,
    pub series_id: i64,
    pub path: String,
    pub date_added: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Episode {
    pub id: i64,
    pub episode_file_id: i64,
    pub season_number: i64,
    pub episode_number: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Movie {
    pub id: i64,
    pub title: String,
    pub year: Option<i64>,
    pub movie_file: Option<MovieFile>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MovieFile {
    pub id: i64,
    pub path: String,
    pub date_added: DateTime<Utc>,
}

impl Arr {
    pub async fn series(&self) -> Result<Vec<Series>> {
        self.get("series").await
    }
    pub async fn episode_files(&self, series_id: i64) -> Result<Vec<EpisodeFile>> {
        self.get(&format!("episodefile?seriesId={series_id}")).await
    }
    pub async fn episodes(&self, series_id: i64) -> Result<Vec<Episode>> {
        self.get(&format!("episode?seriesId={series_id}")).await
    }
    pub async fn episodes_raw(&self, series_id: i64) -> Result<Vec<Value>> {
        self.get(&format!("episode?seriesId={series_id}")).await
    }
    pub async fn episode(&self, id: i64) -> Result<Value> {
        self.get(&format!("episode/{id}")).await
    }
    pub async fn episode_file(&self, id: i64) -> Result<Value> {
        self.get(&format!("episodefile/{id}")).await
    }
    pub async fn series_by_id(&self, id: i64) -> Result<Value> {
        self.get(&format!("series/{id}")).await
    }
    /// Liga o monitoramento de episódios (depois do import pelo "assistir agora").
    pub async fn monitor_episodes(&self, ids: &[i64]) -> Result<()> {
        let body = json!({ "episodeIds": ids, "monitored": true });
        send::<Value>(self.req(Method::PUT, "episode/monitor").json(&body), &format!("{} monitorar", self.name)).await?;
        Ok(())
    }
    pub async fn movies(&self) -> Result<Vec<Movie>> {
        self.get("movie").await
    }

    /// Busca de metadados (TMDB/TVDB) pelo próprio Radarr/Sonarr.
    pub async fn lookup(&self, what: &str, term: &str) -> Result<Vec<Value>> {
        let term: String = url_encode(term);
        self.get(&format!("{what}/lookup?term={term}")).await
    }

    /// Item já na biblioteca, pelo id externo (`movie?tmdbId=` / `series?tvdbId=`).
    pub async fn find(&self, what: &str, id_param: &str, id: i64) -> Result<Option<Value>> {
        let found: Vec<Value> = self.get(&format!("{what}?{id_param}={id}")).await?;
        Ok(found.into_iter().next())
    }

    pub async fn quality_profile_id(&self, name: &str) -> Result<i64> {
        let profiles: Vec<Value> = self.get("qualityprofile").await?;
        profiles
            .iter()
            .find(|p| p["name"].as_str() == Some(name))
            .and_then(|p| p["id"].as_i64())
            .with_context(|| format!("{}: perfil de qualidade \"{name}\" não existe", self.name))
    }

    /// Adiciona à biblioteca (monitorado) e já manda buscar.
    pub async fn add(&self, what: &str, item: Value) -> Result<Value> {
        send(self.req(Method::POST, what).json(&item), &format!("{} adicionar", self.name)).await
    }

    /// Dispara uma busca (ex.: `{"name":"MoviesSearch","movieIds":[1]}`).
    pub async fn command(&self, body: Value) -> Result<()> {
        send::<Value>(self.req(Method::POST, "command").json(&body), &format!("{} comando", self.name)).await?;
        Ok(())
    }

    pub async fn get_json(&self, path: &str) -> Result<Value> {
        self.get(path).await
    }

    pub async fn send_json(&self, method: Method, path: &str, body: &Value) -> Result<Value> {
        send(self.req(method, path).json(body), &format!("{} {path}", self.name)).await
    }

    /// Versões encontradas nos indexadores, já com a decisão do Radarr/Sonarr
    /// (`rejected` + motivos). Demora alguns segundos: é uma busca de verdade.
    pub async fn releases(&self, filter: &str) -> Result<Vec<Value>> {
        let req = self.req(Method::GET, &format!("release?{filter}")).timeout(Duration::from_secs(120));
        send(req, &format!("{} versões", self.name)).await
    }

    /// Liga/desliga o monitoramento de um filme.
    pub async fn set_movie_monitored(&self, movie_id: i64, monitored: bool) -> Result<()> {
        let body = json!({ "movieIds": [movie_id], "monitored": monitored });
        send::<Value>(self.req(Method::PUT, "movie/editor").json(&body), &format!("{} monitorar", self.name)).await?;
        Ok(())
    }

    pub async fn movie(&self, id: i64) -> Result<Value> {
        self.get(&format!("movie/{id}")).await
    }

    /// Itens na fila de download (`queue/details?movieId=` / `?seriesId=`).
    pub async fn queue(&self, filter: &str) -> Result<Vec<Value>> {
        self.get(&format!("queue/details?{filter}")).await
    }

    /// Desmarca os episódios (senão o Sonarr baixaria de novo) e apaga o arquivo.
    pub async fn delete_episode_file(&self, file_id: i64, episode_ids: &[i64]) -> Result<()> {
        let what = format!("{} desmonitorar episódios", self.name);
        send::<Value>(self.req(Method::PUT, "episode/monitor").json(&json!({ "episodeIds": episode_ids, "monitored": false })), &what).await?;
        send::<Value>(self.req(Method::DELETE, &format!("episodefile/{file_id}")), &format!("{} apagar episódio {file_id}", self.name)).await?;
        Ok(())
    }

    /// Desmarca o filme (senão o Radarr baixaria de novo) e apaga o arquivo.
    pub async fn delete_movie_file(&self, file_id: i64, movie_id: i64) -> Result<()> {
        let what = format!("{} desmonitorar filme", self.name);
        send::<Value>(self.req(Method::PUT, "movie/editor").json(&json!({ "movieIds": [movie_id], "monitored": false })), &what).await?;
        send::<Value>(self.req(Method::DELETE, &format!("moviefile/{file_id}")), &format!("{} apagar filme {file_id}", self.name)).await?;
        Ok(())
    }
}

/// Codifica um termo de busca para a query string.
pub(crate) fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

// ---------------------------------------------------------------- Bazarr

pub struct Bazarr {
    base: String,
    key: String,
    http: Client,
}

impl Bazarr {
    pub fn new(base: &str, key: &str) -> Result<Self> {
        Ok(Self { base: base.trim_end_matches('/').into(), key: key.into(), http: http()? })
    }

    pub async fn profiles(&self) -> Result<Vec<Value>> {
        send(self.http.get(format!("{}/api/system/languages/profiles", self.base)).header("X-API-KEY", &self.key), "Bazarr perfis").await
    }

    /// Grava configurações (o mesmo formulário que a tela do Bazarr usa).
    pub async fn save(&self, form: &[(&str, String)]) -> Result<()> {
        let res = self.http.post(format!("{}/api/system/settings", self.base)).header("X-API-KEY", &self.key).form(form).send().await.context("Bazarr: sem resposta")?;
        if !res.status().is_success() {
            bail!("Bazarr: HTTP {}", res.status());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- qBittorrent

/// De onde vem um torrent: link magnet ou o arquivo .torrent.
#[derive(Debug, Clone)]
pub enum TorrentSource {
    Magnet(String),
    File(bytes::Bytes),
}

pub struct QBittorrent {
    base: String,
    user: String,
    password: String,
    http: Client,
}

#[derive(Debug, Deserialize)]
pub struct TorrentInfo {
    pub hash: String,
    pub name: String,
    pub save_path: String,
    pub progress: f64,
    #[serde(default)]
    pub size: f64,
    /// segundos até terminar (8640000 = "infinito" no qBittorrent)
    #[serde(default)]
    pub eta: i64,
}

#[derive(Debug, Deserialize)]
pub struct TorrentFileInfo {
    /// relativo ao save_path
    pub name: String,
}

impl QBittorrent {
    pub fn new(base: &str, user: &str, password: &str) -> Result<Self> {
        Ok(Self { base: base.trim_end_matches('/').into(), user: user.into(), password: password.into(), http: http()? })
    }

    fn req(&self, method: Method, path: &str) -> RequestBuilder {
        // o qBittorrent confere Referer/Origin contra CSRF
        self.http.request(method, format!("{}/api/v2/{path}", self.base)).header("Referer", &self.base)
    }

    /// Entra e guarda o cookie de sessão (o cliente HTTP mantém os cookies).
    pub async fn login(&self) -> Result<()> {
        let res = self
            .req(Method::POST, "auth/login")
            .form(&[("username", &self.user), ("password", &self.password)])
            .send()
            .await
            .context("qBittorrent login: sem resposta")?;
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        // 204 nas versões novas, "Ok." nas antigas; "Fails." = senha errada
        if !status.is_success() || body.trim() == "Fails." {
            bail!("qBittorrent login recusado (HTTP {status})");
        }
        Ok(())
    }

    pub async fn torrents(&self) -> Result<Vec<TorrentInfo>> {
        send(self.req(Method::GET, "torrents/info"), "qBittorrent torrents").await
    }

    /// Torrents pelos hashes, entrando de novo se a sessão expirou (403) —
    /// sem logar a cada consulta de progresso.
    pub async fn by_hashes(&self, hashes: &[String]) -> Result<Vec<TorrentInfo>> {
        let path = format!("torrents/info?hashes={}", hashes.join("|").to_lowercase());
        let res = self.req(Method::GET, &path).send().await.context("qBittorrent: sem resposta")?;
        if res.status() == reqwest::StatusCode::FORBIDDEN {
            self.login().await?;
            return send(self.req(Method::GET, &path), "qBittorrent torrents").await;
        }
        let status = res.status();
        if !status.is_success() {
            bail!("qBittorrent torrents: HTTP {status}");
        }
        Ok(res.json().await?)
    }

    pub async fn files(&self, hash: &str) -> Result<Vec<TorrentFileInfo>> {
        send(self.req(Method::GET, &format!("torrents/files?hash={hash}")), "qBittorrent arquivos").await
    }

    /// Entrega um torrent já baixado para o qBittorrent semear, na mesma pasta
    /// (`NoSubfolder`: os arquivos ficam direto em `save_path`, como o Anubis
    /// gravou). `only_file`: de um pacote, só este arquivo existe — os outros
    /// ficam com prioridade 0 para o qBittorrent não baixá-los.
    pub async fn add_seeding(&self, source: &TorrentSource, save_path: &str, category: &str, hash: Option<&str>, only_file: Option<usize>) -> Result<()> {
        let form = || {
            let f = reqwest::multipart::Form::new()
                .text("savepath", save_path.to_owned())
                .text("category", category.to_owned())
                .text("contentLayout", "NoSubfolder")
                .text("autoTMM", "false");
            match source {
                TorrentSource::Magnet(m) => f.text("urls", m.clone()),
                TorrentSource::File(b) => f.part("torrents", reqwest::multipart::Part::bytes(b.to_vec()).file_name("anubis.torrent")),
            }
        };
        let res = self.req(Method::POST, "torrents/add").multipart(form()).send().await.context("qBittorrent: sem resposta")?;
        let res = if res.status() == reqwest::StatusCode::FORBIDDEN {
            self.login().await?;
            self.req(Method::POST, "torrents/add").multipart(form()).send().await?
        } else {
            res
        };
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        if !status.is_success() || body.trim() == "Fails." {
            bail!("qBittorrent recusou o torrent (HTTP {status}: {body})");
        }
        if let (Some(keep), Some(hash)) = (only_file, hash) {
            // magnet: a lista de arquivos só aparece depois dos metadados
            for _ in 0..60 {
                let files = self.files(hash).await.unwrap_or_default();
                if !files.is_empty() {
                    let others: Vec<String> = (0..files.len()).filter(|i| *i != keep).map(|i| i.to_string()).collect();
                    if !others.is_empty() {
                        let form = [("hash", hash.to_owned()), ("id", others.join("|")), ("priority", "0".to_owned())];
                        send::<Value>(self.req(Method::POST, "torrents/filePrio").form(&form), "qBittorrent prioridade").await?;
                    }
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            bail!("o qBittorrent não obteve a lista de arquivos do pacote");
        }
        Ok(())
    }

    /// Remove os torrents E os dados deles.
    pub async fn delete(&self, hashes: &[String]) -> Result<()> {
        if hashes.is_empty() {
            return Ok(());
        }
        let form = [("hashes", hashes.join("|")), ("deleteFiles", "true".into())];
        send::<Value>(self.req(Method::POST, "torrents/delete").form(&form), "qBittorrent remover").await?;
        Ok(())
    }
}

// ---------------------------------------------------------------- Jellyfin

pub struct Jellyfin {
    base: String,
    key: String,
    http: Client,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct User {
    id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Items {
    items: Vec<Item>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Item {
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Session {
    now_playing_item: Option<Item>,
}

impl Jellyfin {
    pub fn new(base: &str, key: &str) -> Result<Self> {
        Ok(Self { base: base.trim_end_matches('/').into(), key: key.into(), http: http()? })
    }

    fn req(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}/{path}", self.base))
            .header("Authorization", format!("MediaBrowser Token=\"{}\"", self.key))
    }

    /// Caminhos (como o Jellyfin os vê) do que está tocando agora ou pela
    /// metade para qualquer usuário.
    pub async fn watching(&self) -> Result<Vec<String>> {
        let mut paths = Vec::new();
        let sessions: Vec<Session> = send(self.req(Method::GET, "Sessions"), "Jellyfin sessões").await?;
        paths.extend(sessions.into_iter().filter_map(|s| s.now_playing_item?.path));
        let users: Vec<User> = send(self.req(Method::GET, "Users"), "Jellyfin usuários").await?;
        for u in users {
            let path = format!("UserItems/Resume?userId={}&fields=Path&mediaTypes=Video&limit=500", u.id);
            let items: Items = send(self.req(Method::GET, &path), "Jellyfin em andamento").await?;
            paths.extend(items.items.into_iter().filter_map(|i| i.path));
        }
        Ok(paths)
    }

    /// Id do item (filme ou série) cuja pasta é `folder` (caminho do Jellyfin).
    pub async fn find_item(&self, kind: &str, title: &str, folder: &str) -> Result<Option<String>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct It {
            id: String,
            path: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct Its {
            items: Vec<It>,
        }
        let path = format!("Items?Recursive=true&IncludeItemTypes={kind}&Fields=Path&SearchTerm={}", url_encode(title));
        let found: Its = send(self.req(Method::GET, &path), "Jellyfin buscar item").await?;
        let folder = folder.trim_end_matches('/');
        Ok(found
            .items
            .into_iter()
            .find(|i| i.path.as_deref().is_some_and(|p| p == folder || p.starts_with(&format!("{folder}/"))))
            .map(|i| i.id))
    }

    /// Avisa o Jellyfin de arquivos apagados: atualização pontual, sem varrer
    /// a biblioteca. (O Radarr/Sonarr não avisam quando a exclusão vem pela
    /// API deles — confirmado no teste de 2026-09-27.)
    pub async fn notify_deleted(&self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let updates: Vec<Value> = paths.iter().map(|p| json!({ "Path": p, "UpdateType": "Deleted" })).collect();
        send::<Value>(self.req(Method::POST, "Library/Media/Updated").json(&json!({ "Updates": updates })), "Jellyfin atualizar").await?;
        Ok(())
    }
}
