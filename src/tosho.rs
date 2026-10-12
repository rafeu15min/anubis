//! As faixas de verdade de um torrent de anime, pelo Anime Tosho: ele baixa
//! os torrents de anime públicos (Nyaa, nekoBT, AniDex…) e publica o
//! MediaInfo de cada arquivo — os idiomas de cada faixa de áudio e de
//! legenda. O nome da versão engana: "MULTi" e "Multi-Audio" podem não ter
//! português (japonês, inglês, hindi, tâmil…), e "Dual Audio" pode trazer
//! legenda em português sem dizer. Achado pelo hash do torrent, então vale
//! para a versão de qualquer indexador.
//!
//! Torrent que o Tosho não tem (ou ainda não processou) fica sem faixas, e
//! vale o nome da versão, como antes.

use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::warn;

const API: &str = "https://feed.animetosho.net/json?show=torrent&btih=";
/// consultas ao mesmo tempo (é um serviço gratuito)
const PARALLEL: usize = 6;
/// torrent sem faixas (desconhecido ou ainda não processado): pergunta de novo depois disso
const RETRY: Duration = Duration::from_secs(6 * 3600);

/// Idiomas das faixas, como o MediaInfo escreve ("Japanese", "Portuguese (BR)").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tracks {
    pub audio: Vec<String>,
    pub subs: Vec<String>,
}

impl Tracks {
    pub fn portuguese_audio(&self) -> bool {
        self.audio.iter().any(|l| l.starts_with("Portuguese"))
    }

    pub fn portuguese_subs(&self) -> bool {
        self.subs.iter().any(|l| l.starts_with("Portuguese"))
    }

    /// tem áudio neste idioma ("Japanese", como o Sonarr escreve o original;
    /// o MediaInfo às vezes escreve "Japanese (JP)")
    pub fn has_audio(&self, language: &str) -> bool {
        self.audio.iter().any(|l| l == language || l.strip_prefix(language).is_some_and(|r| r.starts_with(" (")))
    }

    /// Os idiomas do áudio no formato do Radarr/Sonarr (para o `languages` da versão).
    pub fn arr_languages(&self) -> Vec<Value> {
        let mut out: Vec<Value> = vec![];
        for l in &self.audio {
            let name = if l.starts_with("Portuguese") { "Portuguese (Brazil)" } else { l.as_str() };
            if !name.is_empty() && !out.iter().any(|v| v["name"] == name) {
                out.push(serde_json::json!({ "name": name }));
            }
        }
        out
    }
}

type Seen = Arc<Mutex<HashMap<String, (Instant, Option<Tracks>)>>>;

pub struct Tosho {
    http: Client,
    cache: Seen,
}

impl Tosho {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self { http: Client::builder().timeout(Duration::from_secs(15)).build()?, cache: Default::default() })
    }

    /// As faixas de vários torrents (hash em hexadecimal), no máximo até
    /// `limit`: o que não respondeu a tempo fica de fora (e no cache, se
    /// terminar depois).
    pub async fn tracks(&self, hashes: &[String], limit: Duration) -> HashMap<String, Tracks> {
        let mut out = HashMap::new();
        let mut todo = vec![];
        {
            let cache = self.cache.lock().await;
            for h in hashes {
                let h = h.to_lowercase();
                match cache.get(&h) {
                    Some((_, Some(t))) => {
                        out.insert(h, t.clone());
                    }
                    Some((at, None)) if at.elapsed() < RETRY => {}
                    _ if !todo.contains(&h) => todo.push(h),
                    _ => {}
                }
            }
        }
        let fetch = async {
            for chunk in todo.chunks(PARALLEL) {
                let mut set = tokio::task::JoinSet::new();
                for h in chunk {
                    let (http, cache, h) = (self.http.clone(), self.cache.clone(), h.clone());
                    set.spawn(async move {
                        let t = fetch(&http, &h).await;
                        cache.lock().await.insert(h.clone(), (Instant::now(), t.clone()));
                        (h, t)
                    });
                }
                while let Some(Ok((h, t))) = set.join_next().await {
                    if let Some(t) = t {
                        out.insert(h, t);
                    }
                }
            }
        };
        let _ = tokio::time::timeout(limit, fetch).await;
        out
    }
}

async fn fetch(http: &Client, hash: &str) -> Option<Tracks> {
    let res = match http.get(format!("{API}{hash}")).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("Anime Tosho: {e:#}");
            return None;
        }
    };
    // 404: o Tosho não tem esse torrent
    if !res.status().is_success() {
        return None;
    }
    parse(&res.json::<Value>().await.ok()?)
}

/// As faixas do primeiro arquivo com MediaInfo (o vídeo; num pacote, os
/// episódios têm as mesmas faixas). Legendas extraídas pelo Tosho valem
/// quando o MediaInfo não lista nenhuma.
pub(crate) fn parse(v: &Value) -> Option<Tracks> {
    let files = v["files"].as_array()?;
    let file = files.iter().find(|f| f["info"]["mediainfo"].is_string())?;
    let mut t = Tracks::default();
    for block in file["info"]["mediainfo"].as_str().unwrap_or_default().split("\n\n") {
        let head = block.lines().next().unwrap_or_default();
        let lang = block
            .lines()
            .find_map(|l| l.strip_prefix("Language").and_then(|r| r.trim_start().strip_prefix(':')).map(|v| v.trim().to_owned()))
            .unwrap_or_default();
        if head.starts_with("Audio") {
            t.audio.push(lang);
        } else if head.starts_with("Text") {
            t.subs.push(lang);
        }
    }
    if t.subs.is_empty() {
        t.subs = file["attachments"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|a| a["type"] == "subtitle")
            .filter_map(|a| a["info"]["language"].as_str().map(str::to_owned))
            .collect();
    }
    (!t.audio.is_empty()).then_some(t)
}

/// O hash de uma versão do Prowlarr: o campo `infoHash` ou o do magnet.
pub(crate) fn info_hash(release: &Value) -> Option<String> {
    if let Some(h) = release["infoHash"].as_str().filter(|h| h.len() == 40) {
        return Some(h.to_lowercase());
    }
    [&release["magnetUrl"], &release["guid"]].into_iter().filter_map(Value::as_str).find_map(|m| {
        let h = m.strip_prefix("magnet:?")?.split('&').find_map(|p| p.strip_prefix("xt=urn:btih:"))?;
        (h.len() == 40 && h.chars().all(|c| c.is_ascii_hexdigit())).then(|| h.to_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MI: &str = "General\nComplete name : a.mkv\n\nVideo\nID : 1\nFormat : AV1\n\nAudio #1\nID : 2\nLanguage                                 : Japanese\n\nAudio #2\nLanguage : Portuguese (BR)\n\nText #1\nTitle : Full Subtitles\nLanguage : English\n\nText #2\nTitle : Brazilian_CR\nLanguage : Portuguese (BR)";

    #[test]
    fn reads_the_languages_of_each_track() {
        let v = json!({ "files": [{ "info": null }, { "info": { "mediainfo": MI }, "attachments": [] }] });
        let t = parse(&v).unwrap();
        assert_eq!(t.audio, ["Japanese", "Portuguese (BR)"]);
        assert_eq!(t.subs, ["English", "Portuguese (BR)"]);
        assert!(t.portuguese_audio() && t.portuguese_subs() && t.has_audio("Japanese"));
        assert_eq!(t.arr_languages(), [json!({ "name": "Japanese" }), json!({ "name": "Portuguese (Brazil)" })]);
    }

    #[test]
    fn multi_audio_without_portuguese_is_not_dubbed() {
        let mi = "Audio #1\nLanguage : Japanese\n\nAudio #2\nLanguage : English\n\nAudio #3\nLanguage : Hindi\n\nText #1\nLanguage : English";
        let t = parse(&json!({ "files": [{ "info": { "mediainfo": mi } }] })).unwrap();
        assert!(!t.portuguese_audio() && !t.portuguese_subs());
    }

    #[test]
    fn extracted_subtitles_count_when_mediainfo_has_none() {
        let v = json!({ "files": [{ "info": { "mediainfo": "Audio\nLanguage : Japanese" }, "attachments": [
            { "type": "subtitle", "info": { "language": "Portuguese (Brazil)" } }, { "type": "font", "info": {} } ] }] });
        assert_eq!(parse(&v).unwrap().subs, ["Portuguese (Brazil)"]);
        // nada processado: sem faixas
        assert_eq!(parse(&json!({ "files": [] })), None);
        assert_eq!(parse(&json!({ "error": "Requested torrent not found" })), None);
    }

    #[test]
    fn original_language_with_region() {
        let t = Tracks { audio: vec!["Japanese (JP)".into()], subs: vec![] };
        assert!(t.has_audio("Japanese") && !t.has_audio("Japan") && !t.has_audio("English"));
    }

    #[test]
    fn hash_from_field_or_magnet() {
        let h = "05bd3a45348e1338117e89cf1e25fd76b7a30de3";
        assert_eq!(info_hash(&json!({ "infoHash": h.to_uppercase() })), Some(h.to_owned()));
        assert_eq!(info_hash(&json!({ "guid": format!("magnet:?xt=urn:btih:{h}&dn=x") })), Some(h.to_owned()));
        assert_eq!(info_hash(&json!({ "guid": "https://nyaa.si/view/1" })), None);
    }
}
