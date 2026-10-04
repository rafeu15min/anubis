//! Dublagem automática: filmes e episódios da biblioteca sem áudio em
//! português ganham a faixa PT-BR quando uma dublagem aparece nas fontes.
//!
//! - De 6 em 6 h (ou na hora, por POST /autodub/…) lista no Radarr/Sonarr os
//!   arquivos sem português (nem no nome nem nas faixas que o mediainfo leu),
//!   fora os títulos de idioma original português e os especiais.
//! - Para cada um, a busca de dublagem (`dub_sources`) e o "juntar dublagem"
//!   com o arquivo da biblioteca como vídeo (`start_dub`): só a dublagem é
//!   baixada, a sincronia é conferida contra o arquivo da biblioteca e a faixa
//!   PT-BR entra nele (vídeo, áudio original e legendas ficam).
//! - Tenta também versões que dizem ter zero fontes: sem ninguém esperando dá
//!   para esperar, e os sites brasileiros informam 0 em quase tudo.
//! - Sem dublagem nas fontes, tenta de novo em 2 dias. Uma dublagem que não
//!   serviu (parada, cortes diferentes) fica marcada por 30 dias.
//! - Um título por vez; fora os pedidos na hora, só com o site ocioso (quem
//!   assiste não divide a internet) e com pausa entre as buscas, que os
//!   indexadores castigam rajadas.

use crate::mux::{LibraryVideo, Step, Target};
use crate::orchestrator::Orchestrator;
use crate::search::Kind;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};
use tracing::{info, warn};

const STATE_FILE: &str = "dublagem.json";
const SWEEP: Duration = Duration::from_secs(6 * 3600);
/// site sem ninguém por este tempo antes de começar um título (fora os pedidos)
const QUIET: Duration = Duration::from_secs(10 * 60);
/// entre uma busca e outra
const PAUSE: Duration = Duration::from_secs(30);
/// buscas por passada (o resto fica para a próxima)
const MAX_SEARCHES: usize = 30;
/// dublagens tentadas por título numa passada
const MAX_TRIES: usize = 3;
const RETRY_NOTHING: i64 = 2 * 86400;
const RETRY_FAILED: i64 = 3 * 86400;
const FORGET_FAILED: i64 = 30 * 86400;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Entry {
    /// não tentar antes disto (segundos desde 1970)
    next: i64,
    note: String,
    /// dublagens que não serviram: guid → quando
    #[serde(default)]
    failed: HashMap<String, i64>,
}

#[derive(Default)]
pub struct AutoDub {
    state: Mutex<Option<HashMap<String, Entry>>>,
    /// pedidos na hora (passam na frente e não esperam o site ficar ocioso)
    urgent: Mutex<Vec<String>>,
    wake: Notify,
}

/// Um arquivo da biblioteca sem português.
struct Item {
    target: Target,
    /// id no Radarr (filme) ou no Sonarr (episódio): o da busca
    arr_id: i64,
    title: String,
    /// caminho como o Radarr/Sonarr o veem, e o id do arquivo
    path: String,
    file_id: i64,
}

impl Item {
    fn key(&self) -> String {
        let (kind, id) = self.target.key();
        key(kind, id)
    }
    fn kind(&self) -> Kind {
        self.target.key().0
    }
}

fn key(kind: Kind, id: i64) -> String {
    format!("{}:{id}", if kind == Kind::Movie { "movie" } else { "episode" })
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Português no arquivo: no que o Radarr/Sonarr entenderam do nome ou nas
/// faixas que o mediainfo leu ("por/eng" na API; ["por", "eng"] no webhook).
pub(crate) fn has_portuguese(file: &Value) -> bool {
    let audio = &file["mediaInfo"]["audioLanguages"];
    let tracks: Vec<String> = match audio.as_array() {
        Some(list) => list.iter().filter_map(|l| l.as_str().map(str::to_lowercase)).collect(),
        None => audio.as_str().unwrap_or_default().to_lowercase().split(['/', ',', ' ']).map(str::to_owned).collect(),
    };
    file["languages"].as_array().into_iter().flatten().any(|l| l["name"].as_str().is_some_and(|n| n.starts_with("Portuguese")))
        || tracks.iter().any(|l| ["por", "pob", "pt", "ptb", "portuguese"].contains(&l.as_str()))
}

fn portuguese_original(item: &Value) -> bool {
    crate::stream::original_language(item).is_some_and(|l| l.starts_with("Portuguese"))
}

impl Orchestrator {
    /// Filmes e episódios da biblioteca sem português.
    async fn without_portuguese(&self) -> Result<Vec<Item>> {
        let mut out = vec![];
        if let Some(radarr) = &self.radarr {
            for m in radarr.get_json("movie").await?.as_array().into_iter().flatten() {
                let f = &m["movieFile"];
                if m["hasFile"].as_bool() != Some(true) || has_portuguese(f) || portuguese_original(m) {
                    continue;
                }
                out.push(Item {
                    target: Target::Movie { tmdb: m["tmdbId"].as_i64().unwrap_or(0), movie_id: m["id"].as_i64().unwrap_or(0) },
                    arr_id: m["id"].as_i64().unwrap_or(0),
                    title: m["title"].as_str().unwrap_or_default().to_owned(),
                    path: f["path"].as_str().unwrap_or_default().to_owned(),
                    file_id: f["id"].as_i64().unwrap_or(0),
                });
            }
        }
        if let Some(sonarr) = &self.sonarr {
            for s in sonarr.get_json("series").await?.as_array().into_iter().flatten() {
                let series_id = s["id"].as_i64().unwrap_or(0);
                if s["statistics"]["episodeFileCount"].as_i64() == Some(0) || portuguese_original(s) {
                    continue;
                }
                let files: HashMap<i64, Value> = sonarr
                    .get_json(&format!("episodefile?seriesId={series_id}"))
                    .await?
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|f| !has_portuguese(f))
                    .map(|f| (f["id"].as_i64().unwrap_or(0), f.clone()))
                    .collect();
                if files.is_empty() {
                    continue;
                }
                let episodes = sonarr.episodes_raw(series_id).await?;
                // um arquivo com dois episódios (episódio duplo) fica de fora
                let mut count: HashMap<i64, usize> = HashMap::new();
                for e in &episodes {
                    *count.entry(e["episodeFileId"].as_i64().unwrap_or(0)).or_default() += 1;
                }
                let mut eps: Vec<&Value> = episodes
                    .iter()
                    .filter(|e| e["seasonNumber"].as_i64().unwrap_or(0) > 0)
                    .filter(|e| e["episodeFileId"].as_i64().is_some_and(|f| files.contains_key(&f) && count[&f] == 1))
                    .collect();
                eps.sort_by_key(|e| (e["seasonNumber"].as_i64(), e["episodeNumber"].as_i64()));
                for e in eps {
                    let (season, episode) = (e["seasonNumber"].as_i64().unwrap_or(0), e["episodeNumber"].as_i64().unwrap_or(0));
                    let f = &files[&e["episodeFileId"].as_i64().unwrap_or(0)];
                    out.push(Item {
                        target: Target::Episode { episode_id: e["id"].as_i64().unwrap_or(0), series_id, season, episode },
                        arr_id: e["id"].as_i64().unwrap_or(0),
                        title: format!("{} — S{season:02}E{episode:02}", s["title"].as_str().unwrap_or_default()),
                        path: f["path"].as_str().unwrap_or_default().to_owned(),
                        file_id: f["id"].as_i64().unwrap_or(0),
                    });
                }
            }
        }
        Ok(out)
    }

    /// Pede a dublagem na hora: filme (id do TMDB), episódio (id do Sonarr) ou
    /// série (id do TVDB: os episódios dela sem português). Devolve as chaves.
    pub async fn autodub_now(&self, kind: Kind, id: i64) -> Result<Vec<String>> {
        let keys = match kind {
            Kind::Movie | Kind::Episode => vec![key(kind, id)],
            Kind::Series => {
                let sonarr = self.sonarr.as_ref().context("Sonarr não configurado")?;
                let series = sonarr.find("series", "tvdbId", id).await?.context("série fora do Sonarr")?;
                let series_id = series["id"].as_i64().unwrap_or(0);
                self.without_portuguese()
                    .await?
                    .into_iter()
                    .filter(|i| matches!(i.target, Target::Episode { series_id: s, .. } if s == series_id))
                    .map(|i| i.key())
                    .collect()
            }
        };
        self.autodub.urgent.lock().await.extend(keys.iter().cloned());
        self.autodub.wake.notify_one();
        Ok(keys)
    }

    /// Webhook "Download" do Radarr/Sonarr: o arquivo que entrou não tem
    /// português → a dublagem é procurada na hora, sem esperar a passada.
    pub async fn autodub_after_import(&self, ev: &Value) {
        let (kind, ids, file) = if !ev["movie"].is_null() {
            (Kind::Movie, ev["movie"]["tmdbId"].as_i64().into_iter().collect::<Vec<_>>(), &ev["movieFile"])
        } else {
            (Kind::Episode, ev["episodes"].as_array().into_iter().flatten().filter_map(|e| e["id"].as_i64()).collect(), &ev["episodeFile"])
        };
        if file.is_null() || has_portuguese(file) {
            return;
        }
        for id in ids {
            if let Err(e) = self.autodub_now(kind, id).await {
                warn!("dublagem depois do import: {e:#}");
            }
        }
    }

    /// O que a dublagem automática sabe de cada título (para conferir).
    pub async fn autodub_state(&self) -> Value {
        let state = self.autodub.state.lock().await.clone().unwrap_or_default();
        let mut out = serde_json::Map::new();
        for (k, e) in state {
            out.insert(k, serde_json::json!({ "next": e.next, "note": e.note, "failed": e.failed.len() }));
        }
        // as chaves antes: `mux_status` também trava a lista
        let jobs: Vec<(Kind, i64)> = self.muxes.lock().await.keys().copied().collect();
        for (kind, id) in jobs {
            if let Some(m) = self.mux_status(kind, id).await.filter(|m| m["auto"] == true) {
                out.entry(key(kind, id)).or_insert_with(|| serde_json::json!({})).as_object_mut().unwrap().insert("job".into(), m);
            }
        }
        Value::Object(out)
    }

    async fn load_autodub(&self) {
        let mut st = self.autodub.state.lock().await;
        if st.is_some() {
            return;
        }
        let path = self.streamer().map(|s| s.state_dir.join(STATE_FILE));
        *st = Some(path.ok().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default());
    }

    async fn save_autodub(&self) {
        let Ok(streamer) = self.streamer() else { return };
        let Some(state) = self.autodub.state.lock().await.clone() else { return };
        let path = streamer.state_dir.join(STATE_FILE);
        let tmp = path.with_extension("tmp");
        let res = serde_json::to_vec_pretty(&state).map_err(anyhow::Error::from).and_then(|b| Ok(std::fs::write(&tmp, b)?)).and_then(|_| Ok(std::fs::rename(&tmp, &path)?));
        if let Err(e) = res {
            warn!("não gravei {}: {e:#}", path.display());
        }
    }

    async fn entry(&self, k: &str) -> Entry {
        self.autodub.state.lock().await.as_ref().and_then(|s| s.get(k).cloned()).unwrap_or_default()
    }

    async fn set_entry(&self, k: &str, e: Option<Entry>) {
        if let Some(s) = self.autodub.state.lock().await.as_mut() {
            match e {
                Some(e) => s.insert(k.to_owned(), e),
                None => s.remove(k),
            };
        }
        self.save_autodub().await;
    }

    /// Uma passada: os pedidos na hora primeiro, depois o resto (com o site ocioso).
    async fn autodub_sweep(self: &Arc<Self>) -> Result<()> {
        let mut items = self.without_portuguese().await?;
        let urgent: HashSet<String> = std::mem::take(&mut *self.autodub.urgent.lock().await).into_iter().collect();
        items.sort_by_key(|i| !urgent.contains(&i.key()));
        let lacking: HashSet<String> = items.iter().map(Item::key).collect();
        // o que já tem português (dublado aqui ou por outro caminho) sai do estado
        if let Some(s) = self.autodub.state.lock().await.as_mut() {
            s.retain(|k, _| lacking.contains(k));
        }
        let mut searches = 0;
        for item in items {
            let k = item.key();
            let now_please = urgent.contains(&k);
            if !now_please && (searches >= MAX_SEARCHES || self.entry(&k).await.next > now()) {
                continue;
            }
            if !now_please {
                while self.dht.idle_for() < QUIET {
                    // um pedido na hora não espera a fila
                    if !self.autodub.urgent.lock().await.is_empty() {
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
            }
            if searches > 0 {
                tokio::time::sleep(PAUSE).await;
            }
            searches += 1;
            self.dub_one(&item).await;
        }
        Ok(())
    }

    /// Procura e embute a dublagem de um título; anota o resultado.
    async fn dub_one(self: &Arc<Self>, item: &Item) {
        let k = item.key();
        let mut entry = self.entry(&k).await;
        let t = now();
        entry.failed.retain(|_, at| t - *at < FORGET_FAILED);
        let list = match self.dub_sources(item.kind(), item.arr_id).await {
            Ok(l) => l,
            Err(e) => {
                warn!("{}: busca de dublagem: {e:#}", item.title);
                entry.next = t + 6 * 3600;
                entry.note = format!("a busca falhou: {e:#}");
                return self.set_entry(&k, Some(entry)).await;
            }
        };
        let fresh: Vec<_> = list.into_iter().filter(|c| !entry.failed.contains_key(&c.guid)).collect();
        if fresh.is_empty() {
            info!("{}: nenhuma dublagem nas fontes", item.title);
            entry.next = t + RETRY_NOTHING;
            entry.note = "nenhuma dublagem nas fontes".into();
            return self.set_entry(&k, Some(entry)).await;
        }
        let host = self.cfg.host_path(&item.path);
        let mut last = String::new();
        for c in fresh.iter().take(MAX_TRIES) {
            // espaço para a dublagem e para o arquivo juntado (do tamanho do da biblioteca)
            let need = c.size + std::fs::metadata(&host).map(|m| m.len()).unwrap_or(0) + (5 << 30);
            let dir = self.cfg.stream.as_ref().map(|s| s.download_dir.clone());
            if let Some(d) = dir
                && crate::disk::usage(&d).is_ok_and(|u| u.avail < need)
            {
                last = "pouco espaço em disco".into();
                break;
            }
            let library = LibraryVideo { file: host.clone(), file_id: item.file_id, inode: crate::disk::file_id(&host).map(|f| f.inode) };
            let job = match self.start_dub(item.target, item.title.clone(), library, c).await {
                Ok(j) => j,
                Err(e) => {
                    last = format!("{}: {e:#}", c.title);
                    warn!("{}: dublagem {last}", item.title);
                    entry.failed.insert(c.guid.clone(), now());
                    continue;
                }
            };
            while job.active() {
                tokio::time::sleep(Duration::from_secs(15)).await;
            }
            if job.step() == Step::Done {
                info!("{}: dublagem embutida ({})", item.title, c.title);
                return self.set_entry(&k, None).await;
            }
            last = format!("{}: {}", c.title, job.error().unwrap_or_default());
            entry.failed.insert(c.guid.clone(), now());
            // a tentativa que falhou não deixa downloads para trás
            let _ = self.cancel_mux(item.target.key().0, item.target.key().1).await;
        }
        entry.next = now() + RETRY_FAILED;
        entry.note = format!("nenhuma dublagem serviu; a última: {last}");
        self.set_entry(&k, Some(entry)).await;
    }
}

pub async fn run(orch: Arc<Orchestrator>) {
    if orch.streamer().is_err() {
        return;
    }
    orch.load_autodub().await;
    // deixa o resto subir (retomadas, vitrine) antes da primeira passada
    tokio::time::sleep(Duration::from_secs(120)).await;
    loop {
        if let Err(e) = orch.autodub_sweep().await {
            warn!("dublagem automática: {e:#}");
        }
        // pedido feito durante a passada: outra na hora
        if !orch.autodub.urgent.lock().await.is_empty() {
            continue;
        }
        tokio::select! {
            _ = tokio::time::sleep(SWEEP) => {}
            _ = orch.autodub.wake.notified() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn portuguese_by_name_or_by_track() {
        assert!(has_portuguese(&json!({ "languages": [{ "name": "Portuguese (Brazil)" }] })));
        assert!(has_portuguese(&json!({ "languages": [{ "name": "English" }], "mediaInfo": { "audioLanguages": "eng/por" } })));
        assert!(!has_portuguese(&json!({ "languages": [{ "name": "English" }], "mediaInfo": { "audioLanguages": "eng" } })));
        assert!(!has_portuguese(&json!({ "languages": [], "mediaInfo": { "audioLanguages": "" } })));
        // o webhook manda a lista
        assert!(has_portuguese(&json!({ "languages": [{ "name": "English" }], "mediaInfo": { "audioLanguages": ["eng", "por"] } })));
        assert!(!has_portuguese(&json!({ "languages": [{ "name": "English" }], "mediaInfo": { "audioLanguages": ["eng"] } })));
    }
}
