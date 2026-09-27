//! "Assistir agora": o Anubis baixa com o próprio motor de torrent (librqbit)
//! e entrega o vídeo ao player enquanto baixa.
//!
//! - A versão é escolhida entre as que o Radarr aprova (perfil de qualidade de
//!   sempre); entre elas, prefere as que o navegador toca direto (H.264/AV1).
//! - Sem ninguém assistindo, o librqbit baixa em ordem (começo → fim). Com o
//!   player aberto, as peças da posição dele (janela de 32 MB) passam na
//!   frente — pular para o meio do filme reprioriza o download.
//! - Ao terminar: import no Radarr por hardlink (mesmo /data), e, quando
//!   ninguém assiste há alguns minutos, o torrent passa ao qBittorrent para
//!   semear (a limpeza de armazenamento já sabe lidar com ele).

use crate::orchestrator::Orchestrator;
use crate::search::Kind;
use crate::services::TorrentSource;
use anyhow::{Context, Result, bail};
use librqbit::api::TorrentIdOrHash;
use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, Api, DhtSessionConfig, ManagedTorrent, Session, SessionOptions};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt};
use tokio::sync::Mutex;
use tracing::{info, warn};

const VIDEO_EXT: [&str; 6] = ["mp4", "m4v", "mkv", "webm", "mov", "avi"];
/// sem requisições do player por esse tempo → pode entregar ao qBittorrent
const IDLE_BEFORE_HANDOFF: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Downloading,
    /// baixado, entrando na biblioteca do Radarr
    Importing,
    /// completo (e na biblioteca)
    Done,
    Failed,
}

pub struct Job {
    pub title: String,
    pub release: String,
    movie_id: i64,
    file_id: usize,
    pub file_len: u64,
    pub file_name: String,
    /// o vídeo no disco (host); lido direto depois que o download termina
    host_file: PathBuf,
    folder: PathBuf,
    source: Option<TorrentSource>,
    /// torrent no librqbit; `None` depois de entregue ao qBittorrent (ou se o
    /// filme já estava na biblioteca)
    handle: StdMutex<Option<Arc<ManagedTorrent>>>,
    phase: StdMutex<(Phase, Option<String>)>,
    last_access: StdMutex<Instant>,
}

impl Job {
    pub fn phase(&self) -> (Phase, Option<String>) {
        self.phase.lock().unwrap().clone()
    }
    fn set_phase(&self, p: Phase, msg: Option<String>) {
        *self.phase.lock().unwrap() = (p, msg);
    }
    fn handle(&self) -> Option<Arc<ManagedTorrent>> {
        self.handle.lock().unwrap().clone()
    }
    pub fn touch(&self) {
        *self.last_access.lock().unwrap() = Instant::now();
    }
}

pub struct Streamer {
    api: Api,
    jobs: Mutex<HashMap<(Kind, i64), Arc<Job>>>,
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
    /// 2: o navegador toca direto; 1: talvez; 0: provavelmente não (H.265)
    pub playable: u8,
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
            raw: r,
        })
        .collect();
    // aprovadas primeiro; depois as que tocam no navegador; depois a ordem de
    // preferência do próprio Radarr (releaseWeight) e as mais semeadas
    out.sort_by_key(|c| (c.rejected, std::cmp::Reverse(c.playable), c.raw["releaseWeight"].as_i64().unwrap_or(i64::MAX), std::cmp::Reverse(c.seeders)));
    out
}

fn video_file(files: &librqbit::FileInfos) -> Option<(usize, u64, PathBuf)> {
    files
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            f.relative_filename
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| VIDEO_EXT.contains(&e.to_lowercase().as_str()))
        })
        .max_by_key(|(_, f)| f.len)
        .map(|(i, f)| (i, f.len, f.relative_filename.clone()))
}

/// Nome de pasta seguro a partir do título da versão.
fn folder_name(title: &str) -> String {
    let s: String = title.chars().map(|c| if c == '/' || c.is_control() { ' ' } else { c }).collect();
    s.trim().chars().take(150).collect()
}

impl Streamer {
    pub async fn new() -> Result<Self> {
        let opts = SessionOptions {
            // DHT ligado (acha fontes além dos trackers), sem gravar estado em disco
            dht: Some(DhtSessionConfig { bootstrap_addrs: None, port: None, persistence: None }),
            fastresume: false,
            persistence: None,
            // o FeuVault não tem IPv6 (o socket em [::] falharia)
            ipv4_only: true,
            ..Default::default()
        };
        // sem pasta padrão: todo torrent recebe a pasta dele
        let session = Session::new_with_opts(std::env::temp_dir(), opts).await.context("iniciando o librqbit")?;
        Ok(Self { api: Api::new(session, None), jobs: Mutex::new(HashMap::new()) })
    }

    pub async fn job(&self, kind: Kind, id: i64) -> Option<Arc<Job>> {
        self.jobs.lock().await.get(&(kind, id)).cloned()
    }

    /// Abre o vídeo a partir de `start`. Baixando: pelo librqbit (lê esperando
    /// as peças e as prioriza); completo: direto do disco.
    pub async fn open(&self, job: &Job, start: u64) -> Result<Box<dyn AsyncRead + Send + Unpin>> {
        job.touch();
        if let Some(h) = job.handle() {
            let mut s = h.stream(job.file_id).await?;
            s.seek(std::io::SeekFrom::Start(start)).await?;
            return Ok(Box::new(s));
        }
        let mut f = tokio::fs::File::open(&job.host_file).await.with_context(|| format!("abrindo {}", job.host_file.display()))?;
        f.seek(std::io::SeekFrom::Start(start)).await?;
        Ok(Box::new(f))
    }

    /// Andamento para o player: fase, progresso, velocidade, fontes e os
    /// trechos do arquivo já baixados (frações 0..1, para a barra branca).
    pub fn info(&self, job: &Job) -> Value {
        let (phase, error) = job.phase();
        let Some(h) = job.handle() else {
            return json!({ "phase": phase, "error": error, "progress": 1.0, "have": [[0.0, 1.0]] });
        };
        let stats = serde_json::to_value(h.stats()).unwrap_or_default();
        let total = stats["total_bytes"].as_f64().unwrap_or(0.0);
        let done = stats["progress_bytes"].as_f64().unwrap_or(0.0);
        let have = self.have_ranges(&h, job.file_id).unwrap_or_default();
        json!({
            "phase": phase,
            "error": error,
            "progress": if total > 0.0 { done / total } else { 0.0 },
            // MiB/s (o "mbps" do librqbit é mebibytes por segundo)
            "speed_mbps": stats["live"]["download_speed"]["mbps"],
            "peers": stats["live"]["snapshot"]["peer_stats"]["live"],
            "have": have,
        })
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
}

impl Orchestrator {
    fn streamer(&self) -> Result<&Streamer> {
        self.streamer.as_ref().context("\"assistir agora\" desligado: configure [stream]")
    }

    /// Começa (ou retoma) um "assistir agora". `guid`: usar esta versão mesmo
    /// que o Radarr a recuse (escolha da pessoa na página).
    pub async fn watch(self: &Arc<Self>, kind: Kind, id: i64, guid: Option<&str>) -> Result<Started> {
        let streamer = self.streamer()?;
        if let Some(job) = streamer.job(kind, id).await
            && job.phase().0 != Phase::Failed
        {
            return Ok(Started::Ready);
        }
        if kind != Kind::Movie {
            bail!("por enquanto o \"assistir agora\" é só para filmes");
        }
        let stream_cfg = self.cfg.stream.as_ref().context("configure [stream]")?;
        let radarr = self.radarr.as_ref().context("Radarr não configurado")?;

        // o filme entra no Radarr sem busca e sem monitorar: quem baixa é o Anubis
        let movie = match radarr.find("movie", "tmdbId", id).await? {
            Some(m) => m,
            None => {
                let rcfg = self.cfg.radarr.as_ref().context("Radarr não configurado")?;
                let mut item = radarr.lookup("movie", &format!("tmdb:{id}")).await?.into_iter().next().context("filme não encontrado")?;
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

        // já na biblioteca: toca o arquivo de lá, sem torrent
        if movie["hasFile"].as_bool() == Some(true) {
            let path = movie["movieFile"]["path"].as_str().context("filme sem caminho")?;
            let host_file = self.cfg.host_path(path);
            let file_len = tokio::fs::metadata(&host_file).await?.len();
            let job = Arc::new(Job {
                title,
                release: movie["movieFile"]["relativePath"].as_str().unwrap_or_default().to_owned(),
                movie_id,
                file_id: 0,
                file_len,
                file_name: host_file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                folder: host_file.parent().map(Path::to_path_buf).unwrap_or_default(),
                host_file,
                source: None,
                handle: StdMutex::new(None),
                phase: StdMutex::new((Phase::Done, None)),
                last_access: StdMutex::new(Instant::now()),
            });
            streamer.jobs.lock().await.insert((kind, id), job);
            return Ok(Started::Ready);
        }

        let list = candidates(radarr.releases(&format!("movieId={movie_id}")).await?);
        let pick = match guid {
            Some(g) => list.iter().find(|c| c.guid == g),
            None => list.iter().find(|c| !c.rejected),
        };
        let Some(pick) = pick.cloned() else {
            return Ok(Started::NoApproved(list.into_iter().take(8).collect()));
        };

        let source = self.fetch_source(&pick.raw).await?;
        let folder = stream_cfg.download_dir.join(folder_name(&pick.title));
        let add = match &source {
            TorrentSource::Magnet(m) => AddTorrent::from_url(m.clone()),
            TorrentSource::File(b) => AddTorrent::from_bytes(b.clone()),
        };
        let opts = AddTorrentOptions { output_folder: Some(folder.to_string_lossy().into_owned()), overwrite: true, ..Default::default() };
        info!("assistir agora: {title} — {} ({} MB)", pick.title, pick.size >> 20);
        let added = tokio::time::timeout(Duration::from_secs(120), streamer.api.session().add_torrent(add, Some(opts)))
            .await
            .context("o torrent não respondeu em 2 min (sem fontes?)")??;
        let handle = match added {
            AddTorrentResponse::Added(_, h) | AddTorrentResponse::AlreadyManaged(_, h) => h,
            AddTorrentResponse::ListOnly(_) => bail!("resposta inesperada do librqbit"),
        };
        handle.wait_until_initialized().await?;
        let (file_id, file_len, rel) = handle.with_metadata(|m| video_file(&m.file_infos))?.context("o torrent não tem vídeo")?;

        let job = Arc::new(Job {
            title,
            release: pick.title.clone(),
            movie_id,
            file_id,
            file_len,
            file_name: rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            host_file: folder.join(&rel),
            folder,
            source: Some(source),
            handle: StdMutex::new(Some(handle)),
            phase: StdMutex::new((Phase::Downloading, None)),
            last_access: StdMutex::new(Instant::now()),
        });
        streamer.jobs.lock().await.insert((kind, id), job.clone());
        tokio::spawn(finish(self.clone(), job));
        Ok(Started::Ready)
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
}

/// Espera o download terminar, importa no Radarr e, quando ninguém assiste,
/// entrega o torrent ao qBittorrent.
async fn finish(orch: Arc<Orchestrator>, job: Arc<Job>) {
    let Some(h) = job.handle() else { return };
    if let Err(e) = h.wait_until_completed().await {
        warn!("{}: download falhou: {e:#}", job.title);
        job.set_phase(Phase::Failed, Some(format!("{e:#}")));
        return;
    }
    job.set_phase(Phase::Importing, None);
    info!("{}: download completo, importando no Radarr", job.title);
    if let Err(e) = import_and_hand_off(&orch, &job, &h).await {
        warn!("{}: {e:#}", job.title);
        job.set_phase(Phase::Failed, Some(format!("{e:#}")));
    }
}

async fn import_and_hand_off(orch: &Arc<Orchestrator>, job: &Arc<Job>, h: &Arc<ManagedTorrent>) -> Result<()> {
    let radarr = orch.radarr.as_ref().context("Radarr não configurado")?;
    let container = orch.cfg.container_path(&job.folder).context("a pasta de download está fora dos [[paths]]")?;
    // "Copy" com hardlinks ligados = hardlink (o arquivo continua no torrent)
    radarr.command(json!({ "name": "DownloadedMoviesScan", "path": container, "importMode": "Copy" })).await?;
    let mut imported = false;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        if radarr.movie(job.movie_id).await?["hasFile"].as_bool() == Some(true) {
            imported = true;
            break;
        }
    }
    if !imported {
        bail!("o Radarr não importou o filme em 5 min (veja Activity → Queue no Radarr)");
    }
    radarr.set_movie_monitored(job.movie_id, true).await?;
    job.set_phase(Phase::Done, None);
    info!("{}: na biblioteca", job.title);

    // entrega ao qBittorrent só quando ninguém está assistindo: tirar o
    // torrent do librqbit derrubaria uma reprodução em andamento
    loop {
        let idle = job.last_access.lock().unwrap().elapsed();
        if idle >= IDLE_BEFORE_HANDOFF {
            break;
        }
        tokio::time::sleep(IDLE_BEFORE_HANDOFF - idle).await;
    }
    if let Some(source) = &job.source {
        orch.qbit.add_seeding(source, &container, "anubis").await?;
    }
    *job.handle.lock().unwrap() = None;
    orch.streamer()?.api.session().delete(TorrentIdOrHash::Id(h.id()), false).await?;
    info!("{}: torrent entregue ao qBittorrent para semear", job.title);
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
    fn prefers_approved_then_browser_playable() {
        let rel = |title: &str, rejected: bool, weight: i64, seeds: i64| {
            json!({ "protocol": "torrent", "guid": title, "title": title, "size": 1, "seeders": seeds, "rejected": rejected, "releaseWeight": weight })
        };
        let list = candidates(vec![
            rel("Filme.1080p.x265-A", false, 1, 90),
            rel("Filme.1080p.x264-B", false, 2, 10),
            rel("Filme.720p.x264-C", true, 0, 500),
            rel("Filme.sem.fontes", false, 0, 0),
        ]);
        let order: Vec<_> = list.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(order, ["Filme.1080p.x264-B", "Filme.1080p.x265-A", "Filme.720p.x264-C"]);
    }

    #[test]
    fn folder_names_are_safe() {
        assert_eq!(folder_name("A/B\u{0007} C"), "A B  C");
    }
}
