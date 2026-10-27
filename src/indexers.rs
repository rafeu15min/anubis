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
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// a lista fica guardada por 1 min (cada consulta são 4–5 chamadas)
const TTL: Duration = Duration::from_secs(60);
/// de quanto em quanto tempo os indexadores de castigo são testados sozinhos
const RETEST_EVERY: Duration = Duration::from_secs(15 * 60);

/// Teto do intervalo entre testes de um indexador que não para de falhar.
const RETEST_MAX: Duration = Duration::from_secs(6 * 3600);

/// Intervalo até o próximo teste de quem já falhou `fails` vezes seguidas:
/// dobra a cada falha, até `RETEST_MAX`. Testar de 15 em 15 min um site que
/// bloqueou o IP (1337x, erro 1006 do Cloudflare) só renova o bloqueio e o
/// castigo do Prowlarr.
fn retest_delay(fails: u32) -> Duration {
    RETEST_EVERY.saturating_mul(1 << fails.min(5)).min(RETEST_MAX)
}

/// Guarda quando cada um que continua fora pode ser testado de novo; quem
/// voltou sai da lista.
fn reschedule(next: &mut HashMap<String, (Instant, u32)>, tested: &[String], still: &[String], now: Instant) {
    for n in tested {
        if !still.contains(n) {
            next.remove(n);
            continue;
        }
        let fails = next.get(n).map_or(1, |(_, f)| f + 1);
        next.insert(n.clone(), (now + retest_delay(fails), fails));
    }
}

/// Esquece quem saiu da lista só depois de `RETEST_MAX` do último agendamento:
/// o aviso de saúde dos *arr some e volta por um ciclo, e zerar o recuo aí
/// recomeçaria os testes ao site bloqueado.
fn forget_old(next: &mut HashMap<String, (Instant, u32)>, down: &[String], now: Instant) {
    next.retain(|n, (at, _)| down.contains(n) || now < *at + RETEST_MAX);
}

/// Testa de novo, de tempos em tempos, os indexadores de castigo. Uma queda
/// curta da internet deixava todos de castigo por horas (o Prowlarr estica o
/// castigo a cada falha): as buscas voltavam vazias muito depois de a
/// conexão voltar. O teste que passa tira do castigo na hora. Quem continua
/// falhando é testado cada vez mais devagar (`retest_delay`).
pub async fn run(orch: Arc<Orchestrator>) {
    let mut next: HashMap<String, (Instant, u32)> = HashMap::new();
    loop {
        tokio::time::sleep(RETEST_EVERY).await;
        let down = orch.refresh_indexers().await;
        let now = Instant::now();
        forget_old(&mut next, &down, now);
        let skip: HashSet<String> = next.iter().filter(|(_, (at, _))| *at > now).map(|(n, _)| n.clone()).collect();
        let due: Vec<String> = down.iter().filter(|n| !skip.contains(*n)).cloned().collect();
        if due.is_empty() {
            continue;
        }
        info!("indexadores de castigo: {}; testando de novo", due.join(", "));
        let still = orch.retest_indexers(&skip).await;
        if !still.is_empty() {
            warn!("continuam fora: {}", still.join(", "));
        }
        reschedule(&mut next, &due, &still, now);
    }
}

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

/// Fora do ar segundo o Prowlarr: castigo (`disabledTill`) ainda valendo, só
/// dos ligados (o desativado guarda o castigo antigo).
fn down_in_prowlarr(status: &Value, indexers: &Value, now: DateTime<Utc>) -> Vec<String> {
    let name_of = |id: i64| indexers.as_array()?.iter().find(|i| i["id"].as_i64() == Some(id) && i["enable"] != false)?["name"].as_str().map(str::to_owned);
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

/// Quem o Radarr/Sonarr dão como fora mas o Prowlarr não pôs de castigo, com o
/// id no Prowlarr: o Radarr/Sonarr chegam ao site pelo Prowlarr, e o 429 do
/// limite de consultas dele (1337x: 1 por hora) vira "falha" do lado deles.
fn suspects(arr_down: &[String], prowlarr_down: &[String], prowlarr_list: &Value) -> Vec<(String, i64)> {
    let id_of = |n: &str| prowlarr_list.as_array()?.iter().find(|i| i["enable"] != false && i["name"].as_str() == Some(n))?["id"].as_i64();
    arr_down.iter().filter(|n| !prowlarr_down.contains(n)).filter_map(|n| Some((n.clone(), id_of(n)?))).collect()
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
    /// Quem só bateu o limite de consultas do Prowlarr não conta como fora:
    /// volta sozinho quando o limite vence, e testá-lo antes disso só daria
    /// outro 429 (e, no Radarr/Sonarr, mais castigo).
    pub async fn refresh_indexers(&self) -> Vec<String> {
        let mut arr_down = vec![];
        for arr in [&self.radarr, &self.sonarr].into_iter().flatten() {
            match down_in_arr(arr).await {
                Ok(n) => arr_down.extend(n),
                Err(e) => warn!("indexadores: {e:#}"),
            }
        }
        arr_down.sort();
        arr_down.dedup();
        let mut down = match &self.prowlarr {
            Some(p) => self.without_limited(p, arr_down).await,
            None => arr_down,
        };
        down.sort();
        down.dedup();
        *self.indexers.0.lock().await = Some((Instant::now(), down.clone()));
        down
    }

    /// Junta os de castigo no Prowlarr e tira dos do Radarr/Sonarr os que só
    /// estão no limite de consultas do Prowlarr.
    async fn without_limited(&self, p: &crate::services::Prowlarr, mut arr_down: Vec<String>) -> Vec<String> {
        let (status, list) = match tokio::try_join!(p.get_json("indexerstatus"), p.get_json("indexer")) {
            Ok(v) => v,
            Err(e) => {
                warn!("indexadores: {e:#}");
                return arr_down;
            }
        };
        let prowlarr_down = down_in_prowlarr(&status, &list, Utc::now());
        for (name, id) in suspects(&arr_down, &prowlarr_down, &list) {
            let Ok(Some(wait)) = p.query_limited(id).await else { continue };
            debug!("{name}: limite de consultas do Prowlarr, libera em {} min (não conta como fora)", wait.as_secs().div_ceil(60));
            arr_down.retain(|n| *n != name);
        }
        arr_down.extend(prowlarr_down);
        arr_down
    }

    /// Testa de novo, um de cada vez, os indexadores fora do ar — no Prowlarr
    /// e em cada Radarr/Sonarr que o tem de castigo. Devolve os que continuam
    /// fora. Um de cada vez: os brasileiros passam todos pelo mesmo
    /// FlareSolverr, que atende um pedido por vez; 9 testes juntos estouravam
    /// o tempo e os deixavam de castigo. Os nomes em `skip` não são testados, e
    /// o que falhou no Prowlarr não é testado de novo no Radarr/Sonarr.
    pub async fn retest_indexers(self: &Arc<Self>, skip: &HashSet<String>) -> Vec<String> {
        let down = self.refresh_indexers().await;
        if down.is_empty() {
            return down;
        }
        let mut failed: HashSet<String> = HashSet::new();
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
            // o teste do Radarr/Sonarr passa pelo Prowlarr: se o dele falhou, o
            // deles falharia igual, e seria mais um pedido ao site bloqueado
            let blocked = if which == "prowlarr" { HashSet::new() } else { failed.clone() };
            for i in list.as_array().into_iter().flatten().filter(|i| i["name"].as_str().is_some_and(|n| down.iter().any(|d| d == base_name(n)) && !skip.contains(base_name(n)) && !blocked.contains(base_name(n)))) {
                let r = match which {
                    "prowlarr" => self.prowlarr.as_ref().unwrap().test_indexer(i).await,
                    "radarr" => self.radarr.as_ref().unwrap().test_indexer(i).await,
                    _ => self.sonarr.as_ref().unwrap().test_indexer(i).await,
                };
                let name = i["name"].as_str().unwrap_or_default();
                failed.extend((which == "prowlarr" && r.is_err()).then(|| base_name(name).to_owned()));
                match r {
                    Ok(()) => info!("{which}: indexador {name} respondeu, saiu do castigo"),
                    Err(e) => warn!("{which}: indexador {name} continua fora: {e:#}"),
                }
            }
        }
        self.refresh_indexers().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn retest_slows_down_and_stops_at_the_cap() {
        let mins = |f| retest_delay(f).as_secs() / 60;
        assert_eq!([mins(1), mins(2), mins(3), mins(4)], [30, 60, 120, 240]);
        assert_eq!(retest_delay(5), RETEST_MAX);
        assert_eq!(retest_delay(40), RETEST_MAX);
    }

    #[test]
    fn forget_old_keeps_the_count_through_a_short_absence() {
        let now = Instant::now();
        let mut next = HashMap::from([("1337x".to_string(), (now, 3))]);
        forget_old(&mut next, &[], now);
        assert_eq!(next["1337x"].1, 3);
        forget_old(&mut next, &[], now + RETEST_MAX);
        assert!(next.is_empty());
    }

    #[test]
    fn reschedule_counts_failures_and_forgets_recovered() {
        let (now, mut next) = (Instant::now(), HashMap::new());
        let names = ["1337x".to_string(), "EZTV".to_string()];
        reschedule(&mut next, &names, &names, now);
        reschedule(&mut next, &names, &names[..1], now);
        assert_eq!(next["1337x"].1, 2);
        assert!(!next.contains_key("EZTV"));
    }

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
        let indexers = json!([{ "id": 1, "name": "1337x" }, { "id": 3, "name": "Comando Torrents (BR)" }, { "id": 6, "name": "bitmagnet (DHT)" }, { "id": 7, "name": "Desligado", "enable": false }]);
        let status = json!([
            { "indexerId": 1, "disabledTill": "2026-09-30T12:30:00Z" },
            // desativado de propósito: o castigo antigo não conta
            { "indexerId": 7, "disabledTill": "2026-09-30T12:30:00Z" },
            // castigo que já acabou
            { "indexerId": 3, "disabledTill": "2026-09-30T11:00:00Z" },
            { "indexerId": 6, "mostRecentFailure": "2026-09-30T11:59:00Z" },
        ]);
        assert_eq!(down_in_prowlarr(&status, &indexers, now), ["1337x"]);
    }

    /// Só vira suspeito de "limite de consultas" quem o Prowlarr não pôs de
    /// castigo e que existe ligado nele (os demais seguem como fora).
    #[test]
    fn suspects_are_down_in_arr_but_fine_in_prowlarr() {
        let list = json!([
            { "id": 1, "name": "1337x", "enable": true },
            { "id": 3, "name": "Comando Torrents (BR)", "enable": true },
            { "id": 6, "name": "bitmagnet (DHT)", "enable": false },
        ]);
        let arr_down = ["1337x", "Comando Torrents (BR)", "bitmagnet (DHT)", "Só no Sonarr"].map(String::from);
        let prowlarr_down = ["Comando Torrents (BR)".to_owned()];
        assert_eq!(suspects(&arr_down, &prowlarr_down, &list), [("1337x".to_owned(), 1)]);
    }
}
