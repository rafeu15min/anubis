//! Um ciclo de limpeza: inventário (APIs + disco) → plano → execução.
//! Um ciclo por vez (mutex): gatilhos simultâneos esperam o anterior acabar.

use crate::config::Config;
use crate::disk;
use crate::planner::{self, Media, MediaRef, Plan, Rules, Torrent, TorrentFile, Watching};
use crate::services::{Arr, Bazarr, Jellyfin, QBittorrent, Tmdb};
use crate::stream::Streamer;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::collections::HashMap;
use tokio::sync::Mutex;
use tracing::{info, warn};

pub struct Orchestrator {
    pub cfg: Config,
    pub(crate) sonarr: Option<Arr>,
    pub(crate) radarr: Option<Arr>,
    pub(crate) qbit: QBittorrent,
    pub(crate) jellyfin: Jellyfin,
    pub(crate) bazarr: Option<Bazarr>,
    pub(crate) tmdb: Option<Tmdb>,
    pub(crate) prowlarr: Option<crate::services::Prowlarr>,
    /// "Abrir no Jellyfin" já logado: (usuário, senha)
    pub(crate) jellyfin_login: Option<(String, String)>,
    /// motor do "assistir agora" (só no modo serve, com [stream] configurado)
    pub(crate) streamer: Option<std::sync::Arc<Streamer>>,
    /// "juntar dublagem" em andamento ou que falharam, por tmdbId
    pub(crate) muxes: Mutex<HashMap<i64, std::sync::Arc<crate::mux::MuxJob>>>,
    /// última busca de versões de um pedido parado (evita repetir nos indexadores)
    /// central de estado (pedidos e downloads) que as rotas consultam
    pub(crate) live: crate::live::Live,
    /// listas do TMDB da vitrine (6 h) e ids do TVDB das séries
    pub(crate) showcase_cache: crate::discover::Cache,
    /// recomendações do TMDB por semente e a fileira "Para você" pronta
    pub(crate) for_you_cache: crate::foryou::Cache,
    /// histórico de outros serviços (Netflix, AniList) para o "Para você"
    pub(crate) external: crate::external::External,
    /// perfis do Sonarr que o Anubis usa (o dos pedidos e o sem dublagem)
    pub(crate) profile_ids: Mutex<Option<crate::prefs::ProfileIds>>,
    pub(crate) versions_cache: Mutex<HashMap<(crate::search::Kind, i64), (std::time::Instant, crate::search::Versions)>>,
    running: Mutex<()>,
    /// último ciclo executado (não os de só planejar), para GET /storage
    pub last: Mutex<Option<serde_json::Value>>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub at: DateTime<Utc>,
    pub dry_run: bool,
    pub plan: Plan,
    /// o que foi de fato apagado (vazio em dry_run)
    pub done: Vec<String>,
    pub errors: Vec<String>,
    /// uso depois da execução
    pub used_percent_after: Option<f64>,
}

impl Orchestrator {
    pub fn new(cfg: Config, streamer: Option<std::sync::Arc<Streamer>>) -> Result<Self> {
        let password = std::fs::read_to_string(&cfg.qbittorrent.password_file)
            .with_context(|| format!("lendo {}", cfg.qbittorrent.password_file.display()))?;
        let jellyfin_login = match (&cfg.jellyfin.login_user, &cfg.jellyfin.login_password_file) {
            (Some(u), Some(f)) => Some((u.clone(), std::fs::read_to_string(f).with_context(|| format!("lendo {}", f.display()))?.trim().to_owned())),
            _ => None,
        };
        Ok(Self {
            jellyfin_login,
            sonarr: cfg.sonarr.as_ref().map(|a| Arr::new("Sonarr", &a.url, &a.api_key)).transpose()?,
            radarr: cfg.radarr.as_ref().map(|a| Arr::new("Radarr", &a.url, &a.api_key)).transpose()?,
            qbit: QBittorrent::new(&cfg.qbittorrent.url, &cfg.qbittorrent.username, password.trim())?,
            jellyfin: Jellyfin::new(&cfg.jellyfin.url, &cfg.jellyfin.api_key)?,
            bazarr: cfg.bazarr.as_ref().map(|b| Bazarr::new(&b.url, &b.api_key)).transpose()?,
            tmdb: cfg.tmdb.as_ref().map(|t| Tmdb::new(&t.api_key)).transpose()?,
            prowlarr: cfg.prowlarr.as_ref().map(|p| crate::services::Prowlarr::new(&p.url, &p.api_key)).transpose()?,
            streamer,
            muxes: Mutex::new(HashMap::new()),
            live: crate::live::Live::default(),
            showcase_cache: crate::discover::Cache::default(),
            for_you_cache: crate::foryou::Cache::default(),
            external: crate::external::External::load(cfg.stream.as_ref().map_or_else(|| std::path::PathBuf::from("/var/lib/anubis"), |s| s.state_dir.clone())),
            profile_ids: Mutex::new(None),
            versions_cache: Mutex::new(HashMap::new()),
            cfg,
            running: Mutex::new(()),
            last: Mutex::new(None),
        })
    }

    fn rules(&self) -> Rules {
        let s = &self.cfg.storage;
        Rules { start_percent: s.start_percent, stop_percent: s.stop_percent, min_age: Duration::days(s.min_age_days) }
    }

    /// Roda um ciclo. `execute = false` só planeja (como o dry_run, sem
    /// depender da configuração).
    pub async fn cycle(&self, execute: bool) -> Result<Report> {
        let report = self.cycle_inner(execute).await?;
        if execute {
            *self.last.lock().await = serde_json::to_value(&report).ok();
        }
        Ok(report)
    }

    async fn cycle_inner(&self, execute: bool) -> Result<Report> {
        let _one_at_a_time = self.running.lock().await;
        let usage = disk::usage(&self.cfg.storage.path)?;
        let (library, torrents, watching) = self.inventory().await?;
        let plan = planner::plan(usage, library, &torrents, &watching, self.rules(), Utc::now());

        let dry_run = self.cfg.storage.dry_run || !execute;
        let mut report = Report { at: Utc::now(), dry_run, plan, done: vec![], errors: vec![], used_percent_after: None };
        if report.plan.deletions.is_empty() {
            return Ok(report);
        }
        for d in &report.plan.deletions {
            let torrents: String = d.torrents.iter().map(|t| format!(" + torrent {}", t.name)).collect();
            info!(dry_run, "{} {} ({} MB, entrou {}){torrents}", if dry_run { "apagaria" } else { "apagando" }, d.media.title, d.frees >> 20, d.media.added.date_naive());
        }
        if dry_run {
            return Ok(report);
        }

        let mut gone = Vec::new(); // caminhos (do Jellyfin) apagados com sucesso
        for d in &report.plan.deletions {
            // primeiro a biblioteca: se o torrent falhar depois, o próximo
            // ciclo ainda o encontra; o contrário deixaria dado órfão
            let res = match &d.media.media {
                MediaRef::Episode { file_id, episode_ids, .. } => match &self.sonarr {
                    Some(s) => s.delete_episode_file(*file_id, episode_ids).await,
                    None => Ok(()),
                },
                MediaRef::Movie { file_id, movie_id } => match &self.radarr {
                    Some(r) => r.delete_movie_file(*file_id, *movie_id).await,
                    None => Ok(()),
                },
            };
            let res = match res {
                Ok(()) => self.qbit.delete(&d.torrents.iter().map(|t| t.hash.clone()).collect::<Vec<_>>()).await,
                Err(e) => Err(e),
            };
            match res {
                Ok(()) => {
                    report.done.push(d.media.title.clone());
                    gone.extend(self.cfg.jellyfin_path(&d.media.path));
                }
                Err(e) => {
                    warn!("falhou ao apagar {}: {e:#}", d.media.title);
                    report.errors.push(format!("{}: {e:#}", d.media.title));
                }
            }
        }
        if let Err(e) = self.jellyfin.notify_deleted(&gone).await {
            // não desfaz nada: a próxima varredura do Jellyfin tira os itens
            warn!("Jellyfin não foi avisado das exclusões: {e:#}");
            report.errors.push(format!("aviso ao Jellyfin: {e:#}"));
        }
        report.used_percent_after = disk::usage(&self.cfg.storage.path).ok().map(|u| u.used_percent());
        info!("limpeza: {} apagados, {} erros, uso agora {:.1}%", report.done.len(), report.errors.len(), report.used_percent_after.unwrap_or(f64::NAN));
        Ok(report)
    }

    async fn inventory(&self) -> Result<(Vec<Media>, Vec<Torrent>, Watching)> {
        let mut library = Vec::new();

        if let Some(sonarr) = &self.sonarr {
            for series in sonarr.series().await? {
                let files = sonarr.episode_files(series.id).await?;
                if files.is_empty() {
                    continue;
                }
                let mut eps: HashMap<i64, Vec<(i64, i64, i64)>> = HashMap::new();
                for e in sonarr.episodes(series.id).await? {
                    if e.episode_file_id > 0 {
                        eps.entry(e.episode_file_id).or_default().push((e.id, e.season_number, e.episode_number));
                    }
                }
                for f in files {
                    let es = eps.remove(&f.id).unwrap_or_default();
                    let code = es.iter().map(|(_, s, n)| format!("S{s:02}E{n:02}")).collect::<Vec<_>>().join("-");
                    let media = MediaRef::Episode { file_id: f.id, series_id: f.series_id, episode_ids: es.iter().map(|e| e.0).collect() };
                    library.extend(self.media(media, format!("{} {code}", series.title), &f.path, f.date_added));
                }
            }
        }
        if let Some(radarr) = &self.radarr {
            for m in radarr.movies().await? {
                let Some(f) = m.movie_file else { continue };
                let title = match m.year {
                    Some(y) => format!("{} ({y})", m.title),
                    None => m.title,
                };
                library.extend(self.media(MediaRef::Movie { file_id: f.id, movie_id: m.id }, title, &f.path, f.date_added));
            }
        }

        self.qbit.login().await?;
        let mut torrents = Vec::new();
        for t in self.qbit.torrents().await? {
            if t.progress < 1.0 {
                continue; // baixando: não é candidato
            }
            let files = self
                .qbit
                .files(&t.hash)
                .await?
                .into_iter()
                .filter_map(|f| disk::file_id(&self.cfg.host_path(&format!("{}/{}", t.save_path.trim_end_matches('/'), f.name))))
                .map(|id| TorrentFile { inode: id.inode, size: id.size, nlink: id.nlink })
                .collect();
            torrents.push(Torrent { hash: t.hash, name: t.name, files });
        }

        let watching = Watching { paths: self.jellyfin.watching().await?.iter().map(|p| self.cfg.host_path_from_jellyfin(p)).collect() };
        Ok((library, torrents, watching))
    }

    /// Junta o que a API diz com o que o disco diz; `None` se o arquivo sumiu.
    fn media(&self, media: MediaRef, title: String, path: &str, added: DateTime<Utc>) -> Option<Media> {
        let host = self.cfg.host_path(path);
        let Some(id) = disk::file_id(&host) else {
            warn!("{title}: arquivo não encontrado em {}", host.display());
            return None;
        };
        Some(Media { media, title, path: host, size: id.size, added, inode: id.inode, nlink: id.nlink })
    }
}
