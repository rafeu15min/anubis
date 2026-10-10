//! Perfil de estudo: o que alimenta Técnicos e Artigos.
//!
//! - Áreas da configuração (`[[leitura.areas]]`: nome e termos, em português
//!   e inglês) são o ponto de partida.
//! - O resto é automático: cada livro de Técnicos e cada artigo de Artigos do
//!   Drive é procurado no OpenAlex pelo título, e o tema dele (primary_topic)
//!   soma peso — lido 3, lendo 2, só na estante 1. Tema com peso 2 ou mais
//!   vira área também; área configurada que casa com o que se tem pesa mais.
//!
//! Artigos vêm do OpenAlex (aberto, sem chave), só os de acesso livre, nas
//! fileiras dos filmes: Para você (mais citados da área, desde 2021),
//! Destaques (mais citados do último ano), Lançamentos (os mais novos) e
//! Populares no Brasil (de instituições brasileiras, mais citados em 3
//! anos). O tema de cada livro fica em `state_dir/estudo.json`.

use crate::config::AreaCfg;
use crate::gosto::{Taste, fold, keyword};
use crate::leitura::Suggestion;
use anyhow::Result;
use chrono::Datelike;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::Mutex;

const BASE: &str = "https://api.openalex.org";
const GAP: Duration = Duration::from_millis(150);
/// áreas por montagem: 4 buscas cada (a cota do OpenAlex sem chave é de
/// umas 100 buscas por dia)
const PER_AREA: usize = 5;
/// títulos novos procurados por montagem (o tema fica guardado para sempre)
const NEW_TITLES: usize = 15;

/// De onde veio um tema.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origem {
    /// as áreas de estudo da configuração
    #[default]
    Config,
    /// o tema do que está no Drive (OpenAlex)
    Drive,
    /// o gosto do que se assiste e lê
    Gosto,
    /// posto pela pessoa (src/temas.rs)
    Voce,
}

/// Um tema de busca (área de estudo, tema do Drive, do gosto ou da pessoa).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Area {
    pub nome: String,
    /// o filtro no OpenAlex
    #[serde(skip)]
    filter: String,
    /// os termos no vocabulário comum (para casar com os assuntos dos livros)
    #[serde(skip)]
    pub(crate) keywords: Vec<String>,
    /// os termos como foram escritos (para buscar livros e mangás)
    #[serde(skip)]
    pub(crate) termos: Vec<String>,
    pub peso: f64,
    pub origem: Origem,
    /// tema que é um gênero (unificado do TMDB): mangás buscam pelo gênero
    #[serde(skip)]
    pub(crate) genre: Option<i64>,
}

impl Area {
    pub fn new(nome: String, termos: Vec<String>, peso: f64, origem: Origem) -> Self {
        Area {
            filter: format!("title_and_abstract.search:{}", phrases(&termos)),
            keywords: termos.iter().filter_map(|t| keyword(t)).collect(),
            nome,
            termos,
            peso,
            origem,
            genre: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Topic {
    id: String,
    nome: String,
}

pub struct Estudo {
    http: reqwest::Client,
    areas: Vec<AreaCfg>,
    /// chave gratuita do OpenAlex (mais cota por dia); sem ela, a cota anônima
    api_key: Option<String>,
    /// campos do conhecimento do OpenAlex (ids: 17 = Ciência da Computação,
    /// 26 = Matemática…); vazio: todos
    fields: Vec<u32>,
    path: PathBuf,
    /// caminho no Drive → tema no OpenAlex (None: não achou)
    topics: Mutex<HashMap<String, Option<Topic>>>,
    turn: Mutex<()>,
}

/// `"a"|"b"` para o filtro do OpenAlex (aspas: a frase inteira).
fn phrases(terms: &[String]) -> String {
    terms.iter().map(|t| format!("\"{}\"", t.replace('"', ""))).collect::<Vec<_>>().join("|")
}

impl Estudo {
    pub fn load(areas: Vec<AreaCfg>, api_key: Option<String>, fields: Vec<u32>, state_dir: PathBuf) -> Result<Self> {
        let path = state_dir.join("estudo.json");
        let topics = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).user_agent("Anubis/0.1 (leitura pessoal)").build()?;
        Ok(Self { http, areas, api_key, fields, path, topics: Mutex::new(topics), turn: Mutex::new(()) })
    }

    async fn get(&self, query: &str) -> Result<Value> {
        let _turn = self.turn.lock().await;
        let key = self.api_key.as_deref().map(|k| format!("&api_key={k}")).unwrap_or_default();
        let v = self.http.get(format!("{BASE}/works?{query}{key}")).send().await?.error_for_status()?.json().await?;
        tokio::time::sleep(GAP).await;
        Ok(v)
    }

    /// O tema de um livro do Drive (busca uma vez e guarda). `budget`: quantas
    /// buscas novas ainda cabem nesta montagem.
    async fn topic_of(&self, path: &str, title: &str, budget: &mut usize) -> Option<Topic> {
        if let Some(t) = self.topics.lock().await.get(path) {
            return t.clone();
        }
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        // vírgula e dois-pontos separam filtros no OpenAlex: fora do título
        let clean: String = title.chars().map(|c| if matches!(c, ',' | ':' | '|' | '"') { ' ' } else { c }).collect();
        let q = format!("filter=title.search:{}&per-page=1&select=title,primary_topic", urlencode(clean.trim()));
        let found = match self.get(&q).await {
            Ok(v) => v["results"][0].clone(),
            Err(e) => {
                tracing::info!("OpenAlex ({title}): {e:#}");
                return None; // sem guardar: tenta de novo depois
            }
        };
        // só vale se o título achado é mesmo o do livro
        let same = found["title"].as_str().is_some_and(|t| {
            let (a, b) = (fold(t), fold(title));
            a.contains(&b) || b.contains(&a)
        });
        let topic = same
            .then(|| Some(Topic { id: found["primary_topic"]["id"].as_str()?.rsplit('/').next()?.to_string(), nome: found["primary_topic"]["display_name"].as_str()?.to_string() }))
            .flatten();
        let mut topics = self.topics.lock().await;
        topics.insert(path.to_string(), topic.clone());
        if let Ok(bytes) = serde_json::to_vec(&*topics) {
            let tmp = self.path.with_extension("json.tmp");
            if tokio::fs::write(&tmp, bytes).await.is_ok() {
                let _ = tokio::fs::rename(&tmp, &self.path).await;
            }
        }
        topic
    }

    /// As áreas, da mais forte para a mais fraca. `books`: (caminho, título,
    /// peso) dos livros de Técnicos e Artigos do Drive.
    pub async fn profile(&self, books: &[(String, String, f64)]) -> Vec<Area> {
        let mut areas: Vec<Area> = self.areas.iter().map(|a| Area::new(a.nome.clone(), a.termos.clone(), 1.0, Origem::Config)).collect();
        let mut by_topic: HashMap<String, (Topic, f64)> = HashMap::new();
        let mut budget = NEW_TITLES;
        for (path, title, w) in books {
            let t = fold(title);
            // área configurada que casa com o título do que se tem pesa mais
            for a in areas.iter_mut().filter(|a| a.origem == Origem::Config) {
                let cfg = self.areas.iter().find(|c| c.nome == a.nome);
                if cfg.is_some_and(|c| c.termos.iter().any(|x| t.contains(&fold(x)))) {
                    a.peso += w;
                }
            }
            if let Some(topic) = self.topic_of(path, title, &mut budget).await {
                by_topic.entry(topic.id.clone()).or_insert((topic, 0.0)).1 += w;
            }
        }
        for (id, (topic, w)) in by_topic {
            if w >= 2.0 {
                areas.push(Area {
                    keywords: keyword(&topic.nome).into_iter().collect(),
                    termos: vec![topic.nome.clone()],
                    nome: topic.nome,
                    filter: format!("primary_topic.id:{id}"),
                    peso: w,
                    origem: Origem::Drive,
                    genre: None,
                });
            }
        }
        areas.sort_by(|a, b| b.peso.total_cmp(&a.peso));
        areas
    }

    /// O gosto de Técnicos: os termos das áreas, pelo peso de cada uma.
    pub fn taste(areas: &[Area]) -> Taste {
        let mut t = Taste::default();
        for a in areas {
            t.add(a.peso, &[], &a.keywords);
        }
        t.normalized()
    }

    /// Artigos de acesso livre de uma área (`extra`: mais filtros; `sort`).
    async fn works(&self, area: &Area, extra: &str, sort: &str, n: usize) -> Result<Vec<Suggestion>> {
        // só português e inglês; e, se configurado, só os campos do conhecimento
        let fields = if self.fields.is_empty() { String::new() } else { format!(",primary_topic.field.id:{}", self.fields.iter().map(|f| f.to_string()).collect::<Vec<_>>().join("|")) };
        let q = format!(
            "filter={},open_access.is_oa:true,type:article,language:pt|en{fields}{extra}&sort={sort}&per-page={n}&select=id,title,publication_year,language,authorships,best_oa_location,doi",
            urlencode(&area.filter)
        );
        let v = self.get(&q).await.map_err(|e| anyhow::anyhow!("OpenAlex ({}): {e:#}", area.nome))?;
        Ok(v["results"].as_array().into_iter().flatten().filter_map(|w| {
            let title = w["title"].as_str()?.trim();
            (title.len() >= 8).then_some(())?;
            let landing = w["best_oa_location"]["landing_page_url"].as_str().or(w["doi"].as_str());
            Some(Suggestion {
                key: format!("oa:{}", w["id"].as_str()?.rsplit('/').next()?),
                title: title.to_string(),
                author: w["authorships"][0]["author"]["display_name"].as_str().unwrap_or_default().to_string(),
                year: w["publication_year"].as_i64(),
                cover: None,
                portuguese: w["language"] == "pt",
                reason: format!("Da sua área: {}", area.nome),
                link: landing.map(str::to_string),
                estante: None,
                pdf: w["best_oa_location"]["pdf_url"].as_str().map(str::to_string),
                pasta: Some("Artigos".into()),
            })
        }).collect())
    }

    /// As fileiras de Artigos: as áreas mais fortes primeiro, intercaladas.
    pub async fn article_rows(&self, areas: &[Area]) -> Result<crate::leitura_recs::Rows> {
        let today = chrono::Local::now().date_naive();
        let ago = |days: i64| (today - chrono::Duration::days(days)).to_string();
        // as 3 mais fortes sempre; as outras se revezam (2 por dia), para toda
        // área aparecer
        let day = chrono::Local::now().ordinal() as usize;
        let rest: Vec<&Area> = areas.iter().skip(3).collect();
        let mut top: Vec<&Area> = areas.iter().take(3).collect();
        for i in 0..(PER_AREA - 3).min(rest.len()) {
            top.push(rest[(day * (PER_AREA - 3) + i) % rest.len()]);
        }
        let mut pv = vec![];
        let mut dest = vec![];
        let mut lanc = vec![];
        let mut pop = vec![];
        for a in &top {
            // as mais fortes dão mais artigos ao Para você
            let n = if a.peso >= 2.0 { 6 } else { 4 };
            pv.push(self.works(a, ",from_publication_date:2021-01-01", "cited_by_count:desc", n).await?);
            dest.push(self.works(a, &format!(",from_publication_date:{}", ago(365)), "cited_by_count:desc", 4).await?);
            lanc.push(self.works(a, &format!(",from_publication_date:{},to_publication_date:{today}", ago(90)), "publication_date:desc", 4).await?);
            pop.push(self.works(a, &format!(",authorships.institutions.country_code:BR,from_publication_date:{}", ago(3 * 365)), "cited_by_count:desc", 4).await?);
        }
        Ok(crate::leitura_recs::Rows { temas: vec![], para_voce: mix(pv), destaques: mix(dest), lancamentos: mix(lanc), populares: mix(pop) })
    }
}

/// Uma lista de cada área, intercaladas, sem repetir artigo.
fn mix(lists: Vec<Vec<Suggestion>>) -> Vec<Suggestion> {
    let mut out: Vec<Suggestion> = vec![];
    let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
    for i in 0..longest {
        for l in &lists {
            if let Some(s) = l.get(i)
                && !out.iter().any(|o| o.key == s.key)
            {
                out.push(s.clone());
            }
        }
    }
    out.truncate(24);
    out
}

/// O que vai no endereço sem quebrar a consulta (aspas, espaços, acentos).
fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' | b'|' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtro_com_frases() {
        assert_eq!(phrases(&["metodologias ativas".into(), "active learning".into()]), r#""metodologias ativas"|"active learning""#);
        assert_eq!(urlencode(r#"title_and_abstract.search:"LGPD"|"proteção""#), "title_and_abstract.search:%22LGPD%22|%22prote%C3%A7%C3%A3o%22");
    }
}
