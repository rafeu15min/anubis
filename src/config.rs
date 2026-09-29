//! Configuração do Anubis: um arquivo TOML (ver `anubis.example.toml`).
//! As chaves de API ficam só nesse arquivo, fora do repositório.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: Server,
    pub storage: Storage,
    /// Tradução de caminhos: como cada container enxerga o disco → caminho
    /// real no host. O Anubis roda no host e precisa do caminho real para
    /// medir e comparar inodes.
    #[serde(default)]
    pub paths: Vec<PathMap>,
    pub sonarr: Option<Arr>,
    pub radarr: Option<Arr>,
    pub qbittorrent: QBittorrent,
    pub jellyfin: Jellyfin,
    pub stream: Option<Stream>,
    pub bazarr: Option<BazarrCfg>,
    pub tmdb: Option<TmdbCfg>,
}

/// "Onde assistir": serviços de streaming do país, pelo TMDB.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TmdbCfg {
    /// chave da API (v3) ou token de leitura (v4)
    pub api_key: String,
    #[serde(default = "default_region")]
    pub region: String,
}

fn default_region() -> String {
    "BR".into()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BazarrCfg {
    pub url: String,
    pub api_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// Um endereço ou uma lista, na ordem de prioridade (a página troca
    /// sozinha para o mais rápido que alcança): ex. cabo direto, rede local,
    /// Tailscale. O primeiro é o que o próprio Anubis usa (ffmpeg).
    #[serde(deserialize_with = "one_or_many")]
    pub listen: Vec<String>,
    /// Se definido, as rotas que alteram algo exigem `Authorization: Bearer <token>`.
    pub token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    /// Qualquer caminho dentro do sistema de arquivos vigiado.
    pub path: PathBuf,
    /// Começa a limpar acima deste uso (%).
    pub start_percent: f64,
    /// Para de limpar ao chegar neste uso (%).
    pub stop_percent: f64,
    /// Nunca apaga o que entrou na biblioteca há menos que isso.
    pub min_age_days: i64,
    /// Checagem periódica, além dos gatilhos (webhook).
    pub interval_minutes: u64,
    /// true: só registra o que apagaria.
    pub dry_run: bool,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct PathMap {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Arr {
    pub url: String,
    pub api_key: String,
    /// perfil de qualidade dos pedidos feitos pela busca
    #[serde(default = "default_profile")]
    pub quality_profile: String,
    /// pasta raiz dos pedidos (como o Sonarr/Radarr a enxerga)
    pub root_folder: Option<String>,
    /// Sonarr: pasta raiz dos animes (séries com o gênero "Anime")
    pub anime_root_folder: Option<String>,
}

fn default_profile() -> String {
    "Any".into()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QBittorrent {
    pub url: String,
    pub username: String,
    /// Arquivo com a senha (uma linha), para não repetir o segredo aqui.
    pub password_file: PathBuf,
}

/// "Assistir agora": o Anubis baixa com o próprio motor (librqbit) e toca
/// enquanto baixa.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stream {
    /// onde baixar (caminho do host), dentro do mesmo /data do Sonarr/Radarr
    /// e do qBittorrent para o import virar hardlink
    pub download_dir: PathBuf,
    /// endereços internos do Docker nos links de download → endereço alcançável
    /// pelo host (ex.: "http://prowlarr:9696" → "http://10.10.10.2:9696")
    #[serde(default)]
    pub url_rewrites: Vec<PathMap>,
    /// estado que sobrevive a reinícios: sessão do librqbit e pedidos em
    /// andamento (o systemd cria /var/lib/anubis com StateDirectory=)
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    #[serde(default)]
    pub transcode: Transcode,
}

fn default_state_dir() -> PathBuf {
    PathBuf::from("/var/lib/anubis")
}

/// Conversão na hora para o que o navegador não toca (H.265, áudio AC3/DTS…).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Transcode {
    pub ffmpeg: String,
    pub ffprobe: String,
    /// rodar o ffmpeg dentro deste container (ex.: "jellyfin", que traz o
    /// jellyfin-ffmpeg) com `docker exec`, em vez de um ffmpeg do host
    pub container: Option<String>,
    /// "x264" (CPU) ou "vaapi" (GPU Intel/AMD, via `device`)
    pub encoder: String,
    pub device: String,
    /// altura máxima do vídeo convertido (o original menor fica como está)
    pub max_height: u32,
}

impl Default for Transcode {
    fn default() -> Self {
        Self {
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
            container: None,
            encoder: "x264".into(),
            device: "/dev/dri/renderD128".into(),
            max_height: 720,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Jellyfin {
    pub url: String,
    pub api_key: String,
    /// como o Jellyfin enxerga a biblioteca (ex.: "/media")...
    pub media_path: String,
    /// ...e onde ela está no host (ex.: "/mnt/raid0/anubis/media")
    pub host_media_path: String,
    /// endereço do Jellyfin para os links abertos no navegador
    pub public_url: Option<String>,
}

fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("lendo {}", path.display()))?;
        let cfg: Config = toml::from_str(&text).with_context(|| format!("interpretando {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        let s = &self.storage;
        if !(0.0..=100.0).contains(&s.stop_percent) || !(0.0..=100.0).contains(&s.start_percent) {
            bail!("storage: percentuais devem estar entre 0 e 100");
        }
        if s.stop_percent >= s.start_percent {
            bail!("storage: stop_percent ({}) precisa ser menor que start_percent ({})", s.stop_percent, s.start_percent);
        }
        if s.min_age_days < 0 || s.interval_minutes == 0 {
            bail!("storage: min_age_days >= 0 e interval_minutes > 0");
        }
        if self.server.listen.is_empty() {
            bail!("server: listen precisa de ao menos um endereço");
        }
        if self.sonarr.is_none() && self.radarr.is_none() {
            bail!("configure ao menos [sonarr] ou [radarr]");
        }
        Ok(())
    }

    /// Caminho do container → caminho no host (o prefixo mais longo vence).
    pub fn host_path(&self, container_path: &str) -> PathBuf {
        map_path(&self.paths, container_path)
    }

    fn jellyfin_map(&self) -> PathMap {
        PathMap { from: self.jellyfin.media_path.clone(), to: self.jellyfin.host_media_path.clone() }
    }

    /// Caminho do Jellyfin → caminho no host.
    pub fn host_path_from_jellyfin(&self, p: &str) -> PathBuf {
        map_path(&[self.jellyfin_map()], p)
    }

    /// Caminho no host → como o Sonarr/Radarr/qBittorrent o enxergam.
    pub fn container_path(&self, host: &std::path::Path) -> Option<String> {
        let rev: Vec<PathMap> = self.paths.iter().map(|m| PathMap { from: m.to.clone(), to: m.from.clone() }).collect();
        let host = host.to_str()?;
        let mapped = map_path(&rev, host);
        (mapped.as_os_str() != host).then(|| mapped.to_string_lossy().into_owned())
    }

    /// Caminho de um container (Sonarr/Radarr) → caminho do Jellyfin.
    pub fn jellyfin_path_from_container(&self, p: &str) -> Option<String> {
        self.jellyfin_path(&self.host_path(p))
    }

    /// Caminho no host → caminho do Jellyfin (`None` se fora da biblioteca dele).
    pub fn jellyfin_path(&self, host: &std::path::Path) -> Option<String> {
        let m = self.jellyfin_map();
        let rev = PathMap { from: m.to, to: m.from };
        let host = host.to_str()?;
        let mapped = map_path(std::slice::from_ref(&rev), host);
        (mapped.as_os_str() != host).then(|| mapped.to_string_lossy().into_owned())
    }
}

pub fn map_path(maps: &[PathMap], p: &str) -> PathBuf {
    let best = maps
        .iter()
        .filter(|m| p == m.from || p.starts_with(&format!("{}/", m.from.trim_end_matches('/'))))
        .max_by_key(|m| m.from.len());
    match best {
        Some(m) => PathBuf::from(format!("{}{}", m.to.trim_end_matches('/'), &p[m.from.trim_end_matches('/').len()..])),
        None => PathBuf::from(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn maps() -> Vec<PathMap> {
        vec![
            PathMap { from: "/data".into(), to: "/mnt/raid0/anubis".into() },
            PathMap { from: "/media".into(), to: "/mnt/raid0/anubis/media".into() },
        ]
    }

    #[test]
    fn maps_container_paths_to_host() {
        let m = maps();
        assert_eq!(map_path(&m, "/data/media/series/X/e1.mkv"), PathBuf::from("/mnt/raid0/anubis/media/series/X/e1.mkv"));
        assert_eq!(map_path(&m, "/media/movies/F (2020)/f.mkv"), PathBuf::from("/mnt/raid0/anubis/media/movies/F (2020)/f.mkv"));
        assert_eq!(map_path(&m, "/data"), PathBuf::from("/mnt/raid0/anubis"));
    }

    #[test]
    fn maps_host_paths_back_to_jellyfin() {
        let toml = r#"
            [server]
            listen = "127.0.0.1:0"
            [storage]
            path = "/"
            start_percent = 85.0
            stop_percent = 75.0
            min_age_days = 7
            interval_minutes = 30
            dry_run = true
            [radarr]
            url = "http://x"
            api_key = "k"
            [qbittorrent]
            url = "http://x"
            username = "admin"
            password_file = "/dev/null"
            [jellyfin]
            url = "http://x"
            api_key = "k"
            media_path = "/media"
            host_media_path = "/mnt/raid0/anubis/media"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let host = std::path::Path::new("/mnt/raid0/anubis/media/movies/F (1968)/f.mp4");
        assert_eq!(cfg.jellyfin_path(host).as_deref(), Some("/media/movies/F (1968)/f.mp4"));
        assert_eq!(cfg.host_path_from_jellyfin("/media/movies/F (1968)/f.mp4"), host);
        assert_eq!(cfg.jellyfin_path(std::path::Path::new("/mnt/raid0/anubis/blackhole/x.mp4")), None);
    }

    #[test]
    fn prefix_must_end_at_a_path_boundary() {
        // "/database" não é "/data"
        assert_eq!(map_path(&maps(), "/database/x"), PathBuf::from("/database/x"));
    }
}
