//! "Juntar dublagem": o vídeo de uma versão (em qualquer idioma) com o áudio
//! em PT-BR de outra. Dois usos:
//! - a dublagem automática (`autodub`): o vídeo é o arquivo que já está na
//!   biblioteca (filme ou episódio) e só a dublagem é baixada;
//! - o pedido avulso de um filme (POST /mux, fora da página): as duas versões
//!   vêm da busca do Radarr e as duas são baixadas.
//!
//! 1. As versões vêm da mesma busca do "assistir".
//! 2. O Anubis baixa com o librqbit só o vídeo de cada torrent (num pacote de
//!    temporada, só o episódio). Download parado por `STALL`: desiste.
//! 3. Sincronia: em três pontos do filme, procura onde o áudio da dublagem cai
//!    no áudio do vídeo (música e efeitos são os mesmos; a dublagem troca só
//!    as vozes). A mesma distância nos pontos é um atraso fixo, que o ffmpeg
//!    corrige. Se a duração indica outra velocidade (25 × 23,976 quadros/s), a
//!    dublagem é ajustada e conferida de novo. Nada bate (cortes diferentes:
//!    versão estendida, cena a mais): não junta, e diz por quê.
//! 4. Junta com o ffmpeg (vídeo copiado; áudio PT-BR primeiro, o original
//!    depois) e entrega ao Radarr/Sonarr pelo import manual, com o idioma
//!    marcado; no lugar do arquivo da biblioteca, quando é ele o vídeo (e o
//!    torrent que o semeava sai, se não semeia mais nada da biblioteca).
//! 5. Apaga os downloads.

use crate::orchestrator::Orchestrator;
use crate::search::Kind;
use crate::services::{Method, TorrentSource, url_encode};
use crate::stream::{Candidate, candidates, folder_name, video_file};
use anyhow::{Context, Result, bail};
use librqbit::ManagedTorrent;
use librqbit::api::TorrentIdOrHash;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::process::Command;
use tracing::{info, warn};

const JOBS_FILE: &str = "juntar.json";
/// quadros de envelope por segundo (10 ms cada)
const FPS: usize = 100;
/// taxa do áudio analisado (mono)
const RATE: usize = 8000;
/// trecho da dublagem comparado em cada ponto, e quanto procurar para cada lado
const PROBE_SECS: f64 = 120.0;
const SEARCH_SECS: f64 = 60.0;
/// pontos conferidos (fração da duração), longe da abertura e dos créditos
const POINTS: [f64; 3] = [0.2, 0.5, 0.8];
/// pico confiável: medido em áudio real, o trecho certo dá 28–75 e um trecho
/// de outro lugar do filme, 4–5
const MIN_SHARPNESS: f32 = 10.0;
/// diferença máxima entre os pontos para o atraso contar como fixo (s)
const TOLERANCE: f64 = 0.1;
/// quadros por segundo comuns entre versões
const FRAME_RATES: [f64; 3] = [24000.0 / 1001.0, 24.0, 25.0];
/// id do idioma "Portuguese (Brazil)" no Radarr e no Sonarr
const RADARR_PT_BR: i64 = 30;
const SONARR_PT_BR: i64 = 33;
/// download sem avançar por este tempo: ninguém compartilhando
const STALL: Duration = Duration::from_secs(20 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Downloading,
    /// conferindo a sincronia
    Checking,
    Joining,
    Importing,
    Done,
    Failed,
}

/// Uma das duas versões baixadas.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Source {
    title: String,
    info_hash: String,
    magnet: Option<String>,
    torrent_file: Option<PathBuf>,
    folder: PathBuf,
    file_id: usize,
    only_file: bool,
    host_file: PathBuf,
}

/// O que recebe a dublagem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    Movie { tmdb: i64, movie_id: i64 },
    /// ids do Sonarr
    Episode { episode_id: i64, series_id: i64, season: i64, episode: i64 },
}

impl Target {
    /// chave dos pedidos: filme pelo id do TMDB, episódio pelo do Sonarr
    pub fn key(&self) -> (Kind, i64) {
        match *self {
            Target::Movie { tmdb, .. } => (Kind::Movie, tmdb),
            Target::Episode { episode_id, .. } => (Kind::Episode, episode_id),
        }
    }
    fn episode(&self) -> Option<(i64, i64)> {
        match *self {
            Target::Episode { season, episode, .. } => Some((season, episode)),
            Target::Movie { .. } => None,
        }
    }
    /// para nomes de pasta e de container
    fn label(&self) -> String {
        match *self {
            Target::Movie { tmdb, .. } => format!("{tmdb}"),
            Target::Episode { episode_id, .. } => format!("ep{episode_id}"),
        }
    }
}

/// O arquivo da biblioteca usado como vídeo (a dublagem automática).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryVideo {
    /// caminho no host
    pub file: PathBuf,
    /// id do arquivo no Radarr (movieFile) ou no Sonarr (episodeFile)
    pub file_id: i64,
    /// dado do arquivo: o torrent que o semeia sai depois da troca
    pub inode: Option<(u64, u64)>,
}

/// O que é gravado para retomar depois de um reinício.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MuxInfo {
    target: Target,
    title: String,
    /// vídeo baixado, ou (`library`) o que já está na biblioteca
    video: Option<Source>,
    #[serde(default)]
    library: Option<LibraryVideo>,
    audio: Source,
    step: Step,
    error: Option<String>,
}

/// Resultado da conferência de sincronia.
#[derive(Debug, Clone, Serialize)]
pub struct SyncReport {
    /// segundos a cortar do início da dublagem (negativo: silêncio a acrescentar)
    pub offset: f64,
    /// duração da dublagem ÷ duração do vídeo nas cenas em comum (1 = mesma velocidade)
    pub speed: f64,
    /// cada ponto conferido: minuto do filme, atraso medido e nitidez do pico
    pub points: Vec<Point>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Point {
    pub minute: f64,
    pub offset: f64,
    pub sharpness: f32,
}

pub struct MuxJob {
    info: StdMutex<MuxInfo>,
    sync: StdMutex<Option<SyncReport>>,
    /// torrents no librqbit (vídeo, dublagem); vazio depois da limpeza
    handles: StdMutex<Vec<Arc<ManagedTorrent>>>,
}

impl MuxJob {
    fn new(info: MuxInfo, handles: Vec<Arc<ManagedTorrent>>) -> Arc<Self> {
        Arc::new(Self { info: StdMutex::new(info), sync: StdMutex::new(None), handles: StdMutex::new(handles) })
    }
    fn info(&self) -> MuxInfo {
        self.info.lock().unwrap().clone()
    }
    fn set(&self, step: Step, error: Option<String>) {
        let mut i = self.info.lock().unwrap();
        i.step = step;
        i.error = error;
    }
    pub fn active(&self) -> bool {
        !matches!(self.info.lock().unwrap().step, Step::Done | Step::Failed)
    }
    pub fn step(&self) -> Step {
        self.info.lock().unwrap().step
    }
    pub fn error(&self) -> Option<String> {
        self.info.lock().unwrap().error.clone()
    }
    fn sources(&self) -> Vec<Source> {
        let i = self.info();
        i.video.into_iter().chain([i.audio]).collect()
    }
}

/// As versões que a página oferece: o vídeo (qualquer idioma, melhor imagem
/// primeiro) e a dublagem (as que têm áudio em português).
#[derive(Debug, Serialize)]
pub struct Options {
    pub title: String,
    pub video: Vec<Candidate>,
    pub audio: Vec<Candidate>,
}

// ------------------------------------------------------------ sincronia

/// Envelope de "ataques" do áudio: quanto a energia sobe a cada 10 ms. Batidas,
/// portas, música e efeitos caem nos mesmos lugares na dublagem e no original.
fn envelope(pcm: &[i16], rate: usize) -> Vec<f32> {
    let hop = rate / FPS;
    let energy: Vec<f32> = pcm
        .chunks_exact(hop)
        .map(|c| (c.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / hop as f64 + 1.0).ln() as f32)
        .collect();
    let mut onset: Vec<f32> = energy.windows(2).map(|w| (w[1] - w[0]).max(0.0)).collect();
    let n = onset.len().max(1) as f32;
    let mean = onset.iter().sum::<f32>() / n;
    let sd = (onset.iter().map(|x| (x - mean).powi(2)).sum::<f32>() / n).sqrt().max(1e-6);
    onset.iter_mut().for_each(|x| *x = (*x - mean) / sd);
    onset
}

/// Onde `probe` começa dentro de `reference`, em quadros, e a nitidez do pico
/// (quantos desvios-padrão acima das outras posições).
fn locate(reference: &[f32], probe: &[f32]) -> Option<(usize, f32)> {
    let n = reference.len().checked_sub(probe.len())? + 1;
    let scores: Vec<f32> = (0..n).map(|lag| reference[lag..lag + probe.len()].iter().zip(probe).map(|(a, b)| a * b).sum()).collect();
    let (best, peak) = scores.iter().copied().enumerate().max_by(|a, b| a.1.total_cmp(&b.1))?;
    let mean = scores.iter().sum::<f32>() / n as f32;
    let sd = (scores.iter().map(|x| (x - mean).powi(2)).sum::<f32>() / n as f32).sqrt().max(1e-6);
    Some((best, (peak - mean) / sd))
}

/// Velocidades a tentar, dada a razão entre as durações: primeiro a mesma
/// (durações diferem também por créditos e logos), depois as conversões
/// comuns de quadros por segundo próximas da razão.
fn speeds(ratio: f64) -> Vec<f64> {
    let mut other: Vec<f64> = FRAME_RATES
        .iter()
        .flat_map(|a| FRAME_RATES.iter().map(move |b| a / b))
        .filter(|r| (r - 1.0).abs() > 1e-6 && (r - ratio).abs() < 0.01)
        .collect();
    other.sort_by(|a, b| (a - ratio).abs().total_cmp(&(b - ratio).abs()));
    other.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    std::iter::once(1.0).chain(other).collect()
}

/// O atraso, se os pontos confiáveis concordam: ao menos dois, e nenhum
/// confiável discordando (isso seria um corte diferente entre eles).
fn agree(points: &[Point]) -> Option<f64> {
    let mut good: Vec<f64> = points.iter().filter(|p| p.sharpness >= MIN_SHARPNESS).map(|p| p.offset).collect();
    if good.len() < 2 {
        return None;
    }
    good.sort_by(f64::total_cmp);
    (good[good.len() - 1] - good[0] <= TOLERANCE).then(|| good[good.len() / 2])
}

/// O atraso medido, em palavras: positivo = a cena chega depois na dublagem.
fn describe(offset: f64) -> String {
    if offset.abs() < 0.05 {
        "em sincronia".into()
    } else {
        format!("dublagem {:.2} s {}", offset.abs(), if offset > 0.0 { "atrasada" } else { "adiantada" })
    }
}

// ------------------------------------------------------------ faixas de áudio

#[derive(Debug, Clone)]
pub(crate) struct Track {
    /// posição entre as faixas de áudio do arquivo (0:a:N)
    pub(crate) n: usize,
    pub(crate) lang: String,
    pub(crate) title: String,
    pub(crate) channels: u32,
}

#[derive(Debug)]
pub(crate) struct Probed {
    pub(crate) duration: f64,
    pub(crate) tracks: Vec<Track>,
}

fn probed(v: &Value) -> Probed {
    let tracks = v["streams"]
        .as_array()
        .map(|a| a.iter().filter(|s| s["codec_type"] == "audio").collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(n, s)| Track {
            n,
            lang: s["tags"]["language"].as_str().unwrap_or_default().to_lowercase(),
            title: s["tags"]["title"].as_str().unwrap_or_default().to_owned(),
            channels: s["channels"].as_u64().unwrap_or(2) as u32,
        })
        .collect();
    Probed { duration: v["format"]["duration"].as_str().and_then(|d| d.parse().ok()).unwrap_or(0.0), tracks }
}

pub(crate) fn is_portuguese(t: &Track) -> bool {
    let title = t.title.to_lowercase();
    ["por", "pt", "pob", "ptb", "pt-br"].contains(&t.lang.as_str()) || ["portug", "brasil", "brazil", "dublad"].iter().any(|k| title.contains(k))
}

/// (faixa PT-BR da dublagem, faixa da dublagem comparada, faixa do vídeo comparada).
/// Numa dublagem "dual áudio", o original dela comparado com o do vídeo tem
/// as mesmas vozes e dá o pico mais nítido; sem ele, compara a própria dublagem.
fn pick_tracks(video: &[Track], dub: &[Track]) -> Result<(usize, usize, usize)> {
    let pt = match dub.iter().find(|t| is_portuguese(t)) {
        Some(t) => t.n,
        None if dub.len() == 1 => dub[0].n,
        None => bail!("a versão da dublagem não tem faixa de áudio marcada como português"),
    };
    let original = video.iter().find(|t| !is_portuguese(t)).or(video.first()).context("a versão do vídeo não tem áudio")?;
    let pair = dub.iter().filter(|d| d.n != pt && !d.lang.is_empty()).find_map(|d| video.iter().find(|v| v.lang == d.lang).map(|v| (d.n, v.n)));
    Ok(match pair {
        Some((d, v)) => (pt, d, v),
        None => (pt, pt, original.n),
    })
}

/// Layout AAC padrão para `channels` canais (e quantos ficam): até 2 como
/// vêm, mais que isso em 5.1 — o maior que os navegadores tocam.
fn aac_layout(channels: u32) -> (&'static str, u32) {
    match channels {
        0 | 1 => ("mono", 1),
        2 => ("stereo", 2),
        _ => ("5.1", 6),
    }
}

// ------------------------------------------------------------ ffmpeg

impl Orchestrator {
    /// ffmpeg/ffprobe com acesso aos downloads. Com `container` configurado,
    /// roda numa cópia descartável da mesma imagem com os [[paths]] montados
    /// como o Radarr os vê (o container do Jellyfin só enxerga a biblioteca);
    /// sem, o do host. `name` dá nome ao container (um reinício o derruba).
    pub(crate) async fn media_tool(&self, bin: &str, name: Option<&str>) -> Result<Command> {
        let t = &self.streamer()?.transcode;
        let Some(c) = &t.container else { return Ok(Command::new(bin)) };
        let out = Command::new("docker").args(["inspect", "-f", "{{.Config.Image}}", c]).output().await.context("docker inspect")?;
        let image = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if !out.status.success() || image.is_empty() {
            bail!("container {c}: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let mut cmd = Command::new("docker");
        cmd.args(["run", "--rm", "--user", &format!("{}:{}", rustix::process::getuid().as_raw(), rustix::process::getgid().as_raw()), "--entrypoint", bin]);
        for m in &self.cfg.paths {
            cmd.args(["-v", &format!("{}:{}", m.to, m.from)]);
        }
        if let Some(n) = name {
            let _ = Command::new("docker").args(["rm", "-f", n]).stdout(Stdio::null()).stderr(Stdio::null()).status().await;
            cmd.args(["--name", n]);
        }
        cmd.arg(image);
        Ok(cmd)
    }

    /// Caminho como o ffmpeg o enxerga.
    pub(crate) fn tool_path(&self, host: &Path) -> Result<String> {
        if self.streamer()?.transcode.container.is_none() {
            return Ok(host.to_string_lossy().into_owned());
        }
        self.cfg.container_path(host).with_context(|| format!("{} está fora dos [[paths]]", host.display()))
    }

    pub(crate) async fn probe_file(&self, file: &Path) -> Result<Probed> {
        let t = self.streamer()?.transcode.ffprobe.clone();
        let out = self
            .media_tool(&t, None)
            .await?
            .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams", &self.tool_path(file)?])
            .kill_on_drop(true)
            .output()
            .await?;
        if !out.status.success() {
            bail!("ffprobe: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(probed(&serde_json::from_slice(&out.stdout)?))
    }

    /// `len` segundos da faixa `track` a partir de `start`, em mono 8 kHz,
    /// tocados `speed` vezes mais devagar (tempo da dublagem → tempo do vídeo).
    async fn pcm(&self, file: &Path, track: usize, start: f64, len: f64, speed: f64) -> Result<Vec<i16>> {
        let t = self.streamer()?.transcode.ffmpeg.clone();
        // asetrate muda a velocidade sem artefatos (tom junto, que não importa aqui)
        let filter = if (speed - 1.0).abs() < 1e-9 { format!("aresample={RATE}") } else { format!("aresample=48000,asetrate={},aresample={RATE}", (48000.0 * speed).round()) };
        let args: Vec<String> = vec![
            "-nostdin".into(),
            "-loglevel".into(),
            "error".into(),
            "-ss".into(),
            format!("{start:.3}"),
            "-t".into(),
            format!("{len:.3}"),
            "-i".into(),
            self.tool_path(file)?,
            "-map".into(),
            format!("0:a:{track}"),
            "-ac".into(),
            "1".into(),
            "-af".into(),
            filter,
            "-f".into(),
            "s16le".into(),
            "pipe:1".into(),
        ];
        let out = tokio::time::timeout(Duration::from_secs(300), self.media_tool(&t, None).await?.args(&args).kill_on_drop(true).output())
            .await
            .context("a leitura do áudio demorou demais")??;
        if !out.status.success() {
            bail!("ffmpeg: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(out.stdout.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect())
    }

    /// Mede o atraso da dublagem no minuto `at` do vídeo, supondo a velocidade `speed`.
    async fn measure(&self, video: &Path, vtrack: usize, dub: &Path, dtrack: usize, at: f64, speed: f64) -> Result<Point> {
        let reference = self.pcm(video, vtrack, at - SEARCH_SECS, PROBE_SECS + 2.0 * SEARCH_SECS, 1.0).await?;
        let probe = self.pcm(dub, dtrack, at * speed, PROBE_SECS * speed, speed).await?;
        let (lag, sharpness) = tokio::task::spawn_blocking(move || locate(&envelope(&reference, RATE), &envelope(&probe, RATE)))
            .await?
            .context("trecho curto demais para comparar")?;
        // o trecho que começa em `at·speed` na dublagem aparece em
        // `at − SEARCH + lag` no vídeo
        let offset = speed * (SEARCH_SECS - lag as f64 / FPS as f64);
        Ok(Point { minute: at / 60.0, offset, sharpness })
    }

    pub(crate) async fn check_sync(&self, video: &Path, v: &Probed, vtrack: usize, dub: &Path, d: &Probed, dtrack: usize) -> Result<SyncReport> {
        if v.duration < 600.0 || d.duration < 600.0 {
            bail!("vídeo curto demais para conferir a sincronia");
        }
        // o que se mediu na mesma velocidade é o que explica a recusa
        let mut first = None;
        for speed in speeds(d.duration / v.duration) {
            let mut points = vec![];
            for f in POINTS {
                points.push(self.measure(video, vtrack, dub, dtrack, v.duration * f, speed).await?);
            }
            info!("sincronia (velocidade {speed:.5}): {points:?}");
            if let Some(offset) = agree(&points) {
                return Ok(SyncReport { offset, speed, points });
            }
            first.get_or_insert(points);
        }
        let seen: Vec<String> = first
            .unwrap_or_default()
            .iter()
            .map(|p| format!("{:.0} min: {}", p.minute, if p.sharpness >= MIN_SHARPNESS { describe(p.offset) } else { "não achou".into() }))
            .collect();
        bail!(
            "as duas versões não batem ({}). Provavelmente cortes diferentes (versão estendida, cena a mais) — escolha outra dublagem ou outro vídeo",
            seen.join("; ")
        )
    }

    /// Junta o vídeo com a dublagem (primeira faixa, a padrão) e os áudios e
    /// legendas originais. A dublagem é recodificada (AAC): cortar o atraso e
    /// ajustar a velocidade com precisão pede filtro, e copiar um áudio
    /// "cortado" dependeria de onde caem os pacotes.
    async fn join(&self, label: &str, video: &Path, dub: &Path, pt: &Track, sync: &SyncReport) -> Result<PathBuf> {
        let stream_cfg = self.cfg.stream.as_ref().context("configure [stream]")?;
        let name = format!("{} (brazilian)", folder_name(video.file_stem().and_then(|s| s.to_str()).unwrap_or("filme")));
        let dir = stream_cfg.download_dir.join(&name);
        std::fs::create_dir_all(&dir).with_context(|| format!("criando {}", dir.display()))?;
        let out = dir.join(format!("{name}.mkv"));

        let mut chain = if sync.offset >= 0.0 {
            format!("atrim=start={:.3},asetpts=PTS-STARTPTS", sync.offset)
        } else {
            format!("adelay=delays={}:all=1", (-sync.offset * 1000.0).round())
        };
        if (sync.speed - 1.0).abs() > 1e-9 {
            // atempo = velocidade de reprodução (duração ÷ speed); mantém o tom
            chain.push_str(&format!(",atempo={:.9}", sync.speed));
        }
        // layout padrão: sem ele (dublagem com layout "unknown") o AAC grava os
        // canais num PCE, que os navegadores recusam ("não tocou no navegador")
        let (layout, channels) = aac_layout(pt.channels);
        chain.push_str(&format!(",aformat=channel_layouts={layout}"));
        let mp4 = video.extension().and_then(|e| e.to_str()).is_some_and(|e| ["mp4", "m4v", "mov"].contains(&e.to_lowercase().as_str()));
        // legendas numa terceira leitura do vídeo: vindas da mesma do vídeo, as
        // esparsas (minutos sem pacote) fazem o ffmpeg ler o vídeo adiante da
        // dublagem, e ela acaba gravada longe da cena (Loki S02E05: os últimos
        // 10 min no fim do arquivo; o player, que pula até a cena, quebra)
        let mut args: Vec<String> = vec!["-nostdin".into(), "-y".into(), "-loglevel".into(), "error".into(), "-i".into(), self.tool_path(video)?, "-i".into(), self.tool_path(dub)?, "-i".into(), self.tool_path(video)?];
        args.extend(["-filter_complex".into(), format!("[1:a:{}]{chain}[dub]", pt.n)]);
        args.extend(["-map", "0:v:0", "-map", "[dub]", "-map", "0:a", "-map", "2:s?", "-map", "2:t?", "-map_metadata", "0", "-map_chapters", "0"].map(String::from));
        // tudo copiado, menos a dublagem; legendas de MP4 (mov_text) não entram em MKV
        args.extend(["-c", "copy", "-c:a:0", "aac"].map(String::from));
        args.extend(["-b:a:0".into(), format!("{}k", (channels * 64).max(128))]);
        if mp4 {
            args.extend(["-c:s", "srt"].map(String::from));
        }
        args.extend(["-metadata:s:a:0", "language=por", "-metadata:s:a:0", "title=Português (BR)", "-disposition:a", "0", "-disposition:a:0", "default"].map(String::from));
        args.push(self.tool_path(&out)?);

        let t = self.streamer()?.transcode.ffmpeg.clone();
        let res = self.media_tool(&t, Some(&format!("anubis-juntar-{label}"))).await?.args(&args).kill_on_drop(true).output().await?;
        if !res.status.success() {
            let _ = std::fs::remove_dir_all(&dir);
            bail!("ffmpeg: {}", String::from_utf8_lossy(&res.stderr).trim());
        }
        Ok(out)
    }
}

// ------------------------------------------------------------ pedido

impl Orchestrator {
    /// Versões para escolher: vídeo (melhor imagem primeiro) e dublagem.
    pub async fn mux_options(&self, tmdb: i64) -> Result<Options> {
        let radarr = self.radarr.as_ref().context("Radarr não configurado")?;
        let movie = self.ensure_movie(tmdb).await?;
        let movie_id = movie["id"].as_i64().context("filme sem id")?;
        // filme recém-adicionado: a primeira busca do Radarr volta vazia
        let mut all = vec![];
        for wait in [0, 5, 10] {
            tokio::time::sleep(Duration::from_secs(wait)).await;
            all = candidates(radarr.releases(&format!("movieId={movie_id}")).await?);
            if !all.is_empty() {
                break;
            }
        }
        let portuguese = |c: &Candidate| c.portuguese();
        let mut video: Vec<Candidate> = all.iter().filter(|c| !portuguese(c)).cloned().collect();
        video.sort_by_key(|c| (std::cmp::Reverse(c.raw["qualityWeight"].as_i64().unwrap_or(0)), std::cmp::Reverse(c.seeders)));
        let mut audio: Vec<Candidate> = all.into_iter().filter(portuguese).collect();
        audio.sort_by_key(|c| std::cmp::Reverse(c.seeders));
        video.truncate(15);
        audio.truncate(15);
        Ok(Options { title: movie["title"].as_str().unwrap_or_default().to_owned(), video, audio })
    }

    /// Começa a juntar: baixa as duas versões e segue sozinho até o Radarr.
    pub async fn start_mux(self: &Arc<Self>, tmdb: i64, video_guid: &str, audio_guid: &str) -> Result<()> {
        if video_guid == audio_guid {
            bail!("escolha versões diferentes para o vídeo e a dublagem");
        }
        let old = self.muxes.lock().await.get(&(Kind::Movie, tmdb)).cloned();
        if old.as_ref().is_some_and(|j| j.active()) {
            bail!("este filme já está sendo juntado");
        }
        let radarr = self.radarr.as_ref().context("Radarr não configurado")?;
        let stream_cfg = self.cfg.stream.as_ref().context("configure [stream]")?;
        let movie = self.ensure_movie(tmdb).await?;
        let movie_id = movie["id"].as_i64().context("filme sem id")?;
        let all = candidates(radarr.releases(&format!("movieId={movie_id}")).await?);
        let find = |g: &str| all.iter().find(|c| c.guid == g).cloned().context("a versão sumiu dos indexadores — busque de novo");
        let (v, a) = (find(video_guid)?, find(audio_guid)?);
        let base = stream_cfg.download_dir.join(format!(".juntar-{tmdb}"));
        let (video, vh) = self.mux_source(&v, &base, None, crate::stream::ADD_TIMEOUT).await?;
        let (audio, ah) = self.mux_source(&a, &base, None, crate::stream::ADD_TIMEOUT).await?;
        // tentativa anterior (que falhou): fica só o que for reaproveitado
        if let Some(old) = old {
            let keep: HashSet<String> = [video.info_hash.clone(), audio.info_hash.clone()].into();
            self.discard_sources(&old, &keep).await;
        }
        let title = movie["title"].as_str().unwrap_or_default().to_owned();
        info!("juntar dublagem: {title} — vídeo {} + áudio {}", v.title, a.title);
        let target = Target::Movie { tmdb, movie_id };
        let job = MuxJob::new(MuxInfo { target, title, video: Some(video), library: None, audio, step: Step::Downloading, error: None }, vec![vh, ah]);
        self.muxes.lock().await.insert(target.key(), job.clone());
        self.save_muxes().await;
        tokio::spawn(run(self.clone(), job));
        Ok(())
    }

    /// Dublagem automática: baixa só a dublagem `dub` e a embute no arquivo
    /// da biblioteca. Segue sozinho até o Radarr/Sonarr; o job diz como acabou.
    pub async fn start_dub(self: &Arc<Self>, target: Target, title: String, library: LibraryVideo, dub: &Candidate) -> Result<Arc<MuxJob>> {
        let key = target.key();
        let old = self.muxes.lock().await.get(&key).cloned();
        if old.as_ref().is_some_and(|j| j.active()) {
            bail!("já recebendo dublagem");
        }
        let stream_cfg = self.cfg.stream.as_ref().context("configure [stream]")?;
        let base = stream_cfg.download_dir.join(format!(".juntar-{}", target.label()));
        // sem ninguém esperando: a dublagem tem mais tempo para achar quem a tenha
        let (audio, ah) = self.mux_source(dub, &base, target.episode(), Duration::from_secs(10 * 60)).await?;
        if let Some(old) = old {
            self.discard_sources(&old, &[audio.info_hash.clone()].into()).await;
        }
        info!("dublagem automática: {title} — {}", dub.title);
        let job = MuxJob::new(MuxInfo { target, title, video: None, library: Some(library), audio, step: Step::Downloading, error: None }, vec![ah]);
        self.muxes.lock().await.insert(key, job.clone());
        self.save_muxes().await;
        tokio::spawn(run(self.clone(), job.clone()));
        Ok(job)
    }

    async fn mux_source(&self, c: &Candidate, base: &Path, episode: Option<(i64, i64)>, wait: Duration) -> Result<(Source, Arc<ManagedTorrent>)> {
        let streamer = self.streamer()?;
        let source = self.fetch_source(&c.raw).await?;
        let folder = base.join(folder_name(&c.title));
        let h = self.add_torrent(&source, &folder, wait).await?;
        let (file_id, _, rel) = h.with_metadata(|m| video_file(&m.file_infos, episode))?.context("o torrent não tem o vídeo")?;
        let only_file = h.with_metadata(|m| m.file_infos.len())? > 1;
        if only_file {
            streamer.api.api_torrent_action_update_only_files(TorrentIdOrHash::Id(h.id()), &HashSet::from([file_id])).await?;
        }
        let info_hash = h.info_hash().as_string();
        let (magnet, torrent_file) = match &source {
            TorrentSource::Magnet(m) => (Some(m.clone()), None),
            TorrentSource::File(b) => {
                let f = streamer.state_dir.join("torrents").join(format!("{info_hash}.torrent"));
                std::fs::write(&f, b)?;
                (None, Some(f))
            }
        };
        let host_file = folder.join(&rel);
        Ok((Source { title: c.title.clone(), info_hash, magnet, torrent_file, folder, file_id, only_file, host_file }, h))
    }

    /// Estado para a página.
    pub async fn mux_status(&self, kind: Kind, id: i64) -> Option<Value> {
        let job = self.muxes.lock().await.get(&(kind, id)).cloned()?;
        let info = job.info();
        let handles = job.handles.lock().unwrap().clone();
        let progress = |hash: &str| {
            handles.iter().find(|h| h.info_hash().as_string() == hash).map_or(1.0, |h| {
                let s = h.stats();
                if s.total_bytes > 0 { s.progress_bytes as f64 / s.total_bytes as f64 } else { 0.0 }
            })
        };
        let jellyfin_url = if info.step == Step::Done && kind == Kind::Movie { self.status(kind, id).await.ok().and_then(|s| s.jellyfin_url) } else { None };
        let video = match &info.video {
            Some(v) => json!({ "title": v.title, "progress": progress(&v.info_hash) }),
            None => json!({ "title": "o arquivo da biblioteca", "progress": 1.0 }),
        };
        Some(json!({
            "title": info.title,
            "auto": info.library.is_some(),
            "step": info.step,
            "error": info.error,
            "video": video,
            "audio": { "title": info.audio.title, "progress": progress(&info.audio.info_hash) },
            "sync": *job.sync.lock().unwrap(),
            "jellyfin_url": jellyfin_url,
        }))
    }

    /// Desiste (ou limpa uma tentativa que falhou): apaga os downloads.
    pub async fn cancel_mux(&self, kind: Kind, id: i64) -> Result<()> {
        let job = self.muxes.lock().await.remove(&(kind, id)).context("nada sendo juntado aqui")?;
        if matches!(job.info().step, Step::Joining | Step::Importing) {
            self.muxes.lock().await.insert((kind, id), job);
            bail!("já está juntando — espere terminar");
        }
        self.discard_sources(&job, &HashSet::new()).await;
        self.save_muxes().await;
        Ok(())
    }

    /// Tira os torrents do librqbit e apaga os arquivos (menos `keep`).
    async fn discard_sources(&self, job: &MuxJob, keep: &HashSet<String>) {
        let Ok(streamer) = self.streamer() else { return };
        let info = job.info();
        let handles: Vec<Arc<ManagedTorrent>> = std::mem::take(&mut *job.handles.lock().unwrap());
        for h in handles.iter().filter(|h| !keep.contains(&h.info_hash().as_string())) {
            if let Err(e) = streamer.api.session().delete(TorrentIdOrHash::Id(h.id()), true).await {
                warn!("{}: não tirei o torrent do librqbit: {e:#}", info.title);
            }
        }
        for s in job.sources().iter().filter(|s| !keep.contains(&s.info_hash)) {
            let _ = std::fs::remove_dir_all(&s.folder);
            if let Some(f) = &s.torrent_file {
                let _ = std::fs::remove_file(f);
            }
        }
        // a pasta .juntar-N fica vazia quando nada foi reaproveitado
        if let Some(base) = info.audio.folder.parent() {
            let _ = std::fs::remove_dir(base);
        }
    }

    async fn save_muxes(&self) {
        let Ok(streamer) = self.streamer() else { return };
        let list: Vec<MuxInfo> = self.muxes.lock().await.values().map(|j| j.info()).filter(|i| i.step != Step::Done).collect();
        let path = streamer.state_dir.join(JOBS_FILE);
        let tmp = path.with_extension("tmp");
        let res = serde_json::to_vec_pretty(&list).map_err(anyhow::Error::from).and_then(|b| Ok(std::fs::write(&tmp, b)?)).and_then(|_| Ok(std::fs::rename(&tmp, &path)?));
        if let Err(e) = res {
            warn!("não gravei {}: {e:#}", path.display());
        }
    }

    /// Retoma o que estava sendo juntado antes de um reinício.
    pub async fn restore_muxes(self: &Arc<Self>) -> Result<()> {
        let streamer = self.streamer()?;
        let path = streamer.state_dir.join(JOBS_FILE);
        let Ok(bytes) = std::fs::read(&path) else { return Ok(()) };
        let saved: Vec<MuxInfo> = serde_json::from_slice(&bytes).with_context(|| format!("lendo {}", path.display()))?;
        for info in saved {
            let mut handles = vec![];
            for s in info.video.iter().chain([&info.audio]) {
                match streamer.find_handle(&s.info_hash) {
                    Some(h) => handles.push(h),
                    None if info.step != Step::Failed => match self.readd(s).await {
                        Ok(h) => handles.push(h),
                        Err(e) => warn!("{}: não deu para retomar {}: {e:#}", info.title, s.title),
                    },
                    None => {}
                }
            }
            let (key, step) = (info.target.key(), info.step);
            let job = MuxJob::new(info, handles);
            self.muxes.lock().await.insert(key, job.clone());
            if step != Step::Failed {
                tokio::spawn(run(self.clone(), job));
            }
        }
        Ok(())
    }

    async fn readd(&self, s: &Source) -> Result<Arc<ManagedTorrent>> {
        let source = match (&s.magnet, &s.torrent_file) {
            (Some(m), _) => TorrentSource::Magnet(m.clone()),
            (None, Some(f)) => TorrentSource::File(std::fs::read(f)?.into()),
            _ => bail!("sem magnet nem .torrent gravado"),
        };
        let h = self.add_torrent(&source, &s.folder, crate::stream::ADD_TIMEOUT).await?;
        if s.only_file {
            self.streamer()?.api.api_torrent_action_update_only_files(TorrentIdOrHash::Id(h.id()), &HashSet::from([s.file_id])).await?;
        }
        Ok(h)
    }

    async fn run_mux(&self, job: &MuxJob) -> Result<()> {
        let info = job.info();
        let handles = job.handles.lock().unwrap().clone();
        if handles.len() < job.sources().len() {
            bail!("um dos downloads se perdeu — comece de novo");
        }
        job.set(Step::Downloading, None);
        for h in &handles {
            wait_or_stall(h).await?;
        }

        job.set(Step::Checking, None);
        self.save_muxes().await;
        let video = match (&info.library, &info.video) {
            (Some(l), _) => &l.file,
            (None, Some(v)) => &v.host_file,
            (None, None) => bail!("sem vídeo"),
        };
        let dub = &info.audio.host_file;
        let (v, d) = (self.probe_file(video).await?, self.probe_file(dub).await?);
        let (pt, dtrack, vtrack) = pick_tracks(&v.tracks, &d.tracks)?;
        let sync = self.check_sync(video, &v, vtrack, dub, &d, dtrack).await?;
        info!("{}: dublagem {:+.2} s, velocidade {:.5}", info.title, sync.offset, sync.speed);
        *job.sync.lock().unwrap() = Some(sync.clone());

        job.set(Step::Joining, None);
        let pt = d.tracks.iter().find(|t| t.n == pt).cloned().context("faixa da dublagem sumiu")?;
        let out = self.join(&info.target.label(), video, dub, &pt, &sync).await?;

        job.set(Step::Importing, None);
        let imported = match info.target {
            Target::Movie { .. } => self.import_joined(&info, &out).await,
            Target::Episode { .. } => self.import_episode(&info, &out).await,
        };
        if let Some(dir) = out.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
        imported?;
        if let Some(inode) = info.library.as_ref().and_then(|l| l.inode)
            && let Err(e) = self.drop_replaced_torrent(inode).await
        {
            warn!("{}: torrent da versão sem dublagem não removido: {e:#}", info.title);
        }
        self.discard_sources(job, &HashSet::new()).await;
        job.set(Step::Done, None);
        self.save_muxes().await;
        info!("{}: dublagem juntada e na biblioteca", info.title);
        Ok(())
    }

    /// Import manual no Radarr: o idioma vai marcado (PT-BR + o original), sem
    /// depender do nome do arquivo.
    async fn import_joined(&self, info: &MuxInfo, out: &Path) -> Result<()> {
        let Target::Movie { movie_id, .. } = info.target else { bail!("não é filme") };
        let radarr = self.radarr.as_ref().context("Radarr não configurado")?;
        let folder = self.cfg.container_path(out.parent().context("arquivo sem pasta")?).context("a pasta de download está fora dos [[paths]]")?;
        // só a pasta: com movieId o Radarr lista a pasta do filme na biblioteca
        let items = radarr.get_json(&format!("manualimport?folder={}&filterExistingFiles=false", url_encode(&folder))).await?;
        let name = out.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let item = items
            .as_array()
            .and_then(|a| a.iter().find(|i| i["path"].as_str().is_some_and(|p| p.ends_with(name))))
            .context("o Radarr não viu o arquivo juntado")?;
        if let Some(r) = item["rejections"].as_array().filter(|r| !r.is_empty()) {
            warn!("{}: o Radarr apontou {r:?} (import manual segue mesmo assim)", info.title);
        }
        let mut languages = vec![json!({ "id": RADARR_PT_BR, "name": "Portuguese (Brazil)" })];
        languages.extend(
            item["languages"].as_array().into_iter().flatten().filter(|l| l["id"].as_i64().is_some_and(|id| id > 0) && !l["name"].as_str().unwrap_or_default().starts_with("Portuguese")).cloned(),
        );
        let file = json!({
            "path": item["path"],
            "movieId": movie_id,
            "quality": item["quality"],
            "languages": languages,
            "releaseGroup": item["releaseGroup"],
            "indexerFlags": 0,
        });
        let old = info.library.as_ref().map(|l| l.file_id);
        radarr.send_json(Method::POST, "command", &json!({ "name": "ManualImport", "files": [file], "importMode": "move" })).await?;
        for _ in 0..120 {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let now = radarr.movie(movie_id).await?["movieFile"]["id"].as_i64();
            if !out.exists() && now.is_some() && now != old {
                return radarr.set_movie_monitored(movie_id, true).await;
            }
        }
        bail!("o Radarr não importou em 10 min (veja System → Events no Radarr)")
    }

    /// O mesmo no Sonarr, dizendo de que série e episódio é.
    async fn import_episode(&self, info: &MuxInfo, out: &Path) -> Result<()> {
        let Target::Episode { episode_id, series_id, .. } = info.target else { bail!("não é episódio") };
        let sonarr = self.sonarr.as_ref().context("Sonarr não configurado")?;
        let folder = self.cfg.container_path(out.parent().context("arquivo sem pasta")?).context("a pasta de download está fora dos [[paths]]")?;
        let items = sonarr.get_json(&format!("manualimport?folder={}&filterExistingFiles=false", url_encode(&folder))).await?;
        let name = out.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let item = items
            .as_array()
            .and_then(|a| a.iter().find(|i| i["path"].as_str().is_some_and(|p| p.ends_with(name))))
            .context("o Sonarr não viu o arquivo juntado")?;
        let mut languages = vec![json!({ "id": SONARR_PT_BR, "name": "Portuguese (Brazil)" })];
        languages.extend(
            item["languages"].as_array().into_iter().flatten().filter(|l| l["id"].as_i64().is_some_and(|id| id > 0) && !l["name"].as_str().unwrap_or_default().starts_with("Portuguese")).cloned(),
        );
        let file = json!({
            "path": item["path"],
            "seriesId": series_id,
            "episodeIds": [episode_id],
            "quality": item["quality"],
            "languages": languages,
            "releaseGroup": item["releaseGroup"],
            "releaseType": "singleEpisode",
            "indexerFlags": 0,
        });
        let old = info.library.as_ref().map(|l| l.file_id);
        sonarr.send_json(Method::POST, "command", &json!({ "name": "ManualImport", "files": [file], "importMode": "move" })).await?;
        for _ in 0..120 {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let now = sonarr.episode(episode_id).await?["episodeFileId"].as_i64().filter(|id| *id > 0);
            if !out.exists() && now.is_some() && now != old {
                return sonarr.monitor_episodes(&[episode_id]).await;
            }
        }
        bail!("o Sonarr não importou em 10 min (veja System → Events no Sonarr)")
    }
}

/// Espera o download terminar; parado por `STALL`, desiste.
async fn wait_or_stall(h: &Arc<ManagedTorrent>) -> Result<()> {
    let (mut seen, mut since) = (h.stats().progress_bytes, std::time::Instant::now());
    let done = h.wait_until_completed();
    tokio::pin!(done);
    loop {
        tokio::select! {
            r = &mut done => return r,
            _ = tokio::time::sleep(Duration::from_secs(60)) => {
                let now = h.stats().progress_bytes;
                if now != seen {
                    (seen, since) = (now, std::time::Instant::now());
                } else if since.elapsed() >= STALL {
                    bail!("o download ficou {} min parado: ninguém compartilhando", STALL.as_secs() / 60);
                }
            }
        }
    }
}

async fn run(orch: Arc<Orchestrator>, job: Arc<MuxJob>) {
    if let Err(e) = orch.run_mux(&job).await {
        let title = job.info().title;
        warn!("{title}: juntar dublagem: {e:#}");
        job.set(Step::Failed, Some(format!("{e:#}")));
        orch.save_muxes().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(path: &str) -> Vec<i16> {
        std::fs::read(path).unwrap().chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect()
    }

    // áudio real (trechos do mesmo filme): ANUBIS_PCM=pasta cargo test -- --ignored
    #[test]
    #[ignore]
    fn locates_real_audio() {
        let dir = std::env::var("ANUBIS_PCM").unwrap();
        let reference = envelope(&pcm(&format!("{dir}/en_ref.raw")), 8000);
        for (name, secs) in [("pt_same", 60.0), ("en_shift", 67.3), ("pt_shift", 67.3), ("pt_other3000", -1.0), ("pt_other600", -1.0)] {
            let (lag, z) = locate(&reference, &envelope(&pcm(&format!("{dir}/{name}.raw")), 8000)).unwrap();
            eprintln!("{name}: {:.2} s (esperado {secs}), nitidez {z:.1}", lag as f32 / FPS as f32);
        }
    }

    // No FeuVault (docker e /data de verdade), com uma "dublagem" de resposta
    // conhecida feita com ffmpeg a partir de um filme DUAL da biblioteca
    // (ex.: adelay de 7,5 s → EXPECT="7.5,1"; atempo 25/23,976 → velocidade
    // 0.959040959; trecho removido no meio → EXPECT=falha). O binário de
    // teste (cargo test --no-run) roda lá com:
    // ANUBIS_CONFIG=… VIDEO=… DUB=… EXPECT=… binário --ignored joins_known_files
    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn joins_known_files() {
        let env = |k: &str| std::env::var(k).unwrap();
        let mut cfg = crate::config::Config::load(Path::new(&env("ANUBIS_CONFIG"))).unwrap();
        let s = cfg.stream.as_mut().unwrap();
        let state = std::env::temp_dir().join("anubis-teste-juntar");
        let streamer = crate::stream::Streamer::new(state, std::mem::take(&mut s.transcode), String::new()).await.unwrap();
        let orch = Orchestrator::new(cfg, Some(Arc::new(streamer))).unwrap();
        let (video, dub) = (PathBuf::from(env("VIDEO")), PathBuf::from(env("DUB")));
        let (v, d) = (orch.probe_file(&video).await.unwrap(), orch.probe_file(&dub).await.unwrap());
        let (pt, dtrack, vtrack) = pick_tracks(&v.tracks, &d.tracks).unwrap();
        eprintln!("faixas: dublagem {pt}, comparadas {dtrack} × {vtrack}");
        let t0 = std::time::Instant::now();
        let sync = orch.check_sync(&video, &v, vtrack, &dub, &d, dtrack).await;
        eprintln!("conferência em {:.0} s: {sync:?}", t0.elapsed().as_secs_f64());
        let expect = env("EXPECT");
        if expect == "falha" {
            assert!(sync.is_err());
            return;
        }
        let sync = sync.unwrap();
        let (offset, speed) = expect.split_once(',').unwrap();
        assert!((sync.offset - offset.parse::<f64>().unwrap()).abs() < 0.05, "atraso {}", sync.offset);
        assert!((sync.speed - speed.parse::<f64>().unwrap()).abs() < 1e-4, "velocidade {}", sync.speed);

        let track = d.tracks.iter().find(|t| t.n == pt).unwrap().clone();
        let t0 = std::time::Instant::now();
        let out = orch.join("teste", &video, &dub, &track, &sync).await.unwrap();
        eprintln!("juntado em {:.0} s: {}", t0.elapsed().as_secs_f64(), out.display());
        // a dublagem juntada contra a faixa PT-BR original do vídeo (mesmo som): atraso zero
        let o = orch.probe_file(&out).await.unwrap();
        eprintln!("faixas do resultado: {:?}", o.tracks);
        assert_eq!(o.tracks.len(), v.tracks.len() + 1);
        let vpt = v.tracks.iter().find(|t| is_portuguese(t)).unwrap().n;
        for f in POINTS {
            let p = orch.measure(&video, vpt, &out, 0, v.duration * f, 1.0).await.unwrap();
            eprintln!("resultado em {:.0} min: {:+.3} s (nitidez {:.0})", p.minute, p.offset, p.sharpness);
            assert!(p.offset.abs() <= 0.05 && p.sharpness >= MIN_SHARPNESS);
        }
        // opcional: entrega a um filme de teste do Radarr (apague depois)
        if let Ok(id) = std::env::var("IMPORT_MOVIE_ID") {
            let src = Source { title: String::new(), info_hash: String::new(), magnet: None, torrent_file: None, folder: PathBuf::new(), file_id: 0, only_file: false, host_file: PathBuf::new() };
            let info = MuxInfo { target: Target::Movie { tmdb: 0, movie_id: id.parse().unwrap() }, title: "teste".into(), video: Some(src.clone()), library: None, audio: src, step: Step::Importing, error: None };
            orch.import_joined(&info, &out).await.unwrap();
            let file = &orch.radarr.as_ref().unwrap().movie(id.parse().unwrap()).await.unwrap()["movieFile"];
            eprintln!("importado: {} · idiomas {}", file["path"], file["languages"]);
        }
    }

    #[test]
    fn locates_a_shifted_signal() {
        // ruído com volume sorteado a cada 10 ms (os "ataques"); o trecho
        // começa 3,5 s dentro
        let mut x: u32 = 7;
        let mut next = || {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            x >> 16
        };
        let mut pcm = Vec::with_capacity(RATE * 30);
        for _ in 0..30 * FPS {
            let shift = next() % 10;
            pcm.extend((0..RATE / FPS).map(|_| (next() as u16 as i16) >> shift));
        }
        let probe = &pcm[RATE * 7 / 2..RATE * 7 / 2 + RATE * 10];
        let (lag, z) = locate(&envelope(&pcm, RATE), &envelope(probe, RATE)).unwrap();
        assert_eq!(lag, 350);
        assert!(z > MIN_SHARPNESS);
    }

    #[test]
    fn tries_same_speed_first_then_close_frame_rate_changes() {
        assert_eq!(speeds(1.0).first(), Some(&1.0));
        // 23,976 → 25 quadros/s (dublagem mais curta); 24 → 25 logo depois
        let s = speeds(0.9591);
        assert_eq!(s.len(), 3);
        assert!((s[1] - 24000.0 / 1001.0 / 25.0).abs() < 1e-9);
        assert!((s[2] - 24.0 / 25.0).abs() < 1e-9);
        assert_eq!(speeds(0.8), vec![1.0]);
    }

    #[test]
    fn needs_two_sharp_points_that_agree() {
        let p = |offset, sharpness| Point { minute: 0.0, offset, sharpness };
        assert_eq!(agree(&[p(1.20, 30.0), p(1.25, 28.0), p(9.0, 4.0)]), Some(1.25));
        // um corte entre os pontos: dois confiáveis discordando
        assert_eq!(agree(&[p(1.2, 30.0), p(1.2, 28.0), p(5.0, 25.0)]), None);
        assert_eq!(agree(&[p(1.2, 30.0), p(3.0, 4.0), p(9.0, 3.0)]), None);
    }

    #[test]
    fn standard_aac_layouts() {
        assert_eq!(aac_layout(2), ("stereo", 2));
        assert_eq!(aac_layout(6), ("5.1", 6));
        // 7.1 desce para 5.1; 4.0 sobe (o AAC de 4 canais também iria num PCE)
        assert_eq!(aac_layout(8), ("5.1", 6));
        assert_eq!(aac_layout(4), ("5.1", 6));
    }

    #[test]
    fn picks_the_dub_and_the_matching_original() {
        let t = |n, lang: &str, title: &str| Track { n, lang: lang.into(), title: title.into(), channels: 6 };
        // dual áudio: compara o inglês da dublagem com o inglês do vídeo
        assert_eq!(pick_tracks(&[t(0, "eng", ""), t(1, "spa", "")], &[t(0, "por", ""), t(1, "eng", "")]).unwrap(), (0, 1, 0));
        // só a dublagem, sem idioma marcado
        assert_eq!(pick_tracks(&[t(0, "eng", "")], &[t(0, "", "")]).unwrap(), (0, 0, 0));
        // idioma só no título
        assert_eq!(pick_tracks(&[t(0, "fre", "")], &[t(0, "und", "Original"), t(1, "und", "Português Dublado")]).unwrap(), (1, 1, 0));
        assert!(pick_tracks(&[t(0, "eng", "")], &[t(0, "eng", ""), t(1, "spa", "")]).is_err());
    }
}
