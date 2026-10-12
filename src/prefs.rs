//! Preferências que a pessoa controla pela página do Anubis: idioma do áudio
//! (filmes e séries) e legendas. A fonte da verdade continua no Radarr, no
//! Sonarr e no Bazarr — o Anubis lê de lá e grava lá, sem estado próprio.
//!
//! Idioma: um formato personalizado "Anubis · Português (BR)" em cada Arr,
//! pontuado no perfil de qualidade dos pedidos:
//! - obrigatório: pontuação mínima = a do formato (o resto é recusado);
//! - preferido: aceita qualquer um e troca pela versão em PT-BR quando surgir;
//! - tanto faz: português ou o idioma original, sem preferência.
//!
//! Sem português, só o idioma original (formato "Anubis · Idioma original"):
//! nunca a dublagem num terceiro idioma (um anime dublado em inglês).

use crate::orchestrator::Orchestrator;
use crate::services::{Arr, Method};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const FORMAT: &str = "Anubis · Português (BR)";
const SCORE: i64 = 100;
/// áudio no idioma original do título; vale menos que o dublado. O Radarr e
/// o Sonarr tomam o nome sem idioma como original ("[SubsPlease] Frieren - 01")
pub const ORIGINAL_FORMAT: &str = "Anubis · Idioma original";
const ORIGINAL_SCORE: i64 = 50;
/// id do idioma "Original" nos formatos do Radarr e do Sonarr
const ORIGINAL_LANGUAGE: i64 = -2;
/// legenda em português pelo nome da versão; vale menos que o dublado
pub const SUBS_FORMAT: &str = "Anubis · Legenda PT-BR";
const SUBS_SCORE: i64 = 10;
/// "Multiple Subtitle" e "[POR-BR]": a lista de legendas dos grupos de anime
const SUBS_REGEX: &str = r"\b(legendad[oa]s?|leg|multi[ ._-]?subs?|multiple[ ._-]?subtitles?|por[ ._-]?br|pt[ ._-]?br[ ._-]?subs?|subs?[ ._-]?pt[ ._-]?br)\b";
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

/// A regra de idioma num perfil: pontos do dublado, do idioma original e da
/// legenda em português (onde o legendado é aceito), mínimo e alvo. O mínimo
/// fica no ponto do original: o que não é dublado nem original é recusado.
fn apply_audio(profile: &mut Value, dub: i64, original: i64, subs: Option<i64>, rule: AudioRule) -> Result<()> {
    let score = if rule == AudioRule::Any { ORIGINAL_SCORE } else { SCORE };
    let original_score = if rule == AudioRule::Required { 0 } else { ORIGINAL_SCORE };
    let items = profile["formatItems"].as_array_mut().context("perfil sem formatItems")?;
    let mut set = |fid: i64, name: &str, score: i64| match items.iter_mut().find(|i| i["format"].as_i64() == Some(fid)) {
        Some(i) => i["score"] = json!(score),
        None => items.push(json!({ "format": fid, "name": name, "score": score })),
    };
    set(dub, FORMAT, score);
    set(original, ORIGINAL_FORMAT, original_score);
    if let Some(s) = subs {
        set(s, SUBS_FORMAT, SUBS_SCORE);
    }
    profile["minFormatScore"] = json!(if rule == AudioRule::Required { SCORE } else { ORIGINAL_SCORE });
    // preferido: continua trocando até achar a versão em PT-BR
    profile["cutoffFormatScore"] = json!(score);
    profile["upgradeAllowed"] = json!(true);
    if profile.get("language").is_some() {
        // Radarr: o idioma passa a ser decidido pelo formato
        profile["language"] = json!({ "id": -1, "name": "Any" });
    }
    Ok(())
}

/// A regra de idioma que um perfil tem hoje (`formats`: os formatos do Arr).
fn rule_in(profile: &Value, formats: &Value) -> AudioRule {
    let score_of = |name: &str| {
        let fid = formats.as_array().and_then(|a| a.iter().find(|f| f["name"] == name)).and_then(|f| f["id"].as_i64());
        fid.and_then(|id| profile["formatItems"].as_array()?.iter().find(|i| i["format"].as_i64() == Some(id)).and_then(|i| i["score"].as_i64()))
            .unwrap_or(0)
    };
    let (dub, original) = (score_of(FORMAT), score_of(ORIGINAL_FORMAT));
    let min = profile["minFormatScore"].as_i64().unwrap_or(0);
    // o campo de idioma do perfil (só no Radarr) também pode exigir PT-BR
    let legacy = profile["language"]["name"].as_str() == Some("Portuguese (Brazil)");
    // dublado valendo mais que o original: obrigatório ou preferido; os dois
    // iguais (ou sem pontos): tanto faz
    if (dub > original && min >= dub) || legacy {
        AudioRule::Required
    } else if dub > original {
        AudioRule::Preferred
    } else {
        AudioRule::Any
    }
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

/// Qualidade dos pedidos (além do idioma), no mesmo perfil do Radarr/Sonarr:
/// a fila de qualidades aceitas, da preferida para a menos preferida, e o
/// alvo. Os nomes são os do perfil (uma qualidade, ou um grupo como "WEB 720p").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityPrefs {
    pub queue: Vec<String>,
    /// com um arquivo nesta qualidade (ou melhor), o Radarr/Sonarr não trocam
    /// sozinhos por uma melhor da fila — o "assistir" oferece a troca
    pub cutoff: String,
    /// as que estão fora da fila e podem entrar (só leitura)
    #[serde(default)]
    pub others: Vec<String>,
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

/// Nome de um item do perfil: o da qualidade ou o do grupo.
fn item_name(item: &Value) -> &str {
    item["quality"]["name"].as_str().or(item["name"].as_str()).unwrap_or_default()
}

/// A fila de um perfil: as aceitas, da melhor para a pior (o perfil guarda
/// da pior para a melhor). "Unknown" nunca é oferecida.
pub(crate) fn queue_of(profile: &Value) -> Vec<String> {
    let items = profile["items"].as_array().cloned().unwrap_or_default();
    items.iter().rev().filter(|i| i["allowed"] == true).map(|i| item_name(i).to_owned()).collect()
}

/// Posição de uma qualidade (ex. "WEBDL-1080p") na fila do perfil — dentro
/// de um grupo conta a posição do grupo. `None`: fora da fila.
pub(crate) fn rank_in(profile: &Value, quality: &str) -> Option<usize> {
    let items = profile["items"].as_array()?;
    items.iter().rev().filter(|i| i["allowed"] == true).position(|i| names(i).contains(&quality))
}

/// Posição do alvo na fila.
pub(crate) fn cutoff_rank(profile: &Value) -> Option<usize> {
    let cutoff = profile["cutoff"].as_i64()?;
    let items = profile["items"].as_array()?;
    items.iter().rev().filter(|i| i["allowed"] == true).position(|i| cutoff_id(i) == Some(cutoff))
}

fn quality_prefs(profile: &Value) -> QualityPrefs {
    let items = profile["items"].as_array().cloned().unwrap_or_default();
    let cutoff = profile["cutoff"].as_i64();
    QualityPrefs {
        queue: queue_of(profile),
        cutoff: items.iter().find(|i| cutoff_id(i) == cutoff).map(|i| item_name(i).to_owned()).unwrap_or_default(),
        others: items.iter().rev().filter(|i| i["allowed"] != true && item_name(i) != "Unknown").map(|i| item_name(i).to_owned()).collect(),
    }
}

/// Aplica a fila no perfil: a ordem (o perfil vai da pior para a melhor; o
/// que está fora da fila fica abaixo de tudo, na ordem de antes), quais são
/// aceitas e o alvo.
fn apply_quality(profile: &mut Value, q: &QualityPrefs) -> Result<()> {
    if q.queue.is_empty() {
        bail!("a fila de qualidades não pode ficar vazia");
    }
    if !q.queue.contains(&q.cutoff) {
        bail!("o alvo ({}) precisa estar na fila", q.cutoff);
    }
    let items = profile["items"].as_array_mut().context("perfil sem qualidades")?;
    // numa cópia: com erro, o perfil fica como estava
    let mut rest = items.clone();
    let mut queue = Vec::new();
    for name in q.queue.iter().rev() {
        let pos = rest.iter().position(|i| item_name(i) == name).with_context(|| format!("qualidade \"{name}\" não existe neste perfil (ou repetida)"))?;
        let mut it = rest.remove(pos);
        it["allowed"] = json!(true);
        queue.push(it);
    }
    for it in rest.iter_mut() {
        it["allowed"] = json!(false);
    }
    let cutoff = queue.iter().find(|i| item_name(i) == q.cutoff).and_then(cutoff_id).context("alvo sem id")?;
    rest.extend(queue);
    *items = rest;
    profile["cutoff"] = json!(cutoff);
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

    /// Id de um formato personalizado do Anubis, criando na primeira vez; o
    /// que já existe é regravado se a regra mudou (a regex da legenda).
    async fn format_id(arr: &Arr, name: &str, spec: Value) -> Result<i64> {
        let formats = arr.get_json("customformat").await?;
        let body = json!({ "name": name, "includeCustomFormatWhenRenaming": false, "specifications": [spec] });
        if let Some(f) = formats.as_array().and_then(|a| a.iter().find(|f| f["name"] == name)) {
            let id = f["id"].as_i64().context("formato sem id")?;
            let value = |s: &Value| s["fields"].as_array().and_then(|a| a.iter().find(|x| x["name"] == "value")).map(|x| x["value"].clone());
            if value(&f["specifications"][0]) != value(&spec) {
                let mut body = body;
                body["id"] = json!(id);
                arr.send_json(Method::PUT, &format!("customformat/{id}"), &body).await?;
            }
            return Ok(id);
        }
        arr.send_json(Method::POST, "customformat", &body).await?["id"].as_i64().context("formato sem id")
    }

    async fn dub_format(arr: &Arr, lang_id: i64) -> Result<i64> {
        let spec = json!({
            "name": "Português (Brasil)", "implementation": "LanguageSpecification", "negate": false, "required": true,
            "fields": [{ "name": "value", "value": lang_id }, { "name": "exceptLanguage", "value": false }]
        });
        Self::format_id(arr, FORMAT, spec).await
    }

    async fn original_format(arr: &Arr) -> Result<i64> {
        let spec = json!({
            "name": "Idioma original", "implementation": "LanguageSpecification", "negate": false, "required": true,
            "fields": [{ "name": "value", "value": ORIGINAL_LANGUAGE }, { "name": "exceptLanguage", "value": false }]
        });
        Self::format_id(arr, ORIGINAL_FORMAT, spec).await
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
        Ok(rule_in(&profile, &formats))
    }

    async fn set_audio_rule(&self, which: &str, rule: AudioRule) -> Result<()> {
        let (arr, profile_name, lang_id) = self.arr_named(which)?;
        self.set_rule_of(arr, &profile_name, lang_id, rule).await
    }

    async fn set_rule_of(&self, arr: &Arr, profile_name: &str, lang_id: i64, rule: AudioRule) -> Result<()> {
        let fid = Self::dub_format(arr, lang_id).await?;
        let original = Self::original_format(arr).await?;
        // legenda em português só conta onde o legendado é aceito
        let subs = if rule == AudioRule::Required { None } else { Some(Self::subs_format(arr).await?) };
        // o perfil já traz formatItems de todos os formatos (score 0 nos novos)
        let mut profile = Self::profile(arr, profile_name).await?;
        let pid = profile["id"].as_i64().context("perfil sem id")?;
        apply_audio(&mut profile, fid, original, subs, rule)?;
        arr.send_json(Method::PUT, &format!("qualityprofile/{pid}"), &profile).await?;
        Ok(())
    }

    /// Perfil do Anubis no Radarr/Sonarr (cópia do perfil dos pedidos),
    /// criando na primeira vez; a regra `rule` é aplicada sempre (perfis de
    /// versões antigas ganham o que mudou). Devolve o id.
    async fn ensure_profile(&self, which: &str, name: &str, rule: AudioRule) -> Result<i64> {
        let (arr, base, lang_id) = self.arr_named(which)?;
        if let Some(p) = Self::find_profile(arr, name).await? {
            self.set_rule_of(arr, name, lang_id, rule).await?;
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
    /// nenhuma fonte (cópia do perfil dos pedidos, em "preferido") e a regra
    /// de idioma de cada perfil regravada (o formato do idioma original entra
    /// nos perfis de antes dele). Pode rodar de novo sem efeito.
    pub async fn setup_profiles(&self) -> Result<()> {
        for which in ["radarr", "sonarr"] {
            if self.arr_named(which).is_ok() {
                let rule = self.audio_rule(which).await?;
                self.set_audio_rule(which, rule).await?;
            }
        }
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
    fn quality_queue_reads_and_writes_the_profile() {
        // Radarr "Any" (pior → melhor), alvo WEBDL-1080p
        let web720 = json!({ "id": 1001, "name": "WEB 720p", "allowed": true, "items": [q(5, "WEBDL-720p", true), q(14, "WEBRip-720p", true)] });
        let mut p = json!({ "cutoff": 3, "items": [q(0, "Unknown", false), q(25, "CAM", false), web720, q(15, "WEBRip-1080p", true), q(3, "WEBDL-1080p", true), q(7, "Bluray-1080p", true), q(19, "Bluray-2160p", false)] });
        let r = quality_prefs(&p);
        assert_eq!(r.queue, ["Bluray-1080p", "WEBDL-1080p", "WEBRip-1080p", "WEB 720p"]);
        assert_eq!(r.cutoff, "WEBDL-1080p");
        assert_eq!(r.others, ["Bluray-2160p", "CAM"], "Unknown nunca é oferecida");
        // 4K entra no topo, WEBRip sai, o alvo vai para o Bluray 1080p
        let want = QualityPrefs { queue: vec!["Bluray-2160p".into(), "Bluray-1080p".into(), "WEBDL-1080p".into(), "WEB 720p".into()], cutoff: "Bluray-1080p".into(), others: vec![] };
        apply_quality(&mut p, &want).unwrap();
        assert_eq!(order(&p), ["Unknown✗", "CAM✗", "WEBRip-1080p✗", "WEBDL-720p+WEBRip-720p", "WEBDL-1080p", "Bluray-1080p", "Bluray-2160p"]);
        assert_eq!(p["cutoff"], 7);
        let back = quality_prefs(&p);
        assert_eq!((back.queue, back.cutoff), (want.queue.clone(), want.cutoff.clone()));
        // posições na fila (qualidade dentro de grupo conta o grupo)
        assert_eq!(rank_in(&p, "Bluray-2160p"), Some(0));
        assert_eq!(rank_in(&p, "WEBRip-720p"), Some(3));
        assert_eq!(rank_in(&p, "WEBRip-1080p"), None);
        assert_eq!(cutoff_rank(&p), Some(1));
        // o perfil continua com todas as qualidades (o Radarr exige)
        assert_eq!(p["items"].as_array().unwrap().len(), 7);
    }

    #[test]
    fn quality_queue_refuses_bad_input() {
        let mut p = json!({ "cutoff": 3, "items": [q(3, "WEBDL-1080p", true), q(7, "Bluray-1080p", true)] });
        let bad = |queue: &[&str], cutoff: &str| QualityPrefs { queue: queue.iter().map(|s| s.to_string()).collect(), cutoff: cutoff.into(), others: vec![] };
        assert!(apply_quality(&mut p.clone(), &bad(&[], "WEBDL-1080p")).is_err());
        assert!(apply_quality(&mut p.clone(), &bad(&["Bluray-1080p"], "WEBDL-1080p")).is_err(), "alvo fora da fila");
        assert!(apply_quality(&mut p.clone(), &bad(&["Remux-1080p"], "Remux-1080p")).is_err(), "qualidade inexistente");
        assert!(apply_quality(&mut p.clone(), &bad(&["Bluray-1080p", "Bluray-1080p"], "Bluray-1080p")).is_err(), "repetida");
        // erro não estraga o perfil
        let before = p.clone();
        assert!(apply_quality(&mut p, &bad(&["Bluray-1080p", "Remux-1080p"], "Bluray-1080p")).is_err());
        assert_eq!(p, before);
    }

    #[test]
    fn subtitle_names_of_anime_groups() {
        let re = regex::RegexBuilder::new(SUBS_REGEX).case_insensitive(true).build().unwrap();
        for t in [
            "[Erai-raws] Sousou no Frieren 2nd Season - 05 [1080p CR WEB-DL AVC AAC][MultiSub][42940134]",
            "[ToonsHub] Frieren Beyond Journeys End S02E05 1080p BILI WEB-DL AAC2.0 H.265 (Sousou no Frieren, Multi-Subs)",
            "[Erai-raws] Kimetsu no Yaiba - 01 [1080p][Multiple Subtitle] [ENG][POR-BR][SPA-LA]",
            "Frieren S02E01 LEGENDADO",
        ] {
            assert!(re.is_match(t), "{t}");
        }
        for t in ["[SubsPlease] Sousou no Frieren S2 - 05 (1080p) [6AAEC79A].mkv", "Pecadores.2025.1080p.WEB-DL.DUAL.5.1 (brazilian, eng)"] {
            assert!(!re.is_match(t), "{t}");
        }
    }

    #[test]
    fn audio_rule_refuses_a_third_language_and_reads_back() {
        let formats = json!([{ "id": 1, "name": FORMAT }, { "id": 2, "name": ORIGINAL_FORMAT }, { "id": 3, "name": SUBS_FORMAT }]);
        // pontos de uma versão: dublado 1, original 2, legenda PT-BR 3
        let score = |p: &Value, cfs: &[i64]| -> i64 {
            p["formatItems"].as_array().unwrap().iter().filter(|i| cfs.contains(&i["format"].as_i64().unwrap())).map(|i| i["score"].as_i64().unwrap()).sum()
        };
        let passes = |p: &Value, cfs: &[i64]| score(p, cfs) >= p["minFormatScore"].as_i64().unwrap();
        for rule in [AudioRule::Required, AudioRule::Preferred, AudioRule::Any] {
            let mut p = json!({ "formatItems": [], "minFormatScore": 0 });
            apply_audio(&mut p, 1, 2, (rule != AudioRule::Required).then_some(3), rule).unwrap();
            assert_eq!(rule_in(&p, &formats), rule);
            // dublado passa sempre; terceiro idioma (mesmo com legenda PT-BR) nunca
            assert!(passes(&p, &[1]));
            assert!(!passes(&p, &[]));
            assert!(!passes(&p, &[3]));
            // original: só onde o legendado é aceito
            assert_eq!(passes(&p, &[2]), rule != AudioRule::Required);
        }
        // preferido: dublado acima do original, e o original legendado acima do sem legenda
        let mut p = json!({ "formatItems": [] });
        apply_audio(&mut p, 1, 2, Some(3), AudioRule::Preferred).unwrap();
        assert!(score(&p, &[1]) > score(&p, &[2, 3]) && score(&p, &[2, 3]) > score(&p, &[2]));
        // perfis de antes do formato do original continuam lidos como eram
        let old = |dub: i64, min: i64| json!({ "formatItems": [{ "format": 1, "score": dub }], "minFormatScore": min });
        assert_eq!(rule_in(&old(100, 100), &formats), AudioRule::Required);
        assert_eq!(rule_in(&old(100, 0), &formats), AudioRule::Preferred);
        assert_eq!(rule_in(&old(0, 0), &formats), AudioRule::Any);
    }

}
