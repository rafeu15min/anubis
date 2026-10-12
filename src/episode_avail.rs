//! O que as fontes têm de cada episódio que ainda não está na biblioteca
//! (a lista de episódios mostra "só legendado" e "sem fonte", como os cartões)
//! e, com todos avaliados, o selo do título inteiro: algum episódio dublado,
//! nada; senão, algum legendado, "só legendado"; senão, "sem fonte".
//!
//! Uma avaliação busca a série toda de uma vez (cada temporada que falta, o
//! nome, o nome em português e a DHT) e julga cada versão pelo `parse` do
//! Sonarr, como o "assistir"; um pacote de temporada vale para os episódios
//! dela. Leva de 10 s a alguns minutos (uma busca por temporada), então roda
//! em segundo plano e a página pergunta de novo. Fica em memória pelo tempo
//! de validade do selo.

use crate::avail::{Audio, Avail, arr_audio, br_audio, is_br, query, skipped};
use crate::orchestrator::Orchestrator;
use crate::search::Kind;
use crate::stream::{norm, original_language, scene_title, series_name};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{info, warn};

#[derive(Clone)]
enum Entry {
    Checking,
    Done { at: Instant, eps: HashMap<i64, Avail> },
}

#[derive(Default)]
pub struct Cache(Mutex<HashMap<i64, Entry>>);

/// por quanto tempo vale uma avaliação (a menor validade entre os selos)
const TTL: Duration = Duration::from_secs(24 * 3600);

impl Orchestrator {
    /// O que se sabe dos episódios da série (id do episódio no Sonarr → selo);
    /// sem avaliação recente, começa uma em segundo plano e devolve None.
    pub async fn episode_availability(self: &Arc<Self>, tvdb: i64) -> Option<HashMap<i64, Avail>> {
        let mut cache = self.episode_avail.0.lock().await;
        match cache.get(&tvdb) {
            Some(Entry::Done { at, eps }) if at.elapsed() < TTL => return Some(eps.clone()),
            Some(Entry::Checking) => return None,
            _ => {}
        }
        cache.insert(tvdb, Entry::Checking);
        drop(cache);
        let me = self.clone();
        tokio::spawn(async move {
            let t = Instant::now();
            let entry = match me.evaluate_episodes(tvdb).await {
                Ok(eps) => {
                    info!("episódios da série {tvdb}: {} avaliados em {} s", eps.len(), t.elapsed().as_secs());
                    Entry::Done { at: Instant::now(), eps }
                }
                Err(e) => {
                    warn!("episódios da série {tvdb}: {e:#}");
                    // tenta de novo na próxima pergunta
                    Entry::Done { at: Instant::now() - TTL, eps: HashMap::new() }
                }
            };
            me.episode_avail.0.lock().await.insert(tvdb, entry);
        });
        None
    }

    async fn evaluate_episodes(&self, tvdb: i64) -> Result<HashMap<i64, Avail>> {
        let sonarr = self.sonarr.as_ref().context("Sonarr não configurado")?;
        let p = self.prowlarr.as_ref().context("Prowlarr não configurado")?;
        let series = sonarr.find("series", "tvdbId", tvdb).await?.context("série fora do Sonarr")?;
        let sid = series["id"].as_i64().context("série sem id")?;
        let now = chrono::Utc::now().to_rfc3339();
        // os que faltam e já saíram (especiais de fora)
        let eps = sonarr.episodes_raw(sid).await?;
        let missing: Vec<&Value> = eps
            .iter()
            .filter(|e| e["seasonNumber"].as_i64().unwrap_or(0) > 0 && e["hasFile"] != true)
            .filter(|e| e["airDateUtc"].as_str().is_some_and(|d| *d <= *now.as_str()))
            .collect();
        if missing.is_empty() {
            return Ok(HashMap::new());
        }
        let name = series_name(series["title"].as_str().unwrap_or_default());
        let pt = match (&self.tmdb, series["tmdbId"].as_i64().filter(|id| *id > 0)) {
            (Some(t), Some(id)) => t.get(&format!("tv/{id}?language=pt-BR")).await.ok().and_then(|v| v["name"].as_str().map(series_name)),
            _ => None,
        }
        .filter(|pt| norm(pt) != norm(&name));
        // anime: o nome romaji e os de cada temporada que falta ("Sousou no
        // Frieren 2nd Season"), que é como os grupos de fora chamam
        let romaji = if series["seriesType"] == "anime" { self.romaji(&series).await.filter(|r| norm(r) != norm(&name)) } else { None };
        let names: Vec<String> = [Some(name.clone()), pt.clone(), romaji.clone()].into_iter().flatten().map(|n| norm(&n)).collect();
        let original = original_language(&series).map(str::to_owned);
        // como a checagem dos cartões: só os indexadores de torrent, sem o 1337x
        let indexers = p.get_json("indexer").await?;
        let ids: Vec<i64> = indexers
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["enable"] == true && i["protocol"] == "torrent" && !skipped(i["name"].as_str().unwrap_or_default()))
            .filter_map(|i| i["id"].as_i64())
            .collect();
        let seasons: BTreeSet<i64> = missing.iter().filter_map(|e| e["seasonNumber"].as_i64()).collect();
        let mut queries: Vec<String> = seasons.iter().map(|s| format!("{} S{s:02}", query(&name, None))).collect();
        queries.push(query(&name, None));
        queries.extend(pt.as_deref().map(|pt| query(pt, None)));
        queries.dedup();
        // os nomes romaji só nos sites de fora (os brasileiros não os usam)
        let mut romaji_queries: Vec<String> = vec![];
        if let Some(r) = &romaji {
            romaji_queries.push(query(r, None));
            for s in seasons.iter().filter(|s| **s > 1) {
                let scene = series["alternateTitles"].as_array().into_iter().flatten().filter(|a| a["sceneSeasonNumber"].as_i64() == Some(*s));
                romaji_queries.extend(scene.filter_map(|a| a["title"].as_str()).find(|t| t.is_ascii()).map(|t| query(t, None)));
            }
        }
        romaji_queries.retain(|q| !queries.contains(q));
        romaji_queries.dedup();
        let foreign: Vec<i64> = crate::stream::foreign_indexers(&indexers).into_iter().filter(|i| ids.contains(i)).collect();
        let mut found: Vec<Value> = vec![];
        let all = queries.iter().map(|q| (q, &ids)).chain(romaji_queries.iter().map(|q| (q, &foreign)));
        for (q, ids) in all {
            // uma por vez: os sites com Cloudflare bloqueiam rajadas
            found.extend(p.search_in(q, &[], crate::services::ONE_PAGE, ids).await.with_context(|| format!("busca \"{q}\""))?);
        }
        if self.cfg.bitmagnet.is_some() {
            found.extend(self.bitmagnet_search(&name).await.unwrap_or_default());
        }
        // o melhor áudio achado para cada episódio que falta
        let by_number: HashMap<(i64, i64), i64> =
            missing.iter().filter_map(|e| Some(((e["seasonNumber"].as_i64()?, e["episodeNumber"].as_i64()?), e["id"].as_i64()?))).collect();
        let mut best: HashMap<i64, Audio> = HashMap::new();
        let mut seen = std::collections::HashSet::new();
        for r in found {
            if r["protocol"] != "torrent" || r["seeders"].as_i64().unwrap_or(0) <= 0 || !seen.insert(r["guid"].as_str().unwrap_or_default().to_owned()) {
                continue;
            }
            let title = r["title"].as_str().unwrap_or_default();
            let Ok(parsed) = sonarr.parse(&scene_title(title)).await else { continue };
            let pe = &parsed["parsedEpisodeInfo"];
            let same_series = match parsed["series"]["id"].as_i64() {
                Some(id) => id == sid,
                None => names.iter().any(|n| !n.is_empty() && norm(title).contains(n.as_str())),
            };
            if !same_series {
                continue;
            }
            let langs: Vec<&str> = pe["languages"].as_array().into_iter().flatten().filter_map(|l| l["name"].as_str()).collect();
            let audio = match arr_audio(&langs, original.as_deref()) {
                Audio::Portuguese => Audio::Portuguese,
                other if is_br(&r) => br_audio(title).min(other),
                other => other,
            };
            // os episódios desta versão: os que o Sonarr casou, os números, ou a temporada inteira
            let mut covered: Vec<i64> = parsed["episodes"].as_array().into_iter().flatten().filter_map(|e| e["id"].as_i64()).collect();
            if covered.is_empty()
                && let Some(season) = pe["seasonNumber"].as_i64()
            {
                if pe["fullSeason"] == true {
                    covered = by_number.iter().filter(|((s, _), _)| *s == season).map(|(_, id)| *id).collect();
                } else {
                    covered = pe["episodeNumbers"].as_array().into_iter().flatten().filter_map(Value::as_i64).filter_map(|n| by_number.get(&(season, n)).copied()).collect();
                }
            }
            for id in covered {
                let e = best.entry(id).or_insert(Audio::Other);
                *e = (*e).min(audio);
            }
        }
        let eps: HashMap<i64, Avail> = by_number
            .values()
            .map(|id| {
                let a = match best.get(id) {
                    Some(Audio::Portuguese) => Avail::Dubbed,
                    Some(Audio::Original) => Avail::Subbed,
                    _ => Avail::Nothing,
                };
                (*id, a)
            })
            .collect();
        // o título inteiro: o que vale para todos os episódios que faltam
        let whole = if eps.values().any(|a| *a == Avail::Dubbed) {
            Avail::Dubbed
        } else if eps.values().any(|a| *a == Avail::Subbed) {
            Avail::Subbed
        } else {
            Avail::Nothing
        };
        self.availability.set(Kind::Series, tvdb, whole).await;
        Ok(eps)
    }
}
