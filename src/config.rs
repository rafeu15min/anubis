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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// ex.: "10.10.10.2:7070" (link direto PC <-> FeuVault)
    pub listen: String,
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
