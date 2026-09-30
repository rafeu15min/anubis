//! Indexadores fora do ar. O Radarr, o Sonarr e o Prowlarr põem de castigo o
//! indexador que falha e param de consultá-lo por horas — aí "nenhuma versão"
//! pode ser só a fonte que não respondeu. A página avisa quais estão fora e
//! oferece testar de novo: o teste que passa tira do castigo na hora.
//!
//! O Radarr/Sonarr não têm rota de status dos indexadores: o sinal é o
//! `/health` deles (verificações IndexerStatusCheck e IndexerLongTermStatusCheck,
//! com os nomes no fim da mensagem). O Prowlarr tem `indexerstatus`.

use crate::orchestrator::Orchestrator;
use crate::services::Arr;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{info, warn};

/// a lista fica guardada por 1 min (cada consulta são 4–5 chamadas)
const TTL: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct Cache(Mutex<Option<(Instant, Vec<String>)>>);

/// Nome do indexador sem o " (Prowlarr)" que o Radarr/Sonarr acrescentam aos
/// que vêm do Prowlarr — o mesmo indexador tem um nome só na página.
fn base_name(n: &str) -> &str {
    let n = n.trim();
    n.strip_suffix(" (Prowlarr)").unwrap_or(n)
}

/// Fora do ar segundo o `/health` do Radarr/Sonarr, e se são todos ("All
/// indexers are unavailable…", sem a lista de nomes).
fn down_in_health(health: &Value) -> (Vec<String>, bool) {
    let (mut names, mut all) = (vec![], false);
    for h in health.as_array().into_iter().flatten() {
        if !matches!(h["source"].as_str(), Some("IndexerStatusCheck" | "IndexerLongTermStatusCheck")) {
            continue;
        }
        match h["message"].as_str().unwrap_or_default().rsplit_once(": ") {
            Some((_, list)) => names.extend(list.split(", ").map(|n| base_name(n).to_owned())),
            None => all = true,
        }
    }
    (names, all)
}

/// Fora do ar segundo o Prowlarr: castigo (`disabledTill`) ainda valendo.
fn down_in_prowlarr(status: &Value, indexers: &Value, now: DateTime<Utc>) -> Vec<String> {
    let name_of = |id: i64| indexers.as_array()?.iter().find(|i| i["id"].as_i64() == Some(id))?["name"].as_str().map(str::to_owned);
    status
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["disabledTill"].as_str().and_then(|t| t.parse::<DateTime<Utc>>().ok()).is_some_and(|t| t > now))
        .filter_map(|s| name_of(s["indexerId"].as_i64()?))
        .collect()
}

/// Nomes (sem o " (Prowlarr)") dos indexadores ligados de uma lista `GET indexer`.
fn enabled_names(list: &Value) -> Vec<String> {
    list.as_array().into_iter().flatten().filter(|i| i["enable"] != false).filter_map(|i| i["name"].as_str().map(|n| base_name(n).to_owned())).collect()
}

async fn down_in_arr(arr: &Arr) -> anyhow::Result<Vec<String>> {
    let (mut names, all) = down_in_health(&arr.get_json("health").await?);
    if all {
        names.extend(enabled_names(&arr.get_json("indexer").await?));
    }
    Ok(names)
}

impl Orchestrator {
    /// Indexadores fora do ar agora (nomes sem repetição), do último minuto.
    pub async fn indexers_down(&self) -> Vec<String> {
        if let Some((at, v)) = &*self.indexers.0.lock().await
            && at.elapsed() < TTL
        {
            return v.clone();
        }
        self.refresh_indexers().await
    }

    /// O mesmo, consultando agora (depois de uma busca que pode ter falhado).
    pub async fn refresh_indexers(&self) -> Vec<String> {
        let mut down = vec![];
        for arr in [&self.radarr, &self.sonarr].into_iter().flatten() {
            match down_in_arr(arr).await {
                Ok(n) => down.extend(n),
                Err(e) => warn!("indexadores: {e:#}"),
            }
        }
        if let Some(p) = &self.prowlarr {
            match tokio::try_join!(p.get_json("indexerstatus"), p.get_json("indexer")) {
                Ok((status, list)) => down.extend(down_in_prowlarr(&status, &list, Utc::now())),
                Err(e) => warn!("indexadores: {e:#}"),
            }
        }
        down.sort();
        down.dedup();
        *self.indexers.0.lock().await = Some((Instant::now(), down.clone()));
        down
    }

    /// Testa de novo, ao mesmo tempo, os indexadores fora do ar — no Prowlarr
    /// e em cada Radarr/Sonarr que o tem de castigo. Devolve os que continuam fora.
    pub async fn retest_indexers(self: &Arc<Self>) -> Vec<String> {
        let down = self.refresh_indexers().await;
        if down.is_empty() {
            return down;
        }
        let mut set = tokio::task::JoinSet::new();
        for which in ["prowlarr", "radarr", "sonarr"] {
            let list = match which {
                "prowlarr" => match &self.prowlarr {
                    Some(p) => p.get_json("indexer").await,
                    None => continue,
                },
                "radarr" => match &self.radarr {
                    Some(a) => a.get_json("indexer").await,
                    None => continue,
                },
                _ => match &self.sonarr {
                    Some(a) => a.get_json("indexer").await,
                    None => continue,
                },
            };
            let list = match list {
                Ok(l) => l,
                Err(e) => {
                    warn!("indexadores: {e:#}");
                    continue;
                }
            };
            for i in list.as_array().into_iter().flatten().filter(|i| i["name"].as_str().is_some_and(|n| down.iter().any(|d| d == base_name(n)))) {
                let (me, i) = (self.clone(), i.clone());
                set.spawn(async move {
                    let r = match which {
                        "prowlarr" => me.prowlarr.as_ref().unwrap().test_indexer(&i).await,
                        "radarr" => me.radarr.as_ref().unwrap().test_indexer(&i).await,
                        _ => me.sonarr.as_ref().unwrap().test_indexer(&i).await,
                    };
                    let name = i["name"].as_str().unwrap_or_default();
                    match r {
                        Ok(()) => info!("{which}: indexador {name} respondeu, saiu do castigo"),
                        Err(e) => warn!("{which}: indexador {name} continua fora: {e:#}"),
                    }
                });
            }
        }
        while set.join_next().await.is_some() {}
        self.refresh_indexers().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_the_indexers_down_from_arr_health() {
        let health = json!([
            { "source": "UpdateCheck", "type": "warning", "message": "New update is available: v4.0.20.3014" },
            { "source": "IndexerLongTermStatusCheck", "type": "warning", "message": "Indexers unavailable due to failures for more than 6 hours: bitmagnet (DHT) (Prowlarr)" },
            { "source": "IndexerStatusCheck", "type": "warning", "message": "Indexers unavailable due to failures: 1337x (Prowlarr), Comando Torrents (BR) (Prowlarr)" },
        ]);
        let (names, all) = down_in_health(&health);
        assert_eq!(names, ["bitmagnet (DHT)", "1337x", "Comando Torrents (BR)"]);
        assert!(!all);
        let (names, all) = down_in_health(&json!([{ "source": "IndexerStatusCheck", "message": "All indexers are unavailable due to failures" }]));
        assert!(names.is_empty() && all);
        assert_eq!(enabled_names(&json!([{ "name": "Nyaa.si (Prowlarr)", "enable": true }, { "name": "Velho", "enable": false }])), ["Nyaa.si"]);
    }

    #[test]
    fn reads_the_indexers_down_from_prowlarr() {
        let now: DateTime<Utc> = "2026-09-30T12:00:00Z".parse().unwrap();
        let indexers = json!([{ "id": 1, "name": "1337x" }, { "id": 3, "name": "Comando Torrents (BR)" }, { "id": 6, "name": "bitmagnet (DHT)" }]);
        let status = json!([
            { "indexerId": 1, "disabledTill": "2026-09-30T12:30:00Z" },
            // castigo que já acabou
            { "indexerId": 3, "disabledTill": "2026-09-30T11:00:00Z" },
            { "indexerId": 6, "mostRecentFailure": "2026-09-30T11:59:00Z" },
        ]);
        assert_eq!(down_in_prowlarr(&status, &indexers, now), ["1337x"]);
    }
}
