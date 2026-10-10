//! Gosto comum a filmes e livros: o mesmo vocabulário para os dois lados,
//! para o que se assiste puxar o que se lê, e o contrário.
//!
//! - Gêneros: os unificados do TMDB (`foryou::common_genre`: Ação e Aventura,
//!   Sci-Fi & Fantasia, Mistério, Terror…). Os assuntos dos livros (Open
//!   Library) e os gêneros do AniList são traduzidos para eles. Os que só
//!   existem na tela (Animação, Documentário, programas de TV) não valem para
//!   livros.
//! - Palavras-chave: as do TMDB ("cyberpunk", "dystopia"), os assuntos dos
//!   livros ("Cyberpunk fiction", "Hackers") e as tags do AniList, todos
//!   normalizados do mesmo jeito (minúsculas, no singular, sem "fiction"), e
//!   sem o ruído (nível de leitura, prêmios, códigos de biblioteca, "sequel").

use std::collections::HashMap;

/// Gêneros só de tela: não contam para livros.
pub const SCREEN_ONLY: [i64; 7] = [16, 99, 10763, 10764, 10766, 10767, 10770];

/// Assunto de livro (minúsculo, sem acento) → gênero unificado do TMDB.
const SUBJECT_GENRES: &[(&str, i64)] = &[
    ("science fiction", 10765), ("ciencia-ficcion", 10765), ("ficcao cientifica", 10765), ("cyberpunk", 10765),
    ("fantasy", 10765), ("fantasia", 10765), ("dystopia", 10765), ("space opera", 10765),
    ("horror", 27), ("terror", 27), ("ghost stor", 27),
    ("mystery", 9648), ("detective", 9648), ("misterio", 9648),
    ("crime", 80), ("policial", 80), ("noir", 80),
    ("thriller", 53), ("suspense", 53), ("espionage", 53), ("spy stor", 53),
    ("romance", 10749), ("love stor", 10749),
    ("adventure", 10759), ("aventura", 10759), ("action", 10759), ("sea stor", 10759),
    ("war stor", 10768), ("guerra", 10768), ("military fiction", 10768),
    ("historical fiction", 36), ("romance historico", 36),
    ("humor", 35), ("humorous", 35), ("comedy", 35), ("satire", 35),
    ("western", 37),
    ("psychological fiction", 18), ("literary fiction", 18), ("domestic fiction", 18), ("drama", 18), ("bildungsroman", 18),
    ("juvenile fiction", 10751), ("children", 10751),
    ("music", 10402),
];

/// Gêneros que também são temas: viram palavra-chave além do gênero.
const THEMES: &[&str] = &["cyberpunk", "dystopia", "space opera", "noir", "espionage", "satire", "western"];

/// Gênero do AniList → unificado do TMDB.
fn anilist_genre(g: &str) -> Option<i64> {
    Some(match g {
        "Action" | "Adventure" => 10759,
        "Comedy" => 35,
        "Drama" | "Slice of Life" => 18,
        "Fantasy" | "Sci-Fi" | "Mecha" | "Supernatural" | "Mahou Shoujo" => 10765,
        "Horror" => 27,
        "Mystery" => 9648,
        "Psychological" | "Thriller" => 53,
        "Romance" => 10749,
        "Music" => 10402,
        _ => return None,
    })
}

/// Palavras que não dizem nada sozinhas.
const NOISE_WORDS: &[&str] = &[
    "fiction", "fictions", "ficcion", "ficcao", "american", "english", "general", "novel", "novels", "stories", "story",
    "literature", "and", "the", "of", "in", "a", "an", "juvenile", "imaginary", "place", "fictitious", "character",
    "children's", "children", "kid", "kids", "roman", "romans", "romanzo", "novela", "nouvelle", "novella", "novelle", "contemporary", "classic", "classics", "young", "adult", "reader", "readers", "book", "books",
];
/// Assuntos inteiros que são ruído (começo do texto).
const NOISE_SUBJECTS: &[&str] = &[
    "reading level", "award:", "nyt:", "open library", "long now", "accessible book", "protected daisy", "in library",
    "lending library", "large type", "specimen", "translation", "english language", "staff pick", "new york times",
    "fiction", "literature", "novel", "series:", "collectionid", "comic book", "comics", "graphic novel", "manga",
];
/// Palavras-chave do TMDB que falam do filme, não do assunto.
const META_KEYWORDS: &[&str] = &[
    "sequel", "based on novel or book", "based on comic", "based on manga", "based on video game", "based on true story",
    "duringcreditsstinger", "aftercreditsstinger", "woman director", "remake", "anime", "live action", "biography",
    "slow burn", "surprise ending", "short film", "adult animation",
];

/// Sem acento e em minúsculas.
pub fn fold(s: &str) -> String {
    s.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            c => c,
        })
        .collect()
}

fn singular(w: &str) -> String {
    let w = w.trim_end_matches("'s").trim_end_matches('\'');
    if w.len() > 4 && w.ends_with("ies") {
        format!("{}y", &w[..w.len() - 3])
    } else if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") && !w.ends_with("us") && !w.ends_with("is") {
        w[..w.len() - 1].to_string()
    } else {
        w.to_string()
    }
}

/// Uma palavra-chave no vocabulário comum (None: ruído ou genérica demais).
pub fn keyword(raw: &str) -> Option<String> {
    let mut s = fold(raw);
    // "(a.i.)", "(fiction)": fora
    while let (Some(a), Some(b)) = (s.find('('), s.find(')')) {
        if b < a {
            break;
        }
        s.replace_range(a..=b, " ");
    }
    let s = s.trim();
    if META_KEYWORDS.contains(&s) {
        return None;
    }
    let words: Vec<String> = s
        .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '\''))
        .filter(|w| !w.is_empty())
        .map(singular)
        .filter(|w| !NOISE_WORDS.contains(&w.as_str()))
        .collect();
    let ok = !words.is_empty() && words.len() <= 3 && !words.iter().any(|w| w.chars().any(|c| c.is_ascii_digit())) && words.iter().map(|w| w.len()).sum::<usize>() >= 4;
    ok.then(|| words.join(" "))
}

/// Gêneros e palavras-chave dos assuntos de um livro (Open Library).
pub fn subject_terms(subjects: &[String]) -> (Vec<i64>, Vec<String>) {
    let mut genres = vec![];
    let mut keywords = vec![];
    for subject in subjects {
        let f = fold(subject);
        if NOISE_SUBJECTS.iter().any(|n| f.starts_with(n)) && !f.contains(", ") && !f.contains("--") {
            // "Fiction" sozinho é ruído; "Fiction, science fiction, cyberpunk" não
            continue;
        }
        for (pat, g) in SUBJECT_GENRES {
            if f.contains(pat) && !genres.contains(g) {
                genres.push(*g);
            }
        }
        // "Fiction, science fiction, cyberpunk" e "Hackers--fiction": cada parte
        // (o hífen sozinho fica: "ciencia-ficcion" é uma palavra)
        for part in f.split("--").flat_map(|p| p.split(&[',', '/'][..])).map(str::trim).filter(|p| !p.is_empty()) {
            let plain_genre = SUBJECT_GENRES.iter().any(|(p, _)| part == *p) && !THEMES.contains(&part);
            if NOISE_SUBJECTS.iter().any(|n| part.starts_with(n)) || plain_genre {
                continue;
            }
            // "Fantasy fiction" → "fantasy": é gênero, não tema (o gênero já entrou)
            let genre_word = |k: &str| SUBJECT_GENRES.iter().any(|(p, _)| keyword(p).as_deref() == Some(k)) && !THEMES.contains(&k);
            if let Some(k) = keyword(part)
                && !keywords.contains(&k)
                && !genre_word(&k)
            {
                keywords.push(k);
            }
        }
    }
    (genres, keywords)
}

/// Gêneros e palavras-chave de um mangá (AniList).
pub fn anilist_terms(genres: &[String], tags: &[String]) -> (Vec<i64>, Vec<String>) {
    let mut g: Vec<i64> = genres.iter().filter_map(|x| anilist_genre(x)).collect();
    g.dedup();
    let mut k: Vec<String> = tags.iter().chain(genres).filter_map(|t| keyword(t)).collect();
    k.dedup();
    (g, k)
}

/// O gosto: peso por gênero unificado e por palavra-chave (o maior vale 1).
#[derive(Debug, Clone, Default)]
pub struct Taste {
    pub genres: HashMap<i64, f64>,
    pub keywords: HashMap<String, f64>,
}

fn normalize<K: std::hash::Hash + Eq>(m: &mut HashMap<K, f64>) {
    let top = m.values().map(|x| x.abs()).fold(0.0, f64::max);
    if top > 0.0 {
        m.values_mut().for_each(|x| *x /= top);
    }
}

/// O quanto duas palavras-chave são a mesma: iguais = 1; uma contém a outra
/// palavra por palavra ("computer hacker" e "hacker") = 0,6.
fn kw_match(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    let (short, long) = if a.len() < b.len() { (a, b) } else { (b, a) };
    let lw: Vec<&str> = long.split(' ').collect();
    let sw: Vec<&str> = short.split(' ').collect();
    if lw.windows(sw.len()).any(|w| w == sw.as_slice()) { 0.6 } else { 0.0 }
}

impl Taste {
    pub fn add(&mut self, w: f64, genres: &[i64], keywords: &[String]) {
        for g in genres {
            *self.genres.entry(*g).or_insert(0.0) += w;
        }
        for k in keywords {
            *self.keywords.entry(k.clone()).or_insert(0.0) += w;
        }
    }

    pub fn normalized(mut self) -> Self {
        normalize(&mut self.genres);
        normalize(&mut self.keywords);
        // uma palavra-chave que só apareceu uma vez, de leve, é ruído
        self.keywords.retain(|_, v| v.abs() >= 0.05);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.genres.is_empty() && self.keywords.is_empty()
    }

    /// Soma ponderada de gostos já normalizados (normaliza o resultado).
    pub fn blend(parts: &[(f64, &Taste)]) -> Taste {
        let mut out = Taste::default();
        for (w, t) in parts {
            for (g, x) in &t.genres {
                *out.genres.entry(*g).or_insert(0.0) += w * x;
            }
            for (k, x) in &t.keywords {
                *out.keywords.entry(k.clone()).or_insert(0.0) += w * x;
            }
        }
        out.normalized()
    }

    /// Só os gêneros que valem para livros.
    pub fn for_books(&self) -> Taste {
        let mut t = self.clone();
        t.genres.retain(|g, _| !SCREEN_ONLY.contains(g));
        t
    }

    /// As palavras-chave mais fortes (positivas).
    pub fn top_keywords(&self, n: usize) -> Vec<(String, f64)> {
        let mut v: Vec<(String, f64)> = self.keywords.iter().filter(|(_, x)| **x > 0.0).map(|(k, x)| (k.clone(), *x)).collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1));
        v.truncate(n);
        v
    }

    /// Quanto um título combina (≈ 0–1): a média do gosto pelos gêneros dele
    /// e, das palavras-chave, a soma do que casa (até 1). A palavra-chave que
    /// mais casou volta junto (o "porquê").
    pub fn fit(&self, genres: &[i64], keywords: &[String]) -> (f64, Option<String>) {
        let g = if genres.is_empty() { 0.0 } else { genres.iter().map(|g| self.genres.get(g).copied().unwrap_or(0.0)).sum::<f64>() / genres.len() as f64 };
        let mut best: Option<(String, f64)> = None;
        let mut k = 0.0;
        for item in keywords {
            for (kw, w) in &self.keywords {
                let m = kw_match(item, kw) * w;
                if m != 0.0 {
                    k += m;
                    if m > 0.0 && best.as_ref().is_none_or(|b| m > b.1) {
                        best = Some((kw.clone(), m));
                    }
                }
            }
        }
        (0.6 * g + 0.4 * k.clamp(-1.0, 1.0), best.map(|b| b.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn neuromancer_e_cyberpunk_e_ficcao_cientifica() {
        let (g, k) = subject_terms(&s(&[
            "Computer hackers", "Fiction", "Science fiction", "Reading Level-Grade 9", "award:hugo_award=1985",
            "Fiction, science fiction, cyberpunk", "Hackers (fiction)", "813/.54", "Cyberpunk fiction", "Virtual reality",
            "Open Library Staff Picks", "Matrix (Imaginary place)",
        ]));
        assert_eq!(g, vec![10765]);
        for want in ["cyberpunk", "hacker", "computer hacker", "virtual reality"] {
            assert!(k.contains(&want.to_string()), "{want} em {k:?}");
        }
        assert!(!k.iter().any(|x| x.contains("reading") || x.contains("award") || x.contains("staff") || x == "science"), "{k:?}");
        let (g, k) = subject_terms(&s(&["Fantasy fiction", "Children's fiction", "Dragons"]));
        assert_eq!(g, vec![10765, 10751]);
        assert_eq!(k, vec!["dragon".to_string()]);
        let (g, k) = subject_terms(&s(&["Ciencia-ficción", "Nouvelles", "Fiction, contemporary"]));
        assert_eq!(g, vec![10765]);
        assert!(k.is_empty(), "{k:?}");
    }

    #[test]
    fn palavras_do_tmdb_casam_com_assuntos_de_livro() {
        assert_eq!(keyword("artificial intelligence (a.i.)").as_deref(), Some("artificial intelligence"));
        assert_eq!(keyword("Dystopias").as_deref(), Some("dystopia"));
        assert_eq!(keyword("sequel"), None);
        assert_eq!(keyword("2040s"), None);
        let mut film = Taste::default();
        film.add(1.0, &[10765], &s(&["cyberpunk", "dystopia", "hacker"]));
        let film = film.normalized();
        let (_, k) = subject_terms(&s(&["Fiction, science fiction, cyberpunk", "Computer hackers"]));
        let (score, why) = film.fit(&[10765], &k);
        assert!(score > 0.8, "{score}");
        assert_eq!(why.as_deref(), Some("cyberpunk"));
        let (romance, _) = film.fit(&[10749], &s(&["love"]));
        assert!(romance < 0.1);
    }

    #[test]
    fn animacao_nao_vale_para_livros() {
        let mut t = Taste::default();
        t.add(3.0, &[16, 10765], &[]);
        let books = t.normalized().for_books();
        assert!(!books.genres.contains_key(&16));
        assert!(books.genres.contains_key(&10765));
    }

    #[test]
    fn manga_do_anilist() {
        let (g, k) = anilist_terms(&s(&["Action", "Sci-Fi"]), &s(&["Cyberpunk", "Male Protagonist"]));
        assert_eq!(g, vec![10759, 10765]);
        assert!(k.contains(&"cyberpunk".to_string()));
    }
}
