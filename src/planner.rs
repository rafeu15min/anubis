//! Planejamento da limpeza: decide O QUE apagar, sem apagar nada.
//!
//! Regras (combinadas com o Rafeu):
//! - só age com o disco acima de `start_percent`, e para ao chegar em `stop_percent`;
//! - apaga do mais antigo para o mais novo, pela data em que entrou na biblioteca;
//! - pula o que chegou há menos de `min_age`, o que está sendo assistido e as
//!   séries com algum episódio em andamento.
//!
//! Hardlinks: o vídeo da biblioteca e o arquivo do torrent são o MESMO dado
//! no disco (mesmo inode). Apagar só um dos nomes não libera nada — então o
//! plano apaga a biblioteca E remove o torrent, e só conta como espaço
//! liberado o que perder todos os nomes.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

/// (dispositivo, inode): identifica o dado no disco, seja qual for o nome.
pub type Inode = (u64, u64);

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Usage {
    pub total: u64,
    pub avail: u64,
}

impl Usage {
    pub fn used_percent(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        100.0 * (self.total - self.avail) as f64 / self.total as f64
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum MediaRef {
    Episode { file_id: i64, series_id: i64, episode_ids: Vec<i64> },
    Movie { file_id: i64, movie_id: i64 },
}

impl MediaRef {
    /// Agrupamento para proteção: uma série inteira, ou o próprio filme.
    fn group(&self) -> Group {
        match self {
            MediaRef::Episode { series_id, .. } => Group::Series(*series_id),
            MediaRef::Movie { movie_id, .. } => Group::Movie(*movie_id),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Group {
    Series(i64),
    Movie(i64),
}

/// Um vídeo da biblioteca (Sonarr/Radarr), já com o que o disco diz dele.
#[derive(Debug, Clone, Serialize)]
pub struct Media {
    pub media: MediaRef,
    pub title: String,
    pub path: PathBuf,
    pub size: u64,
    pub added: DateTime<Utc>,
    #[serde(skip)]
    pub inode: Inode,
    /// quantos nomes (hardlinks) o dado tem no disco
    pub nlink: u64,
}

#[derive(Debug, Clone)]
pub struct TorrentFile {
    pub inode: Inode,
    pub size: u64,
    pub nlink: u64,
}

#[derive(Debug, Clone)]
pub struct Torrent {
    pub hash: String,
    pub name: String,
    pub files: Vec<TorrentFile>,
}

/// O que está sendo assistido agora ou está pela metade (Jellyfin).
#[derive(Debug, Default)]
pub struct Watching {
    pub paths: HashSet<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
pub struct Rules {
    pub start_percent: f64,
    pub stop_percent: f64,
    pub min_age: Duration,
}

#[derive(Debug, Clone, Serialize)]
pub struct TorrentRef {
    pub hash: String,
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct Deletion {
    #[serde(flatten)]
    pub media: Media,
    /// torrents removidos junto, com os dados
    pub torrents: Vec<TorrentRef>,
    /// bytes que voltam a ficar livres por causa deste item
    pub frees: u64,
}

#[derive(Debug, Serialize)]
pub struct Skip {
    pub title: String,
    pub reason: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Plan {
    pub used_percent: f64,
    /// bytes a liberar para chegar em `stop_percent` (0 se não precisa)
    pub need: u64,
    pub deletions: Vec<Deletion>,
    pub skipped: Vec<Skip>,
    pub freed: u64,
    pub projected_percent: f64,
    /// true se nem apagando tudo o que é permitido dá para chegar na meta
    pub short: bool,
}

pub fn plan(usage: Usage, mut library: Vec<Media>, torrents: &[Torrent], watching: &Watching, rules: Rules, now: DateTime<Utc>) -> Plan {
    let used = usage.used_percent();
    let target_avail = (usage.total as f64 * (1.0 - rules.stop_percent / 100.0)).ceil() as u64;
    let need = if used > rules.start_percent { target_avail.saturating_sub(usage.avail) } else { 0 };

    let mut out = Plan { used_percent: used, need, deletions: vec![], skipped: vec![], freed: 0, projected_percent: used, short: false };
    if need == 0 {
        return out;
    }

    // de qual torrent cada inode faz parte
    let mut by_inode: HashMap<Inode, Vec<usize>> = HashMap::new();
    for (i, t) in torrents.iter().enumerate() {
        for f in &t.files {
            by_inode.entry(f.inode).or_default().push(i);
        }
    }
    // séries/filmes com algo em andamento: protegidos por inteiro
    let protected: BTreeSet<Group> = library.iter().filter(|m| watching.paths.contains(&m.path)).map(|m| m.media.group()).collect();

    library.sort_by_key(|m| m.added);
    let mut removed_torrents: HashSet<usize> = HashSet::new();
    // nomes já apagados por inode (biblioteca + arquivos de torrent removidos)
    let mut gone: HashMap<Inode, u64> = HashMap::new();

    for m in library {
        if out.freed >= need {
            break;
        }
        if protected.contains(&m.media.group()) {
            out.skipped.push(Skip { title: m.title, reason: "sendo assistido" });
            continue;
        }
        if now - m.added < rules.min_age {
            out.skipped.push(Skip { title: m.title, reason: "recente" });
            continue;
        }

        // torrents que contêm este dado: saem junto
        let ts: Vec<usize> = by_inode.get(&m.inode).cloned().unwrap_or_default();
        let new_ts: Vec<usize> = ts.iter().copied().filter(|t| !removed_torrents.contains(t)).collect();

        // quantos nomes deste dado somem: o da biblioteca + os dos torrents
        let links_gone = 1 + ts.iter().map(|&t| torrents[t].files.iter().filter(|f| f.inode == m.inode).count() as u64).sum::<u64>();
        if links_gone < m.nlink {
            // outro nome fora do Sonarr/Radarr/qBittorrent: apagar não libera nada
            out.skipped.push(Skip { title: m.title, reason: "outra cópia fora do controle do Anubis" });
            continue;
        }

        let mut frees = m.size;
        *gone.entry(m.inode).or_default() += links_gone;
        // arquivos só do torrent (sem nome na biblioteca) também saem
        for &t in &new_ts {
            for f in &torrents[t].files {
                if f.inode == m.inode {
                    continue;
                }
                let g = gone.entry(f.inode).or_default();
                *g += 1;
                if *g == f.nlink {
                    frees += f.size;
                }
            }
            removed_torrents.insert(t);
        }

        out.freed += frees;
        out.deletions.push(Deletion {
            torrents: new_ts.iter().map(|&t| TorrentRef { hash: torrents[t].hash.clone(), name: torrents[t].name.clone() }).collect(),
            media: m,
            frees,
        });
    }

    out.short = out.freed < need;
    out.projected_percent = Usage { total: usage.total, avail: usage.avail + out.freed }.used_percent();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1 << 30;

    fn rules() -> Rules {
        Rules { start_percent: 85.0, stop_percent: 75.0, min_age: Duration::days(7) }
    }
    fn now() -> DateTime<Utc> {
        "2026-09-27T12:00:00Z".parse().unwrap()
    }
    fn days_ago(d: i64) -> DateTime<Utc> {
        now() - Duration::days(d)
    }
    fn movie(id: i64, days: i64, size_gb: u64, nlink: u64) -> Media {
        Media {
            media: MediaRef::Movie { file_id: id, movie_id: id },
            title: format!("filme {id}"),
            path: format!("/lib/m{id}.mkv").into(),
            size: size_gb * GB,
            added: days_ago(days),
            inode: (1, id as u64),
            nlink,
        }
    }
    fn episode(file: i64, series: i64, days: i64, size_gb: u64) -> Media {
        Media {
            media: MediaRef::Episode { file_id: file, series_id: series, episode_ids: vec![file] },
            title: format!("s{series} e{file}"),
            path: format!("/lib/s{series}/e{file}.mkv").into(),
            size: size_gb * GB,
            added: days_ago(days),
            inode: (1, 1000 + file as u64),
            nlink: 1,
        }
    }
    /// disco de 100 GB com `used` GB ocupados
    fn disk(used: u64) -> Usage {
        Usage { total: 100 * GB, avail: (100 - used) * GB }
    }

    #[test]
    fn below_start_does_nothing() {
        let p = plan(disk(85), vec![movie(1, 90, 10, 1)], &[], &Watching::default(), rules(), now());
        assert_eq!(p.need, 0);
        assert!(p.deletions.is_empty());
    }

    #[test]
    fn deletes_oldest_first_until_stop_percent() {
        // 90% usado → precisa liberar 15 GB para chegar a 75%
        let lib = vec![movie(1, 10, 10, 1), movie(2, 300, 10, 1), movie(3, 100, 10, 1)];
        let p = plan(disk(90), lib, &[], &Watching::default(), rules(), now());
        assert_eq!(p.need, 15 * GB);
        let order: Vec<_> = p.deletions.iter().map(|d| d.media.title.as_str()).collect();
        assert_eq!(order, ["filme 2", "filme 3"]); // 300 e 100 dias; o de 10 dias fica
        assert_eq!(p.freed, 20 * GB);
        assert!(!p.short);
        assert!(p.projected_percent <= 75.0);
    }

    #[test]
    fn skips_recent_items() {
        let p = plan(disk(90), vec![movie(1, 3, 20, 1)], &[], &Watching::default(), rules(), now());
        assert!(p.deletions.is_empty());
        assert_eq!(p.skipped[0].reason, "recente");
        assert!(p.short);
    }

    #[test]
    fn protects_the_whole_series_being_watched() {
        // e1 (antigo) e e2 da série 7; alguém está no meio do e2 → a série inteira fica
        let lib = vec![episode(1, 7, 200, 10), episode(2, 7, 200, 10), movie(9, 50, 20, 1)];
        let watching = Watching { paths: [PathBuf::from("/lib/s7/e2.mkv")].into() };
        let p = plan(disk(90), lib, &[], &watching, rules(), now());
        let titles: Vec<_> = p.deletions.iter().map(|d| d.media.title.as_str()).collect();
        assert_eq!(titles, ["filme 9"]);
        assert_eq!(p.skipped.iter().filter(|s| s.reason == "sendo assistido").count(), 2);
    }

    #[test]
    fn removes_the_seeding_torrent_so_space_is_really_freed() {
        // o filme tem 2 nomes: biblioteca + arquivo do torrent (hardlink)
        let lib = vec![movie(1, 90, 20, 2)];
        let t = Torrent { hash: "abc".into(), name: "Filme.1".into(), files: vec![TorrentFile { inode: (1, 1), size: 20 * GB, nlink: 2 }] };
        let p = plan(disk(90), lib, &[t], &Watching::default(), rules(), now());
        assert_eq!(p.deletions[0].torrents[0].hash, "abc");
        assert_eq!(p.freed, 20 * GB);
    }

    #[test]
    fn counts_torrent_only_files_and_removes_a_pack_once() {
        // pacote com e1, e2 (na biblioteca) e um extra que só existe no torrent
        let mut e1 = episode(1, 5, 120, 10);
        let mut e2 = episode(2, 5, 110, 10);
        e1.nlink = 2;
        e2.nlink = 2;
        let pack = Torrent {
            hash: "pack".into(),
            name: "Serie.S01".into(),
            files: vec![
                TorrentFile { inode: e1.inode, size: 10 * GB, nlink: 2 },
                TorrentFile { inode: e2.inode, size: 10 * GB, nlink: 2 },
                TorrentFile { inode: (1, 5555), size: 2 * GB, nlink: 1 },
            ],
        };
        let p = plan(disk(99), vec![e1, e2], &[pack], &Watching::default(), rules(), now());
        // e1 sai com o pacote: libera e1 + o extra; e2 perde o nome do torrent
        // junto (pacote já removido), e libera ao sair da biblioteca
        assert_eq!(p.deletions[0].torrents[0].name, "Serie.S01");
        assert_eq!(p.deletions[0].frees, 12 * GB);
        assert!(p.deletions[1].torrents.is_empty());
        assert_eq!(p.deletions[1].frees, 10 * GB);
        assert_eq!(p.freed, 22 * GB);
    }

    #[test]
    fn skips_data_with_names_outside_anubis_control() {
        // 3 nomes, mas só a biblioteca e 1 torrent são conhecidos: sobraria 1
        let lib = vec![movie(1, 90, 20, 3)];
        let t = Torrent { hash: "abc".into(), name: "F".into(), files: vec![TorrentFile { inode: (1, 1), size: 20 * GB, nlink: 3 }] };
        let p = plan(disk(90), lib, &[t], &Watching::default(), rules(), now());
        assert!(p.deletions.is_empty());
        assert_eq!(p.skipped[0].reason, "outra cópia fora do controle do Anubis");
    }
}
