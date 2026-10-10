//! As listas de fora para as fileiras da Leitura, como as dos filmes:
//! - Populares no Brasil: os mais vendidos da Amazon Brasil, por categoria;
//! - Lançamentos: as novidades mais vendidas da Amazon Brasil;
//! - Destaques: os mais lidos da semana no Goodreads (o par do "em alta na
//!   semana" do TMDB): no Brasil (Ficção) ou por gênero (mangá, HQ);
//! - AniList: mangás de um gênero ou tag (o "Para você" de mangás).
//!
//! A Amazon só responde a um navegador: as páginas vêm pelo FlareSolverr
//! (o mesmo do Prowlarr). "Em alta" e "Mais desejados" da Amazon vêm vazios
//! até por ele, por isso os Destaques são do Goodreads. Cada lista fica 12 h
//! em `state_dir/vitrine-livros.json`.

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::sync::Mutex;

const TTL_SECS: i64 = 12 * 3600;
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/140.0 Safari/537.36";

/// Categorias de livros da Amazon Brasil (o número do "/gp/bestsellers/books/N").
pub mod amazon {
    pub const LITERATURA: &str = "7872687011";
    pub const FANTASIA_HORROR_FC: &str = "7841775011";
    pub const POLICIAL: &str = "7872829011";
    pub const ROMANCE: &str = "7882388011";
    pub const HQS_MANGAS: &str = "7842710011";
    // Técnicos: as subcategorias da área de estudo (as categorias inteiras
    // trazem revista de carro e autoajuda)
    pub const SEGURANCA: &str = "16311318011";
    pub const PROGRAMACAO: &str = "7842670011";
    pub const HARDWARE: &str = "7842653011";
    pub const ESCOLAS_ENSINO: &str = "7842873011";
    pub const EDUCACAO: &str = "7841883011";
    pub const MATEMATICA: &str = "7842558011";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    /// "amz:ASIN", "gr:ID", "al:ID"
    pub key: String,
    pub title: String,
    pub author: Option<String>,
    pub cover: Option<String>,
    /// AniList: gêneros e tags (para o gosto)
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub year: Option<i64>,
}

static AMZ_ITEM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"data-asin="([0-9A-Z]{10})""#).unwrap());
static AMZ_IMG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"<img alt="([^"]+)" src="([^"]+)""#).unwrap());
static AMZ_AUTHOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<div class="a-row a-size-small">\s*<(?:a|span)[^>]*class="a-size-small[^"]*"[^>]*>\s*(?:<div[^>]*>)?([^<]+)<"#).unwrap()
});
static GR_ROW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<tr itemscope itemtype="http://schema.org/Book">.*?href="/book/show/(\d+)[^"]*">\s*<img[^>]*src="([^"]+)".*?<span itemprop='name'[^>]*>([^<]+)</span>.*?class="authorName"[^>]*><span itemprop="name">([^<]+)</span>"#).unwrap()
});
static GR_ITEM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<a href="/book/show/(\d+)[^"]*"><img alt="([^"]+)"[^>]*class="bookImage" src="([^"]+)""#).unwrap()
});

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&").replace("&quot;", "\"").replace("&#39;", "'").replace("&#039;", "'").replace("&nbsp;", " ").trim().to_string()
}

/// Título da Amazon sem o que é da edição: "Corte de harmonias partidas
/// (Vol. 6 …) - acompanha sobrecapa" → "Corte de harmonias partidas".
pub fn clean_title(t: &str) -> String {
    let mut s = t.to_string();
    for cut in [" - ", " – ", " — ", ": Edição", " (Edição", "(Português"] {
        if let Some(i) = s.find(cut)
            && i > 3
        {
            s.truncate(i);
        }
    }
    if let Some(i) = s.find(" (")
        && i > 3
    {
        s.truncate(i);
    }
    s.trim().to_string()
}

/// Os itens de uma lista "mais vendidos"/"novidades" da Amazon, na ordem.
pub fn parse_amazon(page: &str) -> Vec<Item> {
    let starts: Vec<(usize, String)> = AMZ_ITEM.captures_iter(page).filter_map(|c| Some((c.get(0)?.start(), c[1].to_string()))).collect();
    let mut out: Vec<Item> = vec![];
    for (i, (at, asin)) in starts.iter().enumerate() {
        let end = starts.get(i + 1).map_or(page.len(), |n| n.0);
        let block = &page[*at..end];
        let Some(img) = AMZ_IMG.captures(block) else { continue };
        // revista avulsa ("Revista Quatro Rodas [ed.810]") não é livro
        let alt = unescape(&img[1]);
        if out.iter().any(|o| o.key.ends_with(asin.as_str())) || alt.starts_with("Revista ") || alt.contains("[ed.") {
            continue;
        }
        out.push(Item {
            key: format!("amz:{asin}"),
            title: clean_title(&unescape(&img[1])),
            author: AMZ_AUTHOR.captures(block).map(|c| unescape(&c[1])).filter(|a| !a.is_empty()),
            cover: Some(img[2].to_string()),
            genres: vec![],
            tags: vec![],
            year: None,
        });
    }
    out
}

/// "Onyx Storm (The Empyrean, #3)" → "Onyx Storm": a série sai do título.
/// "ガチアクタ 1 [Gachiakuta 1]" → "Gachiakuta 1": o transliterado.
pub fn strip_series(t: &str) -> String {
    if let (Some(a), true) = (t.rfind('['), t.ends_with(']'))
        && !t[..a].is_ascii()
    {
        return t[a + 1..t.len() - 1].trim().to_string();
    }
    match t.rfind(" (") {
        Some(i) if t.ends_with(')') && t[i..].contains('#') => t[..i].to_string(),
        _ => t.to_string(),
    }
}

/// Os itens de "Most Read This Week" de um gênero do Goodreads.
pub fn parse_goodreads(page: &str) -> Vec<Item> {
    let mut out: Vec<Item> = vec![];
    for c in GR_ITEM.captures_iter(page) {
        let key = format!("gr:{}", &c[1]);
        if out.iter().any(|o| o.key == key) {
            continue;
        }
        out.push(Item { key, title: strip_series(&unescape(&c[2])), author: None, cover: Some(c[3].to_string()), genres: vec![], tags: vec![], year: None });
    }
    out
}

/// Os itens de "Most Read Books This Week In Brazil" (a tabela do Goodreads).
pub fn parse_goodreads_table(page: &str) -> Vec<Item> {
    let mut out: Vec<Item> = vec![];
    for c in GR_ROW.captures_iter(page) {
        let key = format!("gr:{}", &c[1]);
        if out.iter().any(|o| o.key == key) {
            continue;
        }
        // a capa da tabela é miniatura: a maior tem o mesmo endereço sem o "._SY75_"
        let cover = c[2].replace("._SY75_", "").replace("._SX50_", "");
        out.push(Item { key, title: strip_series(&unescape(&c[3])), author: Some(unescape(&c[4])), cover: Some(cover), genres: vec![], tags: vec![], year: None });
    }
    out
}

#[derive(Default, Serialize, Deserialize)]
struct Store {
    /// endereço → (quando, itens)
    lists: HashMap<String, (i64, Vec<Item>)>,
}

pub struct Vitrine {
    http: reqwest::Client,
    flaresolverr: String,
    path: PathBuf,
    store: Mutex<Store>,
    /// uma página por vez (o FlareSolverr abre um navegador para cada)
    turn: Mutex<()>,
}

impl Vitrine {
    pub fn load(flaresolverr: String, state_dir: PathBuf) -> Result<Self> {
        let path = state_dir.join("vitrine-livros.json");
        let store = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let http = reqwest::Client::builder().user_agent(UA).timeout(Duration::from_secs(150)).build()?;
        Ok(Self { http, flaresolverr, path, store: Mutex::new(store), turn: Mutex::new(()) })
    }

    async fn cached(&self, url: &str) -> Option<Vec<Item>> {
        let s = self.store.lock().await;
        let (at, items) = s.lists.get(url)?;
        (chrono::Utc::now().timestamp() - at < TTL_SECS).then(|| items.clone())
    }

    async fn keep(&self, url: &str, items: &[Item]) {
        let bytes = {
            let mut s = self.store.lock().await;
            s.lists.insert(url.to_string(), (chrono::Utc::now().timestamp(), items.to_vec()));
            serde_json::to_vec(&*s)
        };
        if let Ok(bytes) = bytes {
            let tmp = self.path.with_extension("json.tmp");
            if tokio::fs::write(&tmp, bytes).await.is_ok() {
                let _ = tokio::fs::rename(&tmp, &self.path).await;
            }
        }
    }

    /// A lista guardada mesmo vencida (a fonte falhou agora: melhor a de ontem).
    async fn stale(&self, url: &str) -> Vec<Item> {
        self.store.lock().await.lists.get(url).map(|(_, i)| i.clone()).unwrap_or_default()
    }

    /// Página pelo FlareSolverr (um navegador de verdade).
    async fn browser_get(&self, url: &str) -> Result<String> {
        let _turn = self.turn.lock().await;
        let body = json!({ "cmd": "request.get", "url": url, "maxTimeout": 90000 });
        let v: Value = self.http.post(format!("{}/v1", self.flaresolverr.trim_end_matches('/'))).json(&body).send().await
            .context("FlareSolverr fora do ar")?.json().await?;
        if v["status"] != "ok" {
            bail!("FlareSolverr: {}", v["message"].as_str().unwrap_or("?"));
        }
        Ok(v["solution"]["response"].as_str().unwrap_or_default().to_string())
    }

    /// Mais vendidos (`novidades` = false) ou novidades de uma categoria da Amazon Brasil.
    pub async fn amazon(&self, cat: &str, novidades: bool) -> Vec<Item> {
        let url = format!("https://www.amazon.com.br/gp/{}/books/{cat}", if novidades { "new-releases" } else { "bestsellers" });
        if let Some(items) = self.cached(&url).await {
            return items;
        }
        match self.browser_get(&url).await.map(|p| parse_amazon(&p)) {
            Ok(items) if !items.is_empty() => {
                self.keep(&url, &items).await;
                items
            }
            Ok(_) => {
                tracing::warn!("Amazon ({url}): página sem itens");
                self.stale(&url).await
            }
            Err(e) => {
                tracing::warn!("Amazon ({url}): {e:#}");
                self.stale(&url).await
            }
        }
    }

    /// Os mais lidos da semana no Goodreads: de um gênero ("manga") ou, com
    /// "brasil", os do Brasil.
    pub async fn goodreads(&self, genre: &str) -> Vec<Item> {
        let brasil = genre == "brasil";
        let url = if brasil { "https://www.goodreads.com/book/most_read?category=all&country=BR&duration=w".to_string() } else { format!("https://www.goodreads.com/genres/most_read/{genre}") };
        if let Some(items) = self.cached(&url).await {
            return items;
        }
        let page = async { self.http.get(&url).send().await?.error_for_status()?.text().await };
        match page.await.map(|p| if brasil { parse_goodreads_table(&p) } else { parse_goodreads(&p) }) {
            Ok(items) if !items.is_empty() => {
                self.keep(&url, &items).await;
                items
            }
            Ok(_) => self.stale(&url).await,
            Err(e) => {
                tracing::warn!("Goodreads ({genre}): {e}");
                self.stale(&url).await
            }
        }
    }

    /// Mangás populares de gêneros e tags do AniList.
    pub async fn anilist(&self, genres: &[&str], tags: &[&str]) -> Vec<Item> {
        self.anilist_query(genres, tags, None).await
    }

    /// Mangás de uma busca livre no AniList (o tema que a pessoa escreveu).
    pub async fn anilist_search(&self, term: &str) -> Vec<Item> {
        self.anilist_query(&[], &[], Some(term)).await
    }

    async fn anilist_query(&self, genres: &[&str], tags: &[&str], search: Option<&str>) -> Vec<Item> {
        let url = format!("anilist:{}|{}|{}", genres.join(","), tags.join(","), search.unwrap_or(""));
        if let Some(items) = self.cached(&url).await {
            return items;
        }
        let query = r#"query ($g: [String], $t: [String], $s: String) { Page(perPage: 30) { media(type: MANGA, isAdult: false, sort: POPULARITY_DESC, genre_in: $g, tag_in: $t, search: $s) {
            id title { romaji english } coverImage { large } genres tags { name rank } startDate { year }
            staff(perPage: 2, sort: RELEVANCE) { nodes { name { full } } } } } }"#;
        let vars = json!({ "g": if genres.is_empty() { Value::Null } else { json!(genres) }, "t": if tags.is_empty() { Value::Null } else { json!(tags) }, "s": search });
        let res = async {
            let v: Value = self.http.post("https://graphql.anilist.co").json(&json!({ "query": query, "variables": vars })).send().await?.error_for_status()?.json().await?;
            anyhow::Ok(v)
        };
        let v = match res.await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("AniList: {e:#}");
                return self.stale(&url).await;
            }
        };
        let items: Vec<Item> = v["data"]["Page"]["media"].as_array().into_iter().flatten().filter_map(|m| {
            Some(Item {
                key: format!("al:{}", m["id"].as_i64()?),
                title: m["title"]["english"].as_str().or(m["title"]["romaji"].as_str())?.to_string(),
                author: m["staff"]["nodes"][0]["name"]["full"].as_str().map(str::to_string),
                cover: m["coverImage"]["large"].as_str().map(str::to_string),
                genres: m["genres"].as_array().into_iter().flatten().filter_map(|g| g.as_str().map(str::to_string)).collect(),
                // só as tags fortes (rank alto) dizem do que é a obra
                tags: m["tags"].as_array().into_iter().flatten().filter(|t| t["rank"].as_i64().unwrap_or(0) >= 60).filter_map(|t| t["name"].as_str().map(str::to_string)).collect(),
                year: m["startDate"]["year"].as_i64(),
            })
        }).collect();
        if !items.is_empty() {
            self.keep(&url, &items).await;
        }
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // trecho real de /gp/bestsellers/books/7841775011 (2026-10-10)
    const AMZ: &str = r#"<div id="p13n-asin-index-1" class="a-cardui"><div data-asin="8535923691" class="_cDEzb_iveVideoWrapper_JJ34T"><div class="a-section zg-bdg-ctr"><span class="zg-bdg-text">#2</span></div><div class="zg-grid-general-faceout"><span><div id="8535923691" class="p13n-sc-uncoverable-faceout"><a aria-hidden="true" class="a-link-normal aok-block" tabindex="-1" href="/dp/8535923691"><div class="a-section a-spacing-mini _cDEzb_noop_3Xbw5"><img alt="A cabeça do santo" src="https://images-na.ssl-images-amazon.com/images/I/71-YhujYUxS._AC_UL600_SR600,400_.jpg" class="a-dynamic-image"></div></a><div><div><a class="a-link-normal aok-block" href="/dp/8535923691" role="link"><span><div class="_cDEzb_p13n-sc-css-line-clamp-1_1Fn1y">A cabeça do santo</div></span></a><div class="a-row a-size-small"><a class="a-size-small a-link-child" href="/Socorro-Acioli/e/B0095HTVW2"><div class="_cDEzb_p13n-sc-css-line-clamp-1_1Fn1y">Socorro Acioli</div></a></div><div class="a-row a-size-small"><span class="a-size-small a-color-secondary a-text-normal">Capa comum</span></div></div></div></div></span></div></div>
<div data-asin="6555320893" class="x"><img alt="Corte de harmonias partidas (Vol. 6 Corte de espinhos e rosas) - acompanha sobrecapa e marcadores" src="https://images-na.ssl-images-amazon.com/images/I/X.jpg" class="a-dynamic-image"><div class="a-row a-size-small"><span class="a-size-small a-color-base">Sarah J. Maas</span></div></div>"#;

    #[test]
    fn le_a_lista_da_amazon() {
        let items = parse_amazon(AMZ);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].key, "amz:8535923691");
        assert_eq!(items[0].title, "A cabeça do santo");
        assert_eq!(items[0].author.as_deref(), Some("Socorro Acioli"));
        assert!(items[0].cover.as_deref().unwrap().starts_with("https://images-na.ssl-images-amazon.com/"));
        assert_eq!(items[1].title, "Corte de harmonias partidas");
        assert_eq!(items[1].author.as_deref(), Some("Sarah J. Maas"));
    }

    #[test]
    fn le_os_mais_lidos_do_goodreads() {
        let page = r#"<div class="coverWrapper" id="bookCover170708_60433195"> <a href="/book/show/60433195-dandadan-vol-1"><img alt="Dandadan, Vol. 1" title="" width="115" class="bookImage" src="https://i.gr-assets.com/images/S/compressed.photo.goodreads.com/books/1656251636l/60433195.jpg" /></a> </div>"#;
        let items = parse_goodreads(page);
        assert_eq!(items.len(), 1);
        assert_eq!((items[0].key.as_str(), items[0].title.as_str()), ("gr:60433195", "Dandadan, Vol. 1"));
        assert_eq!(strip_series("ガチアクタ 1 [Gachiakuta 1]"), "Gachiakuta 1");
        assert_eq!(strip_series("Threshing Day (The Empyrean, #3.5)"), "Threshing Day");
    }

    #[test]
    fn le_os_mais_lidos_do_brasil() {
        // trecho real de /book/most_read?country=BR (2026-10-10)
        let page = r#"<tr itemscope itemtype="http://schema.org/Book"> <td valign="top" class="number">1</td> <td width="5%" valign="top"> <div id="251172932" class="u-anchorTarget"></div> <a title="Threshing Day" href="/book/show/251172932-threshing-day"> <img alt="Threshing Day (The Empyrean..." class="bookCover" itemprop="image" src="https://i.gr-assets.com/images/S/compressed.photo.goodreads.com/books/1784132149i/251172932._SY75_.jpg" /> </a> </td> <td width="100%" valign="top"> <a class="bookTitle" itemprop="url" href="/book/show/251172932-threshing-day"> <span itemprop='name' role='heading' aria-level='4'>Threshing Day (The Empyrean, #3.5)</span> </a> <br/> <span class='by'>by</span> <span itemprop='author' itemscope='' itemtype='http://schema.org/Person'> <div class='authorName__container'> <a class="authorName" itemprop="url" href="https://www.goodreads.com/author/show/7539785.Rebecca_Yarros"><span itemprop="name">Rebecca Yarros</span></a>"#;
        let items = parse_goodreads_table(page);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Threshing Day");
        assert_eq!(items[0].author.as_deref(), Some("Rebecca Yarros"));
        assert!(!items[0].cover.as_deref().unwrap().contains("_SY75_"));
    }
}
