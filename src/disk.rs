//! O que o disco diz: uso do sistema de arquivos e identidade (inode) dos arquivos.

use crate::planner::{Inode, Usage};
use anyhow::{Context, Result};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

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
