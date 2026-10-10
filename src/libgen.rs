//! Library Genesis (libgen.li): livros e HQs para baixar direto, sem conta
//! nem créditos, com o idioma de cada arquivo informado. Na cadeia da
//! leitura é a fonte de português (os torrents quase não têm PT-BR).
//!
//! Busca: a página de resultados (título e autor, ficção e não ficção); cada
//! linha traz idioma, tamanho e extensão. Download: `ads.php?md5=` dá o link
//! `get.php?md5=…&key=…`, que redireciona ao servidor do arquivo
//! (cdnN.booksdl.lc, fixo por arquivo).
//!
//! Os servidores de arquivo ficam sobrecarregados (visto em 2026-10-10: o
//! cdn5 respondia "User 'libgen_get' has exceeded the 'max_user_connections'
//! resource (current value: 80)", ou travava até virar 504 na Cloudflare) e
//! alguns são lentos (cdn3: 50–160 KB/s). Por isso: erro 5xx ou demora é
//! temporário (`Error::Busy`, vale tentar mais tarde), o download continua de
//! onde parou (Range) e o limite é por inatividade, não pelo total.

use anyhow::{Context, Result};
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const BASE: &str = "https://libgen.li";

#[derive(Debug, Clone)]
pub struct Entry {
    pub md5: String,
    /// título e autor como a linha mostra (para casar com o pedido)
    pub text: String,
    pub language: String,
    pub ext: String,
    /// em bytes (aproximado: "718 kB")
    pub size: u64,
}

static ROW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<tr>(.*?)</tr>").unwrap());
static CELL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<td[^>]*>(.*?)</td>").unwrap());
static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").unwrap());
static MD5: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"md5=([0-9a-fA-F]{32})").unwrap());
static GET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"href="(get\.php\?md5=[^"]+)""#).unwrap());

fn text(html: &str) -> String {
    let t = TAG.replace_all(html, " ");
    t.replace("&amp;", "&").replace("&quot;", "\"").replace("&#039;", "'").split_whitespace().collect::<Vec<_>>().join(" ")
}

fn size(s: &str) -> u64 {
    let mut it = s.split_whitespace();
    let n: f64 = it.next().and_then(|n| n.replace(',', ".").parse().ok()).unwrap_or(0.0);
    let mult = match it.next().unwrap_or("").to_lowercase().as_str() {
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        _ => 1.0,
    };
    (n * mult) as u64
}

/// As linhas de arquivo de uma página de resultados.
pub fn parse(page: &str) -> Vec<Entry> {
    ROW.captures_iter(page)
        .filter_map(|row| {
            let row = row.get(1)?.as_str();
            let cells: Vec<String> = CELL.captures_iter(row).filter_map(|c| c.get(1)).map(|c| text(c.as_str())).collect();
            // título | autor | editora | ano | idioma | páginas | tamanho | extensão | espelhos
            if cells.len() < 9 {
                return None;
            }
            let n = cells.len();
            Some(Entry {
                md5: MD5.captures(row)?.get(1)?.as_str().to_lowercase(),
                text: format!("{} {}", cells[0], cells[1]),
                language: cells[n - 5].clone(),
                ext: cells[n - 2].to_lowercase(),
                size: size(&cells[n - 3]),
            })
        })
        .collect()
}

/// Sem bytes novos por tanto tempo: a conexão morreu (ou o servidor travou).
const IDLE: Duration = Duration::from_secs(90);

#[derive(Debug)]
pub enum Error {
    /// servidor sobrecarregado, fora do ar ou lento demais: tentar mais tarde
    Busy(String),
    /// o arquivo não existe ou não veio (página no lugar dele)
    Gone(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Busy(s) => write!(f, "LibGen ocupado: {s}"),
            Error::Gone(s) => write!(f, "LibGen: {s}"),
        }
    }
}

pub struct LibGen {
    http: reqwest::Client,
}

impl LibGen {
    pub fn new() -> Result<Self> {
        // sem timeout total: o download longo é vigiado por inatividade
        let http = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/140.0 Safari/537.36")
            .connect_timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { http })
    }

    pub async fn search(&self, query: &str) -> Result<Vec<Entry>> {
        let url = reqwest::Url::parse_with_params(
            &format!("{BASE}/index.php"),
            [("req", query), ("res", "50"), ("columns[]", "t"), ("columns[]", "a"), ("topics[]", "l"), ("topics[]", "f"), ("topics[]", "c")],
        )?;
        let page = self.http.get(url).timeout(Duration::from_secs(60)).send().await.context("LibGen: sem resposta")?.error_for_status()?.text().await?;
        Ok(parse(&page))
    }

    /// O link do arquivo (a página às vezes vem sem ele: tenta 3 vezes).
    async fn link(&self, md5: &str) -> Result<String, Error> {
        for _ in 0..3 {
            let page = async { self.http.get(format!("{BASE}/ads.php?md5={md5}")).timeout(Duration::from_secs(30)).send().await?.error_for_status()?.text().await };
            match page.await {
                Ok(ads) => {
                    if let Some(get) = GET.captures(&ads).and_then(|c| c.get(1)) {
                        return Ok(format!("{BASE}/{}", get.as_str().replace("&amp;", "&")));
                    }
                }
                Err(e) => tracing::info!("LibGen ads.php: {e}"),
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        Err(Error::Busy("a página de download não trouxe o link".into()))
    }

    /// Baixa o arquivo para `dest`, continuando o que já estiver lá;
    /// `progress(baixado, total)`.
    pub async fn download(&self, md5: &str, dest: &Path, progress: impl Fn(u64, u64)) -> Result<(), Error> {
        let url = self.link(md5).await?;
        let have = tokio::fs::metadata(dest).await.map(|m| m.len()).unwrap_or(0);
        let mut req = self.http.get(&url);
        if have > 0 {
            req = req.header("Range", format!("bytes={have}-"));
        }
        let mut res = match tokio::time::timeout(IDLE, req.send()).await {
            Err(_) => return Err(Error::Busy("o servidor do arquivo não respondeu".into())),
            Ok(Err(e)) => return Err(Error::Busy(format!("{e}"))),
            Ok(Ok(r)) => r,
        };
        let status = res.status();
        if status.is_server_error() || status.as_u16() == 429 {
            let body = res.text().await.unwrap_or_default();
            let why = if body.contains("max_user_connections") { "sobrecarregado (limite de conexões)".into() } else { format!("HTTP {status}") };
            return Err(Error::Busy(why));
        }
        if !status.is_success() {
            return Err(Error::Gone(format!("HTTP {status}")));
        }
        if res.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|c| c.starts_with("text/html")) {
            return Err(Error::Gone("veio uma página no lugar do arquivo".into()));
        }
        // 206: continua; 200: o servidor mandou tudo de novo
        let resumed = status.as_u16() == 206;
        let mut done = if resumed { have } else { 0 };
        let total = done + res.content_length().unwrap_or(0);
        let mut f = tokio::fs::OpenOptions::new().create(true).write(true).append(resumed).truncate(!resumed).open(dest).await.map_err(|e| Error::Gone(e.to_string()))?;
        loop {
            let chunk = match tokio::time::timeout(IDLE, res.chunk()).await {
                Err(_) => return Err(Error::Busy(format!("parou em {}% (sem dados por 90 s)", done * 100 / total.max(1)))),
                Ok(Err(e)) => return Err(Error::Busy(format!("conexão caiu: {e}"))),
                Ok(Ok(None)) => break,
                Ok(Ok(Some(c))) => c,
            };
            f.write_all(&chunk).await.map_err(|e| Error::Gone(e.to_string()))?;
            done += chunk.len() as u64;
            progress(done, total);
        }
        f.flush().await.map_err(|e| Error::Gone(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_a_linha_de_resultado() {
        let page = r#"<table><tr><td><a href="edition.php?id=1">Ali Hazelwood - A Hipótese do Amor</a></td><td>Ali Hazelwood</td><td>Desrotina</td><td>2022</td><td>Portuguese</td><td>0</td><td>2 MB</td><td>epub</td><td><a href="/ads.php?md5=0DC0C1E8D8DEDEE0606529AAA357E8F5">1</a></td></tr></table>"#;
        let e = parse(page);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].md5, "0dc0c1e8d8dedee0606529aaa357e8f5");
        assert_eq!((e[0].language.as_str(), e[0].ext.as_str(), e[0].size), ("Portuguese", "epub", 2_000_000));
        assert!(e[0].text.contains("Hipótese do Amor"));
    }
}
