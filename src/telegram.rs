//! Telegram como fonte de livros em português: os canais e grupos aprovados
//! (`[leitura.telegram] canais`), na conta da pessoa (cliente MTProto, não
//! robô: robô não lê canal alheio).
//!
//! O login é feito uma vez, pela linha de comando (`anubis telegram entrar
//! +55…`): o Telegram manda o código para os outros aparelhos da conta, e o
//! comando espera o código no arquivo `state_dir/telegram.codigo` (e a senha
//! da verificação em duas etapas, se houver, em `telegram.senha`). A sessão
//! fica em `state_dir/telegram-sessao.json` (as chaves de autorização: é o
//! que mantém a conta logada; arquivo só do dono).
//!
//! `anubis telegram canais` lista os canais e grupos da conta com quantos
//! livros (EPUB/PDF) cada um tem, para escolher os aprovados;
//! `anubis telegram buscar "título"` mostra o que a cadeia acharia.

use crate::config::TelegramCfg;
use anyhow::{Context, Result, bail};
use grammers_client::media::{Document, Media};
use grammers_client::peer::Peer;
use grammers_client::{Client, SenderPool, SignInError, tl};
use grammers_session::storages::MemorySession;
use grammers_session::types::{DcOption, PeerId, PeerInfo, PeerRef, UpdateState, UpdatesState};
use grammers_session::{BoxFuture, Session, SessionData};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

/// formatos de livro e de HQ que valem a pena baixar
const FORMATOS: &[&str] = &["epub", "azw3", "mobi", "fb2", "pdf", "djvu", "cbz", "cbr"];
/// resultados por busca em cada canal
const POR_CANAL: usize = 30;
/// um pedaço do download que não chega em tanto tempo: a conexão caiu
const OCIOSO: Duration = Duration::from_secs(90);
/// quanto o login espera o código (e a senha)
const ESPERA_CODIGO: Duration = Duration::from_secs(10 * 60);

// ---- sessão gravada em arquivo -------------------------------------------

/// O que precisa sobreviver a um reinício: o DC da conta e as chaves de
/// autorização. Os pares (canais, usuários) se refazem listando os diálogos.
#[derive(Default, Serialize, Deserialize)]
struct Gravado {
    home_dc: Option<i32>,
    dcs: Vec<DcOption>,
}

#[derive(Debug)]
pub struct ErroSessao(String);

impl std::fmt::Display for ErroSessao {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sessão do Telegram: {}", self.0)
    }
}

impl std::error::Error for ErroSessao {}

fn erro(e: impl std::fmt::Display) -> ErroSessao {
    ErroSessao(e.to_string())
}

/// Sessão em memória que grava no arquivo o DC e as chaves quando mudam.
struct Sessao {
    mem: MemorySession,
    path: PathBuf,
    gravado: std::sync::Mutex<Gravado>,
}

impl Sessao {
    fn abrir(path: &Path) -> Result<Self> {
        let gravado: Gravado = match std::fs::read(path) {
            Ok(b) => serde_json::from_slice(&b).with_context(|| format!("lendo {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Gravado::default(),
            Err(e) => return Err(e).with_context(|| format!("lendo {}", path.display())),
        };
        let mut data = SessionData::default();
        if let Some(dc) = gravado.home_dc {
            data.home_dc = dc;
        }
        for dc in &gravado.dcs {
            data.dc_options.insert(dc.id, dc.clone());
        }
        Ok(Self { mem: MemorySession::from(data), path: path.to_path_buf(), gravado: std::sync::Mutex::new(gravado) })
    }

    fn gravar(&self, f: impl FnOnce(&mut Gravado)) -> Result<(), ErroSessao> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut g = self.gravado.lock().map_err(erro)?;
        f(&mut g);
        let bytes = serde_json::to_vec(&*g).map_err(erro)?;
        let tmp = self.path.with_extension("json.tmp");
        let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).map_err(erro)?;
        file.write_all(&bytes).map_err(erro)?;
        std::fs::rename(&tmp, &self.path).map_err(erro)
    }
}

impl Session for Sessao {
    type Error = ErroSessao;

    fn home_dc_id(&self) -> Result<i32, ErroSessao> {
        self.mem.home_dc_id().map_err(erro)
    }

    fn set_home_dc_id(&self, dc_id: i32) -> BoxFuture<'_, Result<(), ErroSessao>> {
        Box::pin(async move {
            self.mem.set_home_dc_id(dc_id).await.map_err(erro)?;
            self.gravar(|g| g.home_dc = Some(dc_id))
        })
    }

    fn dc_option(&self, dc_id: i32) -> Result<Option<DcOption>, ErroSessao> {
        self.mem.dc_option(dc_id).map_err(erro)
    }

    fn set_dc_option(&self, dc_option: &DcOption) -> BoxFuture<'_, Result<(), ErroSessao>> {
        let dc = dc_option.clone();
        Box::pin(async move {
            self.mem.set_dc_option(&dc).await.map_err(erro)?;
            self.gravar(|g| {
                g.dcs.retain(|d| d.id != dc.id);
                g.dcs.push(dc);
            })
        })
    }

    fn peer(&self, peer: PeerId) -> BoxFuture<'_, Result<Option<PeerInfo>, ErroSessao>> {
        Box::pin(async move { self.mem.peer(peer).await.map_err(erro) })
    }

    fn cache_peer(&self, peer: &PeerInfo) -> BoxFuture<'_, Result<(), ErroSessao>> {
        let peer = peer.clone();
        Box::pin(async move { self.mem.cache_peer(&peer).await.map_err(erro) })
    }

    fn updates_state(&self) -> BoxFuture<'_, Result<UpdatesState, ErroSessao>> {
        Box::pin(async move { self.mem.updates_state().await.map_err(erro) })
    }

    fn set_update_state(&self, update: UpdateState) -> BoxFuture<'_, Result<(), ErroSessao>> {
        Box::pin(async move { self.mem.set_update_state(update).await.map_err(erro) })
    }
}

// ---- cliente --------------------------------------------------------------

/// Um arquivo achado num canal.
#[derive(Clone)]
pub struct Achado {
    /// o canal (o título)
    pub canal: String,
    /// "telegram:<canal>:<mensagem>": não volta depois de reprovado
    pub chave: String,
    /// nome do arquivo
    pub nome: String,
    pub ext: String,
    pub tamanho: u64,
    /// a legenda da mensagem (costuma ter título e autor)
    pub texto: String,
    doc: Document,
}

#[derive(Clone)]
struct Canal {
    nome: String,
    peer: PeerRef,
}

struct Conexao {
    client: Client,
    canais: Vec<Canal>,
}

pub struct Telegram {
    api_id: i32,
    hash: String,
    /// como estão na configuração
    aprovados: Vec<String>,
    sessao: PathBuf,
    conn: Mutex<Option<Conexao>>,
}

/// Minúsculas, sem acento, só letras e números (para casar títulos de canal).
fn chave(s: &str) -> String {
    crate::gosto::fold(s).chars().filter(|c| c.is_alphanumeric()).collect()
}

fn titulo(peer: &Peer) -> String {
    peer.name().unwrap_or_default().to_string()
}

impl Telegram {
    pub fn new(cfg: &TelegramCfg, state_dir: &Path) -> Result<Self> {
        let hash = std::fs::read_to_string(&cfg.hash_file).with_context(|| format!("lendo {}", cfg.hash_file.display()))?.trim().to_string();
        if hash.len() != 32 {
            bail!("{}: o api_hash tem 32 caracteres", cfg.hash_file.display());
        }
        Ok(Self { api_id: cfg.api_id, hash, aprovados: cfg.canais.clone(), sessao: state_dir.join("telegram-sessao.json"), conn: Mutex::new(None) })
    }

    fn conectar(&self) -> Result<Client> {
        let sessao = Arc::new(Sessao::abrir(&self.sessao)?);
        let SenderPool { runner, handle, .. } = SenderPool::new(sessao, self.api_id);
        tokio::spawn(runner.run());
        Ok(Client::new(handle))
    }

    /// O cliente logado e os canais aprovados (resolvidos pelos diálogos da conta).
    async fn conexao(&self) -> Result<(Client, Vec<Canal>)> {
        let mut conn = self.conn.lock().await;
        if let Some(c) = conn.as_ref() {
            return Ok((c.client.clone(), c.canais.clone()));
        }
        let client = self.conectar()?;
        if !client.is_authorized().await? {
            client.disconnect();
            bail!("Telegram sem login (rode `anubis telegram entrar +55…` no FeuVault)");
        }
        let mut canais = vec![];
        let mut dialogs = client.iter_dialogs();
        while let Some(d) = dialogs.next().await? {
            let peer = d.peer();
            if matches!(peer, Peer::User(_)) {
                continue;
            }
            let nome = titulo(peer);
            let user = peer.username().map(|u| u.to_lowercase());
            let aprovado = self.aprovados.iter().any(|a| match a.strip_prefix('@') {
                Some(u) => user.as_deref() == Some(&u.to_lowercase()),
                None => chave(a) == chave(&nome),
            });
            if aprovado {
                canais.push(Canal { nome, peer: d.peer_ref() });
            }
        }
        if canais.len() < self.aprovados.len() {
            tracing::warn!("telegram: {} de {} canais aprovados não estão na conta", self.aprovados.len() - canais.len(), self.aprovados.len());
        }
        *conn = Some(Conexao { client: client.clone(), canais: canais.clone() });
        Ok((client, canais))
    }

    /// Erro de rede ou de sessão: a próxima chamada conecta de novo.
    async fn largar(&self) {
        if let Some(c) = self.conn.lock().await.take() {
            c.client.disconnect();
        }
    }

    /// Arquivos de livro com esse texto nos canais aprovados.
    pub async fn buscar(&self, consulta: &str) -> Result<Vec<Achado>> {
        let res = self.buscar_em(consulta).await;
        if res.is_err() {
            self.largar().await;
        }
        res
    }

    async fn buscar_em(&self, consulta: &str) -> Result<Vec<Achado>> {
        let (client, canais) = self.conexao().await?;
        let mut out = vec![];
        for canal in &canais {
            let mut it = client
                .search_messages(canal.peer)
                .query(consulta)
                .filter(tl::enums::MessagesFilter::InputMessagesFilterDocument)
                .limit(POR_CANAL);
            while let Some(m) = it.next().await? {
                let Some(Media::Document(doc)) = m.media() else { continue };
                let nome = doc.name().unwrap_or_default().to_string();
                let ext = nome.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default();
                if !FORMATOS.contains(&ext.as_str()) {
                    continue;
                }
                out.push(Achado {
                    canal: canal.nome.clone(),
                    chave: format!("telegram:{}:{}", canal.peer.id.bot_api_dialog_id().unwrap_or_default(), m.id()),
                    tamanho: doc.size().unwrap_or(0) as u64,
                    texto: m.text().to_string(),
                    nome,
                    ext,
                    doc,
                });
            }
        }
        Ok(out)
    }

    /// Baixa o arquivo; `progresso(feito, total)` a cada pedaço.
    pub async fn baixar(&self, a: &Achado, dest: &Path, progresso: impl Fn(u64, u64)) -> Result<()> {
        let res = self.baixar_em(a, dest, progresso).await;
        if res.is_err() {
            self.largar().await;
            let _ = tokio::fs::remove_file(dest).await;
        }
        res
    }

    async fn baixar_em(&self, a: &Achado, dest: &Path, progresso: impl Fn(u64, u64)) -> Result<()> {
        let (client, _) = self.conexao().await?;
        let mut file = tokio::fs::File::create(dest).await?;
        let mut it = client.iter_download(&a.doc);
        let mut feito = 0u64;
        loop {
            let pedaco = tokio::time::timeout(OCIOSO, it.next()).await.context("o Telegram parou de mandar o arquivo")??;
            let Some(pedaco) = pedaco else { break };
            file.write_all(&pedaco).await?;
            feito += pedaco.len() as u64;
            progresso(feito, a.tamanho);
        }
        file.flush().await?;
        if a.tamanho > 0 && feito != a.tamanho {
            bail!("veio {feito} de {} bytes", a.tamanho);
        }
        Ok(())
    }
}

// ---- linha de comando -----------------------------------------------------

/// Espera um arquivo aparecer (o código, a senha), lê e apaga.
async fn esperar_arquivo(path: &Path) -> Result<String> {
    let fim = tokio::time::Instant::now() + ESPERA_CODIGO;
    loop {
        if let Ok(s) = tokio::fs::read_to_string(path).await {
            let _ = tokio::fs::remove_file(path).await;
            let s = s.trim().to_string();
            if !s.is_empty() {
                return Ok(s);
            }
        }
        if tokio::time::Instant::now() > fim {
            bail!("{} não apareceu em {} min", path.display(), ESPERA_CODIGO.as_secs() / 60);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// `anubis telegram entrar|canais|buscar …`
pub async fn cli(cfg: &TelegramCfg, state_dir: &Path, args: &[String]) -> Result<()> {
    let tg = Telegram::new(cfg, state_dir)?;
    match args.first().map(String::as_str) {
        Some("entrar") => {
            let telefone = args.get(1).context("uso: anubis telegram entrar +55DDNÚMERO")?;
            let client = tg.conectar()?;
            if client.is_authorized().await? {
                println!("já está logado");
                return Ok(());
            }
            let token = client.request_login_code(telefone, &tg.hash).await?;
            let codigo = state_dir.join("telegram.codigo");
            let _ = tokio::fs::remove_file(&codigo).await;
            println!("código pedido: grave-o em {}", codigo.display());
            let user = match client.sign_in(&token, &esperar_arquivo(&codigo).await?).await {
                Ok(u) => u,
                Err(SignInError::PasswordRequired(pt)) => {
                    let senha = state_dir.join("telegram.senha");
                    println!("a conta tem verificação em duas etapas (dica: {}): grave a senha em {}", pt.hint().unwrap_or("nenhuma"), senha.display());
                    client.check_password(pt, esperar_arquivo(&senha).await?).await?
                }
                Err(e) => bail!("login: {e}"),
            };
            println!("logado como {}", user.full_name());
        }
        Some("canais") => {
            let client = tg.conectar()?;
            anyhow::ensure!(client.is_authorized().await?, "sem login: anubis telegram entrar +55…");
            let mut dialogs = client.iter_dialogs();
            println!("livros\tepub\tpdf\taprovado\tcanal");
            while let Some(d) = dialogs.next().await? {
                let peer = d.peer();
                if matches!(peer, Peer::User(_)) {
                    continue;
                }
                let conta = |q: &'static str| {
                    let it = client.search_messages(d.peer_ref()).filter(tl::enums::MessagesFilter::InputMessagesFilterDocument);
                    async move { if q.is_empty() { it } else { it.query(q) }.total().await.unwrap_or(0) }
                };
                let (epub, pdf) = (conta("epub").await, conta("pdf").await);
                let nome = titulo(peer);
                let user = peer.username().map(|u| format!(" @{u}")).unwrap_or_default();
                let aprovado = cfg.canais.iter().any(|a| a.strip_prefix('@').map_or(chave(a) == chave(&nome), |u| user.trim().eq_ignore_ascii_case(&format!("@{u}"))));
                println!("{}\t{epub}\t{pdf}\t{}\t{nome}{user}", conta("").await, if aprovado { "sim" } else { "" });
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
        Some("buscar") => {
            let q = args.get(1).context("uso: anubis telegram buscar \"título\"")?;
            for a in tg.buscar(q).await? {
                println!("{}\t{:.1} MB\t{}\t{}", a.canal, a.tamanho as f64 / 1e6, a.nome, a.texto.lines().next().unwrap_or(""));
            }
        }
        _ => bail!("uso: anubis telegram entrar +55… | canais | buscar \"título\""),
    }
    Ok(())
}
