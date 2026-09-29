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
/// legenda em português pelo nome da versão; vale menos que o dublado
const SUBS_FORMAT: &str = "Anubis · Legenda PT-BR";
const SUBS_SCORE: i64 = 10;
const SUBS_REGEX: &str = r"\b(legendad[oa]s?|leg|multi[ ._-]?subs?|pt[ ._-]?br[ ._-]?subs?|subs?[ ._-]?pt[ ._-]?br)\b";
/// Sonarr: perfil de uma série (ou anime) sem versão dublada em nenhuma
/// fonte, quando a pessoa aceita o legendado: baixa legendado e troca pelo
/// dublado se ele surgir
pub const FALLBACK_PROFILE: &str = "Anubis · Sem dublagem";

/// Ids dos perfis "Sem dublagem" que o Anubis usa (None: Arr não configurado).
#[derive(Debug, Clone, Copy)]
pub struct ProfileIds {
    pub series: Option<i64>,
    pub movies: Option<i64>,
}

/// A regra de idioma num perfil: pontos do dublado (e da legenda em
/// português, onde o legendado é aceito), mínimo e alvo.
fn apply_audio(profile: &mut Value, dub: i64, subs: Option<i64>, rule: AudioRule) -> Result<()> {
    let score = if rule == AudioRule::Any { 0 } else { SCORE };
    let items = profile["formatItems"].as_array_mut().context("perfil sem formatItems")?;
    let mut set = |fid: i64, name: &str, score: i64| match items.iter_mut().find(|i| i["format"].as_i64() == Some(fid)) {
        Some(i) => i["score"] = json!(score),
        None => items.push(json!({ "format": fid, "name": name, "score": score })),
    };
    set(dub, FORMAT, score);
    if let Some(s) = subs {
        set(s, SUBS_FORMAT, SUBS_SCORE);
    }
    profile["minFormatScore"] = json!(if rule == AudioRule::Required { SCORE } else { 0 });
    // preferido: continua trocando até achar a versão em PT-BR
    profile["cutoffFormatScore"] = json!(score);
    profile["upgradeAllowed"] = json!(true);
    if profile.get("language").is_some() {
        // Radarr: o idioma passa a ser decidido pelo formato
        profile["language"] = json!({ "id": -1, "name": "Any" });
    }
    Ok(())
}

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

/// Qualidade dos pedidos (além do idioma), no mesmo perfil do Radarr/Sonarr.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityPrefs {
    /// Bluray 1080p aceito e preferido, logo acima do WEB 1080p — que vira a
    /// segunda opção e já conta como bom o bastante (não baixa de novo
    /// sozinho; o "assistir" oferece a troca)
    pub bluray_first: bool,
    /// recusar gravações de cinema (só filmes: o Sonarr não tem essas qualidades)
    pub no_cinema: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prefs {
    pub movies_audio: Option<AudioRule>,
    pub series_audio: Option<AudioRule>,
    pub subs: Option<SubPrefs>,
    #[serde(default)]
    pub movies_quality: Option<QualityPrefs>,
    #[serde(default)]
    pub series_quality: Option<QualityPrefs>,
}

const CINEMA: [&str; 4] = ["CAM", "TELESYNC", "TELECINE", "WORKPRINT"];
const BLURAY: &str = "Bluray-1080p";
const WEB: [&str; 2] = ["WEBDL-1080p", "WEBRip-1080p"];

/// Qualidades de um item do perfil: a própria, ou as de um grupo.
fn names(item: &Value) -> Vec<&str> {
    match item["quality"]["name"].as_str() {
        Some(n) => vec![n],
        None => item["items"].as_array().into_iter().flatten().filter_map(|i| i["quality"]["name"].as_str()).collect(),
    }
}

/// Id que o perfil usa no `cutoff`: o da qualidade ou o do grupo.
fn cutoff_id(item: &Value) -> Option<i64> {
    item["quality"]["id"].as_i64().or_else(|| item["id"].as_i64())
}

fn quality_prefs(profile: &Value) -> QualityPrefs {
    let items = profile["items"].as_array().cloned().unwrap_or_default();
    let allowed = |q: &str| items.iter().any(|i| names(i).contains(&q) && i["allowed"].as_bool() == Some(true));
    let has_cinema = items.iter().any(|i| names(i).iter().any(|n| CINEMA.contains(n)));
    QualityPrefs { bluray_first: allowed(BLURAY), no_cinema: has_cinema.then(|| !CINEMA.iter().any(|c| allowed(c))) }
}

/// Aplica as escolhas no perfil (a lista vai da pior para a melhor qualidade).
fn apply_quality(profile: &mut Value, q: &QualityPrefs) -> Result<()> {
    let cutoff = profile["cutoff"].as_i64();
    let items = profile["items"].as_array_mut().context("perfil sem qualidades")?;
    let pos = items.iter().position(|i| i["quality"]["name"] == BLURAY).context("Bluray 1080p está num grupo do perfil — separe no Radarr/Sonarr")?;
    let mut bluray = items.remove(pos);
    bluray["allowed"] = json!(q.bluray_first);
    // primeira opção: logo acima da última entrada WEB 1080p
    let web = items.iter().rposition(|i| names(i).iter().any(|n| WEB.contains(n))).context("perfil sem WEB 1080p")?;
    let web_dl = items.iter().find(|i| names(i).contains(&WEB[0])).and_then(cutoff_id);
    let bluray_id = cutoff_id(&bluray);
    items.insert(web + 1, bluray);
    if let Some(no) = q.no_cinema {
        for i in items.iter_mut().filter(|i| names(i).iter().any(|n| CINEMA.contains(n))) {
            i["allowed"] = json!(!no);
        }
    }
    // alvo no WEB: um WEB baixado já basta (a troca pelo Bluray é oferecida no
    // "assistir", não feita sozinha). Um alvo escolhido à mão fica como está.
    if cutoff == bluray_id {
        profile["cutoff"] = json!(web_dl);
    }
    Ok(())
}

impl Orchestrator {
    fn arr_named(&self, which: &str) -> Result<(&Arr, String, i64)> {
        // (cliente, perfil dos pedidos, id do idioma PT-BR naquele Arr)
        match which {
            "radarr" => Ok((self.radarr.as_ref().context("Radarr não configurado")?, self.cfg.radarr.as_ref().unwrap().quality_profile.clone(), 30)),
            _ => Ok((self.sonarr.as_ref().context("Sonarr não configurado")?, self.cfg.sonarr.as_ref().unwrap().quality_profile.clone(), 33)),
        }
    }

    pub(crate) async fn profile(arr: &Arr, name: &str) -> Result<Value> {
        Self::find_profile(arr, name).await?.with_context(|| format!("perfil \"{name}\" não existe"))
    }

    async fn find_profile(arr: &Arr, name: &str) -> Result<Option<Value>> {
        let all = arr.get_json("qualityprofile").await?;
        Ok(all.as_array().and_then(|a| a.iter().find(|p| p["name"] == name).cloned()))
    }

    /// Id de um formato personalizado do Anubis, criando na primeira vez.
    async fn format_id(arr: &Arr, name: &str, spec: Value) -> Result<i64> {
        let formats = arr.get_json("customformat").await?;
        if let Some(id) = formats.as_array().and_then(|a| a.iter().find(|f| f["name"] == name)).and_then(|f| f["id"].as_i64()) {
            return Ok(id);
        }
        let body = json!({ "name": name, "includeCustomFormatWhenRenaming": false, "specifications": [spec] });
        arr.send_json(Method::POST, "customformat", &body).await?["id"].as_i64().context("formato sem id")
    }

    async fn dub_format(arr: &Arr, lang_id: i64) -> Result<i64> {
        let spec = json!({
            "name": "Português (Brasil)", "implementation": "LanguageSpecification", "negate": false, "required": true,
            "fields": [{ "name": "value", "value": lang_id }, { "name": "exceptLanguage", "value": false }]
        });
        Self::format_id(arr, FORMAT, spec).await
    }

    /// Legenda em português pelo nome da versão: entre as não dubladas, estas
    /// vêm primeiro (o Bazarr continua buscando legenda para as outras).
    async fn subs_format(arr: &Arr) -> Result<i64> {
        let spec = json!({
            "name": "Legendado / MultiSub", "implementation": "ReleaseTitleSpecification", "negate": false, "required": true,
            "fields": [{ "name": "value", "value": SUBS_REGEX }]
        });
        Self::format_id(arr, SUBS_FORMAT, spec).await
    }

    async fn audio_rule(&self, which: &str) -> Result<AudioRule> {
        let (arr, profile_name, _) = self.arr_named(which)?;
        self.rule_of(arr, &profile_name).await
    }

    async fn rule_of(&self, arr: &Arr, profile_name: &str) -> Result<AudioRule> {
        let profile = Self::profile(arr, profile_name).await?;
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
        self.set_rule_of(arr, &profile_name, lang_id, rule).await
    }

    async fn set_rule_of(&self, arr: &Arr, profile_name: &str, lang_id: i64, rule: AudioRule) -> Result<()> {
        let fid = Self::dub_format(arr, lang_id).await?;
        // legenda em português só conta onde o legendado é aceito
        let subs = if rule == AudioRule::Required { None } else { Some(Self::subs_format(arr).await?) };
        // o perfil já traz formatItems de todos os formatos (score 0 nos novos)
        let mut profile = Self::profile(arr, profile_name).await?;
        let pid = profile["id"].as_i64().context("perfil sem id")?;
        apply_audio(&mut profile, fid, subs, rule)?;
        arr.send_json(Method::PUT, &format!("qualityprofile/{pid}"), &profile).await?;
        Ok(())
    }

    /// Perfil do Anubis no Radarr/Sonarr (cópia do perfil dos pedidos),
    /// criando na primeira vez com a regra `rule`. Devolve o id.
    async fn ensure_profile(&self, which: &str, name: &str, rule: AudioRule) -> Result<i64> {
        let (arr, base, lang_id) = self.arr_named(which)?;
        if let Some(p) = Self::find_profile(arr, name).await? {
            return p["id"].as_i64().context("perfil sem id");
        }
        let mut p = Self::profile(arr, &base).await?;
        if let Some(o) = p.as_object_mut() {
            o.remove("id");
        }
        p["name"] = json!(name);
        let id = arr.send_json(Method::POST, "qualityprofile", &p).await?["id"].as_i64().context("perfil sem id")?;
        self.set_rule_of(arr, name, lang_id, rule).await?;
        tracing::info!("{which}: perfil \"{name}\" criado ({rule:?})");
        Ok(id)
    }

    /// Prepara o Radarr e o Sonarr: o perfil do que não tem dublagem em
    /// nenhuma fonte (cópia do perfil dos pedidos, em "preferido"). Pode rodar
    /// de novo sem efeito.
    pub async fn setup_profiles(&self) -> Result<()> {
        let series = match self.sonarr {
            Some(_) => Some(self.ensure_profile("sonarr", FALLBACK_PROFILE, AudioRule::Preferred).await?),
            None => None,
        };
        let movies = match self.radarr {
            Some(_) => Some(self.ensure_profile("radarr", FALLBACK_PROFILE, AudioRule::Preferred).await?),
            None => None,
        };
        *self.profile_ids.lock().await = Some(ProfileIds { series, movies });
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

    async fn quality(&self, which: &str) -> Result<QualityPrefs> {
        let (arr, profile_name, _) = self.arr_named(which)?;
        Ok(quality_prefs(&Self::profile(arr, &profile_name).await?))
    }

    async fn set_quality(&self, which: &str, q: &QualityPrefs) -> Result<()> {
        let (arr, profile_name, _) = self.arr_named(which)?;
        // o perfil do que não tem dublagem segue o mesmo
        let names = vec![profile_name, FALLBACK_PROFILE.to_owned()];
        for name in names {
            let Some(mut profile) = Self::find_profile(arr, &name).await? else { continue };
            let pid = profile["id"].as_i64().context("perfil sem id")?;
            apply_quality(&mut profile, q)?;
            arr.send_json(Method::PUT, &format!("qualityprofile/{pid}"), &profile).await?;
        }
        Ok(())
    }

    pub async fn prefs(&self) -> Result<Prefs> {
        Ok(Prefs {
            movies_audio: if self.radarr.is_some() { Some(self.audio_rule("radarr").await?) } else { None },
            series_audio: if self.sonarr.is_some() { Some(self.audio_rule("sonarr").await?) } else { None },
            subs: self.subs_prefs().await?,
            movies_quality: if self.radarr.is_some() { Some(self.quality("radarr").await?) } else { None },
            series_quality: if self.sonarr.is_some() { Some(self.quality("sonarr").await?) } else { None },
        })
    }

    pub async fn set_prefs(&self, p: &Prefs) -> Result<Prefs> {
        if let Some(q) = &p.movies_quality {
            self.set_quality("radarr", q).await?;
        }
        if let Some(q) = &p.series_quality {
            self.set_quality("sonarr", q).await?;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn q(id: i64, name: &str, allowed: bool) -> Value {
        json!({ "quality": { "id": id, "name": name }, "items": [], "allowed": allowed })
    }
    fn order(p: &Value) -> Vec<String> {
        p["items"].as_array().unwrap().iter().map(|i| format!("{}{}", names(i).join("+"), if i["allowed"] == true { "" } else { "✗" })).collect()
    }

    #[test]
    fn bluray_becomes_first_option_and_cinema_is_refused() {
        // Radarr "Any" (pior → melhor), alvo WEBDL-1080p; Bluray recusado e abaixo do WEB
        let mut radarr = json!({ "cutoff": 3, "items": [q(25, "CAM", true), q(9, "HDTV-1080p", true), q(7, "Bluray-1080p", false), q(15, "WEBRip-1080p", true), q(3, "WEBDL-1080p", true), q(30, "Remux-1080p", false)] });
        assert_eq!(quality_prefs(&radarr), QualityPrefs { bluray_first: false, no_cinema: Some(false) });
        let on = QualityPrefs { bluray_first: true, no_cinema: Some(true) };
        apply_quality(&mut radarr, &on).unwrap();
        assert_eq!(order(&radarr), ["CAM✗", "HDTV-1080p", "WEBRip-1080p", "WEBDL-1080p", "Bluray-1080p", "Remux-1080p✗"]);
        // alvo continua no WEB: um WEB baixado não é trocado sozinho
        assert_eq!(radarr["cutoff"], 3);
        assert_eq!(quality_prefs(&radarr), on);
        apply_quality(&mut radarr, &QualityPrefs { bluray_first: false, no_cinema: Some(false) }).unwrap();
        assert_eq!(order(&radarr)[0], "CAM");
        assert_eq!(order(&radarr)[4], "Bluray-1080p✗");

        // Sonarr: Bluray já estava acima do WEB; sem qualidades de cinema
        let mut sonarr = json!({ "cutoff": 3, "items": [q(6, "Bluray-720p", true), q(15, "WEBRip-1080p", true), q(3, "WEBDL-1080p", true), q(7, "Bluray-1080p", false), q(20, "Bluray-1080p Remux", false)] });
        assert_eq!(quality_prefs(&sonarr).no_cinema, None);
        apply_quality(&mut sonarr, &QualityPrefs { bluray_first: true, no_cinema: None }).unwrap();
        assert_eq!(order(&sonarr), ["Bluray-720p", "WEBRip-1080p", "WEBDL-1080p", "Bluray-1080p", "Bluray-1080p Remux✗"]);
        assert_eq!(sonarr["cutoff"], 3);
    }

    #[test]
    fn works_with_the_web_group_and_never_targets_bluray() {
        let group = json!({ "id": 1002, "name": "WEB 1080p", "allowed": true, "items": [q(3, "WEBDL-1080p", true), q(15, "WEBRip-1080p", true)] });
        let mut p = json!({ "cutoff": 7, "items": [q(9, "HDTV-1080p", true), q(7, "Bluray-1080p", true), group] });
        apply_quality(&mut p, &QualityPrefs { bluray_first: true, no_cinema: None }).unwrap();
        assert_eq!(order(&p), ["HDTV-1080p", "WEBDL-1080p+WEBRip-1080p", "Bluray-1080p"]);
        // alvo que estava no Bluray vai para o WEB (o grupo)
        assert_eq!(p["cutoff"], 1002);
        // alvo escolhido à mão (Remux) não muda
        let mut p = json!({ "cutoff": 30, "items": [q(7, "Bluray-1080p", false), q(3, "WEBDL-1080p", true), q(30, "Remux-1080p", true)] });
        apply_quality(&mut p, &QualityPrefs { bluray_first: true, no_cinema: None }).unwrap();
        assert_eq!(p["cutoff"], 30);
    }
}
