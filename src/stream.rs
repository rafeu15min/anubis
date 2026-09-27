//! "Assistir agora": o Anubis baixa com o próprio motor de torrent (librqbit)
//! e entrega o vídeo ao player enquanto baixa. Filmes (Radarr) e episódios
//! (Sonarr).
//!
//! - A versão é escolhida entre as que o Radarr/Sonarr aprovam (perfil de
//!   qualidade de sempre); entre elas, prefere as que o navegador toca direto.
//!   Para episódio sem versão própria, usa o pacote da temporada e baixa só o
//!   arquivo daquele episódio.
//! - Sem ninguém assistindo, o librqbit baixa em ordem. Com o player aberto,
//!   as peças da posição dele (janela de 32 MB) vêm primeiro — pular para o
//!   meio reprioriza o download.
//! - O que o navegador não toca (H.265, áudio AC3/DTS, MKV) é convertido na
//!   hora com ffmpeg: só o contêiner/áudio quando dá, o vídeo quando precisa.
//! - Ao terminar: import no Radarr/Sonarr por hardlink e, quando ninguém
//!   assiste há alguns minutos, o torrent passa ao qBittorrent para semear.
//! - Tudo sobrevive a um reinício: a sessão do librqbit e os pedidos ficam em
//!   `state_dir`, e o Anubis retoma cada um de onde parou.

use crate::config::Transcode;
use crate::orchestrator::Orchestrator;
use crate::search::Kind;
use crate::services::TorrentSource;
use anyhow::{Context, Result, bail};
use librqbit::api::TorrentIdOrHash;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, Api, DhtSessionConfig, ManagedTorrent, Session, SessionOptions, SessionPersistenceConfig,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::Mutex;
use tracing::{info, warn};

const VIDEO_EXT: [&str; 6] = ["mp4", "m4v", "mkv", "webm", "mov", "avi"];
/// sem requisições do player por esse tempo → pode entregar ao qBittorrent
const IDLE_BEFORE_HANDOFF: Duration = Duration::from_secs(5 * 60);
const JOBS_FILE: &str = "pedidos.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Downloading,
    /// baixado, entrando na biblioteca
    Importing,
    /// completo e na biblioteca (falta só entregar ao qBittorrent)
    Done,
    Failed,
}

/// O que dá para reconstruir um pedido depois de um reinício.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobInfo {
    pub kind: Kind,
    pub id: i64,
    pub title: String,
    pub release: String,
    /// Radarr: id do filme; Sonarr: id do episódio
    arr_id: i64,
    series_id: Option<i64>,
    file_id: usize,
    pub file_len: u64,
    pub file_name: String,
    host_file: PathBuf,
    folder: PathBuf,
    /// hash do torrent (hex); `None` = arquivo da biblioteca, sem torrent
    info_hash: Option<String>,
    magnet: Option<String>,
    torrent_file: Option<PathBuf>,
    /// só este arquivo do torrent (episódio dentro de um pacote)
    only_file: bool,
    /// próximo episódio: (id, rótulo)
    pub next: Option<(i64, String)>,
    phase: Phase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// o navegador toca o arquivo como está
    Direct,
    /// vídeo copiado; contêiner e/ou áudio convertidos (leve)
    Remux,
    /// vídeo convertido para H.264 (pesado)
    Transcode,
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaInfo {
    pub duration: Option<f64>,
    pub video: Option<String>,
    pub audio: Option<String>,
    pub mode: Mode,
}

pub struct Job {
    pub info: JobInfo,
    handle: StdMutex<Option<Arc<ManagedTorrent>>>,
    phase: StdMutex<(Phase, Option<String>)>,
    last_access: StdMutex<Instant>,
    media: StdMutex<Option<MediaInfo>>,
    probing: StdMutex<bool>,
}

impl Job {
    fn new(info: JobInfo, handle: Option<Arc<ManagedTorrent>>) -> Arc<Self> {
        Arc::new(Self {
            phase: StdMutex::new((info.phase, None)),
            info,
            handle: StdMutex::new(handle),
            last_access: StdMutex::new(Instant::now()),
            media: StdMutex::new(None),
            probing: StdMutex::new(false),
        })
    }
    pub fn phase(&self) -> (Phase, Option<String>) {
        self.phase.lock().unwrap().clone()
    }
    fn handle(&self) -> Option<Arc<ManagedTorrent>> {
        self.handle.lock().unwrap().clone()
    }
    pub fn touch(&self) {
        *self.last_access.lock().unwrap() = Instant::now();
    }
    pub fn media(&self) -> Option<MediaInfo> {
        self.media.lock().unwrap().clone()
    }
    fn snapshot(&self) -> JobInfo {
        JobInfo { phase: self.phase().0, ..self.info.clone() }
    }
}

pub struct Streamer {
    api: Api,
    jobs: Mutex<HashMap<(Kind, i64), Arc<Job>>>,
    state_dir: PathBuf,
    transcode: Transcode,
    /// endereço da própria API (o ffmpeg lê o vídeo por ela)
    self_url: String,
}

/// Uma versão encontrada, como a página mostra.
#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub guid: String,
    pub title: String,
    pub size: u64,
    pub seeders: i64,
    pub rejected: bool,
    pub reasons: Vec<String>,
    /// 2: o navegador toca direto; 1: talvez; 0: vai precisar de conversão (H.265)
    pub playable: u8,
    /// pacote da temporada inteira
    pub pack: bool,
    #[serde(skip)]
    raw: Value,
}

pub enum Started {
    /// pronto para o player (baixando ou já na biblioteca)
    Ready,
    /// nenhuma versão passa no perfil: a página oferece as recusadas
    NoApproved(Vec<Candidate>),
}

fn playable(title: &str) -> u8 {
    let t = title.to_lowercase();
    if ["x265", "h265", "h.265", "hevc"].iter().any(|k| t.contains(k)) {
        0
    } else if ["x264", "h264", "h.264", "avc", "yts", "yify", "av1"].iter().any(|k| t.contains(k)) {
        2
    } else {
        1
    }
}

fn candidates(releases: Vec<Value>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = releases
        .into_iter()
        .filter(|r| r["protocol"] == "torrent" && r["seeders"].as_i64().unwrap_or(0) > 0)
        .map(|r| Candidate {
            guid: r["guid"].as_str().unwrap_or_default().to_owned(),
            title: r["title"].as_str().unwrap_or_default().to_owned(),
            size: r["size"].as_u64().unwrap_or(0),
            seeders: r["seeders"].as_i64().unwrap_or(0),
            rejected: r["rejected"].as_bool().unwrap_or(false),
            reasons: r["rejections"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default(),
            playable: playable(r["title"].as_str().unwrap_or_default()),
            pack: r["fullSeason"].as_bool().unwrap_or(false),
            raw: r,
        })
        .collect();
    sort_candidates(&mut out);
    out
}

/// Aprovadas primeiro; depois episódio avulso antes de pacote; depois as que
/// tocam no navegador; depois a preferência do próprio Radarr/Sonarr e as
/// mais semeadas.
fn sort_candidates(list: &mut [Candidate]) {
    list.sort_by_key(|c| {
        (c.rejected, c.pack, std::cmp::Reverse(c.playable), c.raw["releaseWeight"].as_i64().unwrap_or(i64::MAX), std::cmp::Reverse(c.seeders))
    });
}

/// O nome do arquivo é do episódio `s`x`e`? (S01E05, s1e05, 1x05)
fn is_episode(name: &str, s: i64, e: i64) -> bool {
    let n = name.to_lowercase();
    let pats = [format!("s{s:02}e{e:02}"), format!("s{s}e{e:02}"), format!("{s}x{e:02}")];
    pats.iter().any(|p| {
        n.match_indices(p.as_str()).any(|(i, _)| {
            let after = n[i + p.len()..].chars().next();
            let before = n[..i].chars().last();
            !after.is_some_and(|c| c.is_ascii_digit()) && !before.is_some_and(|c| c.is_ascii_alphanumeric() && !p.starts_with('s'))
        })
    })
}

/// O Sonarr reconheceu esta versão como sendo o episódio `s`x`e`?
fn is_this_episode(release: &Value, s: i64, e: i64) -> bool {
    release["seasonNumber"].as_i64() == Some(s)
        && release["episodeNumbers"].as_array().is_some_and(|n| n.iter().any(|x| x.as_i64() == Some(e)))
}

/// O vídeo a tocar: o maior, ou (episódio) o do episódio pedido.
fn video_file(files: &librqbit::FileInfos, episode: Option<(i64, i64)>) -> Option<(usize, u64, PathBuf)> {
    let videos: Vec<(usize, &librqbit::file_info::FileInfo)> = files
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            f.relative_filename
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| VIDEO_EXT.contains(&e.to_lowercase().as_str()))
        })
        .collect();
    let pick = match episode {
        Some((s, e)) if videos.len() > 1 => videos.iter().filter(|(_, f)| is_episode(&f.relative_filename.to_string_lossy(), s, e)).max_by_key(|(_, f)| f.len),
        _ => videos.iter().max_by_key(|(_, f)| f.len),
    }?;
    Some((pick.0, pick.1.len, pick.1.relative_filename.clone()))
}

/// Nome de pasta seguro a partir do título da versão.
fn folder_name(title: &str) -> String {
    let s: String = title.chars().map(|c| if c == '/' || c.is_control() { ' ' } else { c }).collect();
    s.trim().chars().take(150).collect()
}

fn kind_path(k: Kind) -> &'static str {
    match k {
        Kind::Movie => "movie",
        Kind::Series => "series",
        Kind::Episode => "episode",
    }
}

impl Streamer {
    pub async fn new(state_dir: PathBuf, transcode: Transcode, self_url: String) -> Result<Self> {
        std::fs::create_dir_all(state_dir.join("torrents")).with_context(|| format!("criando {}", state_dir.display()))?;
        let opts = SessionOptions {
            // DHT ligado (acha fontes além dos trackers), sem gravar estado
            dht: Some(DhtSessionConfig { bootstrap_addrs: None, port: None, persistence: None }),
            // a sessão (torrents e o que já baixou) volta depois de um reinício
            persistence: Some(SessionPersistenceConfig::Json { folder: Some(state_dir.join("sessao")) }),
            fastresume: true,
            // o FeuVault não tem IPv6 (o socket em [::] falharia)
            ipv4_only: true,
            ..Default::default()
        };
        let session = Session::new_with_opts(std::env::temp_dir(), opts).await.context("iniciando o librqbit")?;
        Ok(Self { api: Api::new(session, None), jobs: Mutex::new(HashMap::new()), state_dir, transcode, self_url })
    }

    pub async fn job(&self, kind: Kind, id: i64) -> Option<Arc<Job>> {
        self.jobs.lock().await.get(&(kind, id)).cloned()
    }

    /// Grava os pedidos em andamento (os entregues ao qBittorrent saem).
    async fn save(&self) {
        let jobs: Vec<JobInfo> = self.jobs.lock().await.values().filter(|j| j.handle().is_some()).map(|j| j.snapshot()).collect();
        let path = self.state_dir.join(JOBS_FILE);
        let tmp = path.with_extension("tmp");
        let res = serde_json::to_vec_pretty(&jobs).map_err(anyhow::Error::from).and_then(|b| Ok(std::fs::write(&tmp, b)?)).and_then(|_| Ok(std::fs::rename(&tmp, &path)?));
        if let Err(e) = res {
            warn!("não gravei os pedidos em {}: {e:#}", path.display());
        }
    }

    fn find_handle(&self, info_hash: &str) -> Option<Arc<ManagedTorrent>> {
        self.api.session().with_torrents(|it| it.map(|(_, h)| h).find(|h| h.info_hash().as_string() == info_hash).cloned())
    }

    /// Abre o vídeo a partir de `start`. Baixando: pelo librqbit (espera as
    /// peças e as prioriza); completo: direto do disco.
    pub async fn open(&self, job: &Job, start: u64) -> Result<Box<dyn AsyncRead + Send + Unpin>> {
        job.touch();
        if let Some(h) = job.handle() {
            let mut s = h.stream(job.info.file_id).await?;
            s.seek(std::io::SeekFrom::Start(start)).await?;
            return Ok(Box::new(s));
        }
        let mut f = tokio::fs::File::open(&job.info.host_file).await.with_context(|| format!("abrindo {}", job.info.host_file.display()))?;
        f.seek(std::io::SeekFrom::Start(start)).await?;
        Ok(Box::new(f))
    }

    /// Andamento para o player: fase, progresso, velocidade, fontes, trechos
    /// já baixados (frações 0..1, a barra branca) e o formato do vídeo.
    pub fn info(&self, job: &Job) -> Value {
        let (phase, error) = job.phase();
        let base = json!({ "phase": phase, "error": error, "media": job.media(), "next": job.info.next });
        let Some(h) = job.handle().filter(|_| phase != Phase::Done) else {
            let mut v = base;
            v["progress"] = json!(1.0);
            v["have"] = json!([[0.0, 1.0]]);
            return v;
        };
        let stats = serde_json::to_value(h.stats()).unwrap_or_default();
        let total = stats["total_bytes"].as_f64().unwrap_or(0.0);
        let done = stats["progress_bytes"].as_f64().unwrap_or(0.0);
        let mut v = base;
        v["progress"] = json!(if total > 0.0 { done / total } else { 0.0 });
        // MiB/s (o "mbps" do librqbit é mebibytes por segundo)
        v["speed_mbps"] = stats["live"]["download_speed"]["mbps"].clone();
        v["peers"] = stats["live"]["snapshot"]["peer_stats"]["live"].clone();
        v["have"] = json!(self.have_ranges(&h, job.info.file_id).unwrap_or_default());
        v
    }

    fn have_ranges(&self, h: &Arc<ManagedTorrent>, file_id: usize) -> Result<Vec<[f64; 2]>> {
        let range = h.with_metadata(|m| m.file_infos[file_id].piece_range.clone())?;
        let (bf, _) = self.api.api_dump_haves(TorrentIdOrHash::Id(h.id()))?;
        let n = (range.end - range.start).max(1) as f64;
        let mut out: Vec<[f64; 2]> = Vec::new();
        for p in range.clone() {
            if bf.get(p as usize).map(|b| *b) != Some(true) {
                continue;
            }
            let (a, b) = ((p - range.start) as f64 / n, (p - range.start + 1) as f64 / n);
            match out.last_mut() {
                Some(last) if (last[1] - a).abs() < 1e-9 => last[1] = b,
                _ => out.push([a, b]),
            }
        }
        Ok(out)
    }

    /// Comando do ffmpeg/ffprobe: no host, ou dentro do container.
    fn tool(&self, bin: &str, args: &[String], watchdog: bool) -> Command {
        let Some(c) = &self.transcode.container else {
            let mut cmd = Command::new(bin);
            cmd.args(args);
            return cmd;
        };
        let mut cmd = Command::new("docker");
        if watchdog {
            // `docker exec` terminar não mata o processo dentro do container:
            // um vigia lê a entrada e, quando ela fecha (o Anubis largou o
            // player), encerra o ffmpeg
            cmd.args(["exec", "-i", c, "sh", "-c", r#"exec 3<&0; "$0" "$@" & p=$!; (cat <&3 >/dev/null; kill $p 2>/dev/null) >/dev/null 2>&1 & wait $p"#, bin]);
        } else {
            cmd.args(["exec", c, bin]);
        }
        cmd.args(args);
        cmd
    }

    /// De onde o ffmpeg lê: dentro do container só a API do Anubis alcança
    /// o arquivo; no host, o disco direto quando ele já está completo.
    fn input(&self, job: &Job) -> String {
        if self.transcode.container.is_none() && job.handle().is_none() {
            job.info.host_file.to_string_lossy().into_owned()
        } else {
            self.video_url(job)
        }
    }

    fn video_url(&self, job: &Job) -> String {
        format!("{}/watch/{}/{}/video", self.self_url, kind_path(job.info.kind), job.info.id)
    }

    /// Descobre (uma vez) o formato do vídeo e se o navegador toca direto.
    pub fn probe(self: &Arc<Self>, job: &Arc<Job>) {
        {
            let mut p = job.probing.lock().unwrap();
            if *p || job.media().is_some() {
                return;
            }
            *p = true;
        }
        let (me, job) = (self.clone(), job.clone());
        tokio::spawn(async move {
            let args: Vec<String> = ["-v", "error", "-print_format", "json", "-show_format", "-show_streams"].map(String::from).into_iter().chain([me.input(&job)]).collect();
            let out = tokio::time::timeout(Duration::from_secs(120), me.tool(&me.transcode.ffprobe, &args, false).kill_on_drop(true).output()).await;
            let media = match out {
                Ok(Ok(o)) if o.status.success() => classify(&serde_json::from_slice(&o.stdout).unwrap_or_default(), &job.info.file_name),
                other => {
                    let why = match other {
                        Ok(Ok(o)) => String::from_utf8_lossy(&o.stderr).trim().to_owned(),
                        Ok(Err(e)) => e.to_string(),
                        Err(_) => "tempo esgotado".into(),
                    };
                    warn!("{}: sem ffprobe ({why}), tocando direto", job.info.title);
                    MediaInfo { duration: None, video: None, audio: None, mode: Mode::Direct }
                }
            };
            info!("{}: {:?} (vídeo {:?}, áudio {:?})", job.info.title, media.mode, media.video, media.audio);
            *job.media.lock().unwrap() = Some(media);
            *job.probing.lock().unwrap() = false;
        });
    }

    /// Converte na hora a partir de `start` segundos (vídeo fragmentado em
    /// MP4, que o navegador toca enquanto chega).
    pub fn transcode(&self, job: &Job, start: f64) -> Result<(Child, ChildStdout)> {
        let media = job.media().context("formato do vídeo ainda desconhecido")?;
        let t = &self.transcode;
        let input = self.input(job);
        let mut args: Vec<String> = vec!["-nostdin".into(), "-hide_banner".into(), "-loglevel".into(), "error".into()];
        let vaapi = media.mode == Mode::Transcode && t.encoder == "vaapi";
        if vaapi {
            args.extend(["-vaapi_device".into(), t.device.clone()]);
        }
        args.extend(["-ss".into(), format!("{start:.3}"), "-i".into(), input]);
        args.extend(["-map", "0:v:0", "-map", "0:a:0?", "-sn", "-dn"].map(String::from));
        let scale = format!("scale=-2:'min({},ih)'", t.max_height);
        match (media.mode, vaapi) {
            (Mode::Transcode, true) => args.extend(["-vf".into(), format!("{scale},format=nv12,hwupload"), "-c:v".into(), "h264_vaapi".into(), "-b:v".into(), "5M".into(), "-g".into(), "60".into()]),
            (Mode::Transcode, false) => args.extend(
                ["-vf", &scale, "-c:v", "libx264", "-preset", "veryfast", "-crf", "22", "-pix_fmt", "yuv420p", "-g", "60"].map(String::from),
            ),
            _ => args.extend(["-c:v", "copy"].map(String::from)),
        }
        // áudio: AAC/MP3 passam; o resto (AC3, DTS, TrueHD…) vira AAC estéreo
        if media.audio.as_deref().is_some_and(|a| a == "aac" || a == "mp3") {
            args.extend(["-c:a", "copy"].map(String::from));
        } else {
            args.extend(["-c:a", "aac", "-b:a", "192k", "-ac", "2"].map(String::from));
        }
        args.extend(["-movflags", "frag_keyframe+empty_moov+default_base_moof", "-f", "mp4", "pipe:1"].map(String::from));
        job.touch();
        // stdin fica aberta enquanto o Anubis segura o processo (o vigia do
        // container encerra o ffmpeg quando ela fecha)
        let mut child = self
            .tool(&t.ffmpeg, &args, true)
            .stdin(if t.container.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("iniciando o ffmpeg")?;
        let out = child.stdout.take().context("ffmpeg sem saída")?;
        Ok((child, out))
    }
}

/// Decide, pelo que o ffprobe viu, como o navegador vai tocar.
fn classify(probe: &Value, file_name: &str) -> MediaInfo {
    let streams = probe["streams"].as_array().cloned().unwrap_or_default();
    let codec = |kind: &str| streams.iter().find(|s| s["codec_type"] == kind).and_then(|s| s["codec_name"].as_str().map(str::to_owned));
    let (video, audio) = (codec("video"), codec("audio"));
    let duration = probe["format"]["duration"].as_str().and_then(|d| d.parse().ok());
    let v_ok = video.as_deref().is_some_and(|v| ["h264", "av1", "vp9", "vp8"].contains(&v));
    let a_ok = audio.as_deref().is_none_or(|a| ["aac", "mp3", "opus", "vorbis", "flac"].contains(&a));
    let ext = file_name.rsplit('.').next().unwrap_or_default().to_lowercase();
    let container_ok = ["mp4", "m4v", "mov", "webm"].contains(&ext.as_str());
    let mode = if !v_ok {
        Mode::Transcode
    } else if a_ok && container_ok {
        Mode::Direct
    } else {
        Mode::Remux
    };
    MediaInfo { duration, video, audio, mode }
}

impl Orchestrator {
    pub(crate) fn streamer(&self) -> Result<&Arc<Streamer>> {
        self.streamer.as_ref().context("\"assistir agora\" desligado: configure [stream]")
    }

    /// Retoma os pedidos gravados (depois de um reinício).
    pub async fn restore_streams(self: &Arc<Self>) -> Result<()> {
        let streamer = self.streamer()?;
        let path = streamer.state_dir.join(JOBS_FILE);
        let Ok(bytes) = std::fs::read(&path) else { return Ok(()) };
        let saved: Vec<JobInfo> = serde_json::from_slice(&bytes).with_context(|| format!("lendo {}", path.display()))?;
        for info in saved {
            let Some(hash) = info.info_hash.clone() else { continue };
            let handle = match streamer.find_handle(&hash) {
                Some(h) => h,
                None => match self.add_source(&info).await {
                    Ok(h) => h,
                    Err(e) => {
                        warn!("{}: não deu para retomar: {e:#}", info.title);
                        continue;
                    }
                },
            };
            info!("{}: retomado ({:?})", info.title, info.phase);
            let job = Job::new(info, Some(handle));
            streamer.jobs.lock().await.insert((job.info.kind, job.info.id), job.clone());
            tokio::spawn(finish(self.clone(), job));
        }
        Ok(())
    }

    /// Adiciona de novo um torrent a partir do que ficou gravado.
    async fn add_source(&self, info: &JobInfo) -> Result<Arc<ManagedTorrent>> {
        let source = match (&info.magnet, &info.torrent_file) {
            (Some(m), _) => TorrentSource::Magnet(m.clone()),
            (None, Some(f)) => TorrentSource::File(std::fs::read(f)?.into()),
            _ => bail!("sem magnet nem .torrent gravado"),
        };
        let h = self.add_torrent(&source, &info.folder).await?;
        if info.only_file {
            self.streamer()?.api.api_torrent_action_update_only_files(TorrentIdOrHash::Id(h.id()), &HashSet::from([info.file_id])).await?;
        }
        Ok(h)
    }

    async fn add_torrent(&self, source: &TorrentSource, folder: &Path) -> Result<Arc<ManagedTorrent>> {
        let add = match source {
            TorrentSource::Magnet(m) => AddTorrent::from_url(m.clone()),
            TorrentSource::File(b) => AddTorrent::from_bytes(b.clone()),
        };
        let opts = AddTorrentOptions { output_folder: Some(folder.to_string_lossy().into_owned()), overwrite: true, ..Default::default() };
        let added = tokio::time::timeout(Duration::from_secs(120), self.streamer()?.api.session().add_torrent(add, Some(opts)))
            .await
            .context("o torrent não respondeu em 2 min (sem fontes?)")??;
        let handle = match added {
            AddTorrentResponse::Added(_, h) | AddTorrentResponse::AlreadyManaged(_, h) => h,
            AddTorrentResponse::ListOnly(_) => bail!("resposta inesperada do librqbit"),
        };
        handle.wait_until_initialized().await?;
        Ok(handle)
    }

    /// Começa (ou retoma) um "assistir agora". `guid`: usar esta versão mesmo
    /// que o Radarr/Sonarr a recuse (escolha da pessoa na página).
    pub async fn watch(self: &Arc<Self>, kind: Kind, id: i64, guid: Option<&str>) -> Result<Started> {
        let streamer = self.streamer()?.clone();
        if let Some(job) = streamer.job(kind, id).await
            && job.phase().0 != Phase::Failed
        {
            streamer.probe(&job);
            return Ok(Started::Ready);
        }
        let target = match kind {
            Kind::Movie => self.movie_target(id).await?,
            Kind::Episode => self.episode_target(id).await?,
            Kind::Series => bail!("escolha um episódio"),
        };

        // já na biblioteca: toca o arquivo de lá, sem torrent
        if let Some(path) = &target.library_file {
            let host_file = self.cfg.host_path(path);
            let file_len = tokio::fs::metadata(&host_file).await?.len();
            let name = host_file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let job = Job::new(
                JobInfo {
                    kind,
                    id,
                    title: target.title,
                    release: name.clone(),
                    arr_id: target.arr_id,
                    series_id: target.series_id,
                    file_id: 0,
                    file_len,
                    file_name: name,
                    folder: host_file.parent().map(Path::to_path_buf).unwrap_or_default(),
                    host_file,
                    info_hash: None,
                    magnet: None,
                    torrent_file: None,
                    only_file: false,
                    next: target.next,
                    phase: Phase::Done,
                },
                None,
            );
            streamer.jobs.lock().await.insert((kind, id), job.clone());
            streamer.probe(&job);
            return Ok(Started::Ready);
        }

        let pick = match guid {
            Some(g) => target.candidates.iter().find(|c| c.guid == g),
            None => target.candidates.iter().find(|c| !c.rejected),
        };
        let Some(pick) = pick.cloned() else {
            return Ok(Started::NoApproved(target.candidates.into_iter().take(10).collect()));
        };

        let stream_cfg = self.cfg.stream.as_ref().context("configure [stream]")?;
        let source = self.fetch_source(&pick.raw).await?;
        let folder = stream_cfg.download_dir.join(folder_name(&pick.title));
        info!("assistir agora: {} — {} ({} MB)", target.title, pick.title, pick.size >> 20);
        let handle = self.add_torrent(&source, &folder).await?;
        let (file_id, file_len, rel) = handle.with_metadata(|m| video_file(&m.file_infos, target.episode))?.context("o torrent não tem o vídeo")?;
        let many_files = handle.with_metadata(|m| m.file_infos.len())? > 1;
        // pacote: só o arquivo deste episódio
        let only_file = kind == Kind::Episode && many_files;
        if only_file {
            streamer.api.api_torrent_action_update_only_files(TorrentIdOrHash::Id(handle.id()), &HashSet::from([file_id])).await?;
        }

        let info_hash = handle.info_hash().as_string();
        let (magnet, torrent_file) = match &source {
            TorrentSource::Magnet(m) => (Some(m.clone()), None),
            TorrentSource::File(b) => {
                let f = streamer.state_dir.join("torrents").join(format!("{info_hash}.torrent"));
                std::fs::write(&f, b)?;
                (None, Some(f))
            }
        };
        let job = Job::new(
            JobInfo {
                kind,
                id,
                title: target.title,
                release: pick.title.clone(),
                arr_id: target.arr_id,
                series_id: target.series_id,
                file_id,
                file_len,
                file_name: rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                host_file: folder.join(&rel),
                folder,
                info_hash: Some(info_hash),
                magnet,
                torrent_file,
                only_file,
                next: target.next,
                phase: Phase::Downloading,
            },
            Some(handle),
        );
        streamer.jobs.lock().await.insert((kind, id), job.clone());
        streamer.save().await;
        streamer.probe(&job);
        tokio::spawn(finish(self.clone(), job));
        Ok(Started::Ready)
    }

    async fn movie_target(&self, tmdb: i64) -> Result<Target> {
        let radarr = self.radarr.as_ref().context("Radarr não configurado")?;
        // entra no Radarr sem busca e sem monitorar: quem baixa é o Anubis
        let movie = match radarr.find("movie", "tmdbId", tmdb).await? {
            Some(m) => m,
            None => {
                let rcfg = self.cfg.radarr.as_ref().context("Radarr não configurado")?;
                let mut item = radarr.lookup("movie", &format!("tmdb:{tmdb}")).await?.into_iter().next().context("filme não encontrado")?;
                item["qualityProfileId"] = json!(radarr.quality_profile_id(&rcfg.quality_profile).await?);
                item["rootFolderPath"] = json!(rcfg.root_folder.as_deref().context("configure root_folder em [radarr]")?);
                item["monitored"] = json!(false);
                item["minimumAvailability"] = json!("released");
                item["addOptions"] = json!({ "searchForMovie": false });
                radarr.add("movie", item).await?
            }
        };
        let movie_id = movie["id"].as_i64().context("filme sem id")?;
        let title = movie["title"].as_str().unwrap_or_default().to_owned();
        if movie["hasFile"].as_bool() == Some(true) {
            let path = movie["movieFile"]["path"].as_str().context("filme sem caminho")?.to_owned();
            return Ok(Target { title, arr_id: movie_id, series_id: None, episode: None, next: None, library_file: Some(path), candidates: vec![] });
        }
        let candidates = candidates(radarr.releases(&format!("movieId={movie_id}")).await?);
        Ok(Target { title, arr_id: movie_id, series_id: None, episode: None, next: None, library_file: None, candidates })
    }

    async fn episode_target(&self, episode_id: i64) -> Result<Target> {
        let sonarr = self.sonarr.as_ref().context("Sonarr não configurado")?;
        let ep = sonarr.episode(episode_id).await?;
        let series_id = ep["seriesId"].as_i64().context("episódio sem série")?;
        let (s, e) = (ep["seasonNumber"].as_i64().unwrap_or(0), ep["episodeNumber"].as_i64().unwrap_or(0));
        let series = sonarr.series_by_id(series_id).await?;
        let code = format!("S{s:02}E{e:02}");
        let title = match ep["title"].as_str() {
            Some(t) if !t.is_empty() && t != "TBA" => format!("{} — {code} · {t}", series["title"].as_str().unwrap_or_default()),
            _ => format!("{} — {code}", series["title"].as_str().unwrap_or_default()),
        };

        // próximo episódio (a temporada 0, de especiais, fica de fora)
        let mut all: Vec<(i64, i64, i64, String)> = sonarr
            .episodes_raw(series_id)
            .await?
            .iter()
            .filter(|x| x["seasonNumber"].as_i64().unwrap_or(0) > 0)
            .map(|x| {
                let (xs, xe) = (x["seasonNumber"].as_i64().unwrap_or(0), x["episodeNumber"].as_i64().unwrap_or(0));
                (xs, xe, x["id"].as_i64().unwrap_or(0), format!("S{xs:02}E{xe:02} · {}", x["title"].as_str().unwrap_or_default()))
            })
            .collect();
        all.sort();
        let next = all.iter().find(|(xs, xe, _, _)| (*xs, *xe) > (s, e)).map(|(_, _, id, label)| (*id, label.clone()));

        if ep["hasFile"].as_bool() == Some(true) {
            let f = sonarr.episode_file(ep["episodeFileId"].as_i64().unwrap_or(0)).await?;
            let path = f["path"].as_str().context("episódio sem caminho")?.to_owned();
            return Ok(Target { title, arr_id: episode_id, series_id: Some(series_id), episode: Some((s, e)), next, library_file: Some(path), candidates: vec![] });
        }
        // versões do episódio; sem nenhuma aprovada, também a busca da
        // temporada — que traz pacotes e episódios avulsos que a busca do
        // episódio às vezes não acha (fica só o que é deste episódio)
        let mut list = candidates(sonarr.releases(&format!("episodeId={episode_id}")).await?);
        if list.iter().all(|c| c.rejected) {
            let seen: HashSet<String> = list.iter().map(|c| c.guid.clone()).collect();
            let season = candidates(sonarr.releases(&format!("seriesId={series_id}&seasonNumber={s}")).await?);
            list.extend(season.into_iter().filter(|c| !seen.contains(&c.guid) && (c.pack || is_this_episode(&c.raw, s, e))));
            sort_candidates(&mut list);
        }
        Ok(Target { title, arr_id: episode_id, series_id: Some(series_id), episode: Some((s, e)), next, library_file: None, candidates: list })
    }

    /// Magnet direto, ou o .torrent do link de download (que pode redirecionar
    /// para um magnet — é o que o Prowlarr faz com vários indexadores).
    async fn fetch_source(&self, release: &Value) -> Result<TorrentSource> {
        if let Some(m) = release["magnetUrl"].as_str().filter(|m| m.starts_with("magnet:")) {
            return Ok(TorrentSource::Magnet(m.to_owned()));
        }
        let mut url = release["downloadUrl"].as_str().context("versão sem link de download")?.to_owned();
        if let Some(cfg) = &self.cfg.stream {
            for r in &cfg.url_rewrites {
                if let Some(rest) = url.strip_prefix(&r.from) {
                    url = format!("{}{rest}", r.to);
                }
            }
        }
        let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(30)).build()?;
        for _ in 0..5 {
            let res = http.get(&url).send().await.with_context(|| format!("baixando o .torrent de {url}"))?;
            if res.status().is_redirection() {
                let loc = res.headers().get("location").and_then(|v| v.to_str().ok()).context("redirecionamento sem destino")?.to_owned();
                if loc.starts_with("magnet:") {
                    return Ok(TorrentSource::Magnet(loc));
                }
                url = loc;
                continue;
            }
            if !res.status().is_success() {
                bail!("link de download: HTTP {}", res.status());
            }
            let body = res.bytes().await?;
            if body.first() != Some(&b'd') {
                bail!("o link não devolveu um .torrent");
            }
            return Ok(TorrentSource::File(body));
        }
        bail!("redirecionamentos demais no link de download")
    }

    /// Link do Jellyfin para o player (o filme, ou a série do episódio).
    pub async fn watch_jellyfin_url(&self, job: &Job) -> Option<String> {
        match job.info.kind {
            Kind::Movie => self.status(Kind::Movie, job.info.id).await.ok()?.jellyfin_url,
            _ => {
                let series = self.sonarr.as_ref()?.series_by_id(job.info.series_id?).await.ok()?;
                self.jellyfin_url(Kind::Series, &series).await
            }
        }
    }
}

struct Target {
    title: String,
    arr_id: i64,
    series_id: Option<i64>,
    episode: Option<(i64, i64)>,
    next: Option<(i64, String)>,
    library_file: Option<String>,
    candidates: Vec<Candidate>,
}

/// Espera o download terminar, importa e, quando ninguém assiste, entrega o
/// torrent ao qBittorrent. Continua de onde parou depois de um reinício.
async fn finish(orch: Arc<Orchestrator>, job: Arc<Job>) {
    let Some(h) = job.handle() else { return };
    let set = |p: Phase, msg: Option<String>| {
        *job.phase.lock().unwrap() = (p, msg);
    };
    if matches!(job.phase().0, Phase::Downloading | Phase::Failed) {
        if let Err(e) = h.wait_until_completed().await {
            warn!("{}: download falhou: {e:#}", job.info.title);
            set(Phase::Failed, Some(format!("{e:#}")));
            return;
        }
        set(Phase::Importing, None);
        if let Ok(s) = orch.streamer() {
            s.save().await;
        }
    }
    if job.phase().0 == Phase::Importing {
        info!("{}: download completo, importando", job.info.title);
        if let Err(e) = import(&orch, &job).await {
            warn!("{}: {e:#}", job.info.title);
            set(Phase::Failed, Some(format!("{e:#}")));
            return;
        }
        set(Phase::Done, None);
        if let Ok(s) = orch.streamer() {
            s.save().await;
        }
        info!("{}: na biblioteca", job.info.title);
    }
    if let Err(e) = hand_off(&orch, &job, &h).await {
        warn!("{}: {e:#}", job.info.title);
    }
}

async fn import(orch: &Arc<Orchestrator>, job: &Arc<Job>) -> Result<()> {
    let container = orch.cfg.container_path(&job.info.folder).context("a pasta de download está fora dos [[paths]]")?;
    // "Copy" com hardlinks ligados = hardlink (o arquivo continua no torrent)
    let (arr, cmd) = match job.info.kind {
        Kind::Movie => (orch.radarr.as_ref().context("Radarr não configurado")?, "DownloadedMoviesScan"),
        _ => (orch.sonarr.as_ref().context("Sonarr não configurado")?, "DownloadedEpisodesScan"),
    };
    arr.command(json!({ "name": cmd, "path": container, "importMode": "Copy" })).await?;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let has = match job.info.kind {
            Kind::Movie => arr.movie(job.info.arr_id).await?["hasFile"].as_bool(),
            _ => arr.episode(job.info.arr_id).await?["hasFile"].as_bool(),
        };
        if has == Some(true) {
            return match job.info.kind {
                Kind::Movie => arr.set_movie_monitored(job.info.arr_id, true).await,
                _ => arr.monitor_episodes(&[job.info.arr_id]).await,
            };
        }
    }
    bail!("o import não aconteceu em 5 min (veja Activity → Queue no Radarr/Sonarr)")
}

async fn hand_off(orch: &Arc<Orchestrator>, job: &Arc<Job>, h: &Arc<ManagedTorrent>) -> Result<()> {
    // só quando ninguém está assistindo: tirar o torrent do librqbit
    // derrubaria uma reprodução em andamento
    loop {
        let idle = job.last_access.lock().unwrap().elapsed();
        if idle >= IDLE_BEFORE_HANDOFF {
            break;
        }
        tokio::time::sleep(IDLE_BEFORE_HANDOFF - idle).await;
    }
    let container = orch.cfg.container_path(&job.info.folder).context("a pasta de download está fora dos [[paths]]")?;
    let source = match (&job.info.magnet, &job.info.torrent_file) {
        (Some(m), _) => TorrentSource::Magnet(m.clone()),
        (None, Some(f)) => TorrentSource::File(std::fs::read(f)?.into()),
        _ => bail!("sem magnet nem .torrent para entregar ao qBittorrent"),
    };
    let only = job.info.only_file.then_some(job.info.file_id);
    orch.qbit.add_seeding(&source, &container, "anubis", job.info.info_hash.as_deref(), only).await?;
    let streamer = orch.streamer()?;
    *job.handle.lock().unwrap() = None;
    streamer.api.session().delete(TorrentIdOrHash::Id(h.id()), false).await?;
    if let Some(f) = &job.info.torrent_file {
        let _ = std::fs::remove_file(f);
    }
    streamer.save().await;
    info!("{}: torrent entregue ao qBittorrent para semear", job.info.title);
    Ok(())
}

/// Lê no máximo `len` bytes (resposta de um pedido de faixa).
pub fn limit(r: Box<dyn AsyncRead + Send + Unpin>, len: u64) -> impl AsyncRead + Send + Unpin {
    r.take(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_approved_single_episodes_then_browser_playable() {
        let rel = |title: &str, rejected: bool, weight: i64, seeds: i64, pack: bool| {
            json!({ "protocol": "torrent", "guid": title, "title": title, "size": 1, "seeders": seeds, "rejected": rejected, "releaseWeight": weight, "fullSeason": pack })
        };
        let list = candidates(vec![
            rel("Filme.1080p.x265-A", false, 1, 90, false),
            rel("Filme.1080p.x264-B", false, 2, 10, false),
            rel("Serie.S01.1080p.x264-P", false, 0, 900, true),
            rel("Filme.720p.x264-C", true, 0, 500, false),
            rel("Filme.sem.fontes", false, 0, 0, false),
        ]);
        let order: Vec<_> = list.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(order, ["Filme.1080p.x264-B", "Filme.1080p.x265-A", "Serie.S01.1080p.x264-P", "Filme.720p.x264-C"]);
    }

    #[test]
    fn finds_the_episode_inside_a_pack() {
        assert!(is_episode("Show.S01E05.1080p.mkv", 1, 5));
        assert!(is_episode("show.s1e05.mkv", 1, 5));
        assert!(is_episode("Show 1x05 Title.mp4", 1, 5));
        assert!(!is_episode("Show.S01E50.mkv", 1, 5));
        assert!(!is_episode("Show.S01E15.mkv", 1, 5));
        assert!(!is_episode("Show.S11E05.mkv", 1, 5));
    }

    #[test]
    fn decides_how_the_browser_plays() {
        let p = |v: &str, a: &str| json!({ "streams": [{ "codec_type": "video", "codec_name": v }, { "codec_type": "audio", "codec_name": a }], "format": { "duration": "5400.5" } });
        assert_eq!(classify(&p("h264", "aac"), "f.mp4").mode, Mode::Direct);
        assert_eq!(classify(&p("h264", "ac3"), "f.mp4").mode, Mode::Remux);
        assert_eq!(classify(&p("h264", "aac"), "f.mkv").mode, Mode::Remux);
        assert_eq!(classify(&p("hevc", "aac"), "f.mkv").mode, Mode::Transcode);
        assert_eq!(classify(&p("h264", "aac"), "f.mp4").duration, Some(5400.5));
    }

    #[test]
    fn season_search_keeps_only_this_episode_or_packs() {
        let r = |season: i64, eps: &[i64]| json!({ "seasonNumber": season, "episodeNumbers": eps });
        assert!(is_this_episode(&r(1, &[5]), 1, 5));
        assert!(is_this_episode(&r(1, &[4, 5]), 1, 5));
        assert!(!is_this_episode(&r(1, &[6]), 1, 5));
        assert!(!is_this_episode(&r(2, &[5]), 1, 5));
    }

    #[test]
    fn folder_names_are_safe() {
        assert_eq!(folder_name("A/B\u{0007} C"), "A B  C");
    }
}
