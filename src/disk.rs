//! O que o disco diz: uso do sistema de arquivos e identidade (inode) dos arquivos.

use crate::planner::{Inode, Usage};
use anyhow::{Context, Result};
use serde::Serialize;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use tracing::warn;

/// Uso do sistema de arquivos que contém `path` (o `avail` é o que um
/// usuário comum pode usar, como no `df`).
pub fn usage(path: &Path) -> Result<Usage> {
    let s = rustix::fs::statvfs(path).with_context(|| format!("statvfs {}", path.display()))?;
    Ok(Usage { total: s.f_blocks * s.f_frsize, avail: s.f_bavail * s.f_frsize })
}

pub struct FileId {
    pub inode: Inode,
    pub nlink: u64,
    pub size: u64,
}

/// Identidade do arquivo no disco; `None` se ele não existe mais.
pub fn file_id(path: &Path) -> Option<FileId> {
    let m = std::fs::metadata(path).ok()?;
    m.is_file().then(|| FileId { inode: (m.dev(), m.ino()), nlink: m.nlink(), size: m.len() })
}

/// Grava `value` como JSON em `path` por inteiro e troca de uma vez
/// (`<arquivo>.tmp` e rename): um desligamento no meio não deixa o arquivo
/// pela metade.
pub async fn save_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    let tmp = PathBuf::from(name);
    tokio::fs::write(&tmp, bytes).await.with_context(|| format!("gravando {}", tmp.display()))?;
    tokio::fs::rename(&tmp, path).await.with_context(|| format!("trocando {}", path.display()))
}

/// Como `save_json`, para estado que se refaz sozinho: se não gravar, só avisa no log.
pub async fn save_json_or_warn<T: Serialize + ?Sized>(path: &Path, value: &T) {
    if let Err(e) = save_json(path, value).await {
        warn!("não gravei {}: {e:#}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("anubis-disk-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn save_json_replaces_the_file_and_leaves_no_temp() {
        let dir = scratch("ok");
        let path = dir.join("estado.json");
        save_json(&path, &vec![1, 2, 3]).await.unwrap();
        save_json(&path, &vec![4]).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[4]");
        assert!(!dir.join("estado.json.tmp").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn save_json_reports_a_failed_write() {
        let path = std::env::temp_dir().join("anubis-disk-nao-existe").join("estado.json");
        assert!(save_json(&path, &1).await.is_err());
    }
}
