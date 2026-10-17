//! As fileiras das abas da Leitura (Livros, Artigos, Mangás e HQs), como as
//! dos filmes: Para você, Destaques, Lançamentos e Populares no Brasil
//! (fontes: src/vitrine_livros.rs; Artigos: src/estudo.rs).
//!
//! Cada aba tem temas de busca (src/temas.rs): o sistema propõe (as áreas
//! de estudo, os temas do Drive, o gosto) e a pessoa edita. "Para você" usa
//! o mesmo gosto dos filmes (src/gosto.rs: metade do que se assiste, metade
//! do que se lê), os temas e os autores que se lê. Candidatos: as obras
//! desses autores, as de cada tema, os mangás do AniList e tudo o que está
//! nas outras fileiras. Livros junta as pastas Ficção e Técnicos: cada
//! sugestão diz para qual das duas iria (`pasta`). O gosto do que se lê
//! volta para os filmes (`foryou::Cache::book_taste`).

use crate::estudo::{Area, Origem};
use crate::gosto::{Taste, anilist_terms, subject_terms};
use crate::leitura::{Leitura, Suggestion, author_name, norm, owned_path};
use crate::openlibrary::Work;
use crate::vitrine_livros::{Item, amazon};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// as fileiras prontas, por aba
pub const ROWS_TTL: Duration = Duration::from_secs(12 * 3600);
const ROW_LEN: usize = 20;
const MAX_AUTHORS: usize = 8;
const MAX_PER_AUTHOR: usize = 2;
/// quanto pesa ser de um autor que se lê (o gosto vale até 1)
const AUTHOR_WEIGHT: f64 = 0.5;
/// quanto pesa ter vindo de um tema (pelo peso dele: o mais forte vale isso)
const THEME_WEIGHT: f64 = 0.4;
/// temas buscados por montagem (os da pessoa primeiro)
const MAX_THEMES: usize = 10;
/// lugares garantidos no Para você para cada tema posto pela pessoa
const USER_THEME_SLOTS: usize = 2;
/// quantos títulos de cada lista de fora são conferidos no Open Library
const ENRICH: usize = 15;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Rows {
    /// os temas que montaram as fileiras (editáveis: src/temas.rs)
    #[serde(default)]
    pub temas: Vec<Area>,
    #[serde(default)]
    pub para_voce: Vec<Suggestion>,
    #[serde(default)]
    pub destaques: Vec<Suggestion>,
    #[serde(default)]
    pub lancamentos: Vec<Suggestion>,
    #[serde(default)]
    pub populares: Vec<Suggestion>,
}

/// Uma lista de fora e a pasta do Drive do que vem dela (None: decide pelo
/// assunto).
struct Source {
    amazon: Option<&'static str>,
    goodreads: Option<&'static str>,
    pasta: Option<&'static str>,
}
const fn amz(cat: &'static str, pasta: &'static str) -> Source {
    Source { amazon: Some(cat), goodreads: None, pasta: Some(pasta) }
}
const fn gr(genre: &'static str, pasta: Option<&'static str>) -> Source {
    Source { amazon: None, goodreads: Some(genre), pasta }
}

/// De onde vêm as fileiras de cada aba.
struct Plan {
    /// Lançamentos e Populares no Brasil (Amazon)
    amazon: Vec<Source>,
    /// Destaques (Goodreads)
    goodreads: Vec<Source>,
    anilist: bool,
}

fn plan(tab: &str) -> Plan {
    match tab {
        "Livros" => Plan {
            amazon: vec![
                amz(amazon::LITERATURA, "Ficção"), amz(amazon::SEGURANCA, "Técnicos"), amz(amazon::FANTASIA_HORROR_FC, "Ficção"), amz(amazon::PROGRAMACAO, "Técnicos"),
                amz(amazon::POLICIAL, "Ficção"), amz(amazon::EDUCACAO, "Técnicos"), amz(amazon::ROMANCE, "Ficção"), amz(amazon::MATEMATICA, "Técnicos"),
                amz(amazon::HARDWARE, "Técnicos"), amz(amazon::ESCOLAS_ENSINO, "Técnicos"),
            ],
            goodreads: vec![gr("brasil", None), gr("computer-science", Some("Técnicos")), gr("artificial-intelligence", Some("Técnicos"))],
            anilist: false,
        },
        "Mangás e HQs" => Plan { amazon: vec![amz(amazon::HQS_MANGAS, "Mangás e HQs")], goodreads: vec![gr("manga", Some("Mangás e HQs")), gr("comics", Some("Mangás e HQs"))], anilist: true },
        _ => Plan { amazon: vec![], goodreads: vec![], anilist: false },
    }
}

/// Nome do gênero (unificado do TMDB) para o "porquê" e os temas.
fn genre_name(g: i64) -> Option<&'static str> {
    Some(match g {
        10765 => "ficção científica e fantasia",
        10759 => "ação e aventura",
        27 => "terror",
        9648 => "mistério",
        80 => "crime",
        53 => "suspense",
        10749 => "romance",
        10768 => "guerra",
        36 => "história",
        35 => "comédia",
        37 => "faroeste",
        18 => "drama",
        10402 => "música",
        _ => return None,
    })
}

/// O gênero como assunto do Open Library (para buscar obras dele).
fn genre_subject(g: i64) -> Option<&'static str> {
    Some(match g {
        10765 => "science fiction",
        10759 => "adventure",
        27 => "horror",
        9648 => "mystery",
        80 => "crime",
        53 => "thriller",
        10749 => "romance",
        36 => "historical fiction",
        35 => "humor",
        _ => return None,
    })
}

/// Gênero unificado → gêneros do AniList (para buscar mangás).
fn anilist_genres(g: i64) -> &'static [&'static str] {
    match g {
        10759 => &["Action", "Adventure"],
        35 => &["Comedy"],
        18 => &["Drama"],
        10765 => &["Sci-Fi", "Fantasy"],
        27 => &["Horror"],
        9648 => &["Mystery"],
        53 => &["Psychological", "Thriller"],
        10749 => &["Romance"],
        _ => &[],
    }
}

/// Uma lista intercalada (a 1ª de cada, a 2ª de cada…), sem repetir título;
/// cada item com a pasta da lista de onde veio.
fn interleave(lists: Vec<(Vec<Item>, Option<&'static str>)>) -> Vec<(Item, Option<&'static str>)> {
    let mut out = vec![];
    let mut seen = HashSet::new();
    let longest = lists.iter().map(|l| l.0.len()).max().unwrap_or(0);
    for i in 0..longest {
        for (l, pasta) in &lists {
            if let Some(it) = l.get(i)
                && seen.insert(norm(&it.title))
            {
                out.push((it.clone(), *pasta));
            }
        }
    }
    out
}

/// Ficção ou Técnicos, pelo que o Open Library diz do assunto. Sem assunto
/// nenhum, `default` (a do tema que trouxe a obra; senão Ficção, o mais comum).
fn pasta_by_subject(genres: &[i64], subjects: &[String], default: &'static str) -> &'static str {
    let fiction = !genres.is_empty() || subjects.iter().any(|s| crate::gosto::fold(s).contains("fiction"));
    if fiction { "Ficção" } else if subjects.is_empty() { default } else { "Técnicos" }
}

/// Um candidato ao "Para você": o cartão e o que se sabe do assunto dele.
struct Cand {
    s: Suggestion,
    genres: Vec<i64>,
    keywords: Vec<String>,
    /// pt-BR com certeza (edição da Amazon Brasil)
    pt: bool,
    /// o Open Library diz que é não ficção
    nonfiction: bool,
    /// o tema que o trouxe (nome, peso relativo 0–1)
    theme: Option<(String, f64)>,
}

impl Leitura {
    /// As fileiras da aba se já estão prontas; senão começa a montar.
    pub(crate) async fn rows(self: &Arc<Self>, tab: &'static str) -> Option<Arc<Rows>> {
        let cached = self.rows.lock().await.get(tab).cloned();
        if let Some((at, rows)) = &cached
            && at.elapsed() < ROWS_TTL
        {
            return Some(rows.clone());
        }
        if self.building.lock().await.insert(tab.to_string()) {
            let me = self.clone();
            tokio::spawn(async move {
                match me.build_rows(tab).await {
                    Ok(rows) => {
                        // Artigos dura 24 h (a cota diária do OpenAlex): conta como montado 12 h depois
                        let at = if tab == "Artigos" { Instant::now() + ROWS_TTL } else { Instant::now() };
                        me.rows.lock().await.insert(tab.to_string(), (at, Arc::new(rows)));
                        me.save_rows().await;
                    }
                    Err(e) => {
                        tracing::warn!("Leitura ({tab}): {e:#}");
                        // fica o que havia; nova tentativa em 1 h (não a cada visita)
                        let mut rows = me.rows.lock().await;
                        let old = rows.get(tab).map(|r| r.1.clone()).unwrap_or_default();
                        rows.insert(tab.to_string(), (Instant::now() - ROWS_TTL + Duration::from_secs(3600), old));
                    }
                }
                me.building.lock().await.remove(tab);
            });
        }
        cached.map(|(_, r)| r) // o antigo vale enquanto o novo é montado
    }

    /// Os temas mudaram: monta de novo já (o que havia vale enquanto isso).
    pub(crate) async fn rebuild(self: &Arc<Self>, tab: &'static str) {
        if let Some(r) = self.rows.lock().await.get_mut(tab) {
            r.0 = Instant::now() - ROWS_TTL - Duration::from_secs(1);
        }
        self.rows(tab).await;
    }

    /// As fileiras prontas vão para o disco (`state_dir/leitura-fileiras.json`):
    /// reiniciar o Anubis não as perde.
    async fn save_rows(&self) {
        let now = Instant::now();
        let epoch = chrono::Utc::now().timestamp();
        let snapshot: HashMap<String, (i64, Rows)> = self
            .rows
            .lock()
            .await
            .iter()
            .map(|(k, (at, r))| (k.clone(), (epoch - now.saturating_duration_since(*at).as_secs() as i64 + at.saturating_duration_since(now).as_secs() as i64, (**r).clone())))
            .collect();
        let path = self.orch.cfg.state_dir().join("leitura-fileiras.json");
        crate::disk::save_json_or_warn(&path, &snapshot).await;
    }

    async fn load_rows(&self) {
        let path = self.orch.cfg.state_dir().join("leitura-fileiras.json");
        let Some(saved): Option<HashMap<String, (i64, Rows)>> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()) else { return };
        let now = Instant::now();
        let epoch = chrono::Utc::now().timestamp();
        let mut rows = self.rows.lock().await;
        for (k, (at, r)) in saved {
            // gravada antes dos temas (ou de uma aba que não existe mais): remonta já
            if r.temas.is_empty() {
                rows.insert(k, (now.checked_sub(ROWS_TTL + Duration::from_secs(1)).unwrap_or(now), Arc::new(r)));
                continue;
            }
            let age = Duration::from_secs((epoch - at).max(0) as u64);
            // o instante de quando foi montada (no futuro: Artigos, que vale 24 h)
            let when = if at > epoch { now + Duration::from_secs((at - epoch) as u64) } else { now.checked_sub(age).unwrap_or(now - ROWS_TTL) };
            rows.insert(k, (when, Arc::new(r)));
        }
    }

    /// Monta as fileiras de tempos em tempos: o gosto do que se lê chega aos
    /// filmes mesmo sem abrir a Leitura, e nenhuma aba abre vazia.
    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            me.load_rows().await;
            loop {
                for tab in ["Livros", "Mangás e HQs", "Artigos"] {
                    me.rows(tab).await;
                    // espera essa montar antes da próxima (uma página do FlareSolverr por vez)
                    while me.building.lock().await.contains(tab) {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
                tokio::time::sleep(ROWS_TTL).await;
            }
        });
    }

    /// O peso de um livro do Drive no gosto: lido 3, lendo 2, só na estante 1.
    async fn weight_of(&self, b: &crate::drive::Book) -> f64 {
        let (p, m) = match &self.kosync {
            Some(k) => (k.get(&b.doc).await, k.mark(&b.doc).await),
            None => (None, None),
        };
        let lido = m.map(|m| m.lido).unwrap_or(p.as_ref().is_some_and(|p| p.percentage >= 0.995));
        if lido { 3.0 } else if p.is_some() { 2.0 } else { 1.0 }
    }

    /// O perfil de estudo (src/estudo.rs) a partir de Técnicos e Artigos do Drive.
    async fn study_areas(&self) -> anyhow::Result<Vec<Area>> {
        let books = self.drive.books().await?;
        let mut list = vec![];
        for b in books.iter().filter(|b| b.category == "Técnicos" || b.category == "Artigos") {
            list.push((b.path.clone(), b.title.clone(), self.weight_of(b).await));
        }
        Ok(self.estudo.profile(&list).await)
    }

    /// O gosto de quem lê (assuntos de Ficção e Mangás e HQs do Drive) e os
    /// autores das pastas da aba, com o peso de cada livro: lido 3, lendo 2,
    /// só na estante 1. Volta também o gosto só do que foi lido ou está sendo
    /// lido: é o que vai para os filmes.
    async fn reader_taste(&self, folders: &[&str]) -> anyhow::Result<(Taste, Taste, HashMap<String, f64>)> {
        let books = self.drive.books().await?;
        let mut taste = Taste::default();
        let mut read = Taste::default();
        let mut authors: HashMap<String, f64> = HashMap::new();
        for b in books.iter().filter(|b| b.category == "Ficção" || b.category == "Mangás e HQs" || folders.contains(&b.category.as_str())) {
            let w = self.weight_of(b).await;
            let author = b.author.as_deref().and_then(author_name);
            let work = self.ol.work_of(&b.path, &b.title, author.as_deref()).await;
            if b.category == "Ficção" || b.category == "Mangás e HQs" {
                let (mut g, mut k) = subject_terms(work.as_ref().map(|w| w.subject.as_slice()).unwrap_or(&[]));
                let mut weight = w;
                // edição traduzida sem assuntos: os das obras mais lidas do
                // autor, com metade do peso (ler Funke diz algo do gosto)
                if g.is_empty() && k.is_empty()
                    && let Some(a) = author.clone().or_else(|| work.as_ref().and_then(|w| w.author_name.first().cloned()))
                {
                    let subjects: Vec<String> = self.ol.works_by(&a).await.into_iter().take(3).flat_map(|w| w.subject).collect();
                    (g, k) = subject_terms(&subjects);
                    weight = w / 2.0;
                }
                taste.add(weight, &g, &k);
                if w >= 2.0 {
                    read.add(weight, &g, &k);
                }
            }
            if folders.contains(&b.category.as_str())
                && let Some(a) = author.or_else(|| work.and_then(|w| w.author_name.into_iter().next()))
            {
                *authors.entry(a).or_default() += w;
            }
        }
        Ok((taste.normalized(), read.normalized(), authors))
    }

    /// Cartão de um item de lista de fora, conferido no Open Library (autor,
    /// assuntos, edição em português, se já está na estante, a pasta).
    async fn enrich(&self, it: &Item, from_amazon: bool, pasta: Option<&'static str>, shelf: &HashMap<String, String>) -> Cand {
        let mut it = it.clone();
        it.title = crate::vitrine_livros::strip_series(&it.title);
        let it = &it;
        let work: Option<Work> = self.ol.find_cached(&it.title, it.author.as_deref()).await;
        let (mut genres, mut keywords) = work.as_ref().map(|w| subject_terms(&w.subject)).unwrap_or_default();
        if !it.genres.is_empty() || !it.tags.is_empty() {
            let (g, k) = anilist_terms(&it.genres, &it.tags);
            genres.extend(g);
            keywords.extend(k);
        }
        let titles: Vec<&str> = std::iter::once(it.title.as_str())
            .chain(work.iter().map(|w| w.title.as_str()))
            .chain(work.iter().flat_map(|w| w.alternative_title.iter().map(String::as_str)))
            .collect();
        let estante = titles.iter().find_map(|t| owned_path(t, shelf));
        let subjects = work.as_ref().map(|w| w.subject.clone()).unwrap_or_default();
        let pasta = pasta.unwrap_or_else(|| pasta_by_subject(&genres, &subjects, "Ficção"));
        Cand {
            s: Suggestion {
                key: it.key.clone(),
                title: it.title.clone(),
                author: it.author.clone().or_else(|| work.as_ref().and_then(|w| w.author_name.first().cloned())).unwrap_or_default(),
                year: it.year.or(work.as_ref().and_then(|w| w.first_publish_year)),
                cover: it.cover.clone().or_else(|| work.as_ref().and_then(|w| w.cover())),
                portuguese: from_amazon || work.as_ref().is_some_and(|w| w.has_portuguese()),
                reason: String::new(),
                link: link_of(&it.key),
                estante,
                pdf: None,
                pasta: Some(pasta.to_string()),
            },
            nonfiction: !subjects.is_empty() && !subjects.iter().any(|s| crate::gosto::fold(s).contains("fiction")),
            genres,
            keywords,
            pt: from_amazon,
            theme: None,
        }
    }

    /// Os temas que o sistema propõe para a aba (antes da edição da pessoa).
    async fn system_themes(&self, tab: &str, taste: &Taste) -> anyhow::Result<Vec<Area>> {
        let mut out = vec![];
        if tab == "Livros" || tab == "Artigos" {
            out.extend(self.study_areas().await?);
        }
        if tab == "Livros" || tab == "Mangás e HQs" {
            // o gosto: os gêneros e as palavras-chave mais fortes
            let mut gs: Vec<(&i64, &f64)> = taste.genres.iter().filter(|(_, x)| **x > 0.2).collect();
            gs.sort_by(|a, b| b.1.total_cmp(a.1));
            for (g, x) in gs.into_iter().take(3) {
                if let (Some(nome), Some(subject)) = (genre_name(*g), genre_subject(*g)) {
                    let mut a = Area::new(nome.to_string(), vec![subject.to_string()], *x, Origem::Gosto);
                    a.genre = Some(*g);
                    out.push(a);
                }
            }
            for (k, x) in taste.top_keywords(5) {
                out.push(Area::new(k.clone(), vec![k], x, Origem::Gosto));
            }
        }
        Ok(out)
    }

    async fn build_rows(&self, tab: &str) -> anyhow::Result<Rows> {
        let folders: &[&str] = match tab {
            "Livros" => &["Ficção", "Técnicos"],
            "Artigos" => &["Artigos"],
            _ => &["Mangás e HQs"],
        };
        // primeiro o gosto: ele também busca a obra de cada livro do Drive, que
        // dá o título original para reconhecer o que já se tem
        let (reader, read_only, authors) = self.reader_taste(folders).await?;
        let books = self.drive.books().await?;
        // título (comparável) → caminho no Drive, de todas as pastas
        let mut shelf: HashMap<String, String> = HashMap::new();
        for b in books.iter() {
            shelf.insert(norm(&b.title), b.path.clone());
            if let Some(Some(w)) = self.ol.known(&b.path).await {
                shelf.insert(norm(&w.title), b.path.clone());
            }
        }
        // o que se lê (lido ou lendo) volta para os filmes; os temas que se
        // repetem na estante desempatam
        tracing::info!("Leitura: gosto dos livros lidos para os filmes: {:?}", read_only.top_keywords(8));
        *self.orch.for_you_cache.book_taste.write().await = Some(Taste::blend(&[(0.75, &read_only), (0.25, &reader)]));

        // ---- gosto: metade do que se assiste, metade do que se lê
        let viewer = self.orch.viewer_taste().await.unwrap_or_default().for_books();
        let fiction_taste = match (viewer.is_empty(), reader.is_empty()) {
            (true, _) => reader.clone(),
            (_, true) => viewer.clone(),
            _ => Taste::blend(&[(0.5, &viewer), (0.5, &reader)]),
        };
        // ---- os temas: os do sistema, editados pela pessoa
        let system = self.system_themes(tab, &fiction_taste).await?;
        self.system_themes.lock().await.insert(tab.to_string(), system.clone());
        let themes = self.temas.apply(tab, system).await;

        // Artigos: o OpenAlex, pelos temas (os do gosto não: são de ficção)
        if tab == "Artigos" {
            let mut rows = self.estudo.article_rows(&themes).await?;
            for r in [&mut rows.para_voce, &mut rows.destaques, &mut rows.lancamentos, &mut rows.populares] {
                for s in r.iter_mut() {
                    s.estante = owned_path(&s.title, &shelf);
                    s.pasta = Some("Artigos".into());
                }
            }
            rows.temas = themes;
            return Ok(rows);
        }
        let taste = Taste::blend(&[(0.5, &fiction_taste), (0.5, &crate::estudo::Estudo::taste(&themes))]);

        // ---- listas de fora
        let plan = plan(tab);
        let mut lanc = vec![];
        let mut pop = vec![];
        for src in &plan.amazon {
            if let Some(cat) = src.amazon {
                lanc.push((self.vitrine.amazon(cat, true).await, src.pasta));
                pop.push((self.vitrine.amazon(cat, false).await, src.pasta));
            }
        }
        let mut dest = vec![];
        for src in &plan.goodreads {
            if let Some(g) = src.goodreads {
                dest.push((self.vitrine.goodreads(g).await, src.pasta));
            }
        }
        let row = async |lists: Vec<(Vec<Item>, Option<&'static str>)>, from_amazon: bool| {
            let mut out = vec![];
            for (it, pasta) in interleave(lists).iter().take(ENRICH * 2) {
                out.push(self.enrich(it, from_amazon, *pasta, &shelf).await);
                if out.len() >= ROW_LEN {
                    break;
                }
            }
            out
        };
        let mut destaques = row(dest, false).await;
        let mut lancamentos = row(lanc, true).await;
        let mut populares = row(pop, true).await;
        // infantil fora, a não ser que seja do gosto de quem lê
        if tab == "Livros" {
            let kids = reader.genres.get(&10751).copied().unwrap_or(0.0) >= 0.3;
            for r in [&mut destaques, &mut lancamentos, &mut populares] {
                r.retain(|c| kids || !c.genres.contains(&10751));
            }
        }
        let top_author = authors.values().copied().fold(0.0, f64::max).max(1.0);

        // ---- candidatos do "Para você"
        let mut cands: Vec<Cand> = vec![];
        let mut author_list: Vec<(String, f64)> = authors.clone().into_iter().collect();
        author_list.sort_by(|a, b| b.1.total_cmp(&a.1));
        for (a, _) in author_list.iter().take(MAX_AUTHORS) {
            for w in self.ol.works_by(a).await {
                if !(w.has_portuguese() || w.language.iter().any(|l| l == "eng")) {
                    continue;
                }
                cands.push(work_cand(w, Some(a.clone()), None, "Ficção"));
            }
        }
        // os temas (os da pessoa primeiro: já vêm na frente)
        let top_theme = themes.iter().map(|t| t.peso).fold(0.0, f64::max).max(1.0);
        for t in themes.iter().take(MAX_THEMES) {
            let weight = t.peso / top_theme;
            if plan.anilist {
                let items = match t.genre {
                    Some(g) => self.vitrine.anilist(anilist_genres(g), &[]).await,
                    None => self.vitrine.anilist_search(&t.nome).await,
                };
                for it in items {
                    let mut c = self.enrich(&it, false, Some("Mangás e HQs"), &shelf).await;
                    c.theme = Some((t.nome.clone(), weight));
                    cands.push(c);
                }
                continue;
            }
            // tema da pessoa: busca livre (o nome como ela escreveu); os do
            // sistema: pelo assunto
            let works = if t.origem == Origem::Voce {
                self.ol.search_works(&t.nome).await.unwrap_or_default()
            } else {
                match t.termos.first() {
                    Some(term) => self.ol.works_about(&crate::gosto::keyword(term).unwrap_or_else(|| term.to_lowercase())).await,
                    None => vec![],
                }
            };
            // sem assunto: estudo e os da pessoa vão para Técnicos; o gosto, Ficção
            let default = if t.origem == Origem::Gosto { "Ficção" } else { "Técnicos" };
            for w in works.into_iter().filter(|w| w.has_portuguese() || w.language.iter().any(|l| l == "eng")) {
                cands.push(work_cand(w, None, Some((t.nome.clone(), weight)), default));
            }
        }
        for c in destaques.iter().chain(&lancamentos).chain(&populares) {
            cands.push(Cand { s: c.s.clone(), genres: c.genres.clone(), keywords: c.keywords.clone(), pt: c.pt, nonfiction: c.nonfiction, theme: None });
        }

        // ---- nota de cada um: o gosto + o tema + o autor que se lê + português
        let mut seen = HashSet::new();
        let mut scored: Vec<(f64, Suggestion)> = vec![];
        for mut c in cands {
            let titles_owned = c.s.estante.is_some() || owned_path(&c.s.title, &shelf).is_some();
            // título em outro alfabeto (hebraico, cirílico, japonês): não dá para ler
            let latin = c.s.title.chars().all(|ch| (ch as u32) < 0x0250 || !ch.is_alphabetic());
            if titles_owned || !latin || !seen.insert(norm(&c.s.title)) {
                continue;
            }
            let (fit, why) = taste.fit(&c.genres, &c.keywords);
            let affinity = authors.get(&c.s.author).map(|w| w / top_author).unwrap_or(0.0);
            let theme = c.theme.as_ref().map(|t| t.1).unwrap_or(0.0);
            let score = fit + AUTHOR_WEIGHT * affinity + THEME_WEIGHT * theme + if c.pt || c.s.portuguese { 0.1 } else { 0.0 };
            c.s.reason = if affinity >= 0.3 {
                format!("De {}, que você lê", c.s.author)
            } else if let Some((nome, _)) = &c.theme {
                format!("Do tema: {nome}")
            } else if let Some(k) = why {
                let t = themes.iter().find(|t| t.keywords.contains(&k));
                match t {
                    Some(t) => format!("Do tema: {}", t.nome),
                    None => format!("Porque você curte {k}"),
                }
            } else if let Some(g) = c.genres.iter().max_by(|a, b| taste.genres.get(a).unwrap_or(&0.0).total_cmp(taste.genres.get(b).unwrap_or(&0.0))).and_then(|g| genre_name(*g)) {
                format!("Combina com o seu gosto por {g}")
            } else {
                "Popular agora".to_string()
            };
            scored.push((score, c.s));
        }
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        // tema posto pela pessoa é pedido: os 2 melhores de cada um vêm na frente
        let mut para_voce: Vec<Suggestion> = vec![];
        for t in themes.iter().filter(|t| t.origem == Origem::Voce) {
            let reason = format!("Do tema: {}", t.nome);
            para_voce.extend(scored.iter().filter(|(_, s)| s.reason == reason).take(USER_THEME_SLOTS).map(|(_, s)| s.clone()));
        }
        let mut per_author: HashMap<String, usize> = HashMap::new();
        for s in &para_voce {
            *per_author.entry(s.author.clone()).or_default() += 1;
        }
        let rest: Vec<Suggestion> = scored
            .into_iter()
            .map(|(_, s)| s)
            .filter(|s| !para_voce.iter().any(|p| p.key == s.key))
            .filter(|s| {
                let n = per_author.entry(s.author.clone()).or_default();
                *n += 1;
                *n <= MAX_PER_AUTHOR
            })
            .collect();
        para_voce.extend(rest);
        para_voce.truncate(ROW_LEN);
        Ok(Rows {
            temas: themes,
            para_voce,
            destaques: destaques.into_iter().map(|c| c.s).collect(),
            lancamentos: lancamentos.into_iter().map(|c| c.s).collect(),
            populares: populares.into_iter().map(|c| c.s).collect(),
        })
    }
}

/// Endereço da obra na fonte.
pub fn link_of(key: &str) -> Option<String> {
    let (src, id) = key.split_once(':').unwrap_or(("ol", key));
    Some(match src {
        "amz" => format!("https://www.amazon.com.br/dp/{id}"),
        "gr" => format!("https://www.goodreads.com/book/show/{id}"),
        "al" => format!("https://anilist.co/manga/{id}"),
        _ => format!("https://openlibrary.org{key}"),
    })
}

fn work_cand(w: Work, author: Option<String>, theme: Option<(String, f64)>, default: &'static str) -> Cand {
    let (genres, keywords) = subject_terms(&w.subject);
    let pasta = pasta_by_subject(&genres, &w.subject, default);
    Cand {
        s: Suggestion {
            link: link_of(&w.key),
            key: w.key.clone(),
            title: w.title.clone(),
            author: author.or_else(|| w.author_name.first().cloned()).unwrap_or_default(),
            year: w.first_publish_year,
            cover: w.cover(),
            portuguese: w.has_portuguese(),
            reason: String::new(),
            estante: None,
            pdf: None,
            pasta: Some(pasta.to_string()),
        },
        nonfiction: false,
        genres,
        keywords,
        pt: false,
        theme,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn it(t: &str) -> Item {
        Item { key: t.into(), title: t.into(), author: None, cover: None, genres: vec![], tags: vec![], year: None }
    }

    #[test]
    fn intercala_sem_repetir() {
        let out = interleave(vec![(vec![it("A"), it("B"), it("C")], Some("Ficção")), (vec![it("X"), it("a"), it("Y")], Some("Técnicos"))]);
        let t: Vec<(&str, Option<&str>)> = out.iter().map(|(i, p)| (i.title.as_str(), *p)).collect();
        assert_eq!(t, [("A", Some("Ficção")), ("X", Some("Técnicos")), ("B", Some("Ficção")), ("C", Some("Ficção")), ("Y", Some("Técnicos"))]);
    }

    #[test]
    fn pasta_pelo_assunto() {
        assert_eq!(pasta_by_subject(&[], &["Fiction, science fiction, general".into()], "Técnicos"), "Ficção");
        assert_eq!(pasta_by_subject(&[], &["Computer security".into()], "Ficção"), "Técnicos");
        assert_eq!(pasta_by_subject(&[], &[], "Técnicos"), "Técnicos");
    }
}
