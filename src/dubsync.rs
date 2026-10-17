//! Sincronia da dublagem em arquivos "dual áudio" (dublagem PT-BR e áudio
//! original no mesmo arquivo). Há releases em que a dublagem foi colada com um
//! atraso fixo (Loki S01 "MiXED.DUAL-Vinci": 2,82 s adiantada no E01). A faixa
//! original é a referência: música e efeitos são os mesmos nas duas, então dá
//! para medir onde a dublagem cai, com o mesmo método do "Juntar dublagem".
//!
//! - Só importa quando a primeira faixa (a que toca) é a dublagem e há uma
//!   faixa original para comparar.
//! - Cada arquivo é medido uma vez (`state_dir/sincronia.json`).
//! - Ao tocar, o ffmpeg do player desloca a dublagem (stream::transcode).
//! - Ao terminar um download, a biblioteca recebe uma cópia com a dublagem
//!   corrigida; o original continua no torrent, semeando.

use crate::mux::is_portuguese;
use crate::orchestrator::Orchestrator;
use crate::stream::{Job, Phase, folder_name};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

const CACHE_FILE: &str = "sincronia.json";
/// abaixo disso a diferença não se percebe
const MIN_OFFSET: f64 = 0.05;

/// Estado da dublagem de um arquivo. `seconds` segue a convenção do "Juntar
/// dublagem": positivo = a cena chega depois na dublagem (atrasada).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DubSync {
    Checking,
    /// nada a corrigir: a faixa que toca não é dublagem, não há original para
    /// comparar, ou já está em sincronia
    Ok,
    Offset { seconds: f64 },
    /// não deu para medir (pontos discordam: cortes diferentes); toca como está
    Unknown { reason: String },
}

impl DubSync {
    pub fn offset(&self) -> Option<f64> {
        match self {
            DubSync::Offset { seconds } => Some(*seconds),
            _ => None,
        }
    }
}

/// Uma escrita por vez no arquivo de resultados.
static CACHE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn cache_key(file: &Path, len: u64) -> String {
    format!("{}|{len}", file.display())
}

fn read_cache(path: &Path) -> HashMap<String, DubSync> {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

impl Orchestrator {
    /// Mede (uma vez por arquivo) a sincronia da dublagem de `file`, que precisa
    /// estar completo no disco.
    pub(crate) async fn measure_dub(&self, file: &Path, len: u64) -> DubSync {
        let Ok(streamer) = self.streamer() else { return DubSync::Ok };
        let path = streamer.state_dir.join(CACHE_FILE);
        let key = cache_key(file, len);
        if let Some(known) = read_cache(&path).remove(&key) {
            return known;
        }
        let result = match self.dub_offset(file).await {
            Ok(Some(o)) if o.abs() >= MIN_OFFSET => DubSync::Offset { seconds: o },
            Ok(_) => DubSync::Ok,
            Err(e) => DubSync::Unknown { reason: format!("{e:#}") },
        };
        let _guard = CACHE_LOCK.lock().await;
        let mut all = read_cache(&path);
        all.insert(key, result.clone());
        crate::disk::save_json_or_warn(&path, &all).await;
        result
    }

    /// Atraso da dublagem (primeira faixa) em relação à primeira faixa original.
    /// `None`: nada a medir.
    async fn dub_offset(&self, file: &Path) -> Result<Option<f64>> {
        let p = self.probe_file(file).await?;
        let Some(first) = p.tracks.first() else { return Ok(None) };
        if !is_portuguese(first) {
            return Ok(None);
        }
        let Some(original) = p.tracks.iter().find(|t| !is_portuguese(t)) else { return Ok(None) };
        let report = self.check_sync(file, &p, original.n, file, &p, first.n).await?;
        if (report.speed - 1.0).abs() > 1e-9 {
            bail!("a dublagem parece ter outra velocidade ({:.4}×)", report.speed);
        }
        Ok(Some(report.offset))
    }

    /// Mede a dublagem do que está sendo assistido, se o arquivo já está
    /// completo (baixando, a medição acontece no fim do download).
    pub(crate) fn check_dub(self: &Arc<Self>, job: &Arc<Job>) {
        if job.dub().is_some() || job.phase().0 == Phase::Downloading {
            return;
        }
        job.set_dub(DubSync::Checking);
        let (me, job) = (self.clone(), job.clone());
        tokio::spawn(async move {
            let sync = me.measure_dub(job.host_file(), job.info.file_len).await;
            log_sync(&job.info.title, &sync);
            job.set_dub(sync);
        });
    }

    /// Cópia de `file` com a dublagem (primeira faixa) deslocada para bater
    /// com o vídeo; o resto (vídeo, outros áudios, legendas) é copiado.
    pub(crate) async fn fix_dub(&self, file: &Path, offset: f64) -> Result<PathBuf> {
        let stream_cfg = self.cfg.stream.as_ref().context("configure [stream]")?;
        let probed = self.probe_file(file).await?;
        let pt = probed.tracks.first().context("arquivo sem áudio")?;
        let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("video");
        let dir = stream_cfg.download_dir.join(format!("{} (sincronizado)", folder_name(stem)));
        std::fs::create_dir_all(&dir).with_context(|| format!("criando {}", dir.display()))?;
        let out = dir.join(format!("{stem}.mkv"));

        let chain = if offset >= 0.0 {
            format!("atrim=start={offset:.3},asetpts=PTS-STARTPTS")
        } else {
            format!("adelay=delays={}:all=1", (-offset * 1000.0).round())
        };
        let title = if pt.title.is_empty() { "Português (BR)".to_owned() } else { pt.title.clone() };
        let mp4 = file.extension().and_then(|e| e.to_str()).is_some_and(|e| ["mp4", "m4v", "mov"].contains(&e.to_lowercase().as_str()));
        let mut args: Vec<String> = vec!["-nostdin".into(), "-y".into(), "-loglevel".into(), "error".into(), "-i".into(), self.tool_path(file)?];
        args.extend(["-filter_complex".into(), format!("[0:a:{}]{chain}[dub]", pt.n)]);
        // dublagem corrigida primeiro; os demais áudios sem a dublagem original
        args.extend(["-map".into(), "0:v:0".into(), "-map".into(), "[dub]".into(), "-map".into(), "0:a".into(), "-map".into(), format!("-0:a:{}", pt.n)]);
        args.extend(["-map", "0:s?", "-map", "0:t?", "-map_metadata", "0", "-map_chapters", "0", "-c", "copy", "-c:a:0", "aac"].map(String::from));
        args.extend(["-b:a:0".into(), format!("{}k", (pt.channels.clamp(1, 6) * 64).max(128))]);
        if mp4 {
            args.extend(["-c:s", "srt"].map(String::from));
        }
        args.extend(["-metadata:s:a:0".into(), "language=por".into(), "-metadata:s:a:0".into(), format!("title={title} (sincronizada)")]);
        args.extend(["-disposition:a", "0", "-disposition:a:0", "default"].map(String::from));
        args.push(self.tool_path(&out)?);

        let ffmpeg = self.streamer()?.transcode.ffmpeg.clone();
        let res = self.media_tool(&ffmpeg, Some("anubis-sincronia")).await?.args(&args).kill_on_drop(true).output().await?;
        if !res.status.success() {
            let _ = std::fs::remove_dir_all(&dir);
            bail!("ffmpeg: {}", String::from_utf8_lossy(&res.stderr).trim());
        }
        Ok(out)
    }
}

pub(crate) fn log_sync(title: &str, sync: &DubSync) {
    match sync {
        DubSync::Offset { seconds } => info!(
            "{title}: dublagem {:.2} s {} — corrigindo",
            seconds.abs(),
            if *seconds > 0.0 { "atrasada" } else { "adiantada" }
        ),
        DubSync::Unknown { reason } => warn!("{title}: sincronia da dublagem não medida: {reason}"),
        _ => {}
    }
}
