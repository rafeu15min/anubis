//! Cadeia de download da leitura → Drive. Fontes, nesta ordem: Library
//! Genesis (download direto, idioma informado, sem conta), Bookshelf (o *arr
//! dos livros) e torrent direto (busca do Anubis no Prowlarr, qBittorrent).
//!
//! Português primeiro: o pedido só procura versões em português (LibGen em
//! português, torrents marcados PT-BR), conferidas pelo idioma do texto. Sem
//! nenhuma, para em "sem versão em português" e só baixa em inglês com o
//! clique (`POST /leitura/pedidos/{id}/ingles`): LibGen → Bookshelf → torrent.
//!
//! Cada arquivo passa pelo validador (`quality`) antes de ir para o Drive;
//! versão ruim é descartada e a próxima é tentada. O torrent continua
//! semeando no qBittorrent: o Drive recebe uma cópia. Pedidos em
//! `state_dir/leitura-pedidos.json`, retomados num reinício.
//!
//! O Z-Library (3ª fonte, só com clique: créditos limitados) entra quando
//! houver conta; até lá, sem torrent bom = "sem fonte".

use crate::libgen::LibGen;
use crate::orchestrator::Orchestrator;
use crate::quality::{self, Kind};
use crate::services::TorrentSource;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// onde o qBittorrent grava os torrents diretos (caminho dos contêineres)
const TORRENT_DIR: &str = "/data/downloads/leitura";
const CATEGORY: &str = "leitura";
/// sem nada na fila do Bookshelf depois da busca: passa ao torrent direto
const BOOKSHELF_GRACE: Duration = Duration::from_secs(4 * 60);
/// torrent parado (sem progresso) por tanto tempo: tenta a próxima versão
const STALL: Duration = Duration::from_secs(20 * 60);
const MAX_TORRENTS: usize = 3;
/// LibGen ocupado com a versão em português: tenta de novo a cada…
const LIBGEN_RETRY: Duration = Duration::from_secs(30 * 60);
/// …por até 6 h (12 vezes)
const LIBGEN_RETRIES: u32 = 12;
/// um download direto que caiu no meio continua de onde parou (até tantas vezes)
const RESUMES: u32 = 5;

/// Resultado de uma fonte.
enum Outcome {
    Delivered,
    /// não havia versão que servisse
    Nothing,
    /// havia, mas o servidor estava ocupado: vale tentar mais tarde
    Busy,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Procurando,
    Baixando,
    Conferindo,
    EnviandoAoDrive,
    NoDrive,
    /// nada em português; espera o clique para aceitar inglês
    SemPortugues,
    SemFonte,
    Falhou,
}

impl Stage {
    fn done(self) -> bool {
        matches!(self, Stage::NoDrive | Stage::SemPortugues | Stage::SemFonte | Stage::Falhou)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub title: String,
    pub author: Option<String>,
    /// título original (os indexadores são quase todos em inglês)
    #[serde(default)]
    pub alt_title: Option<String>,
    /// pasta no Drive (a categoria)
    pub folder: String,
    pub stage: Stage,
    /// o que está acontecendo, em português
    pub detail: String,
    /// 0.0 a 1.0 enquanto baixa
    pub progress: f64,
    pub at: i64,
    /// onde ficou no Drive
    pub drive_path: Option<String>,
    /// versões já tentadas (não voltam)
    #[serde(default)]
    pub tried: Vec<String>,
    /// o usuário aceitou inglês (não havia em português)
    #[serde(default)]
    pub english_ok: bool,
}

pub struct Bookshelf {
    base: String,
    key: String,
    http: reqwest::Client,
    root: String,
}

impl Bookshelf {
    pub fn new(url: &str, key: &str) -> Result<Self> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(120)).build()?;
        Ok(Self { base: url.trim_end_matches('/').into(), key: key.into(), http, root: "/data/media/livros".into() })
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> Result<Value> {
        let mut req = self.http.request(method, format!("{}/api/v1/{path}", self.base)).header("X-Api-Key", &self.key);
        if let Some(b) = body {
            req = req.json(b);
        }
        let res = req.send().await.context("Bookshelf: sem resposta")?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("Bookshelf {path}: HTTP {status}: {}", text.chars().take(200).collect::<String>());
        }
        Ok(serde_json::from_str(if text.trim().is_empty() { "null" } else { &text })?)
    }

    /// Acha o livro (Goodreads), põe no Bookshelf monitorado e manda buscar.
    async fn request(&self, title: &str, author: Option<&str>) -> Result<i64> {
        let term = format!("{title} {}", author.unwrap_or(""));
        // o /search traz o livro com o autor completo (o book/lookup, não)
        let found = self.call(reqwest::Method::GET, &format!("search?term={}", urlencode(term.trim())), None).await?;
        let mut book = found.as_array().into_iter().flatten().find_map(|x| x.get("book").cloned()).context("o Goodreads não conhece esse livro")?;
        let foreign = book["foreignBookId"].clone();
        // já está no Bookshelf: só manda buscar de novo
        let existing = self.call(reqwest::Method::GET, "book", None).await?;
        if let Some(b) = existing.as_array().into_iter().flatten().find(|b| b["foreignBookId"] == foreign) {
            let id = b["id"].as_i64().context("livro sem id")?;
            self.call(reqwest::Method::POST, "command", Some(&json!({ "name": "BookSearch", "bookIds": [id] }))).await?;
            return Ok(id);
        }
        book["monitored"] = json!(true);
        book["addOptions"] = json!({ "searchForNewBook": true });
        let a = &mut book["author"];
        a["qualityProfileId"] = json!(1);
        a["metadataProfileId"] = json!(1);
        a["rootFolderPath"] = json!(self.root);
        a["monitored"] = json!(false);
        a["addOptions"] = json!({ "monitor": "none", "searchForMissingBooks": false });
        let added = self.call(reqwest::Method::POST, "book", Some(&book)).await?;
        added["id"].as_i64().context("o Bookshelf não devolveu o id")
    }

    /// (baixando: progresso) ou (importado: caminho no contêiner).
    async fn state(&self, book_id: i64) -> Result<(Option<f64>, Option<String>)> {
        let files = self.call(reqwest::Method::GET, &format!("bookfile?bookId={book_id}"), None).await?;
        if let Some(path) = files.as_array().and_then(|f| f.first()).and_then(|f| f["path"].as_str()) {
            return Ok((None, Some(path.to_string())));
        }
        let queue = self.call(reqwest::Method::GET, "queue?pageSize=100", None).await?;
        let progress = queue["records"].as_array().into_iter().flatten().find(|r| r["bookId"].as_i64() == Some(book_id)).map(|r| {
            let size = r["size"].as_f64().unwrap_or(0.0);
            if size > 0.0 { 1.0 - r["sizeleft"].as_f64().unwrap_or(size) / size } else { 0.0 }
        });
        Ok((progress, None))
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Minúsculas e sem acento (os nomes de torrent quase nunca têm acento).
fn fold(s: &str) -> String {
    s.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            c => c,
        })
        .collect()
}

/// Palavras de um título que importam para casar com o nome de uma versão.
fn words(s: &str) -> Vec<String> {
    fold(s).split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() >= 3).map(String::from).collect()
}

/// Pontos de uma versão do Prowlarr para o livro (None: não serve). Vale
/// o título ou o título original; nome muito maior que título e autor perde
/// pontos (biografia "… the Man Who Wrote Dracula" não é o Drácula).
pub fn score_release(r: &Value, titles: &[&str], author: Option<&str>, comic: bool) -> Option<i64> {
    let name = fold(r["title"].as_str()?);
    let seeders = r["seeders"].as_i64().unwrap_or(0);
    if seeders < 2 || ["audiobook", "audio book", ".m4b", ".mp3", "audiolivro"].iter().any(|a| name.contains(a)) {
        return None;
    }
    let want = titles.iter().map(|t| words(t)).filter(|w| !w.is_empty()).find(|want| {
        let hits = want.iter().filter(|w| name.contains(w.as_str())).count();
        hits as f64 >= (want.len() as f64 * 0.6).ceil()
    })?;
    let known = want.len() + author.map(|a| words(a).len()).unwrap_or(0);
    let extra = words(&name).len().saturating_sub(known + 6) as i64;
    let pt = is_portuguese(&name);
    let fmt = if comic {
        if name.contains("cbz") || name.contains("cbr") { 30 } else if name.contains("pdf") { 10 } else { 0 }
    } else if name.contains("epub") {
        30
    } else if name.contains("azw3") || name.contains("mobi") {
        20
    } else if name.contains("pdf") {
        10
    } else {
        0
    };
    Some(if pt { 100 } else { 0 } + fmt + seeders.min(50) - extra * 5)
}

/// Nome de versão marcado como português.
pub fn is_portuguese(name: &str) -> bool {
    let name = fold(name);
    ["portugu", "pt-br", "ptbr", "pt_br", "brasil", "nacional", "[por]", "(por)", " por "].iter().any(|m| name.contains(m))
}

/// O arquivo do livro num torrent: extensão de livro, mais palavras do título, maior.
fn pick_file(files: &[crate::services::TorrentFileInfo], title: &str, comic: bool) -> Option<usize> {
    let want = words(title);
    let pref = |name: &str| {
        let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
        let rank = if comic { ["cbz", "cbr", "pdf", "epub"].iter().position(|e| *e == ext) } else { ["epub", "azw3", "mobi", "pdf", "djvu"].iter().position(|e| *e == ext) };
        rank.map(|r| 10 - r as i64)
    };
    files
        .iter()
        .enumerate()
        .filter_map(|(i, f)| {
            let p = pref(&f.name)?;
            let lower = fold(&f.name);
            let hits = want.iter().filter(|w| lower.contains(w.as_str())).count() as i64;
            Some((hits * 100 + p, i))
        })
        .max()
        .map(|(_, i)| i)
}

pub struct Downloads {
    orch: Arc<Orchestrator>,
    libgen: LibGen,
    /// downloads diretos (LibGen) antes de irem para o Drive
    tmp: PathBuf,
    bookshelf: Option<Bookshelf>,
    drive: String,
    path: PathBuf,
    jobs: Mutex<HashMap<String, Job>>,
    /// quando a Drive muda (um livro entrou), a estante relista
    pub on_delivered: Arc<tokio::sync::Notify>,
}

fn kind_of(folder: &str) -> Kind {
    match folder {
        "Mangás e HQs" => Kind::Comic,
        "Artigos" => Kind::Article,
        _ => Kind::Book,
    }
}

impl Downloads {
    pub fn load(orch: Arc<Orchestrator>, bookshelf: Option<Bookshelf>, drive: String, state_dir: &Path) -> Result<Arc<Self>> {
        let path = state_dir.join("leitura-pedidos.json");
        let jobs: HashMap<String, Job> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let tmp = state_dir.join("leitura-tmp");
        std::fs::create_dir_all(&tmp)?;
        let me = Arc::new(Self { orch, libgen: LibGen::new()?, tmp, bookshelf, drive, path, jobs: Mutex::new(jobs), on_delivered: Arc::new(tokio::sync::Notify::new()) });
        let resume = me.clone();
        tokio::spawn(async move {
            let pending: Vec<String> = resume.jobs.lock().await.values().filter(|j| !j.stage.done()).map(|j| j.id.clone()).collect();
            for id in pending {
                resume.clone().spawn(id);
            }
        });
        Ok(me)
    }

    pub async fn list(&self) -> Vec<Job> {
        let mut v: Vec<Job> = self.jobs.lock().await.values().cloned().collect();
        v.sort_by(|a, b| b.at.cmp(&a.at));
        v
    }

    /// Pede um livro (o id é o do pedido; repetir o mesmo título não duplica).
    pub async fn request(self: &Arc<Self>, title: String, alt_title: Option<String>, author: Option<String>, folder: String) -> Job {
        let id = format!("{}:{}", folder, words(&title).join("-"));
        let mut jobs = self.jobs.lock().await;
        if let Some(j) = jobs.get(&id)
            && !matches!(j.stage, Stage::SemFonte | Stage::Falhou | Stage::SemPortugues)
        {
            return j.clone();
        }
        let job = Job {
            id: id.clone(),
            title,
            author,
            alt_title,
            folder,
            stage: Stage::Procurando,
            detail: "Procurando".into(),
            progress: 0.0,
            at: chrono::Utc::now().timestamp(),
            drive_path: None,
            tried: vec![],
            english_ok: false,
        };
        jobs.insert(id.clone(), job.clone());
        drop(jobs);
        self.save().await;
        self.clone().spawn(id);
        job
    }

    /// Não havia em português: baixa em inglês (Bookshelf → torrent).
    pub async fn accept_english(self: &Arc<Self>, id: &str) -> Option<Job> {
        let job = self
            .update(id, |j| {
                j.english_ok = true;
                j.stage = Stage::Procurando;
                j.detail = "Procurando em inglês".into();
            })
            .await?;
        self.clone().spawn(id.to_string());
        Some(job)
    }

    fn spawn(self: Arc<Self>, id: String) {
        tokio::spawn(async move {
            if let Err(e) = self.run(&id).await {
                tracing::warn!("leitura: pedido {id}: {e:#}");
                self.update(&id, |j| {
                    j.stage = Stage::Falhou;
                    j.detail = format!("Falhou: {e:#}");
                })
                .await;
            }
        });
    }

    async fn save(&self) {
        let jobs = self.jobs.lock().await;
        if let Ok(bytes) = serde_json::to_vec(&*jobs) {
            let tmp = self.path.with_extension("json.tmp");
            if tokio::fs::write(&tmp, bytes).await.is_ok() {
                let _ = tokio::fs::rename(&tmp, &self.path).await;
            }
        }
    }

    async fn update(&self, id: &str, f: impl FnOnce(&mut Job)) -> Option<Job> {
        let job = {
            let mut jobs = self.jobs.lock().await;
            let j = jobs.get_mut(id)?;
            f(j);
            j.clone()
        };
        self.save().await;
        Some(job)
    }

    async fn job(&self, id: &str) -> Option<Job> {
        self.jobs.lock().await.get(id).cloned()
    }

    async fn run(&self, id: &str) -> Result<()> {
        let job = self.job(id).await.context("pedido sumiu")?;
        let kind = kind_of(&job.folder);
        // português primeiro: LibGen em português, depois torrents marcados
        // PT, conferidos pelo texto
        if !job.english_ok {
            for attempt in 0..=LIBGEN_RETRIES {
                match self.via_libgen(id, kind, true).await? {
                    Outcome::Delivered => return Ok(()),
                    Outcome::Busy if attempt < LIBGEN_RETRIES => {
                        let next = chrono::Local::now() + chrono::Duration::from_std(LIBGEN_RETRY).unwrap_or_default();
                        self.update(id, |j| {
                            j.stage = Stage::Procurando;
                            j.detail = format!("Há versão em português, mas o LibGen está sobrecarregado; tento de novo às {}", next.format("%H:%M"));
                        })
                        .await;
                        tokio::time::sleep(LIBGEN_RETRY).await;
                    }
                    _ => break,
                }
            }
            if self.via_torrent(id, kind, true).await? {
                return Ok(());
            }
            self.update(id, |j| {
                j.stage = Stage::SemPortugues;
                j.detail = "Nada em português nos torrents".into();
            })
            .await;
            return Ok(());
        }
        // 1. LibGen
        if matches!(self.via_libgen(id, kind, false).await?, Outcome::Delivered) {
            return Ok(());
        }
        // 2. Bookshelf (livros; HQ e artigo não)
        if kind == Kind::Book
            && let Some(bs) = &self.bookshelf
        {
            match self.via_bookshelf(bs, &job).await {
                Ok(Some(file)) => {
                    if self.deliver(id, &file, kind, false).await? {
                        return Ok(());
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::info!("leitura: Bookshelf ({}): {e:#}", job.title),
            }
        }
        // 3. torrent direto
        if self.via_torrent(id, kind, false).await? {
            return Ok(());
        }
        self.update(id, |j| {
            j.stage = Stage::SemFonte;
            j.detail = "Nenhuma versão boa em torrent".into();
        })
        .await;
        Ok(())
    }

    /// O arquivo importado pelo Bookshelf (caminho no host), ou None se ele
    /// não achou nada a tempo.
    async fn via_bookshelf(&self, bs: &Bookshelf, job: &Job) -> Result<Option<PathBuf>> {
        self.update(&job.id, |j| j.detail = "Procurando no Bookshelf".into()).await;
        let book_id = bs.request(&job.title, job.author.as_deref()).await?;
        let started = Instant::now();
        let mut seen_queue = false;
        loop {
            let (progress, file) = bs.state(book_id).await?;
            if let Some(path) = file {
                return Ok(Some(self.orch.cfg.host_path(&path)));
            }
            match progress {
                Some(p) => {
                    seen_queue = true;
                    self.update(&job.id, |j| {
                        j.stage = Stage::Baixando;
                        j.detail = "Baixando pelo Bookshelf".into();
                        j.progress = p;
                    })
                    .await;
                }
                None if seen_queue => {} // terminou de baixar, importando
                None if started.elapsed() > BOOKSHELF_GRACE => return Ok(None),
                None => {}
            }
            if started.elapsed() > Duration::from_secs(3 * 3600) {
                return Ok(None);
            }
            tokio::time::sleep(Duration::from_secs(20)).await;
        }
    }

    /// Títulos para buscar: o do pedido e o original, sem acento, com e sem autor.
    fn queries(job: &Job) -> (Vec<String>, Vec<String>) {
        let author = job.author.as_deref().map(fold).unwrap_or_default();
        let titles: Vec<String> = std::iter::once(job.title.clone()).chain(job.alt_title.clone()).collect();
        let mut queries: Vec<String> = titles.iter().map(|t| format!("{} {author}", fold(t)).trim().to_string()).collect();
        queries.extend(titles.iter().map(|t| fold(t)));
        queries.dedup();
        (titles, queries)
    }

    /// Library Genesis: as melhores versões (até 5), baixadas direto.
    async fn via_libgen(&self, id: &str, kind: Kind, only_pt: bool) -> Result<Outcome> {
        let job = self.job(id).await.context("pedido sumiu")?;
        self.update(id, |j| {
            j.stage = Stage::Procurando;
            j.detail = format!("Procurando no Library Genesis{}", if only_pt { " (em português)" } else { "" });
        })
        .await;
        let (titles, queries) = Self::queries(&job);
        let mut found: Vec<crate::libgen::Entry> = vec![];
        for q in &queries {
            match self.libgen.search(q).await {
                Ok(list) => {
                    for e in list {
                        if !found.iter().any(|f| f.md5 == e.md5) {
                            found.push(e);
                        }
                    }
                }
                Err(e) => tracing::info!("leitura: LibGen ({q}): {e:#}"),
            }
        }
        let comic = kind == Kind::Comic;
        let fmt = |ext: &str| -> Option<i64> {
            let order: &[&str] = if comic { &["cbz", "cbr", "pdf", "epub"] } else { &["epub", "azw3", "mobi", "fb2", "pdf", "djvu"] };
            order.iter().position(|e| *e == ext).map(|i| 30 - i as i64 * 5)
        };
        let mut ranked: Vec<(i64, crate::libgen::Entry)> = found
            .into_iter()
            // versão tentada que deu problema de verdade não volta; a que só
            // pegou o servidor ocupado volta na próxima rodada
            .filter(|e| !job.tried.contains(&format!("libgen:{}", e.md5)))
            .filter_map(|e| {
                let pt = e.language.to_lowercase().contains("portug");
                if only_pt && !pt {
                    return None;
                }
                let name = fold(&e.text);
                let matches = titles.iter().map(|t| words(t)).filter(|w| !w.is_empty()).any(|want| {
                    let hits = want.iter().filter(|w| name.contains(w.as_str())).count();
                    hits as f64 >= (want.len() as f64 * 0.6).ceil()
                });
                let too_big = kind == Kind::Book && e.size > 300_000_000;
                (matches && !too_big).then_some(())?;
                Some((if pt { 100 } else { 0 } + fmt(&e.ext)?, e))
            })
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0));
        let mut busy = false;
        for (_, e) in ranked.into_iter().take(5) {
            self.update(id, |j| {
                j.stage = Stage::Baixando;
                j.detail = format!("Baixando do Library Genesis ({}, {})", e.ext.to_uppercase(), e.language);
                j.progress = 0.0;
            })
            .await;
            // o nome no Drive: "Título (Autor).ext", como o resto da estante
            let name = match &job.author {
                Some(a) => format!("{} ({a}).{}", job.title, e.ext),
                None => format!("{}.{}", job.title, e.ext),
            }
            .replace('/', "-");
            let file = self.tmp.join(&name);
            let _ = tokio::fs::remove_file(&file).await;
            // o progresso vai para o pedido sem esperar (a closure é síncrona)
            let shown = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let watcher = {
                let (shown, me, id) = (shown.clone(), self, id.to_string());
                async move {
                    loop {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        let p = shown.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1000.0;
                        me.update(&id, |j| j.progress = p).await;
                    }
                }
            };
            let fetch = async {
                let mut last = Err(crate::libgen::Error::Busy("não começou".into()));
                for _ in 0..RESUMES {
                    last = self
                        .libgen
                        .download(&e.md5, &file, |done, total| {
                            if total > 0 {
                                shown.store(done * 1000 / total, std::sync::atomic::Ordering::Relaxed);
                            }
                        })
                        .await;
                    match &last {
                        // caiu no meio com parte baixada: continua de onde parou
                        Err(crate::libgen::Error::Busy(_)) if tokio::fs::metadata(&file).await.is_ok_and(|m| m.len() > 0) => continue,
                        _ => break,
                    }
                }
                last
            };
            let res = tokio::select! {
                r = fetch => r,
                _ = watcher => unreachable!(),
            };
            let delivered = match res {
                Ok(()) => {
                    let ok = self.deliver(id, &file, kind, only_pt).await?;
                    if !ok {
                        self.update(id, |j| j.tried.push(format!("libgen:{}", e.md5))).await;
                    }
                    ok
                }
                Err(err @ crate::libgen::Error::Busy(_)) => {
                    tracing::info!("leitura: LibGen {}: {err}", e.md5);
                    busy = true;
                    false
                }
                Err(err) => {
                    tracing::info!("leitura: LibGen {}: {err}", e.md5);
                    self.update(id, |j| j.tried.push(format!("libgen:{}", e.md5))).await;
                    false
                }
            };
            let _ = tokio::fs::remove_file(&file).await;
            if delivered {
                return Ok(Outcome::Delivered);
            }
        }
        Ok(if busy { Outcome::Busy } else { Outcome::Nothing })
    }

    /// Busca no Prowlarr e tenta as melhores versões (até MAX_TORRENTS).
    async fn via_torrent(&self, id: &str, kind: Kind, only_pt: bool) -> Result<bool> {
        let job = self.job(id).await.context("pedido sumiu")?;
        let Some(prowlarr) = &self.orch.prowlarr else { return Ok(false) };
        self.update(id, |j| {
            j.stage = Stage::Procurando;
            j.detail = "Procurando torrents".into();
        })
        .await;
        let comic = kind == Kind::Comic;
        let cats: &[u32] = if comic { &[7030, 7000] } else { &[7000] };
        // sem acento (a busca dos sites não acha "Drácula"), título e original,
        // com e sem autor; junta tudo
        let (titles, queries) = Self::queries(&job);
        let titles: Vec<&str> = titles.iter().map(String::as_str).collect();
        let mut found: Vec<Value> = vec![];
        for q in &queries {
            for r in prowlarr.search(q, cats, 20).await.unwrap_or_default() {
                if !found.iter().any(|f| f["guid"] == r["guid"]) {
                    found.push(r);
                }
            }
        }
        let mut ranked: Vec<(i64, Value)> = found
            .into_iter()
            .filter(|r| !job.tried.iter().any(|t| Some(t.as_str()) == r["guid"].as_str()))
            .filter(|r| !only_pt || r["title"].as_str().is_some_and(is_portuguese))
            .filter_map(|r| score_release(&r, &titles, job.author.as_deref(), comic).map(|s| (s, r)))
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, r) in ranked.into_iter().take(MAX_TORRENTS) {
            let guid = r["guid"].as_str().unwrap_or("").to_string();
            self.update(id, |j| j.tried.push(guid.clone())).await;
            match self.torrent(id, &r, kind).await {
                Ok(Some(file)) if self.deliver(id, &file, kind, only_pt).await? => return Ok(true),
                Ok(_) => {}
                Err(e) => tracing::info!("leitura: torrent {}: {e:#}", r["title"]),
            }
        }
        Ok(false)
    }

    /// Baixa uma versão no qBittorrent (só o arquivo do livro). Some(arquivo no host).
    async fn torrent(&self, id: &str, r: &Value, kind: Kind) -> Result<Option<PathBuf>> {
        let job = self.job(id).await.context("pedido sumiu")?;
        let q = &self.orch.qbit;
        let url = [&r["magnetUrl"], &r["downloadUrl"]].into_iter().find_map(|v| v.as_str()).context("versão sem link")?.to_string();
        let dir = format!("{TORRENT_DIR}/{}", words(&job.title).join("-"));
        self.update(id, |j| {
            j.stage = Stage::Baixando;
            j.detail = format!("Baixando: {}", r["title"].as_str().unwrap_or(""));
            j.progress = 0.0;
        })
        .await;
        q.add_seeding(&TorrentSource::Magnet(url), &dir, CATEGORY, None, None).await?;
        // o hash: o que o Prowlarr informa, ou o torrent novo nessa pasta
        let mut hash = r["infoHash"].as_str().map(|h| h.to_lowercase());
        for _ in 0..30 {
            if hash.is_some() {
                break;
            }
            hash = q.torrents().await?.into_iter().find(|t| t.save_path.trim_end_matches('/') == dir).map(|t| t.hash);
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let hash = hash.context("o qBittorrent não mostrou o torrent")?;
        // de um pacote, só o arquivo do livro
        let mut files = vec![];
        for _ in 0..90 {
            files = q.files(&hash).await.unwrap_or_default();
            if !files.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let keep = pick_file(&files, &job.title, kind == Kind::Comic).or_else(|| job.alt_title.as_deref().and_then(|t| pick_file(&files, t, kind == Kind::Comic)));
        let Some(keep) = keep else {
            q.delete(std::slice::from_ref(&hash)).await.ok();
            return Ok(None);
        };
        let others: Vec<usize> = (0..files.len()).filter(|i| *i != keep).collect();
        if !others.is_empty() {
            q.set_file_priority(&hash, &others, 0).await?;
        }
        let mut last = (0.0, Instant::now());
        loop {
            let t = q.by_hashes(std::slice::from_ref(&hash)).await?.into_iter().next().context("o torrent sumiu do qBittorrent")?;
            let file_progress = if others.is_empty() { t.progress } else { t.progress.max(0.0) };
            self.update(id, |j| j.progress = file_progress).await;
            if t.progress >= 0.999 {
                return Ok(Some(self.orch.cfg.host_path(&format!("{}/{}", t.save_path.trim_end_matches('/'), files[keep].name))));
            }
            if t.progress > last.0 {
                last = (t.progress, Instant::now());
            } else if last.1.elapsed() > STALL {
                q.delete(std::slice::from_ref(&hash)).await.ok();
                return Ok(None);
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }

    /// Confere e copia para o Drive. false: o arquivo não passou (tenta outra versão).
    async fn deliver(&self, id: &str, file: &Path, kind: Kind, only_pt: bool) -> Result<bool> {
        self.update(id, |j| {
            j.stage = Stage::Conferindo;
            j.detail = "Conferindo o arquivo".into();
        })
        .await;
        let mut v = quality::check(file, kind).await;
        // marcado PT mas o texto é inglês: não serve na passada do português
        if only_pt && v.language == Some("en") {
            v.problem = Some("o texto está em inglês".into());
        }
        if let Some(problem) = &v.problem {
            tracing::info!("leitura: {} reprovado: {problem}", file.display());
            self.update(id, |j| j.detail = format!("Versão descartada: {problem}")).await;
            return Ok(false);
        }
        let job = self.job(id).await.context("pedido sumiu")?;
        let name = file.file_name().and_then(|n| n.to_str()).context("arquivo sem nome")?;
        let dest = format!("{}/{}", job.folder, name);
        self.update(id, |j| {
            j.stage = Stage::EnviandoAoDrive;
            j.detail = "Enviando ao Drive".into();
        })
        .await;
        let out = tokio::process::Command::new("rclone")
            .args(["copyto", &file.to_string_lossy(), &format!("{}/{dest}", self.drive)])
            .output()
            .await
            .context("rodando o rclone")?;
        if !out.status.success() {
            bail!("rclone copyto: {}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or(""));
        }
        self.update(id, |j| {
            j.stage = Stage::NoDrive;
            j.detail = format!("No Drive{}", v.language.map(|l| if l == "pt" { " (em português)" } else { " (em inglês)" }).unwrap_or(""));
            j.progress = 1.0;
            j.drive_path = Some(dest.clone());
        })
        .await;
        self.on_delivered.notify_waiters();
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(title: &str, seeders: i64) -> Value {
        json!({ "title": title, "seeders": seeders })
    }

    #[test]
    fn versao_em_portugues_e_epub_vem_antes() {
        let t = ["A Hipótese do Amor", "The Love Hypothesis"];
        let a = Some("Ali Hazelwood");
        let pt = score_release(&rel("A Hipotese do Amor - Ali Hazelwood [PT-BR] epub", 5), &t, a, false).unwrap();
        let en = score_release(&rel("The Love Hypothesis - Ali Hazelwood epub", 40), &t, a, false).unwrap();
        assert!(pt > en, "{pt} {en}");
        assert!(score_release(&rel("A Hipotese do Amor audiobook m4b", 90), &t, a, false).is_none());
        assert!(score_release(&rel("Outro livro qualquer epub", 90), &t, a, false).is_none());
        assert!(score_release(&rel("A Hipotese do Amor epub", 1), &t, a, false).is_none());
        // a biografia longa perde para o livro
        let d = ["Drácula"];
        let book = score_release(&rel("Dracula - Bram Stoker [EN EPUB MOBI] [ebook] [ps]", 13), &d, Some("Bram Stoker"), false).unwrap();
        let bio = score_release(&rel("Something in the Blood The Untold Story of Bram Stoker the Man Who Wrote Dracula", 12), &d, Some("Bram Stoker"), false).unwrap();
        assert!(book > bio, "{book} {bio}");
    }

    #[test]
    fn de_um_pacote_escolhe_o_livro() {
        let f = |n: &str| crate::services::TorrentFileInfo { name: n.into() };
        let files = vec![f("Pack/Mistborn 1 - O Imperio Final.epub"), f("Pack/Mistborn 1 - O Imperio Final.pdf"), f("Pack/Elantris.epub"), f("Pack/capa.jpg")];
        assert_eq!(pick_file(&files, "Mistborn O Império Final", false), Some(0));
        assert_eq!(pick_file(&[f("capa.jpg")], "x", false), None);
    }
}
