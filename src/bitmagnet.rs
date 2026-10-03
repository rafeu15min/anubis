//! bitmagnet: o catálogo do que circula na DHT do BitTorrent (acha o que os
//! sites não têm, como episódios recentes de Running Man). Duas partes:
//! - a busca (Torznab), só em último caso: quando as fontes normais não têm
//!   nenhuma versão aprovada do episódio;
//! - a varredura da DHT, que abre milhares de conversas UDP e lota a tabela
//!   de conexões do roteador e do provedor (a internet de casa travava). Ela
//!   roda só com tudo ocioso: `quiet_minutes` (20) sem ninguém no site do
//!   Anubis nem assistindo no Jellyfin. Um acesso ao site pausa na hora
//!   (`docker pause` congela onde estava) e ela volta 20 min depois do
//!   último acesso. Se a internet piorar com ela rodando, pausa por 30 min.

use crate::orchestrator::Orchestrator;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};

const TICK: Duration = Duration::from_secs(30);
/// a internet piorou com a varredura: fica pausada por este tempo
const BACKOFF: Duration = Duration::from_secs(30 * 60);
/// verificações ruins seguidas (de 30 em 30 s) para pausar
const BAD_CHECKS: u32 = 3;

pub struct Dht {
    last_activity: Mutex<Instant>,
    /// a varredura está rodando (o que o Anubis mandou por último)
    running: AtomicBool,
    backoff_until: Mutex<Option<Instant>>,
    bad: AtomicU32,
}

impl Default for Dht {
    fn default() -> Self {
        // começa como se alguém tivesse acabado de usar: a varredura só liga
        // depois de um período ocioso inteiro
        Self { last_activity: Mutex::new(Instant::now()), running: AtomicBool::new(false), backoff_until: Mutex::new(None), bad: AtomicU32::new(0) }
    }
}

impl Dht {
    /// Alguém no site (ou assistindo): a varredura pausa na hora.
    pub fn touch(&self, container: &str) {
        *self.last_activity.lock().unwrap() = Instant::now();
        if self.running.swap(false, Ordering::SeqCst) {
            let c = container.to_owned();
            tokio::spawn(async move {
                match docker(&["pause", &c]).await {
                    Ok(()) => info!("varredura da DHT pausada: alguém usando"),
                    Err(e) => warn!("varredura da DHT: {e:#}"),
                }
            });
        }
    }
}

impl Orchestrator {
    pub fn dht_touch(&self) {
        if let Some(b) = &self.cfg.bitmagnet {
            self.dht.touch(&b.dht_container);
        }
    }

    /// Versões no catálogo da DHT, no formato das do Prowlarr (para o `judge`).
    pub(crate) async fn bitmagnet_search(&self, query: &str) -> Result<Vec<Value>> {
        let b = self.cfg.bitmagnet.as_ref().context("bitmagnet não configurado")?;
        let url = format!("{}/torznab/api?t=search&q={}", b.url.trim_end_matches('/'), crate::services::url_encode(query));
        let xml = reqwest::Client::new().get(&url).timeout(Duration::from_secs(30)).send().await?.error_for_status()?.text().await?;
        Ok(torznab_items(&xml))
    }
}

/// Liga e pausa a varredura conforme o uso (ver o topo do arquivo).
pub async fn run(orch: Arc<Orchestrator>) {
    let Some(cfg) = &orch.cfg.bitmagnet else { return };
    let (c, quiet) = (cfg.dht_container.as_str(), Duration::from_secs(cfg.quiet_minutes * 60));
    // Anubis reiniciado com ela rodando: segue a regra a partir de agora
    if state(c).await.as_deref() == Some("running") {
        orch.dht.running.store(true, Ordering::SeqCst);
        orch.dht_touch();
    }
    loop {
        tokio::time::sleep(TICK).await;
        if orch.jellyfin.playing().await.unwrap_or(false) {
            orch.dht_touch();
        }
        let d = &orch.dht;
        let idle = d.last_activity.lock().unwrap().elapsed() >= quiet;
        let backoff = d.backoff_until.lock().unwrap().is_some_and(|t| Instant::now() < t);
        let running = d.running.load(Ordering::SeqCst);
        if idle && !backoff && !running {
            let r = match state(c).await.as_deref() {
                Some("paused") => docker(&["unpause", c]).await,
                Some("running") => Ok(()),
                _ => docker(&["start", c]).await,
            };
            match r {
                Ok(()) => {
                    d.running.store(true, Ordering::SeqCst);
                    d.bad.store(0, Ordering::SeqCst);
                    info!("varredura da DHT ligada: {} min sem ninguém usando", quiet.as_secs() / 60);
                }
                Err(e) => warn!("varredura da DHT: {e:#}"),
            }
            continue;
        }
        if !running {
            continue;
        }
        // rodando: a internet aguenta? (o que travava era a tabela de conexões)
        if internet_ok().await {
            d.bad.store(0, Ordering::SeqCst);
        } else if d.bad.fetch_add(1, Ordering::SeqCst) + 1 >= BAD_CHECKS {
            *d.backoff_until.lock().unwrap() = Some(Instant::now() + BACKOFF);
            d.bad.store(0, Ordering::SeqCst);
            d.running.store(false, Ordering::SeqCst);
            match docker(&["pause", c]).await {
                Ok(()) => warn!("varredura da DHT pausada por 30 min: a internet piorou com ela rodando"),
                Err(e) => warn!("varredura da DHT: {e:#}"),
            }
        }
    }
}

/// Uma conexão a um servidor conhecido abre rápido? (com a tabela de
/// conexões lotada, falha ou demora)
async fn internet_ok() -> bool {
    let t = Instant::now();
    matches!(tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect("1.1.1.1:443")).await, Ok(Ok(_))) && t.elapsed() < Duration::from_millis(500)
}

async fn docker(args: &[&str]) -> Result<()> {
    let out = tokio::process::Command::new("docker").args(args).output().await.context("rodando docker")?;
    if !out.status.success() {
        bail!("docker {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// "running", "paused", "exited", "created"… (None: container não existe)
async fn state(c: &str) -> Option<String> {
    let out = tokio::process::Command::new("docker").args(["inspect", "-f", "{{.State.Status}}", c]).output().await.ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Os itens de uma resposta Torznab (o XML do bitmagnet é simples: título,
/// tamanho e atributos `torznab:attr`).
fn torznab_items(xml: &str) -> Vec<Value> {
    let tag = |item: &str, name: &str| -> Option<String> {
        let (open, close) = (format!("<{name}>"), format!("</{name}>"));
        let a = item.find(&open)? + open.len();
        let b = item[a..].find(&close)? + a;
        Some(unescape(&item[a..b]))
    };
    let attr = |item: &str, name: &str| -> Option<String> {
        let key = format!("name=\"{name}\" value=\"");
        let a = item.find(&key)? + key.len();
        let b = item[a..].find('"')? + a;
        Some(unescape(&item[a..b]))
    };
    xml.split("<item>")
        .skip(1)
        .filter_map(|item| {
            let title = tag(item, "title")?;
            let hash = attr(item, "infohash")?;
            let magnet = attr(item, "magneturl").unwrap_or_else(|| format!("magnet:?xt=urn:btih:{hash}"));
            let num = |n: &str| attr(item, n).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
            Some(json!({
                "title": title, "guid": hash, "size": num("size"), "seeders": num("seeders"), "leechers": num("leechers"),
                "protocol": "torrent", "indexer": "bitmagnet (DHT)", "magnetUrl": magnet,
            }))
        })
        .collect()
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&apos;", "'").replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_bitmagnet_torznab() {
        let xml = r#"<rss><channel><title>bitmagnet</title>
          <item><title>Running Man - 794 [260315].mp4</title><guid>47ef</guid><size>1396600813</size>
            <enclosure url="magnet:?xt=urn:btih:47ef&amp;dn=Running+Man"></enclosure>
            <torznab:attr name="infohash" value="47ef"></torznab:attr>
            <torznab:attr name="magneturl" value="magnet:?xt=urn:btih:47ef&amp;dn=Running+Man"></torznab:attr>
            <torznab:attr name="seeders" value="6"></torznab:attr>
            <torznab:attr name="size" value="1396600813"></torznab:attr></item>
          <item><title>Sem hash</title></item>
        </channel></rss>"#;
        let items = torznab_items(xml);
        assert_eq!(items.len(), 1, "item sem infohash fica de fora");
        let i = &items[0];
        assert_eq!(i["title"], "Running Man - 794 [260315].mp4");
        assert_eq!(i["seeders"], 6);
        assert_eq!(i["size"], 1396600813_i64);
        assert_eq!(i["magnetUrl"], "magnet:?xt=urn:btih:47ef&dn=Running+Man");
        assert_eq!(i["indexer"], "bitmagnet (DHT)");
    }
}
