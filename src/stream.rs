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
use crate::dubsync::DubSync;
use crate::orchestrator::Orchestrator;
use crate::search::Kind;
use crate::services::TorrentSource;
use crate::tosho::Tracks;
use anyhow::{Context, Result, bail};
use librqbit::api::TorrentIdOrHash;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, Api, DhtSessionConfig, ListenerMode, ListenerOptions, ManagedTorrent, Session, SessionOptions,
    SessionPersistenceConfig,
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
/// Porta em que o librqbit recebe conexões (TCP e uTP). Sem ela, só baixa de
/// quem aceita conexão de fora — e quase ninguém aceita (roteador/NAT), então
/// torrents com poucas fontes não respondiam.
const TORRENT_PORT: u16 = 6882;
/// Porta anunciada aos rastreadores e à DHT. O firewall do roteador (Huawei)
/// só deixa entrar IPv6 de fora nas portas 80 e 443; o do FeuVault
/// (infra/feuvault/firewall) desvia a 443 que chega de fora para TORRENT_PORT.
const ANNOUNCE_PORT: u16 = 443;
/// Rastreadores públicos somados a todo torrent (há magnets sem nenhum, como
/// o pacote de Loki do Comando; as fontes que o site mostra vêm deles).
const PUBLIC_TRACKERS: [&str; 5] = [
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.stealth.si:80/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://exodus.desync.com:6969/announce",
    "udp://tracker.dler.org:6969/announce",
];
/// Tempo para um torrent achar alguém e mandar os metadados.
pub(crate) const ADD_TIMEOUT: Duration = Duration::from_secs(120);
/// No "Assistir", cada versão tem isso antes de dar lugar à próxima (todas
/// dentro de ADD_TIMEOUT); a última da fila tem o tempo que sobrar.
const TRY_TIMEOUT: Duration = Duration::from_secs(40);

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
    /// substitui o arquivo que já estava na biblioteca
    #[serde(default)]
    replaces: Option<Replace>,
    /// legendas que vêm no torrent (caminhos no disco), baixadas antes do vídeo
    #[serde(default)]
    subs: Vec<PathBuf>,
    /// download assumido do qBittorrent: episódios (do pacote) que deixaram de
    /// ser monitorados enquanto o Anubis cuida — voltam depois da importação
    #[serde(default)]
    remonitor: Vec<i64>,
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
    /// altura do vídeo (o menu de qualidade oferece daí para baixo)
    pub height: Option<u32>,
    /// faixas de áudio, na ordem do arquivo (o player escolhe por `n`)
    pub audios: Vec<AudioTrack>,
    /// faixas de legenda em texto embutidas: (posição entre as legendas, idioma, título)
    #[serde(skip)]
    pub subs: Vec<(usize, String, String)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AudioTrack {
    /// posição entre as faixas de áudio (0:a:N)
    pub n: usize,
    pub lang: String,
    pub title: String,
    pub codec: String,
    pub channels: u32,
}

pub struct Job {
    pub info: JobInfo,
    handle: StdMutex<Option<Arc<ManagedTorrent>>>,
    phase: StdMutex<(Phase, Option<String>)>,
    last_access: StdMutex<Instant>,
    media: StdMutex<Option<MediaInfo>>,
    probing: StdMutex<bool>,
    /// legendas do torrent já completas no disco (antes disso, não aparecem)
    subs_ready: StdMutex<HashSet<PathBuf>>,
    /// sincronia da dublagem (None = ainda não medida)
    dub: StdMutex<Option<DubSync>>,
    /// o vídeo no Jellyfin (guardado quando achado: antes de o arquivo
    /// entrar na biblioteca, o Jellyfin ainda não o conhece)
    jellyfin: StdMutex<Option<String>>,
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
            subs_ready: StdMutex::new(HashSet::new()),
            dub: StdMutex::new(None),
            jellyfin: StdMutex::new(None),
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
    /// Formato do vídeo. Com a dublagem fora de sincronia, o que tocaria
    /// direto passa pelo ffmpeg (é ele que corrige o atraso).
    pub fn media(&self) -> Option<MediaInfo> {
        let mut media = self.media.lock().unwrap().clone()?;
        if media.mode == Mode::Direct && self.dub_offset().is_some() {
            media.mode = Mode::Remux;
        }
        Some(media)
    }
    pub fn dub(&self) -> Option<DubSync> {
        self.dub.lock().unwrap().clone()
    }
    pub(crate) fn set_dub(&self, sync: DubSync) {
        *self.dub.lock().unwrap() = Some(sync);
    }
    pub(crate) fn dub_offset(&self) -> Option<f64> {
        self.dub.lock().unwrap().as_ref().and_then(DubSync::offset)
    }
    pub(crate) fn host_file(&self) -> &Path {
        &self.info.host_file
    }
    fn snapshot(&self) -> JobInfo {
        JobInfo { phase: self.phase().0, ..self.info.clone() }
    }
}

pub struct Streamer {
    pub(crate) api: Api,
    jobs: Mutex<HashMap<(Kind, i64), Arc<Job>>>,
    pub(crate) state_dir: PathBuf,
    pub(crate) transcode: Transcode,
    /// endereço da própria API (o ffmpeg lê o vídeo por ela)
    self_url: String,
    /// legendas já convertidas (extrair uma embutida lê o vídeo inteiro)
    sub_cache: StdMutex<HashMap<(Kind, i64, usize), Arc<String>>>,
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
    /// idiomas do áudio, como o Radarr/Sonarr entenderam o nome
    pub languages: Vec<String>,
    pub quality: String,
    #[serde(skip)]
    pub(crate) raw: Value,
}

pub enum Started {
    /// pronto para o player (baixando ou já na biblioteca)
    Ready,
    /// nenhuma versão passa no perfil (ou as aprovadas não acharam fontes):
    /// a página oferece as outras. O segundo: as aprovadas que não
    /// responderam (sem ninguém compartilhando)
    NoApproved(Vec<Candidate>, Vec<String>),
}

/// Troca do arquivo da biblioteca por outra versão.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Replace {
    /// arquivo atual no Radarr/Sonarr (o import termina quando ele muda)
    file_id: i64,
    /// dado do arquivo atual: o torrent que o semeia sai depois da troca
    inode: Option<(u64, u64)>,
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

/// O librqbit só aceita o btih em base32 maiúsculo (o padrão); sites como o
/// Comando mandam minúsculo ("xt=urn:btih:tjlu5dng…"), e o qBittorrent aceita
/// os dois — por isso o "Baixar" funcionava e o "Assistir" não.
fn normalize_magnet(m: &str) -> String {
    let Some(start) = m.find("urn:btih:").map(|i| i + "urn:btih:".len()) else { return m.to_owned() };
    let end = m[start..].find('&').map_or(m.len(), |i| start + i);
    let hash = &m[start..end];
    if hash.len() == 32 {
        format!("{}{}{}", &m[..start], hash.to_ascii_uppercase(), &m[end..])
    } else {
        m.to_owned()
    }
}

/// O info hash de um magnet em hexadecimal minúsculo (o btih vem em hex, 40
/// caracteres, ou em base32, 32), para reconhecer o mesmo torrent vindo de
/// sites diferentes. `.torrent` → None.
fn magnet_hash(source: &TorrentSource) -> Option<String> {
    let TorrentSource::Magnet(m) = source else { return None };
    let start = m.find("urn:btih:")? + "urn:btih:".len();
    let hash = m[start..].split('&').next()?;
    match hash.len() {
        40 if hash.chars().all(|c| c.is_ascii_hexdigit()) => Some(hash.to_ascii_lowercase()),
        32 => {
            let mut bits: u64 = 0;
            let mut n = 0;
            let mut out = String::with_capacity(40);
            for c in hash.chars() {
                let v = match c.to_ascii_uppercase() {
                    c @ 'A'..='Z' => c as u64 - 'A' as u64,
                    c @ '2'..='7' => c as u64 - '2' as u64 + 26,
                    _ => return None,
                };
                bits = (bits << 5) | v;
                n += 5;
                if n >= 8 {
                    n -= 8;
                    out.push_str(&format!("{:02x}", (bits >> n) & 0xff));
                }
            }
            Some(out)
        }
        _ => None,
    }
}

pub(crate) fn candidates(releases: Vec<Value>) -> Vec<Candidate> {
    all_candidates(releases.into_iter().filter(|r| r["seeders"].as_i64().unwrap_or(0) > 0).collect())
}

/// Como `candidates`, mas também as que dizem ter zero fontes: os sites
/// brasileiros informam 0 em quase tudo, inclusive no que baixa bem.
pub(crate) fn all_candidates(releases: Vec<Value>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = releases
        .into_iter()
        .filter(|r| r["protocol"] == "torrent")
        .map(|r| Candidate {
            guid: r["guid"].as_str().unwrap_or_default().to_owned(),
            title: r["title"].as_str().unwrap_or_default().to_owned(),
            size: r["size"].as_u64().unwrap_or(0),
            seeders: r["seeders"].as_i64().unwrap_or(0),
            rejected: r["rejected"].as_bool().unwrap_or(false),
            reasons: r["rejections"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default(),
            playable: playable(r["title"].as_str().unwrap_or_default()),
            pack: r["fullSeason"].as_bool().unwrap_or(false),
            languages: r["languages"].as_array().map(|a| a.iter().filter_map(|l| l["name"].as_str().map(str::to_owned)).collect()).unwrap_or_default(),
            quality: r["quality"]["quality"]["name"].as_str().unwrap_or_default().to_owned(),
            raw: r,
        })
        .collect();
    sort_candidates(&mut out);
    out
}

/// Aprovadas primeiro; depois as dubladas, mesmo num pacote da temporada ou
/// com poucas fontes (com as regras "preferido" e "tanto faz" o Radarr/Sonarr
/// aprovam as outras também, e o português vem antes de tudo); depois
/// episódio avulso antes de pacote; depois as que tocam no navegador; depois
/// as com fontes suficientes para tocar enquanto baixam (um Bluray mal
/// semeado perde para o WEB); depois a preferência do próprio Radarr/Sonarr e
/// as mais semeadas.
fn sort_candidates(list: &mut [Candidate]) {
    list.sort_by_key(|c| {
        (
            c.rejected,
            !c.portuguese(),
            c.pack,
            std::cmp::Reverse(c.playable),
            c.seeders < ENOUGH_SEEDERS,
            c.raw["releaseWeight"].as_i64().unwrap_or(i64::MAX),
            std::cmp::Reverse(c.seeders),
        )
    });
}

/// Gravações de cinema e afins: o áudio vem do ambiente da sala.
fn bad_audio(quality: &str) -> bool {
    let q = quality.to_uppercase();
    ["CAM", "TELESYNC", "TELECINE", "WORKPRINT", "DVDSCR", "REGIONAL"].iter().any(|k| q.contains(k))
}

impl Candidate {
    /// áudio em português, como o Radarr/Sonarr entenderam o nome
    pub fn portuguese(&self) -> bool {
        self.languages.iter().any(|l| l.starts_with("Portuguese"))
    }
}

/// O idioma original de um filme ou série do Radarr/Sonarr.
/// Os indexadores de torrent ligados que não são brasileiros (`language` do
/// Prowlarr), para as buscas pelo nome romaji de um anime.
pub(crate) fn foreign_indexers(list: &Value) -> Vec<i64> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["enable"] == true && i["protocol"] == "torrent" && i["language"] != "pt-BR")
        .filter_map(|i| i["id"].as_i64())
        .collect()
}

pub(crate) fn original_language(item: &Value) -> Option<&str> {
    item["originalLanguage"]["name"].as_str().filter(|l| !l.is_empty() && *l != "Unknown")
}

/// Sem português, só o idioma original: tira as versões dubladas num
/// terceiro idioma (um anime dublado em inglês). Nome sem idioma conta como
/// original — é o que o Radarr e o Sonarr fazem. Sem saber o original, nada sai.
fn original_or_portuguese(list: &mut Vec<Candidate>, original: Option<&str>) {
    let Some(original) = original else { return };
    list.retain(|c| c.portuguese() || c.languages.iter().all(|l| l == "Unknown") || c.languages.iter().any(|l| l == original));
}

/// Nome de site brasileiro no formato de cena, que o Sonarr entende:
/// "O Mentalista - 1ª Temporada Completa" → "O Mentalista - S01 Completa";
/// "Temporada 2 Episódio 5" → "S02E05". O resto do nome fica igual. Sem isso
/// o Sonarr não acha a temporada, e a versão dublada era descartada.
pub(crate) fn scene_title(title: &str) -> String {
    let words: Vec<&str> = title.split_whitespace().collect();
    let low = |w: &str| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
    // "1ª", "2º", "3a" → 1, 2, 3
    let ordinal = |w: &str| {
        let d: String = w.chars().take_while(char::is_ascii_digit).collect();
        (!d.is_empty() && ["ª", "º", "°", "a", "o", ""].contains(&&w[d.len()..])).then(|| d.parse::<u32>().ok()).flatten()
    };
    // "5", "05", "5:" → 5 (só número)
    let number = |w: &str| {
        let t = w.trim_matches(|c: char| !c.is_alphanumeric());
        (!t.is_empty() && t.chars().all(|c| c.is_ascii_digit())).then(|| t.parse::<u32>().ok()).flatten()
    };
    let season_tag = |t: &str| t.len() > 1 && t.starts_with('S') && t[1..].chars().all(|c| c.is_ascii_digit());
    let mut out: Vec<String> = vec![];
    let mut i = 0;
    while i < words.len() {
        let (w, next) = (words[i], words.get(i + 1).copied());
        if let (Some(n), Some(nx)) = (ordinal(w), next)
            && low(nx) == "temporada"
        {
            out.push(format!("S{n:02}"));
            i += 2;
            continue;
        }
        if low(w) == "temporada"
            && let Some(n) = next.and_then(number)
        {
            out.push(format!("S{n:02}"));
            i += 2;
            continue;
        }
        if matches!(low(w).as_str(), "episódio" | "episodio" | "ep")
            && let Some(n) = next.and_then(number)
        {
            match out.last_mut() {
                Some(t) if season_tag(t) => t.push_str(&format!("E{n:02}")),
                _ => out.push(format!("E{n:02}")),
            }
            i += 2;
            continue;
        }
        out.push(w.to_owned());
        i += 1;
    }
    out.join(" ")
}

/// `title` com `name` no lugar do que vem antes da marca de temporada
/// ("O Mentalista - S01 Completa" → "The Mentalist S01 Completa"); `None`
/// sem marca.
fn with_series_name(title: &str, name: &str) -> Option<String> {
    let b = title.as_bytes();
    let at = (0..b.len()).find(|&i| {
        let boundary = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        let digits = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
        let after = b.get(i + 1 + digits).copied();
        boundary && (b[i] == b'S' || b[i] == b's') && (1..=2).contains(&digits) && after.is_none_or(|c| !c.is_ascii_alphanumeric() || c == b'E' || c == b'e')
    })?;
    (at > 0).then(|| format!("{name} {}", &title[at..]))
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

/// Legendas do torrent que são deste vídeo (índices em `paths`): ao lado
/// dele ou numa pasta Subs/Legendas; num pacote, só as do episódio (pelo
/// nome do vídeo ou pelo SxxEyy).
fn subs_for(paths: &[PathBuf], video: &Path, episode: Option<(i64, i64)>) -> Vec<usize> {
    let has_ext = |p: &Path, list: &[&str]| p.extension().and_then(|e| e.to_str()).is_some_and(|e| list.contains(&e.to_lowercase().as_str()));
    let videos = paths.iter().filter(|p| has_ext(p, &VIDEO_EXT)).count();
    let stem = video.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_lowercase();
    paths
        .iter()
        .enumerate()
        .filter(|(_, p)| has_ext(p, &["srt", "vtt", "ass", "ssa"]))
        .filter(|(_, p)| {
            let in_subs_dir = p.components().any(|c| {
                matches!(c.as_os_str().to_str().map(str::to_lowercase).as_deref(), Some("subs" | "sub" | "subtitles" | "legendas" | "legenda"))
            });
            if p.parent() != video.parent() && !in_subs_dir {
                return false;
            }
            let name = p.to_string_lossy().to_lowercase();
            videos <= 1 || name.contains(&stem) || episode.is_some_and(|(s, e)| is_episode(&name, s, e))
        })
        .map(|(i, _)| i)
        .collect()
}

/// O episódio procurado (para `judge`).
struct Wanted {
    series_id: i64,
    /// nomes da série (normalizados): o do Sonarr e o em português
    names: Vec<String>,
    /// idioma original da série no Sonarr ("Japanese")
    original: Option<String>,
    episode_id: i64,
    season: i64,
    episode: i64,
}

/// Letras e números, minúsculos: "The Legend of Vox Machina" = "the.legend.of.vox.machina".
pub(crate) fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Nome da série para buscar: sem o ano entre parênteses ("Doctor Who (2005)").
pub(crate) fn series_name(t: &str) -> String {
    match t.rsplit_once(" (") {
        Some((n, y)) if y.trim_end_matches(')').chars().all(|c| c.is_ascii_digit()) => n.to_owned(),
        _ => t.to_owned(),
    }
}

/// Um resultado do Prowlarr, julgado como o Sonarr julgaria: é desta série e
/// deste episódio (ou da temporada inteira)? A qualidade está no perfil? Os
/// formatos personalizados (dublagem, legenda) somam o mínimo do perfil?
/// Devolve a versão no formato das do Sonarr (com `rejected`/`rejections`),
/// ou None se não é deste episódio.
fn judge(r: &Value, parsed: &Value, profile: &Value, w: &Wanted, tracks: Option<&Tracks>) -> Option<Value> {
    let title = r["title"].as_str().unwrap_or_default();
    let pe = &parsed["parsedEpisodeInfo"];
    // da série: o Sonarr reconheceu, ou o nome (inglês ou português) está no título
    let same_series = match parsed["series"]["id"].as_i64() {
        Some(id) => id == w.series_id,
        None => w.names.iter().any(|n| !n.is_empty() && norm(title).contains(n.as_str())),
    };
    if !same_series {
        return None;
    }
    let episodes: Vec<i64> = pe["episodeNumbers"].as_array().into_iter().flatten().filter_map(Value::as_i64).collect();
    let pack = pe["fullSeason"] == true;
    let this = parsed["episodes"].as_array().into_iter().flatten().any(|x| x["id"].as_i64() == Some(w.episode_id))
        || (pe["seasonNumber"].as_i64() == Some(w.season) && (pack || episodes.contains(&w.episode)));
    if !this {
        return None;
    }
    let mut v = rate(r, title, pe, parsed, parsed["series"].is_null(), profile, w.original.as_deref(), tracks);
    v["fullSeason"] = json!(pack);
    v["seasonNumber"] = json!(w.season);
    v["episodeNumbers"] = json!(episodes);
    Some(v)
}

/// Uma versão de filme de fora do Radarr (a DHT), julgada como o Radarr
/// julgaria: é deste filme (reconhecido, ou o nome no título e o ano certo)?
fn judge_movie(r: &Value, parsed: &Value, profile: &Value, movie: &Value, names: &[String]) -> Option<Value> {
    let title = r["title"].as_str().unwrap_or_default();
    let pm = &parsed["parsedMovieInfo"];
    let same = match parsed["movie"]["id"].as_i64() {
        Some(id) => Some(id) == movie["id"].as_i64(),
        None => {
            names.iter().any(|n| !n.is_empty() && norm(title).contains(n.as_str()))
                && pm["year"].as_i64().filter(|y| *y > 0).is_none_or(|y| Some(y) == movie["year"].as_i64())
        }
    };
    same.then(|| rate(r, title, pm, parsed, parsed["movie"].is_null(), profile, original_language(movie), None))
}

/// A nota de uma versão pelo perfil: a qualidade está entre as aceitas? Os
/// formatos personalizados (dublagem, legenda) somam o mínimo? `info` é o
/// `parsedEpisodeInfo` (Sonarr) ou o `parsedMovieInfo` (Radarr); `unknown`:
/// o Radarr/Sonarr não reconheceu de que título é.
#[allow(clippy::too_many_arguments)]
fn rate(r: &Value, title: &str, info: &Value, parsed: &Value, unknown: bool, profile: &Value, original: Option<&str>, tracks: Option<&Tracks>) -> Value {
    // qualidades do perfil, na ordem (da pior para a melhor), e as aceitas
    let mut order: Vec<(String, bool)> = vec![];
    for it in profile["items"].as_array().into_iter().flatten() {
        match it["quality"]["name"].as_str() {
            Some(q) => order.push((q.to_owned(), it["allowed"] == true)),
            None => {
                for sub in it["items"].as_array().into_iter().flatten() {
                    order.push((sub["quality"]["name"].as_str().unwrap_or_default().to_owned(), it["allowed"] == true && sub["allowed"] != false));
                }
            }
        }
    }
    let quality = info["quality"]["quality"]["name"].as_str().unwrap_or("Unknown");
    let rank = order.iter().position(|(q, _)| q == quality);
    let items: Vec<&Value> = profile["formatItems"].as_array().into_iter().flatten().collect();
    let id_of = |name: &str| items.iter().find(|f| f["name"] == name).and_then(|f| f["format"].as_i64());
    // os formatos personalizados da versão (ids)
    let mut formats: HashSet<i64> = parsed["customFormats"].as_array().into_iter().flatten().filter_map(|c| c["id"].as_i64()).collect();
    let mut languages = info["languages"].clone();
    // título que o Radarr/Sonarr não reconheceu (nome em português, ou da
    // DHT): sem ele, não sabem o idioma original e não marcam o formato do
    // original — marca aqui, tomando o nome sem idioma como original
    if unknown {
        let langs: Vec<&str> = info["languages"].as_array().into_iter().flatten().filter_map(|l| l["name"].as_str()).collect();
        if langs.iter().all(|l| *l == "Unknown") || original.is_some_and(|o| langs.contains(&o)) {
            formats.extend(id_of(crate::prefs::ORIGINAL_FORMAT));
        }
    }
    // as faixas de verdade (Anime Tosho) valem mais que o nome: "MULTi" sem
    // português não é dublado, e a legenda em português conta mesmo sem
    // aparecer no nome
    if let Some(t) = tracks {
        let mut mark = |name: &str, on: bool| {
            if let Some(id) = id_of(name) {
                if on { formats.insert(id) } else { formats.remove(&id) };
            }
        };
        mark(crate::prefs::FORMAT, t.portuguese_audio());
        mark(crate::prefs::SUBS_FORMAT, t.portuguese_subs());
        if let Some(o) = original {
            mark(crate::prefs::ORIGINAL_FORMAT, t.has_audio(o));
        }
        languages = json!(t.arr_languages());
    }
    let score: i64 = items.iter().filter(|f| f["format"].as_i64().is_some_and(|id| formats.contains(&id))).filter_map(|f| f["score"].as_i64()).sum();
    let min = profile["minFormatScore"].as_i64().unwrap_or(0);
    let mut reasons: Vec<String> = vec![];
    if !rank.is_some_and(|i| order[i].1) {
        reasons.push(format!("Qualidade {quality} fora do perfil"));
    }
    if score < min {
        reasons.push(format!("Custom Formats: pontuação {score}, abaixo do mínimo {min} do perfil"));
    }
    // sites só de magnet (os brasileiros): o magnet vem no guid, e o
    // magnetUrl é um link do Prowlarr que redireciona para ele
    let magnet = [&r["magnetUrl"], &r["guid"]].into_iter().find(|v| v.as_str().is_some_and(|m| m.starts_with("magnet:"))).cloned().unwrap_or(Value::Null);
    let download = if r["downloadUrl"].is_string() { r["downloadUrl"].clone() } else { r["magnetUrl"].clone() };
    json!({
        "guid": r["guid"], "title": title, "size": r["size"], "seeders": r["seeders"], "protocol": "torrent", "indexer": r["indexer"],
        "magnetUrl": magnet, "downloadUrl": download,
        "rejected": !reasons.is_empty(), "rejections": reasons,
        "languages": languages, "quality": info["quality"], "customFormatScore": score,
        // menor = melhor: pontuação dos formatos, depois a qualidade
        "releaseWeight": -(score * 100 + rank.unwrap_or(0) as i64),
    })
}

/// A pasta do torrent nos nomes de arquivo do qBittorrent ("Pasta/a.mkv",
/// "Pasta/Subs/b.srt" → "Pasta"); sem pasta comum (um arquivo só), None.
fn torrent_root<'a>(names: &[&'a str]) -> Option<&'a str> {
    let first = names.first()?.split_once('/')?.0;
    names.iter().all(|n| n.split_once('/').is_some_and(|(d, _)| d == first)).then_some(first)
}

/// O vídeo a tocar: o maior, ou (episódio) o do episódio pedido.
pub(crate) fn video_file(files: &librqbit::FileInfos, episode: Option<(i64, i64)>) -> Option<(usize, u64, PathBuf)> {
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
pub(crate) fn folder_name(title: &str) -> String {
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
            // IPv4 e IPv6: a casa está atrás de CGNAT, então só pelo IPv6
            // alguém de fora consegue se conectar (sem IPv6 no kernel, o
            // vendor/librqbit-dualstack-sockets troca "[::]" por "0.0.0.0")
            ipv4_only: false,
            listen: Some(ListenerOptions {
                mode: ListenerMode::TcpAndUtp,
                listen_addr: (std::net::Ipv6Addr::UNSPECIFIED, TORRENT_PORT).into(),
                announce_port: Some(ANNOUNCE_PORT),
                ..Default::default()
            }),
            trackers: PUBLIC_TRACKERS.iter().filter_map(|t| reqwest::Url::parse(t).ok()).collect(),
            ..Default::default()
        };
        let session = Session::new_with_opts(std::env::temp_dir(), opts).await.context("iniciando o librqbit")?;
        Ok(Self { api: Api::new(session, None), jobs: Mutex::new(HashMap::new()), state_dir, transcode, self_url, sub_cache: StdMutex::new(HashMap::new()) })
    }

    pub async fn job(&self, kind: Kind, id: i64) -> Option<Arc<Job>> {
        self.jobs.lock().await.get(&(kind, id)).cloned()
    }

    /// Desiste de um download em andamento (troca de versão no player): tira
    /// o torrent do librqbit e apaga o que já tinha baixado.
    async fn cancel(&self, job: &Job) {
        self.jobs.lock().await.remove(&(job.info.kind, job.info.id));
        let handle = job.handle.lock().unwrap().take();
        if let Some(h) = handle
            && let Err(e) = self.api.session().delete(TorrentIdOrHash::Id(h.id()), true).await
        {
            warn!("{}: não tirei o torrent do librqbit: {e:#}", job.info.title);
        }
        if let Some(f) = &job.info.torrent_file {
            let _ = std::fs::remove_file(f);
        }
        // a pasta é a da versão (download_dir/<versão>), nunca a da biblioteca
        if job.info.info_hash.is_some() {
            let _ = std::fs::remove_dir_all(&job.info.folder);
        }
        self.save().await;
        info!("{}: download de {} cancelado (troca de versão)", job.info.title, job.info.release);
    }

    /// Grava os pedidos em andamento (os entregues ao qBittorrent saem).
    async fn save(&self) {
        let jobs: Vec<JobInfo> = self.jobs.lock().await.values().filter(|j| j.handle().is_some()).map(|j| j.snapshot()).collect();
        let path = self.state_dir.join(JOBS_FILE);
        crate::disk::save_json_or_warn(&path, &jobs).await;
    }

    pub(crate) fn find_handle(&self, info_hash: &str) -> Option<Arc<ManagedTorrent>> {
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
        let base = json!({ "phase": phase, "error": error, "media": job.media(), "next": job.info.next, "dub": job.dub() });
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
                    MediaInfo { duration: None, video: None, audio: None, mode: Mode::Direct, height: None, audios: vec![], subs: vec![] }
                }
            };
            info!("{}: {:?} (vídeo {:?}, áudio {:?})", job.info.title, media.mode, media.video, media.audio);
            *job.media.lock().unwrap() = Some(media);
            *job.probing.lock().unwrap() = false;
        });
    }

    /// Converte na hora a partir de `start` segundos (vídeo fragmentado em
    /// MP4, que o navegador toca enquanto chega).
    /// Vídeo convertido a partir de `start` s. `audio`: faixa de áudio (0:a:N,
    /// padrão a primeira). `height`: altura escolhida no player (menor que a
    /// do arquivo força a conversão do vídeo; sem escolha, o padrão do modo).
    pub fn transcode(&self, job: &Job, start: f64, audio: Option<usize>, height: Option<u32>) -> Result<(Child, ChildStdout)> {
        let media = job.media().context("formato do vídeo ainda desconhecido")?;
        let t = &self.transcode;
        let input = self.input(job);
        let source_h = media.height.unwrap_or(u32::MAX);
        let track = audio.unwrap_or(0);
        let dub = if track == 0 { job.dub_offset() } else { None };
        // vídeo copiado só começa num keyframe (o anterior a `start`), e o
        // áudio convertido começaria exatamente em `start`: o buraco entre os
        // dois vira um primeiro pacote longo, e o navegador toca o som emendado
        // (adiantado até o intervalo entre keyframes). Sem a dublagem, o áudio
        // começa no mesmo keyframe (-noaccurate_seek); com ela, que vem de outra
        // leitura, o vídeo é convertido para os dois começarem em `start`
        let encode = media.mode == Mode::Transcode || height.is_some_and(|h| h < source_h) || (dub.is_some() && start > 0.0);
        let max_h = height.map_or(t.max_height, |h| h.min(source_h));
        let mut args: Vec<String> = vec!["-nostdin".into(), "-hide_banner".into(), "-loglevel".into(), "error".into()];
        let vaapi = encode && t.encoder == "vaapi";
        if vaapi {
            args.extend(["-vaapi_device".into(), t.device.clone()]);
        }
        if !encode {
            args.push("-noaccurate_seek".into());
        }
        args.extend(["-ss".into(), format!("{start:.3}"), "-i".into(), input.clone()]);
        // dublagem fora de sincronia (sempre a primeira faixa): lida à parte,
        // deslocada. O som que vai com o vídeo no segundo T está em T + atraso
        let mut dub_delay = None;
        if let Some(offset) = dub {
            let at = start + offset;
            if at < 0.0 {
                dub_delay = Some(-at);
            }
            args.extend(["-ss".into(), format!("{:.3}", at.max(0.0)), "-i".into(), input]);
        }
        let audio_map = if dub.is_some() { "1:a:0".to_owned() } else { format!("0:a:{track}?") };
        args.extend(["-map".into(), "0:v:0".into(), "-map".into(), audio_map, "-sn".into(), "-dn".into()]);
        let scale = format!("scale=-2:'min({max_h},ih)'");
        // teto de taxa pela altura: a qualidade menor existe para conexões lentas
        let rate = match max_h {
            1440.. => "12M",
            1080.. => "8M",
            720.. => "4M",
            480.. => "2M",
            _ => "1M",
        };
        let buf = format!("{}M", rate.trim_end_matches('M').parse::<u32>().unwrap_or(4) * 2);
        match (encode, vaapi) {
            (true, true) => args.extend(
                ["-vf".into(), format!("{scale},format=nv12,hwupload"), "-c:v".into(), "h264_vaapi".into(), "-b:v".into(), rate.into(), "-maxrate".into(), rate.into(), "-bufsize".into(), buf, "-g".into(), "60".into()],
            ),
            (true, false) => args.extend(
                ["-vf", &scale, "-c:v", "libx264", "-preset", "veryfast", "-crf", "22", "-maxrate", rate, "-bufsize", &buf, "-pix_fmt", "yuv420p", "-g", "60"].map(String::from),
            ),
            _ => args.extend(["-c:v", "copy"].map(String::from)),
        }
        // áudio: AAC/MP3 estéreo passam; o resto (AC3, DTS, TrueHD, AAC de mais
        // canais — que pode vir num PCE, recusado pelos navegadores) vira AAC
        // estéreo. Com a dublagem deslocada, sempre convertido (o corte cai no
        // meio de pacotes)
        if let Some(d) = dub_delay {
            args.extend(["-af".into(), format!("adelay=delays={}:all=1", (d * 1000.0).round())]);
        }
        let codec = media.audios.get(track).map(|a| a.codec.as_str()).or(media.audio.as_deref());
        let stereo = media.audios.get(track).is_none_or(|a| a.channels <= 2);
        if dub.is_none() && stereo && codec.is_some_and(|a| a == "aac" || a == "mp3") {
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

impl Streamer {
    /// Legendas disponíveis: arquivos ao lado do vídeo e, com o arquivo
    /// completo, as embutidas (extraí-las lê o vídeo inteiro).
    pub fn subtitles(&self, job: &Job) -> Vec<crate::subs::Sub> {
        use crate::subs::{Sub, SubSource, external, language};
        let complete = job.handle().is_none() || job.phase().0 != Phase::Downloading;
        let ready = job.subs_ready.lock().unwrap().clone();
        let usable = |p: &PathBuf| complete || ready.contains(p) || !job.info.subs.contains(p);
        let mut out: Vec<Sub> = external(&job.info.host_file).into_iter().filter(|s| !matches!(&s.source, SubSource::File(p) if !usable(p))).collect();
        // as de uma pasta Subs/ ("Subs/2_Portuguese.srt")
        for p in job.info.subs.iter().filter(|p| usable(p)) {
            if out.iter().any(|s| matches!(&s.source, SubSource::File(q) if q == p)) {
                continue;
            }
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            let (label, lang) = language(stem.trim_start_matches(|c: char| c.is_ascii_digit() || c == '_' || c == '.' || c == ' '));
            out.push(Sub { label, lang, source: SubSource::File(p.clone()) });
        }
        if complete && let Some(m) = job.media() {
            for (i, lang, title) in m.subs {
                // o título costuma dizer mais que a tag ("Portuguese (Brazil) [SDH]")
                let t = title.to_lowercase();
                let tag = if t.contains("brazil") || t.contains("brasil") { "pt-BR".to_owned() } else { lang.clone() };
                let (mut label, code) = language(&tag);
                if t.contains("sdh") && !label.contains("SDH") {
                    label.push_str(" (SDH)");
                } else if t.contains("forced") || t.contains("forçad") {
                    label.push_str(" (forçada)");
                } else if !title.is_empty() && !t.contains("portug") && !t.contains("english") {
                    label = format!("{label} — {title}");
                }
                out.push(Sub { label: format!("{label} (embutida)"), lang: code, source: SubSource::Embedded(i) });
            }
        }
        out
    }

    /// A legenda `n` em WebVTT, adiantada em `shift` segundos.
    pub async fn subtitle_vtt(&self, job: &Job, n: usize, shift: f64) -> Result<String> {
        use crate::subs::{SubSource, decode, shift_vtt, srt_to_vtt};
        let key = (job.info.kind, job.info.id, n);
        if let Some(v) = self.sub_cache.lock().unwrap().get(&key).cloned() {
            return Ok(shift_vtt(&v, shift));
        }
        let sub = self.subtitles(job).into_iter().nth(n).context("legenda não encontrada")?;
        let vtt = match &sub.source {
            SubSource::File(p) if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("srt")) => srt_to_vtt(&decode(&tokio::fs::read(p).await?)),
            SubSource::File(p) if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("vtt")) => decode(&tokio::fs::read(p).await?),
            other => {
                // ASS/SSA e embutidas: o ffmpeg converte (lendo pela API, que
                // o container alcança)
                let (input, map) = match other {
                    SubSource::File(_) => (format!("{}/subs/{n}/raw", self.video_url(job).trim_end_matches("/video")), "0:s:0".to_owned()),
                    SubSource::Embedded(i) => (self.input(job), format!("0:s:{i}")),
                };
                let args: Vec<String> = ["-nostdin", "-hide_banner", "-loglevel", "error", "-i", &input, "-map", &map, "-f", "webvtt", "pipe:1"].map(String::from).to_vec();
                let out = tokio::time::timeout(Duration::from_secs(300), self.tool(&self.transcode.ffmpeg, &args, false).kill_on_drop(true).output())
                    .await
                    .context("a conversão da legenda demorou demais")??;
                if !out.status.success() {
                    bail!("ffmpeg: {}", String::from_utf8_lossy(&out.stderr).trim());
                }
                String::from_utf8_lossy(&out.stdout).into_owned()
            }
        };
        let vtt = Arc::new(vtt);
        self.sub_cache.lock().unwrap().insert(key, vtt.clone());
        Ok(shift_vtt(&vtt, shift))
    }

    /// Arquivo de legenda cru (para o ffmpeg do container ler ASS/SSA).
    pub async fn subtitle_raw(&self, job: &Job, n: usize) -> Result<Vec<u8>> {
        match self.subtitles(job).into_iter().nth(n).map(|s| s.source) {
            Some(crate::subs::SubSource::File(p)) => Ok(tokio::fs::read(p).await?),
            _ => bail!("legenda não encontrada"),
        }
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
    let subs = streams
        .iter()
        .filter(|s| s["codec_type"] == "subtitle")
        .enumerate()
        .filter(|(_, s)| s["codec_name"].as_str().is_some_and(|c| crate::subs::TEXT_CODECS.contains(&c)))
        .map(|(i, s)| (i, s["tags"]["language"].as_str().unwrap_or_default().to_owned(), s["tags"]["title"].as_str().unwrap_or_default().to_owned()))
        .collect();
    let height = streams.iter().find(|s| s["codec_type"] == "video").and_then(|s| s["height"].as_u64()).map(|h| h as u32);
    let audios = streams
        .iter()
        .filter(|s| s["codec_type"] == "audio")
        .enumerate()
        .map(|(n, s)| AudioTrack {
            n,
            lang: s["tags"]["language"].as_str().unwrap_or_default().to_owned(),
            title: s["tags"]["title"].as_str().unwrap_or_default().to_owned(),
            codec: s["codec_name"].as_str().unwrap_or_default().to_owned(),
            channels: s["channels"].as_u64().unwrap_or(0) as u32,
        })
        .collect();
    MediaInfo { duration, video, audio, mode, height, audios, subs }
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
        let h = self.add_torrent(&source, &info.folder, ADD_TIMEOUT).await?;
        if info.only_file {
            self.streamer()?.api.api_torrent_action_update_only_files(TorrentIdOrHash::Id(h.id()), &HashSet::from([info.file_id])).await?;
        }
        Ok(h)
    }

    pub(crate) async fn add_torrent(&self, source: &TorrentSource, folder: &Path, wait: Duration) -> Result<Arc<ManagedTorrent>> {
        let add = match source {
            TorrentSource::Magnet(m) => AddTorrent::from_url(m.clone()),
            TorrentSource::File(b) => AddTorrent::from_bytes(b.clone()),
        };
        let opts = AddTorrentOptions { output_folder: Some(folder.to_string_lossy().into_owned()), overwrite: true, ..Default::default() };
        let added = tokio::time::timeout(wait, self.streamer()?.api.session().add_torrent(add, Some(opts)))
            .await
            .with_context(|| format!("o torrent não respondeu em {} s (sem fontes?)", wait.as_secs()))??;
        let handle = match added {
            AddTorrentResponse::Added(_, h) | AddTorrentResponse::AlreadyManaged(_, h) => h,
            AddTorrentResponse::ListOnly(_) => bail!("resposta inesperada do librqbit"),
        };
        handle.wait_until_initialized().await?;
        Ok(handle)
    }

    /// Começa (ou retoma) um "assistir agora". `guid`: usar esta versão mesmo
    /// que o Radarr/Sonarr a recuse (escolha da pessoa na página) — com o item
    /// na biblioteca, é a troca por ela. `keep`: tocar o arquivo da biblioteca
    /// sem perguntar por versão melhor.
    pub async fn watch(self: &Arc<Self>, kind: Kind, id: i64, guid: Option<&str>, keep: bool) -> Result<Started> {
        let streamer = self.streamer()?.clone();
        // um download em andamento segue; um arquivo da biblioteca é conferido
        // de novo (pode ter surgido versão melhor desde a última vez)
        // outra versão escolhida no meio do download: esta sai
        if let Some(job) = streamer.job(kind, id).await
            && guid.is_some()
            && job.handle().is_some()
            && job.phase().0 == Phase::Downloading
        {
            streamer.cancel(&job).await;
        }
        if let Some(job) = streamer.job(kind, id).await
            && job.phase().0 != Phase::Failed
            && (job.handle().is_some() || keep)
            && guid.is_none()
        {
            streamer.probe(&job);
            self.check_dub(&job);
            return Ok(Started::Ready);
        }
        // já baixando pelo "Baixar" (qBittorrent): assume esse download
        if guid.is_none()
            && let Some(started) = self.adopt_download(kind, id).await?
        {
            return Ok(started);
        }
        let mut target = match kind {
            Kind::Movie => self.movie_target(id, true).await?,
            Kind::Episode => self.episode_target(id, true).await?,
            Kind::Series => bail!("escolha um episódio"),
        };

        let mut replaces = None;
        if let Some(lib) = target.library.take() {
            let host = self.cfg.host_path(&lib.path);
            if guid.is_some() {
                // troca: as versões de agora, e o arquivo atual para depois
                replaces = Some(Replace { file_id: lib.file_id, inode: crate::disk::file_id(&host).map(|f| f.inode) });
                target.candidates = self.releases_of(kind, target.arr_id, false).await?;
            } else {
                // toca o que já está no disco na hora; se há versão melhor, o
                // player pergunta depois (GET …/better)
                target.library = Some(lib);
            }
        }

        // já na biblioteca: toca o arquivo de lá, sem torrent
        if let Some(lib) = &target.library {
            let host_file = self.cfg.host_path(&lib.path);
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
                    replaces: None,
                    subs: vec![],
                    remonitor: vec![],
                },
                None,
            );
            streamer.jobs.lock().await.insert((kind, id), job.clone());
            streamer.probe(&job);
            self.check_dub(&job);
            return Ok(Started::Ready);
        }

        // a versão escolhida na página tem o tempo todo; sem escolha, a que não
        // acha ninguém em TRY_TIMEOUT dá lugar à próxima aprovada
        let picks: Vec<Candidate> = match guid {
            Some(g) => target.candidates.iter().filter(|c| c.guid == g).take(1).cloned().collect(),
            None => target.candidates.iter().filter(|c| !c.rejected).cloned().collect(),
        };
        if picks.is_empty() {
            return Ok(Started::NoApproved(target.candidates.into_iter().take(10).collect(), vec![]));
        }
        let mut pool = std::mem::take(&mut target.candidates);
        let per_try = if guid.is_some() { ADD_TIMEOUT } else { TRY_TIMEOUT };

        let stream_cfg = self.cfg.stream.as_ref().context("configure [stream]")?;
        let mut deadline = Instant::now() + ADD_TIMEOUT;
        // o mesmo torrent em dois sites é tentado uma vez só
        let mut tried = HashSet::new();
        let mut last_err = None;
        // aprovadas que não acharam ninguém (para a página explicar)
        let mut dead: Vec<Candidate> = vec![];
        // sem escolha na página: se as aprovadas não acham fontes, a DHT entra
        // uma vez (português sempre, mesmo devagar)
        let mut dht_done = guid.is_some() || self.cfg.bitmagnet.is_none();
        let mut queue: std::collections::VecDeque<Candidate> = picks.into();
        loop {
            let Some(pick) = queue.pop_front() else {
                if !dht_done && !dead.is_empty() {
                    dht_done = true;
                    info!("{}: as versões aprovadas não acharam fontes; procurando na DHT", target.title);
                    match self.releases_of(kind, target.arr_id, true).await {
                        Ok(more) => {
                            let gone: HashSet<&str> = dead.iter().map(|c| c.guid.as_str()).collect();
                            queue.extend(more.iter().filter(|c| !c.rejected && !gone.contains(c.guid.as_str())).cloned());
                            pool = more;
                        }
                        Err(e) => warn!("{}: DHT: {e:#}", target.title),
                    }
                    deadline = Instant::now() + ADD_TIMEOUT;
                    continue;
                }
                break;
            };
            let left = deadline.saturating_duration_since(Instant::now());
            if left < Duration::from_secs(5) {
                break;
            }
            let source = match self.fetch_source(&pick.raw).await {
                Ok(s) => s,
                Err(e) => {
                    warn!("{}: {}: {e:#}", target.title, pick.title);
                    last_err = Some(e);
                    continue;
                }
            };
            if let Some(hash) = magnet_hash(&source)
                && !tried.insert(hash)
            {
                continue;
            }
            let folder = stream_cfg.download_dir.join(folder_name(&pick.title));
            info!("assistir agora: {} — {} ({} MB)", target.title, pick.title, pick.size >> 20);
            // a última que sobrou tem o tempo todo: português mesmo que demore (um
            // torrent pouco semeado pode levar mais de 40 s para achar alguém)
            let this_try = if queue.is_empty() { left } else { per_try.min(left) };
            match self.start_torrent(kind, id, target.clone(), pick.title.clone(), source, folder, true, replaces.clone(), vec![], this_try).await {
                Err(e) if e.root_cause().is::<tokio::time::error::Elapsed>() => {
                    warn!("{}: {} não achou fontes; tentando a próxima versão", target.title, pick.title);
                    dead.push(pick);
                    last_err = Some(e);
                }
                started => return started,
            }
        }
        // nenhuma aprovada respondeu: a página oferece as outras (o legendado,
        // quando não há português vivo), dizendo quais não tinham fontes
        if guid.is_none() && !dead.is_empty() {
            let gone: HashSet<&str> = dead.iter().map(|c| c.guid.as_str()).collect();
            let rest: Vec<Candidate> = pool.iter().filter(|c| !gone.contains(c.guid.as_str())).take(10).cloned().collect();
            if !rest.is_empty() {
                return Ok(Started::NoApproved(rest, dead.into_iter().map(|c| c.title).collect()));
            }
        }
        match last_err {
            Some(e) if tried.len() > 1 => Err(e.context(format!("nenhuma das {} versões achou fontes em {} s", tried.len(), ADD_TIMEOUT.as_secs()))),
            Some(e) => Err(e),
            None => bail!("nenhuma versão respondeu"),
        }
    }

    /// O "Assistir" de algo que o "Baixar" já está baixando no qBittorrent:
    /// o torrent sai do qBittorrent (os arquivos ficam) e entra no librqbit
    /// na mesma pasta — ele confere o que já foi baixado e continua dali,
    /// priorizando o trecho que o player pede. Enquanto isso, o Radarr/Sonarr
    /// deixa de monitorar (senão pegaria outra versão); a importação religa.
    async fn adopt_download(self: &Arc<Self>, kind: Kind, id: i64) -> Result<Option<Started>> {
        let (arr, filter, arr_id) = match kind {
            Kind::Movie => {
                let radarr = self.radarr.as_ref().context("Radarr não configurado")?;
                let Some(m) = radarr.find("movie", "tmdbId", id).await? else { return Ok(None) };
                if m["hasFile"] == true {
                    return Ok(None);
                }
                let mid = m["id"].as_i64().context("filme sem id")?;
                (radarr, format!("movieId={mid}"), mid)
            }
            Kind::Episode => {
                let sonarr = self.sonarr.as_ref().context("Sonarr não configurado")?;
                let ep = sonarr.episode(id).await?;
                if ep["hasFile"] == true {
                    return Ok(None);
                }
                (sonarr, format!("seriesId={}", ep["seriesId"]), id)
            }
            Kind::Series => return Ok(None),
        };
        let queue = arr.queue(&filter).await?;
        let Some(item) = queue.iter().find(|q| {
            q["protocol"] == "torrent" && q["downloadId"].is_string() && (kind == Kind::Movie || q["episodeId"].as_i64() == Some(arr_id))
        }) else {
            return Ok(None);
        };
        let hash = item["downloadId"].as_str().unwrap_or_default().to_lowercase();
        let Some(t) = self.qbit.by_hashes(std::slice::from_ref(&hash)).await?.into_iter().next() else { return Ok(None) };
        let files = self.qbit.files(&hash).await?;
        // o librqbit quer a pasta do torrent (arquivos relativos a ela)
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        let content = t.content_path.trim_end_matches('/');
        let folder_container = match torrent_root(&names) {
            Some(_) => content.to_owned(),
            None => content.rsplit_once('/').map_or(t.save_path.clone(), |(dir, _)| dir.to_owned()),
        };
        let folder = self.cfg.host_path(&folder_container);
        let source = match self.qbit.export(&hash).await {
            Ok(b) => TorrentSource::File(b),
            Err(_) if t.magnet_uri.starts_with("magnet:") => TorrentSource::Magnet(normalize_magnet(&t.magnet_uri)),
            Err(e) => return Err(e),
        };
        let target = match kind {
            Kind::Movie => self.movie_target(id, false).await?,
            _ => self.episode_target(id, false).await?,
        };
        let episodes: Vec<i64> = queue.iter().filter(|q| q["downloadId"] == item["downloadId"]).filter_map(|q| q["episodeId"].as_i64()).collect();
        match kind {
            Kind::Movie => arr.set_movie_monitored(target.arr_id, false).await?,
            _ => arr.set_episodes_monitored(&episodes, false).await?,
        }
        self.qbit.remove_keep_files(&hash).await?;
        info!("assistir agora: {} — assumindo o download do qBittorrent ({:.0}%, {})", target.title, t.progress * 100.0, t.name);
        let remonitor = if kind == Kind::Movie { vec![] } else { episodes };
        self.start_torrent(kind, id, target, t.name, source, folder, false, None, remonitor, ADD_TIMEOUT).await.map(Some)
    }

    /// Põe o torrent no librqbit (arquivos em `folder`, que já podem ter parte
    /// baixada) e abre o "assistir" nele. `pack_select`: num pacote, baixar só
    /// o episódio — não num download assumido, que a pessoa pediu inteiro.
    #[allow(clippy::too_many_arguments)]
    async fn start_torrent(
        self: &Arc<Self>,
        kind: Kind,
        id: i64,
        target: Target,
        release: String,
        source: TorrentSource,
        folder: PathBuf,
        pack_select: bool,
        replaces: Option<Replace>,
        remonitor: Vec<i64>,
        wait: Duration,
    ) -> Result<Started> {
        let streamer = self.streamer()?.clone();
        let job_folder = folder.clone();
        let handle = self.add_torrent(&source, &folder, wait).await?;
        let (file_id, file_len, rel) = handle.with_metadata(|m| video_file(&m.file_infos, target.episode))?.context("o torrent não tem o vídeo")?;
        let many_files = handle.with_metadata(|m| m.file_infos.len())? > 1;
        let sub_files: Vec<(usize, PathBuf)> = handle.with_metadata(|m| {
            let paths: Vec<PathBuf> = m.file_infos.iter().map(|f| f.relative_filename.clone()).collect();
            subs_for(&paths, &rel, target.episode).into_iter().map(|i| (i, paths[i].clone())).collect()
        })?;
        // pacote: só o arquivo deste episódio (e as legendas dele)
        let only_file = pack_select && kind == Kind::Episode && many_files;
        if only_file {
            let keep: HashSet<usize> = std::iter::once(file_id).chain(sub_files.iter().map(|(i, _)| *i)).collect();
            streamer.api.api_torrent_action_update_only_files(TorrentIdOrHash::Id(handle.id()), &keep).await?;
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
                release,
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
                replaces,
                subs: sub_files.iter().map(|(_, r)| job_folder.join(r)).collect(),
                remonitor,
            },
            Some(handle.clone()),
        );
        // legendas antes do vídeo: ler pelo librqbit prioriza as peças delas
        // (são poucos KB); completas, o player já as oferece
        if !sub_files.is_empty() {
            let job = job.clone();
            tokio::spawn(async move {
                for (i, r) in sub_files {
                    let mut buf = Vec::new();
                    match handle.clone().stream(i).await {
                        Ok(mut st) => {
                            if st.read_to_end(&mut buf).await.is_ok() {
                                job.subs_ready.lock().unwrap().insert(job.info.folder.join(&r));
                            }
                        }
                        Err(e) => warn!("legenda {}: {e:#}", r.display()),
                    }
                }
            });
        }
        streamer.jobs.lock().await.insert((kind, id), job.clone());
        streamer.save().await;
        streamer.probe(&job);
        tokio::spawn(finish(self.clone(), job));
        Ok(Started::Ready)
    }

    /// Versões mais leves que a que está baixando (o player oferece quando o
    /// download não acompanha): menores, com fontes suficientes, aceitas (ou
    /// recusadas só porque já existe arquivo) e dubladas se a atual é dublada.
    /// Tocando da biblioteca numa qualidade que já atingiu o alvo da fila
    /// (Preferências → Qualidade), o Radarr/Sonarr não trocam sozinhos: as
    /// versões mais acima na fila, com fontes — o player oferece a troca.
    pub async fn better(&self, job: &Job) -> Result<Vec<Candidate>> {
        if job.handle().is_some() {
            return Ok(vec![]);
        }
        let file = match job.info.kind {
            Kind::Movie => self.radarr.as_ref().context("Radarr não configurado")?.movie(job.info.arr_id).await?["movieFile"].clone(),
            _ => {
                let sonarr = self.sonarr.as_ref().context("Sonarr não configurado")?;
                let ep = sonarr.episode(job.info.arr_id).await?;
                sonarr.episode_file(ep["episodeFileId"].as_i64().unwrap_or(0)).await?
            }
        };
        let lib = Library::from(&file)?;
        let (arr, name) = match job.info.kind {
            Kind::Movie => (self.radarr.as_ref(), self.cfg.radarr.as_ref().map(|c| c.quality_profile.clone())),
            _ => (self.sonarr.as_ref(), self.cfg.sonarr.as_ref().map(|c| c.quality_profile.clone())),
        };
        let profile = Orchestrator::profile(arr.context("Radarr/Sonarr não configurado")?, &name.unwrap_or_default()).await?;
        Ok(better_than(&lib, &self.releases_of(job.info.kind, job.info.arr_id, false).await?, &profile))
    }

    pub async fn lighter(&self, job: &Job) -> Result<Vec<Candidate>> {
        let list = self.releases_of(job.info.kind, job.info.arr_id, false).await?;
        let dubbed = list.iter().find(|c| c.title == job.info.release).is_some_and(|c| c.portuguese());
        let mut out: Vec<Candidate> = list
            .into_iter()
            .filter(|c| c.title != job.info.release && !c.pack && c.size > 0 && c.size < job.info.file_len * 4 / 5)
            .filter(|c| c.seeders >= ENOUGH_SEEDERS)
            .filter(|c| !c.rejected || c.reasons.iter().all(|r| r.starts_with("Existing file")))
            .filter(|c| c.portuguese() || !dubbed)
            .collect();
        out.sort_by_key(|c| (std::cmp::Reverse(c.playable), std::cmp::Reverse(c.seeders)));
        out.truncate(3);
        Ok(out)
    }

    /// Versões de um filme ou episódio já no Radarr/Sonarr (id deles), só em
    /// português ou no idioma original.
    async fn releases_of(&self, kind: Kind, arr_id: i64, always_dht: bool) -> Result<Vec<Candidate>> {
        let (mut list, item) = match kind {
            Kind::Movie => {
                let radarr = self.radarr.as_ref().context("Radarr não configurado")?;
                let movie = radarr.get_json(&format!("movie/{arr_id}")).await?;
                (self.movie_releases(&movie, always_dht).await?, movie)
            }
            _ => {
                // a mesma busca do "assistir" (com o nome em português e, em
                // último caso, a DHT): numa troca, a versão escolhida veio dela
                let sonarr = self.sonarr.as_ref().context("Sonarr não configurado")?;
                let ep = sonarr.episode(arr_id).await?;
                let series = sonarr.series_by_id(ep["seriesId"].as_i64().unwrap_or(0)).await?;
                let list = match &self.prowlarr {
                    Some(p) => candidates(self.prowlarr_episode(p, &series, arr_id, ep["seasonNumber"].as_i64().unwrap_or(0), ep["episodeNumber"].as_i64().unwrap_or(0), always_dht).await?),
                    None => candidates(sonarr.releases(&format!("episodeId={arr_id}")).await?),
                };
                (list, series)
            }
        };
        original_or_portuguese(&mut list, original_language(&item));
        Ok(list)
    }

    /// "Baixar" de um episódio (id no Sonarr): a busca do "assistir" (nome em
    /// português, sites brasileiros, nomes convertidos para o formato de cena)
    /// escolhe, e a versão vai ao Sonarr pelo `release/push` — ele baixa e
    /// importa como sempre. Nada aceito por ele: o Sonarr busca sozinho e fica
    /// de olho (o episódio fica monitorado).
    pub async fn download_episode(&self, episode_id: i64) -> Result<()> {
        let sonarr = self.sonarr.as_ref().context("Sonarr não configurado")?;
        let ep = sonarr.episode(episode_id).await?;
        let series_id = ep["seriesId"].as_i64().context("episódio sem série")?;
        let series = sonarr.series_by_id(series_id).await?;
        let (s, e) = (ep["seasonNumber"].as_i64().unwrap_or(0), ep["episodeNumber"].as_i64().unwrap_or(0));
        sonarr.set_series_monitored(series_id).await?;
        sonarr.monitor_episodes(&[episode_id]).await?;
        if let Some(p) = &self.prowlarr {
            let mut list = candidates(self.prowlarr_episode(p, &series, episode_id, s, e, false).await?);
            original_or_portuguese(&mut list, original_language(&series));
            // a ordem do "assistir": dublada antes, avulso antes de pacote
            for c in list.iter().filter(|c| !c.rejected).take(3) {
                match self.push_to_sonarr(&series, c).await {
                    Ok(()) => {
                        info!("baixar: {} S{s:02}E{e:02} — {}", series["title"].as_str().unwrap_or_default(), c.title);
                        return Ok(());
                    }
                    Err(err) => warn!("baixar: {}: {err:#}", c.title),
                }
            }
        }
        sonarr.command(json!({ "name": "EpisodeSearch", "episodeIds": [episode_id] })).await
    }

    /// "Baixar" de uma série (id no Sonarr), em segundo plano: para cada
    /// temporada com episódios faltando, o pacote que a busca do "assistir"
    /// achar vai ao Sonarr; depois o Sonarr busca o resto (o que já está na
    /// fila ele pula).
    pub fn download_series(self: &Arc<Self>, series_id: i64) {
        let orch = self.clone();
        tokio::spawn(async move {
            if let Err(e) = orch.push_series_packs(series_id).await {
                warn!("baixar série {series_id}: {e:#}");
            }
            if let Some(sonarr) = &orch.sonarr
                && let Err(e) = sonarr.command(json!({ "name": "SeriesSearch", "seriesId": series_id })).await
            {
                warn!("baixar série {series_id}: {e:#}");
            }
        });
    }

    async fn push_series_packs(&self, series_id: i64) -> Result<()> {
        /// temporadas buscadas pelo Anubis (o resto fica com o Sonarr)
        const MAX_SEASONS: usize = 10;
        let (Some(sonarr), Some(p)) = (&self.sonarr, &self.prowlarr) else { return Ok(()) };
        let series = sonarr.series_by_id(series_id).await?;
        let today = chrono::Utc::now().to_rfc3339();
        // o primeiro episódio faltando (e já exibido) de cada temporada
        let mut first: std::collections::BTreeMap<i64, (i64, i64)> = Default::default();
        for x in sonarr.episodes_raw(series_id).await? {
            let s = x["seasonNumber"].as_i64().unwrap_or(0);
            let aired = x["airDateUtc"].as_str().is_some_and(|d| *d < *today.as_str());
            if s > 0 && aired && x["hasFile"] != true && x["monitored"] == true {
                let e = (x["episodeNumber"].as_i64().unwrap_or(0), x["id"].as_i64().unwrap_or(0));
                first.entry(s).and_modify(|v| *v = (*v).min(e)).or_insert(e);
            }
        }
        for (s, (e, episode_id)) in first.into_iter().take(MAX_SEASONS) {
            let mut list = candidates(self.prowlarr_episode(p, &series, episode_id, s, e, false).await?);
            original_or_portuguese(&mut list, original_language(&series));
            for c in list.iter().filter(|c| !c.rejected && c.pack).take(2) {
                match self.push_to_sonarr(&series, c).await {
                    Ok(()) => {
                        info!("baixar série: {} T{s} — {}", series["title"].as_str().unwrap_or_default(), c.title);
                        break;
                    }
                    Err(err) => warn!("baixar série: {}: {err:#}", c.title),
                }
            }
        }
        Ok(())
    }

    /// Entrega a versão ao Sonarr com um nome que ele reconheça: o de cena
    /// (`scene_title`) e, se ele ainda não souber de que série é (nome em
    /// português), o nome da série no Sonarr no lugar do que vem antes do
    /// SxxEyy — a versão já foi julgada como desta série pelo `judge`.
    async fn push_to_sonarr(&self, series: &Value, c: &Candidate) -> Result<()> {
        let sonarr = self.sonarr.as_ref().context("Sonarr não configurado")?;
        let series_id = series["id"].as_i64();
        let mut title = scene_title(&c.title);
        if sonarr.parse(&title).await?["series"]["id"].as_i64() != series_id {
            title = with_series_name(&title, series["title"].as_str().unwrap_or_default()).context("nome sem temporada (SxxEyy)")?;
            if sonarr.parse(&title).await?["series"]["id"].as_i64() != series_id {
                bail!("o Sonarr não reconheceu \"{title}\"");
            }
        }
        let r = &c.raw;
        let body = json!({
            "title": title,
            "downloadUrl": r["downloadUrl"],
            "magnetUrl": r["magnetUrl"],
            "infoHash": r["infoHash"],
            "protocol": "torrent",
            "publishDate": r["publishDate"].as_str().map_or_else(|| chrono::Utc::now().to_rfc3339(), str::to_owned),
            "size": c.size,
            "seeders": c.seeders,
            "indexer": r["indexer"],
        });
        let res = sonarr.push_release(&body).await?;
        let d = res.as_array().and_then(|a| a.first()).unwrap_or(&res);
        if d["approved"] == true {
            return Ok(());
        }
        let why: Vec<&str> = d["rejections"].as_array().into_iter().flatten().filter_map(|x| x.as_str().or(x["reason"].as_str())).collect();
        bail!("recusada pelo Sonarr: {}", why.join("; "))
    }

    /// As dublagens de um título da biblioteca (filme: id no Radarr; episódio:
    /// no Sonarr): versões com áudio em português, inclusive as de zero
    /// fontes, com a DHT sempre consultada. Só o áudio interessa, então a
    /// qualidade da imagem não conta (fora gravações de cinema, de áudio ruim);
    /// as com fontes primeiro, depois as menores.
    pub(crate) async fn dub_sources(&self, kind: Kind, arr_id: i64) -> Result<Vec<Candidate>> {
        let raw = match kind {
            Kind::Movie => {
                let radarr = self.radarr.as_ref().context("Radarr não configurado")?;
                self.movie_releases_raw(&radarr.get_json(&format!("movie/{arr_id}")).await?, true).await?
            }
            _ => {
                let sonarr = self.sonarr.as_ref().context("Sonarr não configurado")?;
                let ep = sonarr.episode(arr_id).await?;
                let (s, e) = (ep["seasonNumber"].as_i64().unwrap_or(0), ep["episodeNumber"].as_i64().unwrap_or(0));
                match &self.prowlarr {
                    Some(p) => self.prowlarr_episode(p, &sonarr.series_by_id(ep["seriesId"].as_i64().unwrap_or(0)).await?, arr_id, s, e, true).await?,
                    None => sonarr.releases(&format!("episodeId={arr_id}")).await?,
                }
            }
        };
        let mut list: Vec<Candidate> = all_candidates(raw).into_iter().filter(|c| c.portuguese() && !bad_audio(&c.quality)).collect();
        list.sort_by_key(|c| (c.seeders == 0, c.size == 0, c.size));
        Ok(list)
    }

    /// Depois de uma troca: o torrent que semeava o arquivo antigo sai do
    /// qBittorrent com os dados — só se nenhum arquivo dele continua na
    /// biblioteca (um pacote de temporada com outros episódios fica).
    pub(crate) async fn drop_replaced_torrent(&self, inode: (u64, u64)) -> Result<()> {
        self.qbit.login().await?;
        for t in self.qbit.torrents().await? {
            let files: Vec<crate::disk::FileId> = self
                .qbit
                .files(&t.hash)
                .await?
                .into_iter()
                .filter_map(|f| crate::disk::file_id(&self.cfg.host_path(&format!("{}/{}", t.save_path.trim_end_matches('/'), f.name))))
                .collect();
            if !files.iter().any(|f| f.inode == inode) {
                continue;
            }
            if files.iter().all(|f| f.nlink == 1) {
                info!("{}: versão antiga trocada, torrent removido", t.name);
                self.qbit.delete(std::slice::from_ref(&t.hash)).await?;
            } else {
                info!("{}: versão antiga trocada; o torrent fica (ainda há arquivos dele na biblioteca)", t.name);
            }
        }
        Ok(())
    }

    /// O filme no Radarr, adicionando se preciso — sem busca e sem monitorar:
    /// quem baixa é o Anubis.
    pub(crate) async fn ensure_movie(&self, tmdb: i64) -> Result<Value> {
        let radarr = self.radarr.as_ref().context("Radarr não configurado")?;
        if let Some(m) = radarr.find("movie", "tmdbId", tmdb).await? {
            return Ok(m);
        }
        let rcfg = self.cfg.radarr.as_ref().context("Radarr não configurado")?;
        let mut item = radarr.lookup("movie", &format!("tmdb:{tmdb}")).await?.into_iter().next().context("filme não encontrado")?;
        item["qualityProfileId"] = json!(radarr.quality_profile_id(&rcfg.quality_profile).await?);
        item["rootFolderPath"] = json!(rcfg.root_folder.as_deref().context("configure root_folder em [radarr]")?);
        item["monitored"] = json!(false);
        item["minimumAvailability"] = json!("released");
        item["addOptions"] = json!({ "searchForMovie": false });
        radarr.add("movie", item).await
    }

    async fn movie_target(&self, tmdb: i64, search: bool) -> Result<Target> {
        let movie = self.ensure_movie(tmdb).await?;
        let movie_id = movie["id"].as_i64().context("filme sem id")?;
        let title = movie["title"].as_str().unwrap_or_default().to_owned();
        if movie["hasFile"].as_bool() == Some(true) {
            let library = Some(Library::from(&movie["movieFile"])?);
            return Ok(Target { title, arr_id: movie_id, series_id: None, episode: None, next: None, library, candidates: vec![] });
        }
        let mut candidates = if search { self.movie_releases(&movie, false).await? } else { vec![] };
        original_or_portuguese(&mut candidates, original_language(&movie));
        Ok(Target { title, arr_id: movie_id, series_id: None, episode: None, next: None, library: None, candidates })
    }

    /// As versões de um filme pelo Radarr e, em último caso (nenhuma
    /// aprovada), o catálogo da DHT (bitmagnet), julgado pelo perfil do filme.
    async fn movie_releases(&self, movie: &Value, always_dht: bool) -> Result<Vec<Candidate>> {
        Ok(candidates(self.movie_releases_raw(movie, always_dht).await?))
    }

    /// As versões cruas (também as de zero fontes) de `movie_releases`.
    async fn movie_releases_raw(&self, movie: &Value, always_dht: bool) -> Result<Vec<Value>> {
        let radarr = self.radarr.as_ref().context("Radarr não configurado")?;
        let movie_id = movie["id"].as_i64().context("filme sem id")?;
        let mut list = radarr.releases(&format!("movieId={movie_id}")).await?;
        if self.cfg.bitmagnet.is_none() || (!always_dht && candidates(list.clone()).iter().any(|c| !c.rejected)) {
            return Ok(list);
        }
        let name = movie["title"].as_str().unwrap_or_default().to_owned();
        let year = movie["year"].as_i64().filter(|y| *y > 0);
        let pt = match (&self.tmdb, movie["tmdbId"].as_i64().filter(|id| *id > 0)) {
            (Some(t), Some(id)) => t.get(&format!("movie/{id}?language=pt-BR")).await.ok().and_then(|v| v["title"].as_str().map(str::to_owned)),
            _ => None,
        }
        .filter(|pt| norm(pt) != norm(&name));
        let profile = radarr.get_json(&format!("qualityprofile/{}", movie["qualityProfileId"])).await?;
        let names: Vec<String> = [Some(name.clone()), pt.clone()].into_iter().flatten().map(|n| norm(&n)).collect();
        let with_year = |n: &str| year.map_or_else(|| n.to_owned(), |y| format!("{n} {y}"));
        let mut seen: HashSet<String> = list.iter().filter_map(|r| r["guid"].as_str().map(str::to_lowercase)).collect();
        let mut extra = vec![];
        for q in [Some(with_year(&name)), pt.as_deref().map(with_year)].into_iter().flatten() {
            for r in self.bitmagnet_search(&q).await.unwrap_or_else(|e| {
                warn!("bitmagnet: {e:#}");
                vec![]
            }) {
                if !seen.insert(r["guid"].as_str().unwrap_or_default().to_lowercase()) {
                    continue;
                }
                let Ok(parsed) = radarr.parse(r["title"].as_str().unwrap_or_default()).await else { continue };
                if let Some(v) = judge_movie(&r, &parsed, &profile, movie, &names) {
                    extra.push(v);
                }
            }
        }
        list.extend(extra);
        Ok(list)
    }

    /// As versões que o "assistir" consideraria, na ordem dele, sem baixar
    /// nada — para entender um "não achou" (filme: id do TMDB; episódio: do Sonarr).
    pub async fn watch_candidates(&self, kind: Kind, id: i64) -> Result<Vec<Candidate>> {
        let t = match kind {
            Kind::Movie => self.movie_target(id, true).await?,
            Kind::Episode => self.episode_target(id, true).await?,
            Kind::Series => bail!("peça pelo episódio"),
        };
        Ok(t.candidates)
    }

    async fn episode_target(&self, episode_id: i64, search: bool) -> Result<Target> {
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
            let library = Some(Library::from(&f)?);
            return Ok(Target { title, arr_id: episode_id, series_id: Some(series_id), episode: Some((s, e)), next, library, candidates: vec![] });
        }
        if !search {
            return Ok(Target { title, arr_id: episode_id, series_id: Some(series_id), episode: Some((s, e)), next, library: None, candidates: vec![] });
        }
        // com o Prowlarr: busca direta, rápida e com o nome em português
        if let Some(p) = &self.prowlarr {
            let mut list = candidates(self.prowlarr_episode(p, &series, episode_id, s, e, false).await?);
            original_or_portuguese(&mut list, original_language(&series));
            return Ok(Target { title, arr_id: episode_id, series_id: Some(series_id), episode: Some((s, e)), next, library: None, candidates: list });
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
        original_or_portuguese(&mut list, original_language(&series));
        Ok(Target { title, arr_id: episode_id, series_id: Some(series_id), episode: Some((s, e)), next, library: None, candidates: list })
    }

    /// As versões de um episódio pelo Prowlarr: o episódio e a temporada com o
    /// nome da série, e o nome em português (os sites brasileiros usam esse),
    /// tudo ao mesmo tempo. Cada nome passa pelo `parse` do Sonarr (temporada,
    /// episódios, qualidade, formatos), e a decisão segue o perfil da série —
    /// a busca do Sonarr fazia uma consulta por variação do nome (inclusive em
    /// espanhol e italiano), uma de cada vez, e não reconhecia o português.
    /// Os nomes vão sem pontuação: "Frieren: Beyond Journey's End S01E05" não
    /// acha nada, "frieren beyond journey s end s01e05" acha.
    ///
    /// Anime: também o nome romaji com o número do episódio ("sousou no
    /// frieren 05" acha "[SubsPlease] Sousou no Frieren S2 - 05" e "[Erai-raws]
    /// Sousou no Frieren 2nd Season - 05") e o absoluto, se for outro; e as
    /// faixas de verdade de cada versão (Anime Tosho) decidem dublagem e
    /// legenda em português.
    async fn prowlarr_episode(&self, p: &crate::services::Prowlarr, series: &Value, episode_id: i64, s: i64, e: i64, always_dht: bool) -> Result<Vec<Value>> {
        use crate::avail::query;
        let sonarr = self.sonarr.as_ref().context("Sonarr não configurado")?;
        let name = series_name(series["title"].as_str().unwrap_or_default());
        let pt = match (&self.tmdb, series["tmdbId"].as_i64().filter(|id| *id > 0)) {
            (Some(t), Some(id)) => t.get(&format!("tv/{id}?language=pt-BR")).await.ok().and_then(|v| v["name"].as_str().map(series_name)),
            _ => None,
        }
        .filter(|pt| norm(pt) != norm(&name));
        let anime = series["seriesType"] == "anime";
        let romaji = if anime { self.romaji(series).await.filter(|r| norm(r) != norm(&name)) } else { None };
        let absolute = async || sonarr.episode(episode_id).await.ok().and_then(|x| x["absoluteEpisodeNumber"].as_i64());
        let abs = if anime { absolute().await } else { None };
        use crate::services::ONE_PAGE;
        // sem filtro de categoria: os sites brasileiros não marcam as temporadas
        // como TV ("O Mentalista - 1ª Temporada" sumia) e até o Pirate Bay perde
        // episódios; o `judge` já confere série, temporada e episódio
        let (q_ep, q_season) = (format!("{} S{s:02}E{e:02}", query(&name, None)), format!("{} S{s:02}", query(&name, None)));
        let mut queries = vec![(q_ep.clone(), vec![]), (q_season.clone(), vec![])];
        queries.extend(pt.as_deref().map(|pt| (query(pt, None), vec![])));
        if let Some(r) = &romaji {
            // o romaji só nos sites de fora: os brasileiros não usam esse nome, e
            // cada busca neles entra na fila do FlareSolverr (~2 s cada)
            let foreign = match p.get_json("indexer").await {
                Ok(list) => foreign_indexers(&list),
                Err(_) => vec![],
            };
            queries.push((format!("{} {e:02}", query(r, None)), foreign.clone()));
            queries.extend(abs.filter(|a| *a != e).map(|a| (format!("{} {a:02}", query(r, None)), foreign)));
        }
        let started = Instant::now();
        let mut set = tokio::task::JoinSet::new();
        for (q, ids) in queries {
            let p = p.clone();
            set.spawn(async move { p.search_in(&q, &[], ONE_PAGE, &ids).await });
        }
        let mut found: Vec<Value> = vec![];
        let mut seen = HashSet::new();
        while let Some(list) = set.join_next().await {
            for r in list.context("busca no Prowlarr")?.unwrap_or_else(|e| {
                warn!("Prowlarr: {e:#}");
                vec![]
            }) {
                if r["protocol"] == "torrent" && seen.insert(r["guid"].as_str().unwrap_or_default().to_owned()) {
                    found.push(r);
                }
            }
        }
        let searched = started.elapsed();
        let profile = sonarr.get_json(&format!("qualityprofile/{}", series["qualityProfileId"])).await?;
        let wanted = Wanted {
            series_id: series["id"].as_i64().unwrap_or(0),
            names: [Some(name.clone()), pt.clone(), romaji.clone()].into_iter().flatten().map(|n| norm(&n)).collect(),
            original: original_language(series).map(str::to_owned),
            episode_id,
            season: s,
            episode: e,
        };
        // as deste episódio (sem nota ainda) e, em anime, as faixas de cada uma
        // (o `parse` de várias ao mesmo tempo: com o romaji passam de 300)
        let mut mine: Vec<(Value, Value)> = vec![];
        for chunk in found.chunks(8) {
            let mut set = tokio::task::JoinSet::new();
            for r in chunk {
                let (sonarr, r) = (sonarr.clone(), r.clone());
                set.spawn(async move {
                    let parsed = sonarr.parse(&scene_title(r["title"].as_str().unwrap_or_default())).await;
                    (r, parsed)
                });
            }
            while let Some(done) = set.join_next().await {
                if let Ok((r, Ok(parsed))) = done
                    && judge(&r, &parsed, &profile, &wanted, None).is_some()
                {
                    mine.push((r, parsed));
                }
            }
        }
        let judged = started.elapsed();
        let tracks = if anime { self.tracks_of(mine.iter().map(|(r, _)| r)).await } else { HashMap::new() };
        info!("{name} S{s:02}E{e:02}: {} versões deste episódio — buscas {searched:.1?}, nomes {:.1?}, faixas {:.1?}", mine.len(), judged - searched, started.elapsed() - judged);
        let mut out: Vec<Value> = mine
            .iter()
            .filter_map(|(r, parsed)| judge(r, parsed, &profile, &wanted, crate::tosho::info_hash(r).and_then(|h| tracks.get(&h))))
            .collect();
        // último caso: nenhuma versão aprovada nas fontes (ou as aprovadas não
        // acharam fontes: `always_dht`) → o catálogo da DHT (bitmagnet), com as
        // mesmas buscas e o mesmo julgamento
        if self.cfg.bitmagnet.is_some() && (always_dht || out.iter().all(|v| v["rejected"] == true)) {
            let mut seen: HashSet<String> = out.iter().filter_map(|v| v["guid"].as_str().map(str::to_lowercase)).collect();
            // na DHT os nomes vêm como os arquivos: programas de variedade sem
            // temporada ("Running Man - 794 [260315]"), daí também o número
            // absoluto e o nome sozinho (o `judge` fica só com este episódio)
            let abs = match abs {
                Some(a) => Some(a),
                None => absolute().await,
            };
            let dht_queries = [Some(q_ep), Some(q_season), pt.as_deref().map(|pt| query(pt, None)), abs.map(|a| format!("{} {a}", query(&name, None))), Some(query(&name, None))];
            for q in dht_queries.into_iter().flatten() {
                for r in self.bitmagnet_search(&q).await.unwrap_or_else(|e| {
                    warn!("bitmagnet: {e:#}");
                    vec![]
                }) {
                    if !seen.insert(r["guid"].as_str().unwrap_or_default().to_lowercase()) {
                        continue;
                    }
                    let title = r["title"].as_str().unwrap_or_default();
                    let Ok(parsed) = sonarr.parse(&scene_title(title)).await else { continue };
                    if let Some(v) = judge(&r, &parsed, &profile, &wanted, None) {
                        out.push(v);
                    }
                }
            }
        }
        Ok(out)
    }

    /// O nome romaji de um anime ("Sousou no Frieren"): o do TMDB, ou o
    /// primeiro nome alternativo do Sonarr que vale para todas as temporadas.
    pub(crate) async fn romaji(&self, series: &Value) -> Option<String> {
        if let (Some(t), Some(id)) = (&self.tmdb, series["tmdbId"].as_i64().filter(|id| *id > 0))
            && let Ok(v) = t.get(&format!("tv/{id}/alternative_titles")).await
            && let Some(r) = crate::avail::romaji_in(&v)
        {
            return Some(r);
        }
        series["alternateTitles"]
            .as_array()?
            .iter()
            .filter(|a| a["sceneSeasonNumber"].as_i64().is_none_or(|n| n < 0))
            .filter_map(|a| a["title"].as_str())
            .find(|t| t.is_ascii())
            .map(str::to_owned)
    }

    /// As faixas (Anime Tosho) das versões mais semeadas, até 6 s.
    pub(crate) async fn tracks_of<'a>(&self, releases: impl Iterator<Item = &'a Value>) -> HashMap<String, Tracks> {
        /// versões conferidas por busca (as mais semeadas)
        const MAX: usize = 24;
        let mut list: Vec<&Value> = releases.collect();
        list.sort_by_key(|r| std::cmp::Reverse(r["seeders"].as_i64().unwrap_or(0)));
        let hashes: Vec<String> = list.iter().filter_map(|r| crate::tosho::info_hash(r)).take(MAX).collect();
        self.tosho.tracks(&hashes, Duration::from_secs(6)).await
    }

    /// Magnet direto, ou o .torrent do link de download (que pode redirecionar
    /// para um magnet — é o que o Prowlarr faz com vários indexadores).
    pub(crate) async fn fetch_source(&self, release: &Value) -> Result<TorrentSource> {
        if let Some(m) = release["magnetUrl"].as_str().filter(|m| m.starts_with("magnet:")) {
            return Ok(TorrentSource::Magnet(normalize_magnet(m)));
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
                    return Ok(TorrentSource::Magnet(normalize_magnet(&loc)));
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

    /// O vídeo no Jellyfin (o filme ou o episódio) e o usuário cujo
    /// progresso o Anubis lê e grava.
    async fn jellyfin_item(&self, job: &Job) -> Option<(String, String)> {
        let user = self.jellyfin_user().await.ok()?;
        if let Some(id) = job.jellyfin.lock().unwrap().clone() {
            return Some((id, user));
        }
        let id = match job.info.kind {
            Kind::Movie => {
                let movie = self.radarr.as_ref()?.movie(job.info.arr_id).await.ok()?;
                let folder = self.cfg.jellyfin_path_from_container(movie["path"].as_str()?)?;
                self.jellyfin.find_item("Movie", movie["title"].as_str()?, &folder).await.ok()??
            }
            _ => {
                let sonarr = self.sonarr.as_ref()?;
                let ep = sonarr.episode(job.info.id).await.ok()?;
                let series = sonarr.series_by_id(job.info.series_id?).await.ok()?;
                let eps = self.jellyfin_episodes(&series, &user).await.ok()?;
                eps.get(&(ep["seasonNumber"].as_i64()?, ep["episodeNumber"].as_i64()?))?.id.clone()
            }
        };
        *job.jellyfin.lock().unwrap() = Some(id.clone());
        Some((id, user))
    }

    /// Onde o usuário parou neste vídeo no Jellyfin (s), para continuar
    /// de lá em qualquer aparelho.
    pub async fn saved_position(&self, job: &Job) -> Option<f64> {
        let (item, user) = self.jellyfin_item(job).await?;
        let ticks = self.jellyfin.user_data(&item, &user).await.ok()?["PlaybackPositionTicks"].as_i64()?;
        (ticks > 0).then(|| ticks as f64 / 1e7)
    }

    /// Grava no Jellyfin o progresso do player: `t` s de `duration`. Como o
    /// próprio Jellyfin, o começo (menos de 5 %) não vira "continuar" e
    /// passar de 90 % marca como assistido. `false`: o Jellyfin ainda não
    /// conhece o vídeo.
    pub async fn save_progress(&self, job: &Job, t: f64, duration: f64) -> Result<bool> {
        let Some((item, user)) = self.jellyfin_item(job).await else { return Ok(false) };
        let Some(data) = progress_update(t, duration) else { return Ok(true) };
        let data = if data["Played"] == true {
            // os avisos seguem chegando até o fim: só o primeiro conta a vez
            let cur = self.jellyfin.user_data(&item, &user).await?;
            if cur["Played"] == true && cur["PlaybackPositionTicks"].as_i64() == Some(0) {
                return Ok(true);
            }
            let mut d = data;
            d["PlayCount"] = json!(cur["PlayCount"].as_i64().unwrap_or(0) + 1);
            d
        } else {
            data
        };
        self.jellyfin.set_user_data(&item, &user, &data).await?;
        Ok(true)
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

#[derive(Clone)]
struct Target {
    title: String,
    arr_id: i64,
    series_id: Option<i64>,
    episode: Option<(i64, i64)>,
    next: Option<(i64, String)>,
    library: Option<Library>,
    candidates: Vec<Candidate>,
}

/// O arquivo que já está na biblioteca.
#[derive(Clone)]
struct Library {
    /// caminho como o Radarr/Sonarr o veem
    path: String,
    /// id do arquivo no Radarr (movieFile) ou no Sonarr (episodeFile)
    file_id: i64,
    quality: String,
    portuguese: bool,
}

impl Library {
    fn from(file: &Value) -> Result<Self> {
        Ok(Self {
            path: file["path"].as_str().context("arquivo sem caminho")?.to_owned(),
            file_id: file["id"].as_i64().unwrap_or(0),
            quality: file["quality"]["quality"]["name"].as_str().unwrap_or_default().to_owned(),
            portuguese: file["languages"].as_array().into_iter().flatten().any(|l| l["name"].as_str().is_some_and(|n| n.starts_with("Portuguese"))),
        })
    }
}

/// Fontes a partir das quais uma versão costuma tocar enquanto baixa sem
/// travar; abaixo disso, uma versão com fontes suficientes vem antes.
const ENOUGH_SEEDERS: i64 = 5;

/// Versões mais acima na fila do perfil que o arquivo da biblioteca, com
/// fontes suficientes, que só foram recusadas porque já existe arquivo. Só
/// quando o arquivo já atingiu o alvo: abaixo dele, o Radarr/Sonarr trocam
/// sozinhos. Dublado não é trocado por não dublado; as melhores primeiro.
fn better_than(lib: &Library, list: &[Candidate], profile: &Value) -> Vec<Candidate> {
    let (Some(have), Some(cutoff)) = (crate::prefs::rank_in(profile, &lib.quality), crate::prefs::cutoff_rank(profile)) else { return vec![] };
    if have > cutoff {
        return vec![];
    }
    let rank = |c: &Candidate| crate::prefs::rank_in(profile, &c.quality);
    let mut out: Vec<Candidate> = list
        .iter()
        .filter(|c| rank(c).is_some_and(|r| r < have) && c.seeders >= ENOUGH_SEEDERS)
        .filter(|c| !c.rejected || c.reasons.iter().all(|r| r.starts_with("Existing file")))
        .filter(|c| c.portuguese() || !lib.portuguese)
        .cloned()
        .collect();
    out.sort_by_key(|c| rank(c));
    out.truncate(5);
    out
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
        // a dublagem é medida antes: com atraso, entra a cópia corrigida
        let sync = orch.measure_dub(&job.info.host_file, job.info.file_len).await;
        crate::dubsync::log_sync(&job.info.title, &sync);
        job.set_dub(sync);
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
    // dublagem fora de sincronia: importa uma cópia corrigida (o original segue
    // no torrent); sem correção, o próprio arquivo, por hardlink
    let fixed = match job.dub_offset() {
        Some(offset) => match orch.fix_dub(&job.info.host_file, offset).await {
            Ok(p) => {
                info!("{}: dublagem corrigida em {}", job.info.title, p.display());
                Some(p)
            }
            Err(e) => {
                warn!("{}: não deu para corrigir a dublagem no arquivo ({e:#}); importando o original", job.info.title);
                None
            }
        },
        None => None,
    };
    let result = import_file(orch, job, fixed.as_deref().unwrap_or(&job.info.host_file), fixed.is_some()).await;
    if let Some(dir) = fixed.as_deref().and_then(Path::parent) {
        let _ = std::fs::remove_dir_all(dir);
    }
    result
}

/// Importa só `file` (num pacote de temporada, a pasta tem os outros
/// episódios ainda vazios: o torrent baixa só o que se assiste).
async fn import_file(orch: &Arc<Orchestrator>, job: &Arc<Job>, file: &Path, moved: bool) -> Result<()> {
    let container = orch.cfg.container_path(file).context("o arquivo baixado está fora dos [[paths]]")?;
    // "Copy" com hardlinks ligados = hardlink (o arquivo continua no torrent);
    // a cópia corrigida é nossa, vai inteira
    let mode = if moved { "Move" } else { "Copy" };
    let (arr, cmd) = match job.info.kind {
        Kind::Movie => (orch.radarr.as_ref().context("Radarr não configurado")?, "DownloadedMoviesScan"),
        _ => (orch.sonarr.as_ref().context("Sonarr não configurado")?, "DownloadedEpisodesScan"),
    };
    arr.command(json!({ "name": cmd, "path": container, "importMode": mode })).await?;
    let old = job.info.replaces.as_ref().map(|r| r.file_id);
    for round in 0..60 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        // nome que o Sonarr não reconhece (o dos sites brasileiros): depois de
        // 30 s, importação manual dizendo qual é a série
        if round == 6
            && job.info.kind != Kind::Movie
            && let Some(series_id) = job.info.series_id
            && let Err(e) = manual_import_episodes(arr, &container, series_id, job.info.arr_id, mode).await
        {
            warn!("{}: importação manual: {e:#}", job.info.title);
        }
        // o arquivo atual no Radarr/Sonarr (numa troca, precisa ser outro)
        let file = match job.info.kind {
            Kind::Movie => arr.movie(job.info.arr_id).await?["movieFile"]["id"].as_i64(),
            _ => arr.episode(job.info.arr_id).await?["episodeFileId"].as_i64().filter(|id| *id > 0),
        };
        if file.is_some() && file != old {
            match job.info.kind {
                Kind::Movie => arr.set_movie_monitored(job.info.arr_id, true).await?,
                _ => arr.monitor_episodes(&[job.info.arr_id]).await?,
            }
            if !job.info.remonitor.is_empty() {
                arr.monitor_episodes(&job.info.remonitor).await?;
            }
            if let Some(inode) = job.info.replaces.as_ref().and_then(|r| r.inode)
                && let Err(e) = orch.drop_replaced_torrent(inode).await
            {
                warn!("{}: torrent da versão antiga não removido: {e:#}", job.info.title);
            }
            return Ok(());
        }
    }
    bail!("o import não aconteceu em 5 min (veja Activity → Queue no Radarr/Sonarr)")
}

/// Importa o episódio `file` dizendo ao Sonarr de que série e episódio é.
/// Lista a pasta e fica só com o arquivo pedido; qualidade e idiomas vêm do
/// que o Sonarr leu do nome.
async fn manual_import_episodes(sonarr: &crate::services::Arr, file: &str, series_id: i64, episode_id: i64, mode: &str) -> Result<()> {
    let folder = file.rsplit_once('/').map_or(file, |(dir, _)| dir);
    // só a pasta: com seriesId o Sonarr lista a pasta da série na biblioteca,
    // não a do download (como o Radarr com movieId).
    // filterExistingFiles=false: com true, o Sonarr responde 500 se a pasta da
    // série ainda não existe (primeiro episódio)
    let items = sonarr.get_json(&format!("manualimport?folder={}&filterExistingFiles=false", crate::services::url_encode(folder))).await?;
    let files: Vec<Value> = items
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["path"].as_str() == Some(file))
        .map(|i| {
            // os episódios que o Sonarr achou, se for esta série (nome que ele
            // não entende: o episódio que está sendo assistido)
            let eps: Vec<i64> = if i["series"]["id"].as_i64() == Some(series_id) {
                i["episodes"].as_array().into_iter().flatten().filter_map(|e| e["id"].as_i64()).collect()
            } else {
                vec![]
            };
            let eps = if eps.is_empty() { vec![episode_id] } else { eps };
            json!({ "path": i["path"], "seriesId": series_id, "episodeIds": eps, "quality": i["quality"], "languages": i["languages"],
                    "releaseGroup": i["releaseGroup"], "indexerFlags": 0, "releaseType": i["releaseType"] })
        })
        .collect();
    if files.is_empty() {
        bail!("o Sonarr não reconheceu o episódio em {file}");
    }
    sonarr.send_json(reqwest::Method::POST, "command", &json!({ "name": "ManualImport", "files": files, "importMode": mode.to_lowercase() })).await?;
    Ok(())
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

/// O que gravar no Jellyfin para a posição `t` de `duration` (s); `None` =
/// nada (começo do vídeo ou duração desconhecida).
fn progress_update(t: f64, duration: f64) -> Option<Value> {
    if duration <= 0.0 || t / duration < 0.05 {
        return None;
    }
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    Some(if t / duration >= 0.9 {
        json!({ "Played": true, "PlaybackPositionTicks": 0, "LastPlayedDate": now })
    } else {
        json!({ "PlaybackPositionTicks": (t * 1e7) as i64, "LastPlayedDate": now })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prowlarr_results_are_judged_like_sonarr() {
        let profile = json!({
            "minFormatScore": 100,
            "items": [
                { "quality": { "name": "WEBDL-720p" }, "allowed": false },
                { "name": "WEB 1080p", "allowed": true, "items": [{ "quality": { "name": "WEBRip-1080p" }, "allowed": true }, { "quality": { "name": "WEBDL-1080p" }, "allowed": true }] },
            ],
            "formatItems": [{ "format": 7, "score": 100 }, { "format": 8, "score": 10 }],
        });
        let w = Wanted {
            series_id: 5,
            names: vec![norm("The Legend of Vox Machina"), norm("A Lenda de Vox Machina")],
            original: Some("English".into()),
            episode_id: 333,
            season: 3,
            episode: 2,
        };
        let r = |t: &str| json!({ "title": t, "guid": t, "seeders": 9, "size": 1 });
        let parsed = |series: Value, season: i64, eps: Value, pack: bool, q: &str, cfs: Value| json!({
            "series": series, "episodes": [],
            "parsedEpisodeInfo": { "seasonNumber": season, "episodeNumbers": eps, "fullSeason": pack, "quality": { "quality": { "name": q } }, "languages": [] },
            "customFormats": cfs,
        });
        // nome em português (o Sonarr não reconhece a série), dublado: aceito
        let dub = judge(&r("A Lenda de Vox Machina S03E01-02-03 WEB-DL 1080p DUAL"), &parsed(Value::Null, 3, json!([1, 2, 3]), false, "WEBDL-1080p", json!([{ "id": 7 }])), &profile, &w, None).unwrap();
        assert_eq!(dub["rejected"], false);
        // em inglês, sem dublagem: recusado pelos formatos (a página oferece o legendado)
        let eng = judge(&r("The.Legend.of.Vox.Machina.S03E02.1080p.WEB"), &parsed(json!({ "id": 5 }), 3, json!([2]), false, "WEBDL-1080p", json!([])), &profile, &w, None).unwrap();
        assert!(eng["rejections"][0].as_str().unwrap().contains("Custom Formats"));
        // qualidade fora do perfil
        let low = judge(&r("The.Legend.of.Vox.Machina.S03E02.720p.WEB"), &parsed(json!({ "id": 5 }), 3, json!([2]), false, "WEBDL-720p", json!([{ "id": 7 }])), &profile, &w, None).unwrap();
        assert!(low["rejections"][0].as_str().unwrap().starts_with("Qualidade"));
        // pacote da temporada serve; outro episódio, outra temporada ou outra série, não
        assert!(judge(&r("The.Legend.of.Vox.Machina.S03.1080p"), &parsed(json!({ "id": 5 }), 3, json!([]), true, "WEBDL-1080p", json!([])), &profile, &w, None).is_some());
        assert!(judge(&r("The.Legend.of.Vox.Machina.S03E05"), &parsed(json!({ "id": 5 }), 3, json!([5]), false, "WEBDL-1080p", json!([])), &profile, &w, None).is_none());
        assert!(judge(&r("The.Legend.of.Vox.Machina.S02E02"), &parsed(json!({ "id": 5 }), 2, json!([2]), false, "WEBDL-1080p", json!([])), &profile, &w, None).is_none());
        assert!(judge(&r("Outra.Serie.S03E02"), &parsed(json!({ "id": 9 }), 3, json!([2]), false, "WEBDL-1080p", json!([])), &profile, &w, None).is_none());
        assert!(judge(&r("Outra Serie S03E02"), &parsed(Value::Null, 3, json!([2]), false, "WEBDL-1080p", json!([])), &profile, &w, None).is_none());
        assert_eq!(series_name("Doctor Who (2005)"), "Doctor Who");

        // perfil sem dublagem: mínimo no idioma original (formato 9, por nome)
        let mut fallback = profile.clone();
        fallback["minFormatScore"] = json!(50);
        fallback["formatItems"] = json!([{ "format": 7, "score": 100 }, { "format": 8, "score": 10 }, { "format": 9, "name": crate::prefs::ORIGINAL_FORMAT, "score": 50 }]);
        let with_langs = |series: Value, langs: &[&str], cfs: Value| {
            let mut p = parsed(series, 3, json!([2]), false, "WEBDL-1080p", cfs);
            p["parsedEpisodeInfo"]["languages"] = langs.iter().map(|l| json!({ "name": l })).collect();
            p
        };
        // nome em português, legendado, sem idioma no nome: o Sonarr não marca o original, o Anubis marca
        let br = judge(&r("A Lenda de Vox Machina S03E02 LEGENDADO"), &with_langs(Value::Null, &["Unknown"], json!([{ "id": 8 }])), &fallback, &w, None).unwrap();
        assert_eq!((br["rejected"].as_bool(), br["customFormatScore"].as_i64()), (Some(false), Some(60)));
        // dublado em espanhol, com legenda em português: terceiro idioma, recusado
        let es = judge(&r("A Lenda de Vox Machina S03E02 Latino LEGENDADO"), &with_langs(Value::Null, &["Spanish"], json!([{ "id": 8 }])), &fallback, &w, None).unwrap();
        assert_eq!(es["rejected"], true);
        // série reconhecida: vale só o que o Sonarr marcou
        let fr = judge(&r("The.Legend.of.Vox.Machina.S03E02.FRENCH"), &with_langs(json!({ "id": 5 }), &["French"], json!([])), &fallback, &w, None).unwrap();
        assert_eq!(fr["rejected"], true);
        let en = judge(&r("The.Legend.of.Vox.Machina.S03E02"), &with_langs(json!({ "id": 5 }), &["Unknown"], json!([{ "id": 9 }])), &fallback, &w, None).unwrap();
        assert_eq!(en["rejected"], false);
    }

    #[test]
    fn romaji_only_on_foreign_indexers() {
        let list = json!([
            { "id": 2, "name": "Nyaa.si", "language": "en-US", "enable": true, "protocol": "torrent" },
            { "id": 3, "name": "Comando Torrents (BR)", "language": "pt-BR", "enable": true, "protocol": "torrent" },
            { "id": 6, "name": "bitmagnet (DHT)", "language": "en-US", "enable": false, "protocol": "torrent" },
            { "id": 11, "name": "NZB", "language": "en-US", "enable": true, "protocol": "usenet" },
        ]);
        assert_eq!(foreign_indexers(&list), [2]);
    }

    #[test]
    fn real_tracks_decide_dub_and_subtitles() {
        let profile = json!({
            "minFormatScore": 100,
            "items": [{ "quality": { "name": "WEBDL-1080p" }, "allowed": true }],
            "formatItems": [
                { "format": 1, "name": crate::prefs::FORMAT, "score": 100 },
                { "format": 2, "name": crate::prefs::ORIGINAL_FORMAT, "score": 0 },
                { "format": 3, "name": crate::prefs::SUBS_FORMAT, "score": 10 },
            ],
        });
        let w = Wanted { series_id: 16, names: vec![norm("Sousou no Frieren")], original: Some("Japanese".into()), episode_id: 7, season: 2, episode: 5 };
        let r = |t: &str| json!({ "title": t, "guid": t, "seeders": 9, "size": 1 });
        // o Sonarr leu "MULTi" e "Multi-Subs": sem idioma, legenda PT pela regex
        let parsed = json!({ "series": { "id": 16 }, "episodes": [{ "id": 7 }], "customFormats": [{ "id": 2 }, { "id": 3 }],
            "parsedEpisodeInfo": { "seasonNumber": 2, "episodeNumbers": [5], "quality": { "quality": { "name": "WEBDL-1080p" } }, "languages": [{ "name": "Unknown" }] } });
        let tracks = |audio: &[&str], subs: &[&str]| Tracks { audio: audio.iter().map(|s| s.to_string()).collect(), subs: subs.iter().map(|s| s.to_string()).collect() };
        let title = "[ToonsHub] Frieren S02E05 1080p CR WEB-DL MULTi (Multi-Audio, Multi-Subs)";
        // pelo nome: não é dublado (dublagem obrigatória recusa)
        assert_eq!(judge(&r(title), &parsed, &profile, &w, None).unwrap()["rejected"], true);
        // as faixas têm português: dublado, aprovado e na frente
        let pt = judge(&r(title), &parsed, &profile, &w, Some(&tracks(&["Japanese", "Portuguese (BR)"], &["English", "Portuguese (BR)"]))).unwrap();
        assert_eq!((pt["rejected"].as_bool(), pt["customFormatScore"].as_i64()), (Some(false), Some(110)));
        assert!(candidates(vec![json!({ "protocol": "torrent", "seeders": 9, "languages": pt["languages"].clone() })])[0].portuguese());
        // "Multi-Subs" sem legenda em português: a legenda não conta
        let hi = judge(&r(title), &parsed, &profile, &w, Some(&tracks(&["Japanese", "English", "Hindi"], &["English", "Spanish"]))).unwrap();
        assert_eq!((hi["rejected"].as_bool(), hi["customFormatScore"].as_i64()), (Some(true), Some(0)));
    }

    #[test]
    fn without_portuguese_only_the_original_language() {
        let rel = |title: &str, langs: &[&str]| json!({ "protocol": "torrent", "guid": title, "title": title, "size": 1, "seeders": 5, "rejected": true, "languages": langs.iter().map(|l| json!({ "name": l })).collect::<Vec<_>>() });
        let all = || candidates(vec![
            rel("Frieren.S01E01.English.Dub", &["English"]),
            rel("[SubsPlease] Frieren - 01", &["Unknown"]),
            rel("Frieren.S01E01.JAP", &["Japanese"]),
            rel("Frieren.S01E01.DUAL", &["Portuguese (Brazil)", "Japanese"]),
            rel("Frieren.S01E01.Latino", &["Spanish"]),
        ]);
        let titles = |l: &[Candidate]| l.iter().map(|c| c.title.clone()).collect::<Vec<_>>();
        let mut anime = all();
        original_or_portuguese(&mut anime, Some("Japanese"));
        assert_eq!(titles(&anime), ["Frieren.S01E01.DUAL", "[SubsPlease] Frieren - 01", "Frieren.S01E01.JAP"]);
        // sem saber o idioma original, nada sai
        let mut unknown = all();
        original_or_portuguese(&mut unknown, None);
        assert_eq!(unknown.len(), 5);
        assert_eq!(original_language(&json!({ "originalLanguage": { "id": 8, "name": "Japanese" } })), Some("Japanese"));
        assert_eq!(original_language(&json!({ "originalLanguage": { "id": 0, "name": "Unknown" } })), None);
    }

    #[test]
    fn qbittorrent_folder_of_the_torrent() {
        assert_eq!(torrent_root(&["Filme (2026)/Filme.mkv", "Filme (2026)/Subs/pt.srt"]), Some("Filme (2026)"));
        assert_eq!(torrent_root(&["Filme.2026.mkv"]), None);
        assert_eq!(torrent_root(&["A/x.mkv", "B/y.mkv"]), None);
    }

    #[test]
    fn subtitles_of_this_video_only() {
        let p = |x: &str| PathBuf::from(x);
        // filme: ao lado e numa pasta Subs
        let movie = [p("F/Filme.mkv"), p("F/Filme.pt-BR.srt"), p("F/Subs/2_Portuguese.srt"), p("F/sample.txt")];
        assert_eq!(subs_for(&movie, &movie[0], None), [1, 2]);
        // pacote: só as do episódio 2
        let pack = [
            p("S/Show.S01E01.mkv"),
            p("S/Show.S01E02.mkv"),
            p("S/Subs/Show.S01E01/3_Portuguese.srt"),
            p("S/Subs/Show.S01E02/3_Portuguese.srt"),
            p("S/Show.S01E02.por.srt"),
        ];
        assert_eq!(subs_for(&pack, &pack[1], Some((1, 2))), [3, 4]);
    }

    #[test]
    fn lowercase_base32_magnets_become_uppercase() {
        let m = "magnet:?xt=urn:btih:tjlu5dng3o22xg452s2jbndartkunrw6&dn=Supergirl%2020";
        assert_eq!(normalize_magnet(m), "magnet:?xt=urn:btih:TJLU5DNG3O22XG452S2JBNDARTKUNRW6&dn=Supergirl%2020");
        // hex (40) fica como está
        let hex = "magnet:?xt=urn:btih:1127588e1cb04f16b7cb0f64dcce32edb792d157&dn=x";
        assert_eq!(normalize_magnet(hex), hex);
    }

    #[test]
    fn same_torrent_from_hex_and_base32_magnets() {
        let hex = TorrentSource::Magnet("magnet:?xt=urn:btih:1127588E1CB04F16B7CB0F64DCCE32EDB792D157&dn=a".into());
        let b32 = TorrentSource::Magnet("magnet:?xt=urn:btih:cetvrdq4wbhrnn6lb5snztrs5w3zfukx&dn=b".into());
        assert_eq!(magnet_hash(&hex).as_deref(), Some("1127588e1cb04f16b7cb0f64dcce32edb792d157"));
        assert_eq!(magnet_hash(&hex), magnet_hash(&b32));
        assert_eq!(magnet_hash(&TorrentSource::File(bytes::Bytes::from_static(b"d4:infoe"))), None);
    }

    #[test]
    fn puts_the_series_name_before_the_season() {
        assert_eq!(with_series_name("O Mentalista - S01 Completa (720p) (brazilian)", "The Mentalist").as_deref(), Some("The Mentalist S01 Completa (720p) (brazilian)"));
        assert_eq!(with_series_name("O.Mentalista.S02E05.1080p.WEB-DL.DUAL", "The Mentalist").as_deref(), Some("The Mentalist S02E05.1080p.WEB-DL.DUAL"));
        // "Season" e nomes com S no meio não são marca
        assert_eq!(with_series_name("Supernatural Season 1", "Supernatural"), None);
        assert_eq!(with_series_name("S01E01 sem nome", "X"), None);
    }

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
    fn offers_what_is_above_in_the_queue_only_when_the_existing_file_is_the_only_obstacle() {
        let rel = |title: &str, quality: &str, lang: &str, seeders: i64, why: &[&str]| {
            json!({ "protocol": "torrent", "guid": title, "title": title, "size": 1, "seeders": seeders, "rejected": !why.is_empty(), "rejections": why,
                    "releaseWeight": 1, "languages": [{ "name": lang }], "quality": { "quality": { "name": quality } } })
        };
        let q = |id: i64, name: &str, allowed: bool| json!({ "quality": { "id": id, "name": name }, "items": [], "allowed": allowed });
        // fila (melhor primeiro): Bluray 4K, Bluray 1080p, WEB-DL 1080p (alvo), HDTV 1080p; Remux fora
        let profile = json!({ "cutoff": 3, "items": [q(9, "HDTV-1080p", true), q(3, "WEBDL-1080p", true), q(7, "Bluray-1080p", true), q(30, "Remux-1080p", false), q(19, "Bluray-2160p", true)] });
        let cut = "Existing file on disk meets quality cutoff: WEBDL-1080p";
        let list = candidates(vec![
            rel("BD-DUB", "Bluray-1080p", "Portuguese (Brazil)", 20, &[cut]),
            rel("BD-EN", "Bluray-1080p", "English", 20, &[cut]),
            rel("BD-POUCAS", "Bluray-1080p", "Portuguese (Brazil)", 2, &[cut]),
            rel("BD-SEM-CF", "Bluray-1080p", "Portuguese (Brazil)", 20, &[cut, "Custom Formats have score 0 below Movie's profile minimum 100"]),
            rel("WEB-DUB", "WEBDL-1080p", "Portuguese (Brazil)", 20, &[cut]),
            rel("REMUX-DUB", "Remux-1080p", "Portuguese (Brazil)", 20, &[cut]),
            rel("4K-DUB", "Bluray-2160p", "Portuguese (Brazil)", 20, &[cut]),
        ]);
        let dubbed = Library { path: String::new(), file_id: 1, quality: "WEBDL-1080p".into(), portuguese: true };
        let titles = |v: Vec<Candidate>| v.into_iter().map(|c| c.title).collect::<Vec<_>>();
        // dublado não vira inglês; poucas fontes, outra recusa ou fora da fila saem; o topo da fila primeiro
        assert_eq!(titles(better_than(&dubbed, &list, &profile)), ["4K-DUB", "BD-DUB"]);
        let english = Library { portuguese: false, ..dubbed };
        assert_eq!(titles(better_than(&english, &list, &profile)), ["4K-DUB", "BD-DUB", "BD-EN"]);
        // em Bluray 1080p: só o 4K está acima
        let bluray = Library { path: String::new(), file_id: 1, quality: "Bluray-1080p".into(), portuguese: true };
        assert_eq!(titles(better_than(&bluray, &list, &profile)), ["4K-DUB"]);
        // abaixo do alvo (HDTV): o Radarr troca sozinho, nada a oferecer
        let hdtv = Library { path: String::new(), file_id: 1, quality: "HDTV-1080p".into(), portuguese: true };
        assert!(better_than(&hdtv, &list, &profile).is_empty());
    }

    #[test]
    fn a_well_seeded_version_beats_a_poorly_seeded_one() {
        let rel = |title: &str, seeders: i64, weight: i64| {
            json!({ "protocol": "torrent", "guid": title, "title": title, "size": 1, "seeders": seeders, "rejected": false, "releaseWeight": weight })
        };
        // o Radarr prefere o Bluray (peso 1), mas com 2 fontes ele trava
        let list = candidates(vec![rel("Filme.1080p.BluRay.x264", 2, 1), rel("Filme.1080p.WEB-DL.x264", 40, 2)]);
        assert_eq!(list[0].title, "Filme.1080p.WEB-DL.x264");
    }

    #[test]
    fn dubbed_comes_first_among_the_approved() {
        let rel = |title: &str, rejected: bool, lang: &str| {
            json!({ "protocol": "torrent", "guid": title, "title": title, "size": 1, "seeders": 5, "rejected": rejected, "releaseWeight": 1, "languages": [{ "name": lang }] })
        };
        let list = candidates(vec![
            rel("Filme.1080p.x264-EN", false, "English"),
            rel("Filme.1080p.x265-DUB", false, "Portuguese (Brazil)"),
            rel("Filme.2160p-DUB", true, "Portuguese (Brazil)"),
        ]);
        let order: Vec<_> = list.iter().map(|c| c.title.as_str()).collect();
        // dublada antes mesmo de tocar direto; recusada continua por último
        assert_eq!(order, ["Filme.1080p.x265-DUB", "Filme.1080p.x264-EN", "Filme.2160p-DUB"]);
    }

    #[test]
    fn dht_movie_versions_are_judged_like_radarr() {
        let q = |id: i64, name: &str, allowed: bool| json!({ "quality": { "id": id, "name": name }, "items": [], "allowed": allowed });
        let profile = json!({ "minFormatScore": 100, "items": [q(3, "WEBDL-1080p", true), q(19, "Bluray-2160p", false)],
                              "formatItems": [{ "format": 1, "name": "Anubis · Português (BR)", "score": 100 }, { "format": 2, "name": crate::prefs::ORIGINAL_FORMAT, "score": 50 }] });
        let movie = json!({ "id": 7, "year": 2026, "originalLanguage": { "name": "English" } });
        let names = vec![norm("Supergirl"), norm("Supergirl: Mulher de Aço")];
        let r = |t: &str| json!({ "title": t, "guid": "h", "seeders": 3, "magnetUrl": "magnet:?xt=urn:btih:h" });
        let parsed = |year: i64, langs: &[&str], cfs: &[i64], movie_id: Option<i64>| json!({
            "movie": movie_id.map_or(Value::Null, |id| json!({ "id": id })),
            "customFormats": cfs.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>(),
            "parsedMovieInfo": { "year": year, "languages": langs.iter().map(|l| json!({ "name": l })).collect::<Vec<_>>(), "quality": { "quality": { "name": "WEBDL-1080p" } } }
        });
        // não reconhecido, nome e ano certos, dublado: aprovado
        let v = judge_movie(&r("Supergirl.2026.1080p.WEB-DL.DUAL"), &parsed(2026, &["Portuguese (Brazil)"], &[1], None), &profile, &movie, &names).unwrap();
        assert_eq!(v["rejected"], false);
        // não reconhecido, sem idioma: conta como original (50 < 100): recusado
        let v = judge_movie(&r("Supergirl 2026 1080p WEB-DL"), &parsed(2026, &["Unknown"], &[], None), &profile, &movie, &names).unwrap();
        assert_eq!((v["rejected"].clone(), v["customFormatScore"].clone()), (json!(true), json!(50)));
        // outro ano (o filme de 1984): fora
        assert!(judge_movie(&r("Supergirl 1984 1080p"), &parsed(1984, &["English"], &[], None), &profile, &movie, &names).is_none());
        // reconhecido pelo Radarr como outro filme: fora; como este: dentro
        assert!(judge_movie(&r("Supergirl 2026"), &parsed(2026, &["English"], &[], Some(99)), &profile, &movie, &names).is_none());
        assert!(judge_movie(&r("Supergirl 2026"), &parsed(2026, &["English"], &[], Some(7)), &profile, &movie, &names).is_some());
    }

    #[test]
    fn brazilian_season_names_become_scene_names() {
        assert_eq!(scene_title("O Mentalista - 1ª Temporada Completa (720p) Baixe do Original (brazilian)"), "O Mentalista - S01 Completa (720p) Baixe do Original (brazilian)");
        assert_eq!(scene_title("O Mentalista - 6º Temporada (1080p)"), "O Mentalista - S06 (1080p)");
        assert_eq!(scene_title("Loki Temporada 2 Episódio 5 Dublado"), "Loki S02E05 Dublado");
        assert_eq!(scene_title("Loki 2a Temporada Ep 01"), "Loki S02E01");
        // nada a traduzir: igual (só espaços repetidos viram um)
        assert_eq!(scene_title("Loki.S02E01.1080p.WEB.H264-LAZYCUNTS"), "Loki.S02E01.1080p.WEB.H264-LAZYCUNTS");
        assert_eq!(scene_title("A Temporada das Bruxas 2024"), "A Temporada das Bruxas 2024");
    }

    #[test]
    fn portuguese_comes_first_even_as_a_season_pack_with_few_seeders() {
        let rel = |title: &str, lang: &str, pack: bool, seeders: i64| {
            json!({ "protocol": "torrent", "guid": title, "title": title, "size": 1, "seeders": seeders, "rejected": false, "rejections": [],
                    "releaseWeight": 1, "fullSeason": pack, "languages": [{ "name": lang }], "quality": { "quality": { "name": "WEBDL-1080p" } } })
        };
        let list = candidates(vec![rel("EN-EP", "English", false, 300), rel("PT-PACK", "Portuguese (Brazil)", true, 3)]);
        assert_eq!(list[0].title, "PT-PACK");
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
    fn progress_sent_to_jellyfin() {
        assert_eq!(progress_update(60.0, 3000.0), None, "começo não vira continuar");
        assert_eq!(progress_update(10.0, 0.0), None);
        let mid = progress_update(1500.0, 3000.0).unwrap();
        assert_eq!(mid["PlaybackPositionTicks"], 15_000_000_000_i64);
        assert!(mid.get("Played").is_none(), "no meio não mexe em assistido");
        let end = progress_update(2800.0, 3000.0).unwrap();
        assert_eq!((end["Played"].clone(), end["PlaybackPositionTicks"].clone()), (json!(true), json!(0)));
    }

    #[test]
    fn height_and_audio_tracks_for_the_player_menus() {
        // como o Loki E01: dublagem e original, com uma legenda no meio
        let probe = json!({ "streams": [
            { "codec_type": "video", "codec_name": "hevc", "height": 804 },
            { "codec_type": "audio", "codec_name": "aac", "channels": 6, "tags": { "language": "por", "title": "DDP5.1 BR" } },
            { "codec_type": "subtitle", "codec_name": "subrip" },
            { "codec_type": "audio", "codec_name": "eac3", "channels": 6, "tags": { "language": "eng" } },
        ] });
        let m = classify(&probe, "f.mkv");
        assert_eq!(m.height, Some(804));
        let tracks: Vec<_> = m.audios.iter().map(|a| (a.n, a.lang.as_str(), a.codec.as_str(), a.channels)).collect();
        assert_eq!(tracks, [(0, "por", "aac", 6), (1, "eng", "eac3", 6)]);
        assert_eq!(m.audios[0].title, "DDP5.1 BR");
        assert_eq!(classify(&json!({}), "f.mkv").height, None);
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
