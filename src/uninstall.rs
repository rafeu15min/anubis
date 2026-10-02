//! Desinstalar um título pela página, por escolha da pessoa: o filme, um
//! episódio ou os episódios baixados de uma série. O arquivo sai da
//! biblioteca (Radarr/Sonarr desmarcam, para não baixar de novo), os torrents
//! do qBittorrent que semeiam o mesmo dado saem com ele e o Jellyfin é avisado.
//!
//! Separado da limpeza automática (disco cheio): aqui não há meta de espaço,
//! idade mínima nem proteção do que está sendo assistido — a pessoa escolheu.

use crate::disk;
use crate::orchestrator::Orchestrator;
use crate::planner::Inode;
use crate::search::Kind;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use tracing::{info, warn};

/// Um arquivo da biblioteca a apagar.
struct Item {
    file: FileRef,
    /// caminho no host
    path: PathBuf,
    inode: Inode,
    nlink: u64,
    size: u64,
}

enum FileRef {
    Movie { file_id: i64, movie_id: i64 },
    Episode { file_id: i64, episode_ids: Vec<i64> },
}

/// Torrent do qBittorrent com algum dado do título.
struct Seeding {
    hash: String,
    name: String,
    /// inodes de todos os arquivos dele
    inodes: Vec<(Inode, u64)>,
}

/// O que sairia, para a página confirmar antes.
#[derive(Debug, Serialize)]
pub struct Preview {
    pub title: String,
    pub files: usize,
    /// bytes que voltam a ficar livres
    pub frees: u64,
    /// torrents que saem junto
    pub torrents: Vec<String>,
    /// torrents que ficam (têm outros títulos da biblioteca): o dado deste
    /// título continua neles, semeando
    pub kept: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Uninstalled {
    pub title: String,
    pub removed: usize,
    pub frees: u64,
    pub errors: Vec<String>,
}

struct Plan {
    title: String,
    items: Vec<Item>,
    remove: Vec<Seeding>,
    kept: Vec<String>,
    frees: u64,
}

impl Orchestrator {
    pub async fn uninstall_preview(&self, kind: Kind, id: i64) -> Result<Preview> {
        let p = self.uninstall_plan(kind, id).await?;
        Ok(Preview { title: p.title, files: p.items.len(), frees: p.frees, torrents: p.remove.into_iter().map(|t| t.name).collect(), kept: p.kept })
    }

    pub async fn uninstall(&self, kind: Kind, id: i64) -> Result<Uninstalled> {
        let p = self.uninstall_plan(kind, id).await?;
        let mut errors = vec![];
        let mut gone = vec![]; // caminhos (do Jellyfin) apagados
        let mut removed = 0;
        // primeiro a biblioteca: se o torrent falhar depois, ele fica, mas o
        // título já saiu (o contrário deixaria a biblioteca apontando para nada)
        for it in &p.items {
            let res = match &it.file {
                FileRef::Movie { file_id, movie_id } => self.radarr.as_ref().context("Radarr não configurado")?.delete_movie_file(*file_id, *movie_id).await,
                FileRef::Episode { file_id, episode_ids } => self.sonarr.as_ref().context("Sonarr não configurado")?.delete_episode_file(*file_id, episode_ids).await,
            };
            match res {
                Ok(()) => {
                    removed += 1;
                    gone.extend(self.cfg.jellyfin_path(&it.path));
                }
                Err(e) => errors.push(format!("{}: {e:#}", it.path.display())),
            }
        }
        if errors.is_empty() && !p.remove.is_empty() {
            let hashes: Vec<String> = p.remove.iter().map(|t| t.hash.clone()).collect();
            if let Err(e) = self.qbit.delete(&hashes).await {
                errors.push(format!("qBittorrent: {e:#}"));
            }
        }
        // a série inteira: o Sonarr para de buscar episódios novos dela
        if let (Kind::Series, Some(sonarr)) = (kind, &self.sonarr)
            && let Ok(Some(s)) = sonarr.find("series", "tvdbId", id).await
            && let Some(sid) = s["id"].as_i64()
            && let Err(e) = sonarr.unmonitor_series(sid).await
        {
            errors.push(format!("Sonarr: {e:#}"));
        }
        if let Err(e) = self.jellyfin.notify_deleted(&gone).await {
            // a próxima varredura do Jellyfin tira o item
            warn!("Jellyfin não foi avisado: {e:#}");
        }
        crate::live::refresh_arr(self).await;
        info!("desinstalado: {} ({removed} arquivos, {} MB, {} torrents){}", p.title, p.frees >> 20, p.remove.len(), if errors.is_empty() { String::new() } else { format!(" — erros: {}", errors.join("; ")) });
        Ok(Uninstalled { title: p.title, removed, frees: if errors.is_empty() { p.frees } else { 0 }, errors })
    }

    /// Os arquivos do título e os torrents que saem com eles.
    async fn uninstall_plan(&self, kind: Kind, id: i64) -> Result<Plan> {
        let (title, files) = self.library_files(kind, id).await?;
        let items: Vec<Item> = files
            .into_iter()
            .filter_map(|(file, path)| {
                let host = self.cfg.host_path(&path);
                let Some(f) = disk::file_id(&host) else {
                    warn!("{title}: arquivo não encontrado em {}", host.display());
                    return None;
                };
                Some(Item { file, path: host, inode: f.inode, nlink: f.nlink, size: f.size })
            })
            .collect();
        if items.is_empty() {
            bail!("{title}: nada na biblioteca");
        }
        let mine: HashSet<Inode> = items.iter().map(|i| i.inode).collect();
        let (remove, kept) = self.seeding_with(&mine).await?;
        // um dado libera espaço quando todos os nomes dele somem: o da
        // biblioteca e os dos torrents que saem
        let mut frees = 0;
        for it in &items {
            let in_torrents: u64 = remove.iter().map(|t| t.inodes.iter().filter(|(i, _)| *i == it.inode).count() as u64).sum();
            if 1 + in_torrents >= it.nlink {
                frees += it.size;
            }
        }
        // arquivos só do torrent (amostras, extras) também liberam espaço
        for t in &remove {
            for (inode, size) in &t.inodes {
                if !mine.contains(inode) {
                    frees += size;
                }
            }
        }
        Ok(Plan { title, items, remove, kept, frees })
    }

    /// Nome e arquivos (com o caminho como o Radarr/Sonarr o veem) do título:
    /// filme e série pelo id da página (TMDB/TVDB), episódio pelo do Sonarr.
    async fn library_files(&self, kind: Kind, id: i64) -> Result<(String, Vec<(FileRef, String)>)> {
        match kind {
            Kind::Movie => {
                let radarr = self.radarr.as_ref().context("Radarr não configurado")?;
                let m = radarr.find("movie", "tmdbId", id).await?.context("filme fora do Radarr")?;
                let title = m["title"].as_str().unwrap_or_default().to_owned();
                let (Some(movie_id), Some(file_id), Some(path)) = (m["id"].as_i64(), m["movieFile"]["id"].as_i64(), m["movieFile"]["path"].as_str()) else {
                    bail!("{title}: sem arquivo na biblioteca");
                };
                Ok((title, vec![(FileRef::Movie { file_id, movie_id }, path.to_owned())]))
            }
            Kind::Series => {
                let sonarr = self.sonarr.as_ref().context("Sonarr não configurado")?;
                let s = sonarr.find("series", "tvdbId", id).await?.context("série fora do Sonarr")?;
                let sid = s["id"].as_i64().context("série sem id")?;
                let title = s["title"].as_str().unwrap_or_default().to_owned();
                let mut eps: HashMap<i64, Vec<i64>> = HashMap::new();
                for e in sonarr.episodes(sid).await? {
                    if e.episode_file_id > 0 {
                        eps.entry(e.episode_file_id).or_default().push(e.id);
                    }
                }
                let files = sonarr.episode_files(sid).await?;
                Ok((title, files.into_iter().map(|f| (FileRef::Episode { file_id: f.id, episode_ids: eps.remove(&f.id).unwrap_or_default() }, f.path)).collect()))
            }
            Kind::Episode => {
                let sonarr = self.sonarr.as_ref().context("Sonarr não configurado")?;
                let e = sonarr.episode(id).await?;
                let file_id = e["episodeFileId"].as_i64().filter(|f| *f > 0).context("episódio sem arquivo na biblioteca")?;
                let sid = e["seriesId"].as_i64().context("episódio sem série")?;
                let series = sonarr.series_by_id(sid).await?;
                let title = format!(
                    "{} S{:02}E{:02}",
                    series["title"].as_str().unwrap_or_default(),
                    e["seasonNumber"].as_i64().unwrap_or(0),
                    e["episodeNumber"].as_i64().unwrap_or(0)
                );
                let file = sonarr.episode_file(file_id).await?;
                let path = file["path"].as_str().context("arquivo sem caminho")?.to_owned();
                // um arquivo pode ter mais de um episódio (S01E01-E02)
                let episode_ids = sonarr.episodes(sid).await?.into_iter().filter(|x| x.episode_file_id == file_id).map(|x| x.id).collect();
                Ok((title, vec![(FileRef::Episode { file_id, episode_ids }, path)]))
            }
        }
    }

    /// Torrents do qBittorrent com algum dos inodes: os que só têm o título
    /// (e extras sem nome na biblioteca) saem; os que têm outros títulos da
    /// biblioteca (um pacote da temporada, ao apagar um episódio) ficam.
    async fn seeding_with(&self, mine: &HashSet<Inode>) -> Result<(Vec<Seeding>, Vec<String>)> {
        self.qbit.login().await?;
        let (mut remove, mut kept) = (vec![], vec![]);
        for t in self.qbit.torrents().await? {
            let inodes: Vec<(Inode, u64, u64)> = self
                .qbit
                .files(&t.hash)
                .await?
                .into_iter()
                .filter_map(|f| disk::file_id(&self.cfg.host_path(&format!("{}/{}", t.save_path.trim_end_matches('/'), f.name))))
                .map(|f| (f.inode, f.size, f.nlink))
                .collect();
            if !inodes.iter().any(|(i, _, _)| mine.contains(i)) {
                continue;
            }
            // outro arquivo com mais de um nome = está na biblioteca como outro título
            if inodes.iter().any(|(i, _, nlink)| !mine.contains(i) && *nlink > 1) {
                kept.push(t.name);
            } else {
                remove.push(Seeding { hash: t.hash, name: t.name, inodes: inodes.into_iter().map(|(i, s, _)| (i, s)).collect() });
            }
        }
        Ok((remove, kept))
    }
}
