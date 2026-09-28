//! Preferências que a pessoa controla pela página do Anubis: idioma do áudio
//! (filmes e séries) e legendas. A fonte da verdade continua no Radarr, no
//! Sonarr e no Bazarr — o Anubis lê de lá e grava lá, sem estado próprio.
//!
//! Idioma: um formato personalizado "Anubis · Português (BR)" em cada Arr,
//! pontuado no perfil de qualidade dos pedidos:
//! - obrigatório: pontuação mínima = a do formato (o resto é recusado);
//! - preferido: aceita qualquer um e troca pela versão em PT-BR quando surgir;
//! - tanto faz: sem pontuação.

use crate::orchestrator::Orchestrator;
use crate::services::{Arr, Method};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const FORMAT: &str = "Anubis · Português (BR)";
const SCORE: i64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioRule {
    Required,
    Preferred,
    Any,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubPrefs {
    /// códigos do Bazarr: "pb" (PT-BR), "pt", "en", "es"…
    pub languages: Vec<String>,
    /// não baixar legenda quando o áudio já está no idioma
    pub skip_if_audio: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prefs {
    pub movies_audio: Option<AudioRule>,
    pub series_audio: Option<AudioRule>,
    pub subs: Option<SubPrefs>,
}

impl Orchestrator {
    fn arr_named(&self, which: &str) -> Result<(&Arr, String, i64)> {
        // (cliente, perfil dos pedidos, id do idioma PT-BR naquele Arr)
        match which {
            "radarr" => Ok((self.radarr.as_ref().context("Radarr não configurado")?, self.cfg.radarr.as_ref().unwrap().quality_profile.clone(), 30)),
            _ => Ok((self.sonarr.as_ref().context("Sonarr não configurado")?, self.cfg.sonarr.as_ref().unwrap().quality_profile.clone(), 33)),
        }
    }

    async fn profile(arr: &Arr, name: &str) -> Result<Value> {
        let all = arr.get_json("qualityprofile").await?;
        all.as_array().and_then(|a| a.iter().find(|p| p["name"] == name).cloned()).with_context(|| format!("perfil \"{name}\" não existe"))
    }

    async fn audio_rule(&self, which: &str) -> Result<AudioRule> {
        let (arr, profile_name, _) = self.arr_named(which)?;
        let profile = Self::profile(arr, &profile_name).await?;
        let formats = arr.get_json("customformat").await?;
        let fid = formats.as_array().and_then(|a| a.iter().find(|f| f["name"] == FORMAT)).and_then(|f| f["id"].as_i64());
        let score = fid
            .and_then(|id| profile["formatItems"].as_array()?.iter().find(|i| i["format"].as_i64() == Some(id)).and_then(|i| i["score"].as_i64()))
            .unwrap_or(0);
        let min = profile["minFormatScore"].as_i64().unwrap_or(0);
        // o campo de idioma do perfil (só no Radarr) também pode exigir PT-BR
        let legacy = profile["language"]["name"].as_str() == Some("Portuguese (Brazil)");
        Ok(if (score > 0 && min >= score) || legacy {
            AudioRule::Required
        } else if score > 0 {
            AudioRule::Preferred
        } else {
            AudioRule::Any
        })
    }

    async fn set_audio_rule(&self, which: &str, rule: AudioRule) -> Result<()> {
        let (arr, profile_name, lang_id) = self.arr_named(which)?;
        // o formato personalizado (cria na primeira vez)
        let formats = arr.get_json("customformat").await?;
        let fid = match formats.as_array().and_then(|a| a.iter().find(|f| f["name"] == FORMAT)).and_then(|f| f["id"].as_i64()) {
            Some(id) => id,
            None => {
                let body = json!({
                    "name": FORMAT,
                    "includeCustomFormatWhenRenaming": false,
                    "specifications": [{
                        "name": "Português (Brasil)",
                        "implementation": "LanguageSpecification",
                        "negate": false,
                        "required": true,
                        "fields": [{ "name": "value", "value": lang_id }, { "name": "exceptLanguage", "value": false }]
                    }]
                });
                arr.send_json(Method::POST, "customformat", &body).await?["id"].as_i64().context("formato sem id")?
            }
        };
        // o perfil já traz formatItems de todos os formatos (score 0 nos novos)
        let mut profile = Self::profile(arr, &profile_name).await?;
        let pid = profile["id"].as_i64().context("perfil sem id")?;
        let score = if rule == AudioRule::Any { 0 } else { SCORE };
        let items = profile["formatItems"].as_array_mut().context("perfil sem formatItems")?;
        match items.iter_mut().find(|i| i["format"].as_i64() == Some(fid)) {
            Some(i) => i["score"] = json!(score),
            None => items.push(json!({ "format": fid, "name": FORMAT, "score": score })),
        }
        profile["minFormatScore"] = json!(if rule == AudioRule::Required { SCORE } else { 0 });
        // preferido: continua trocando até achar a versão em PT-BR
        profile["cutoffFormatScore"] = json!(score);
        profile["upgradeAllowed"] = json!(true);
        if profile.get("language").is_some() {
            // Radarr: o idioma passa a ser decidido pelo formato
            profile["language"] = json!({ "id": -1, "name": "Any" });
        }
        arr.send_json(Method::PUT, &format!("qualityprofile/{pid}"), &profile).await?;
        Ok(())
    }

    async fn subs_prefs(&self) -> Result<Option<SubPrefs>> {
        let Some(b) = &self.bazarr else { return Ok(None) };
        let profiles = b.profiles().await?;
        let Some(p) = profiles.first() else { return Ok(Some(SubPrefs { languages: vec![], skip_if_audio: true })) };
        let items = p["items"].as_array().cloned().unwrap_or_default();
        Ok(Some(SubPrefs {
            languages: items.iter().filter_map(|i| i["language"].as_str().map(str::to_owned)).collect(),
            skip_if_audio: items.first().is_none_or(|i| i["audio_exclude"] == "True"),
        }))
    }

    async fn set_subs_prefs(&self, s: &SubPrefs) -> Result<()> {
        let Some(b) = &self.bazarr else { bail!("Bazarr não configurado") };
        let flag = if s.skip_if_audio { "True" } else { "False" };
        let items: Vec<Value> = s
            .languages
            .iter()
            .enumerate()
            .map(|(i, l)| json!({ "id": i + 1, "language": l, "audio_exclude": flag, "audio_only_include": "False", "hi": "False", "forced": "False" }))
            .collect();
        let profile = json!([{ "profileId": 1, "name": "Anubis", "items": items, "cutoff": null, "mustContain": [], "mustNotContain": [], "originalFormat": false, "tag": null }]);
        let mut form: Vec<(&str, String)> = s.languages.iter().map(|l| ("languages-enabled", l.clone())).collect();
        form.push(("languages-profiles", profile.to_string()));
        b.save(&form).await
    }

    pub async fn prefs(&self) -> Result<Prefs> {
        Ok(Prefs {
            movies_audio: if self.radarr.is_some() { Some(self.audio_rule("radarr").await?) } else { None },
            series_audio: if self.sonarr.is_some() { Some(self.audio_rule("sonarr").await?) } else { None },
            subs: self.subs_prefs().await?,
        })
    }

    pub async fn set_prefs(&self, p: &Prefs) -> Result<Prefs> {
        if let Some(r) = p.movies_audio {
            self.set_audio_rule("radarr", r).await?;
        }
        if let Some(r) = p.series_audio {
            self.set_audio_rule("sonarr", r).await?;
        }
        if let Some(s) = &p.subs {
            self.set_subs_prefs(s).await?;
        }
        self.prefs().await
    }
}
