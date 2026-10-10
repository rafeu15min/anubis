//! As fileiras da Leitura, como as dos filmes: Para você, Destaques,
//! Lançamentos e Populares no Brasil (fontes: src/vitrine_livros.rs).
//!
//! "Para você" usa o mesmo gosto dos filmes (src/gosto.rs): metade vem do que
//! se assiste (gêneros e palavras-chave do TMDB, sem os só de tela, como
//! Animação), metade do que se lê (os assuntos do Open Library dos livros do
//! Drive, pesados como no vídeo: lido 3, lendo 2, só na estante 1). Os
//! autores que se lê contam à parte. Candidatos: as obras desses autores, as
//! dos assuntos mais fortes ("cyberpunk"), os mangás do AniList dos gêneros
//! preferidos e tudo o que está nas outras fileiras. O gosto do que se lê
//! também volta para os filmes (`foryou::Cache::book_taste`).

use crate::gosto::{Taste, anilist_terms, subject_terms};
use crate::leitura::{Leitura, Suggestion, author_name, norm, owned_path};
use crate::openlibrary::Work;
use crate::vitrine_livros::{Item, amazon};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// as fileiras prontas, por categoria
pub const ROWS_TTL: Duration = Duration::from_secs(12 * 3600);
const ROW_LEN: usize = 20;
const MAX_AUTHORS: usize = 8;
const MAX_PER_AUTHOR: usize = 2;
/// quanto pesa ser de um autor que se lê (o gosto vale até 1)
const AUTHOR_WEIGHT: f64 = 0.5;
/// quantos títulos de cada lista de fora são conferidos no Open Library
const ENRICH: usize = 15;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Rows {
    /// Técnicos e Artigos: o perfil de estudo que montou as fileiras
    pub areas: Vec<crate::estudo::Area>,
    pub para_voce: Vec<Suggestion>,
    pub destaques: Vec<Suggestion>,
    pub lancamentos: Vec<Suggestion>,
    pub populares: Vec<Suggestion>,
}

/// De onde vêm as fileiras de cada pasta.
struct Plan {
    amazon: &'static [&'static str],
    goodreads: &'static [&'static str],
    anilist: bool,
}

fn plan(folder: &str) -> Plan {
    match folder {
        "Ficção" => Plan { amazon: &[amazon::LITERATURA, amazon::FANTASIA_HORROR_FC, amazon::POLICIAL, amazon::ROMANCE], goodreads: &["brasil"], anilist: false },
        "Mangás e HQs" => Plan { amazon: &[amazon::HQS_MANGAS], goodreads: &["manga", "comics"], anilist: true },
        "Técnicos" => Plan {
            amazon: &[amazon::SEGURANCA, amazon::PROGRAMACAO, amazon::HARDWARE, amazon::ESCOLAS_ENSINO, amazon::EDUCACAO, amazon::MATEMATICA],
            goodreads: &["computer-science", "artificial-intelligence", "mathematics"],
            anilist: false,
        },
        _ => Plan { amazon: &[], goodreads: &[], anilist: false },
    }
}

/// Nome do gênero (unificado do TMDB) para o "porquê".
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

/// Uma lista intercalada (a 1ª de cada, a 2ª de cada…), sem repetir título.
fn interleave(lists: Vec<Vec<Item>>) -> Vec<Item> {
    let mut out: Vec<Item> = vec![];
    let mut seen = HashSet::new();
    let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
    for i in 0..longest {
        for l in &lists {
            if let Some(it) = l.get(i)
                && seen.insert(norm(&it.title))
            {
                out.push(it.clone());
            }
        }
    }
    out
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
}

impl Leitura {
    /// As fileiras da categoria se já estão prontas; senão começa a montar.
    pub(crate) async fn rows(self: &Arc<Self>, folder: &'static str) -> Option<Arc<Rows>> {
        let cached = self.rows.lock().await.get(folder).cloned();
        if let Some((at, rows)) = &cached
            && at.elapsed() < ROWS_TTL
        {
            return Some(rows.clone());
        }
        if self.building.lock().await.insert(folder.to_string()) {
            let me = self.clone();
            tokio::spawn(async move {
                match me.build_rows(folder).await {
                    Ok(rows) => {
                        // Artigos dura 24 h (a cota diária do OpenAlex): conta como montado 12 h depois
                        let at = if folder == "Artigos" { Instant::now() + ROWS_TTL } else { Instant::now() };
                        me.rows.lock().await.insert(folder.to_string(), (at, Arc::new(rows)));
                    }
                    Err(e) => {
                        tracing::warn!("Leitura ({folder}): {e:#}");
                        // fica o que havia; nova tentativa em 1 h (não a cada visita)
                        let mut rows = me.rows.lock().await;
                        let old = rows.get(folder).map(|r| r.1.clone()).unwrap_or_default();
                        rows.insert(folder.to_string(), (Instant::now() - ROWS_TTL + Duration::from_secs(3600), old));
                    }
                }
                me.building.lock().await.remove(folder);
            });
        }
        cached.map(|(_, r)| r) // o antigo vale enquanto o novo é montado
    }

    /// Monta as fileiras de Ficção e Mangás e HQs de tempos em tempos: o
    /// gosto do que se lê chega aos filmes mesmo sem abrir a Leitura.
    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                for folder in ["Ficção", "Mangás e HQs"] {
                    me.rows(folder).await;
                    // espera essa montar antes da próxima (uma página do FlareSolverr por vez)
                    while me.building.lock().await.contains(folder) {
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
    async fn study_areas(&self) -> anyhow::Result<Vec<crate::estudo::Area>> {
        let books = self.drive.books().await?;
        let mut list = vec![];
        for b in books.iter().filter(|b| b.category == "Técnicos" || b.category == "Artigos") {
            list.push((b.path.clone(), b.title.clone(), self.weight_of(b).await));
        }
        Ok(self.estudo.profile(&list).await)
    }

    /// O gosto de quem lê (Ficção e Mangás e HQs do Drive) e os autores, com
    /// o peso de cada livro: lido 3, lendo 2, só na estante 1.
    /// Volta também o gosto só do que foi lido ou está sendo lido: é o que vai
    /// para os filmes (a estante inteira tem muito livro que nunca foi aberto).
    async fn reader_taste(&self, folder: &str) -> anyhow::Result<(Taste, Taste, HashMap<String, f64>)> {
        let books = self.drive.books().await?;
        let mut taste = Taste::default();
        let mut read = Taste::default();
        let mut authors: HashMap<String, f64> = HashMap::new();
        for b in books.iter().filter(|b| b.category == "Ficção" || b.category == "Mangás e HQs" || b.category == folder) {
            let w = self.weight_of(b).await;
            let author = b.author.as_deref().and_then(author_name);
            // a obra só vale se é mesmo a do livro (o Open Library às vezes
            // devolve outra: um faroeste no lugar de um livro técnico)
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
            if b.category == folder
                && let Some(a) = author.or_else(|| work.and_then(|w| w.author_name.into_iter().next()))
            {
                *authors.entry(a).or_default() += w;
            }
        }
        Ok((taste.normalized(), read.normalized(), authors))
    }

    /// Cartão de um item de lista de fora, conferido no Open Library (autor,
    /// assuntos, edição em português, se já está na estante).
    async fn enrich(&self, it: &Item, from_amazon: bool, shelf: &HashMap<String, String>) -> Cand {
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
        let pt = from_amazon;
        Cand {
            s: Suggestion {
                key: it.key.clone(),
                title: it.title.clone(),
                author: it.author.clone().or_else(|| work.as_ref().and_then(|w| w.author_name.first().cloned())).unwrap_or_default(),
                year: it.year.or(work.as_ref().and_then(|w| w.first_publish_year)),
                cover: it.cover.clone().or_else(|| work.as_ref().and_then(|w| w.cover())),
                portuguese: pt || work.as_ref().is_some_and(|w| w.has_portuguese()),
                reason: String::new(),
                link: link_of(&it.key),
                estante,
                pdf: None,
            },
            nonfiction: work.as_ref().is_some_and(|w| !w.subject.is_empty() && !w.subject.iter().any(|s| crate::gosto::fold(s).contains("fiction"))),
            genres,
            keywords,
            pt,
        }
    }

    async fn build_rows(&self, folder: &str) -> anyhow::Result<Rows> {
        // Artigos: o OpenAlex, pelo perfil de estudo
        if folder == "Artigos" {
            let areas = self.study_areas().await?;
            let books = self.drive.books().await?;
            let shelf: HashMap<String, String> = books.iter().map(|b| (norm(&b.title), b.path.clone())).collect();
            let mut rows = self.estudo.article_rows(&areas).await?;
            for r in [&mut rows.para_voce, &mut rows.destaques, &mut rows.lancamentos, &mut rows.populares] {
                for s in r.iter_mut() {
                    s.estante = owned_path(&s.title, &shelf);
                }
            }
            rows.areas = areas;
            return Ok(rows);
        }
        // primeiro o gosto: ele também busca a obra de cada livro do Drive, que
        // dá o título original para reconhecer o que já se tem
        let (reader, read_only, authors) = self.reader_taste(folder).await?;
        let books = self.drive.books().await?;
        // título (comparável) → caminho no Drive, de todas as pastas
        let mut shelf: HashMap<String, String> = HashMap::new();
        for b in books.iter() {
            shelf.insert(norm(&b.title), b.path.clone());
            if let Some(Some(w)) = self.ol.known(&b.path).await {
                shelf.insert(norm(&w.title), b.path.clone());
            }
        }
        let plan = plan(folder);

        // ---- listas de fora
        let mut lanc = vec![];
        let mut pop = vec![];
        for cat in plan.amazon {
            lanc.push(self.vitrine.amazon(cat, true).await);
            pop.push(self.vitrine.amazon(cat, false).await);
        }
        let mut dest = vec![];
        for g in plan.goodreads {
            dest.push(self.vitrine.goodreads(g).await);
        }
        let row = async |lists: Vec<Vec<Item>>, from_amazon: bool| {
            let mut out = vec![];
            for it in interleave(lists).iter().take(ENRICH * 2) {
                out.push(self.enrich(it, from_amazon, &shelf).await);
                if out.len() >= ROW_LEN {
                    break;
                }
            }
            out
        };
        let mut destaques = row(dest, false).await;
        let mut lancamentos = row(lanc, true).await;
        let mut populares = row(pop, true).await;
        // infantil fora das fileiras de Ficção, a não ser que seja do gosto de
        // quem lê; e não ficção fora (os mais lidos do Brasil misturam): o
        // Open Library deu assuntos e nenhum é de gênero de ficção
        if folder == "Ficção" {
            let kids = reader.genres.get(&10751).copied().unwrap_or(0.0) >= 0.3;
            for r in [&mut destaques, &mut lancamentos, &mut populares] {
                r.retain(|c| (kids || !c.genres.contains(&10751)) && !(c.genres.is_empty() && c.nonfiction));
            }
        }

        // ---- gosto: metade do que se assiste, metade do que se lê
        if folder == "Ficção" || folder == "Mangás e HQs" {
            // o que se lê (lido ou lendo) volta para os filmes
            tracing::info!("Leitura: gosto dos livros lidos para os filmes: {:?}", read_only.top_keywords(8));
            // os temas que se repetem na estante desempatam
            let for_films = Taste::blend(&[(0.75, &read_only), (0.25, &reader)]);
            *self.orch.for_you_cache.book_taste.write().await = Some(for_films);
        }
        // Técnicos: o perfil de estudo no lugar do gosto de ficção
        let study = if folder == "Técnicos" { self.study_areas().await? } else { vec![] };
        let viewer = self.orch.viewer_taste().await.unwrap_or_default().for_books();
        let taste = if folder == "Técnicos" {
            crate::estudo::Estudo::taste(&study)
        } else {
            match (viewer.is_empty(), reader.is_empty()) {
                (true, _) => reader.clone(),
                (_, true) => viewer.clone(),
                _ => Taste::blend(&[(0.5, &viewer), (0.5, &reader)]),
            }
        };
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
                cands.push(work_cand(w, Some(a.clone())));
            }
        }
        if folder == "Ficção" || folder == "Técnicos" {
            for (k, _) in taste.top_keywords(4) {
                for w in self.ol.works_about(&k).await.into_iter().filter(|w| w.has_portuguese() || w.language.iter().any(|l| l == "eng")) {
                    cands.push(work_cand(w, None));
                }
            }
        }
        if plan.anilist {
            let mut gs: Vec<(&i64, &f64)> = taste.genres.iter().filter(|(_, x)| **x > 0.0).collect();
            gs.sort_by(|a, b| b.1.total_cmp(a.1));
            for (g, _) in gs.into_iter().take(2) {
                for it in self.vitrine.anilist(anilist_genres(*g), &[]).await {
                    cands.push(self.enrich(&it, false, &shelf).await);
                }
            }
        }
        for c in destaques.iter().chain(&lancamentos).chain(&populares) {
            cands.push(Cand { s: c.s.clone(), genres: c.genres.clone(), keywords: c.keywords.clone(), pt: c.pt, nonfiction: c.nonfiction });
        }

        // ---- nota de cada um: o gosto + o autor que se lê + português
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
            // Técnicos: só o que é da área (o mesmo nome de autor traz "Faust")
            if folder == "Técnicos" && fit <= 0.05 {
                continue;
            }
            let affinity = authors.get(&c.s.author).map(|w| w / top_author).unwrap_or(0.0);
            let score = fit + AUTHOR_WEIGHT * affinity + if c.pt || c.s.portuguese { 0.1 } else { 0.0 };
            c.s.reason = if affinity >= 0.3 {
                format!("De {}, que você lê", c.s.author)
            } else if let Some(k) = why {
                if folder == "Técnicos" { format!("Da sua área: {k}") } else { format!("Porque você curte {k}") }
            } else if let Some(g) = c.genres.iter().max_by(|a, b| taste.genres.get(a).unwrap_or(&0.0).total_cmp(taste.genres.get(b).unwrap_or(&0.0))).and_then(|g| genre_name(*g)) {
                format!("Combina com o seu gosto por {g}")
            } else {
                "Popular agora".to_string()
            };
            scored.push((score, c.s));
        }
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut per_author: HashMap<String, usize> = HashMap::new();
        let para_voce = scored
            .into_iter()
            .map(|(_, s)| s)
            .filter(|s| {
                let n = per_author.entry(s.author.clone()).or_default();
                *n += 1;
                *n <= MAX_PER_AUTHOR
            })
            .take(ROW_LEN)
            .collect();
        Ok(Rows { areas: study, para_voce, destaques: destaques.into_iter().map(|c| c.s).collect(), lancamentos: lancamentos.into_iter().map(|c| c.s).collect(), populares: populares.into_iter().map(|c| c.s).collect() })
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

fn work_cand(w: Work, author: Option<String>) -> Cand {
    let (genres, keywords) = subject_terms(&w.subject);
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
        },
        nonfiction: false,
        genres,
        keywords,
        pt: false,
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
        let out = interleave(vec![vec![it("A"), it("B"), it("C")], vec![it("X"), it("a"), it("Y")]]);
        let t: Vec<&str> = out.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(t, ["A", "X", "B", "C", "Y"]);
    }
}
