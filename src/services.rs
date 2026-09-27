//! Clientes das APIs que o Anubis orquestra: Sonarr, Radarr, qBittorrent e
//! Jellyfin. Só o necessário para a limpeza de armazenamento. O Jellyfin não
//! precisa ser avisado das exclusões: a conexão "Jellyfin" do Sonarr/Radarr
//! já faz isso quando eles apagam um arquivo.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use reqwest::{Client, Method, RequestBuilder};
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
    pub async fn movies(&self) -> Result<Vec<Movie>> {
        self.get("movie").await
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

// ---------------------------------------------------------------- qBittorrent

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

    pub async fn files(&self, hash: &str) -> Result<Vec<TorrentFileInfo>> {
        send(self.req(Method::GET, &format!("torrents/files?hash={hash}")), "qBittorrent arquivos").await
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
}
