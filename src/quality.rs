//! Validador de EPUB/PDF: o arquivo baixado só vai para o Drive se dá para
//! ler. Critérios calibrados no acervo (35 EPUBs do Drive, 2026-10-09):
//!
//! - EPUB: abre, sem DRM (`rights.xml` ou texto cifrado; fonte embaralhada
//!   com `encryption.xml` é normal e passa), texto de verdade (≥ 60% das
//!   palavras são palavras: o Drácula convertido de um PDF com fonte quebrada
//!   tinha 4%), comprido o bastante para um livro (≥ 8 mil palavras) e sem a
//!   marca da versão de demonstração do Aspose.Words (que corta o livro).
//! - PDF: abre (`pdfinfo`), sem senha para abrir, páginas suficientes. Texto
//!   quebrado num PDF não reprova: a página continua legível.
//! - HQ, mangá e RPG podem ser só imagens.
//!
//! O idioma (pt/en) sai das palavras mais comuns.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Book,
    Comic,
    Article,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Verdict {
    /// None: pode ir para o Drive
    pub problem: Option<String>,
    pub words: usize,
    pub pages: Option<u32>,
    pub language: Option<&'static str>,
}

impl Verdict {
    pub fn ok(&self) -> bool {
        self.problem.is_none()
    }
}

const MIN_WORDS_BOOK: usize = 8_000;
const MIN_REAL_RATIO: f64 = 0.6;
const PT: &[&str] = &["que", "de", "não", "uma", "com", "para", "por", "mais", "como", "mas", "ele", "ela", "seu", "sua", "isso", "está", "foi", "são", "também"];
const EN: &[&str] = &["the", "and", "of", "to", "that", "was", "with", "for", "his", "her", "this", "have", "not", "but", "they", "from"];

pub async fn check(path: &Path, kind: Kind) -> Verdict {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let p = path.to_path_buf();
    let res = match ext.as_str() {
        "epub" => tokio::task::spawn_blocking(move || epub(&p, kind)).await.unwrap_or_else(|e| Err(e.into())),
        "pdf" => pdf(&p, kind).await,
        // CBZ, MOBI, AZW3, DJVU: sem conferência de conteúdo
        _ => Ok(Verdict { problem: None, words: 0, pages: None, language: None }),
    };
    res.unwrap_or_else(|e| Verdict { problem: Some(format!("arquivo ilegível: {e:#}")), words: 0, pages: None, language: None })
}

/// Palavras, proporção de palavras de verdade e idioma de um texto.
fn measure(text: &str) -> (usize, f64, Option<&'static str>) {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let mut real = 0;
    let (mut pt, mut en) = (0, 0);
    for t in &tokens {
        let w = t.trim_matches(|c: char| !c.is_alphanumeric());
        if w.chars().count() >= 2 && w.chars().all(char::is_alphabetic) {
            real += 1;
            let lw = w.to_lowercase();
            pt += PT.contains(&lw.as_str()) as usize;
            en += EN.contains(&lw.as_str()) as usize;
        }
    }
    let lang = if pt as f64 > en as f64 * 1.5 && pt > 20 {
        Some("pt")
    } else if en as f64 > pt as f64 * 1.5 && en > 20 {
        Some("en")
    } else {
        None
    };
    (tokens.len(), real as f64 / tokens.len().max(1) as f64, lang)
}

fn read_entry(z: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Result<String> {
    let mut s = String::new();
    let mut f = z.by_name(name).with_context(|| format!("falta {name}"))?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;
    s.push_str(&String::from_utf8_lossy(&bytes));
    Ok(s)
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let pat = format!("{name}=\"");
    let i = tag.find(&pat)? + pat.len();
    Some(&tag[i..i + tag[i..].find('"')?])
}

/// Texto visível de um XHTML (tags e entidades simples fora).
fn strip(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&nbsp;", " ").replace("&#160;", " ").replace("&#xa0;", " ").replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"")
}

fn epub(path: &Path, kind: Kind) -> Result<Verdict> {
    let mut z = zip::ZipArchive::new(std::fs::File::open(path)?).context("não é um zip")?;
    let names: Vec<String> = z.file_names().map(String::from).collect();
    let bad = |problem: &str, words, language| Ok(Verdict { problem: Some(problem.into()), words, pages: None, language });
    if names.iter().any(|n| n.ends_with("rights.xml")) {
        return bad("DRM", 0, None);
    }
    if names.iter().any(|n| n == "META-INF/encryption.xml") {
        let enc = read_entry(&mut z, "META-INF/encryption.xml")?;
        let fonts = ["otf", "ttf", "woff", "woff2"];
        let cyphered_text = enc.split("URI=\"").skip(1).filter_map(|s| s.split('"').next()).any(|uri| {
            let ext = uri.rsplit('.').next().unwrap_or("").to_lowercase();
            !fonts.contains(&ext.as_str())
        });
        if cyphered_text {
            return bad("DRM", 0, None);
        }
    }
    let container = read_entry(&mut z, "META-INF/container.xml")?;
    let opf_path = attr(&container, "full-path").context("container.xml sem o OPF")?.to_string();
    let opf = read_entry(&mut z, &opf_path)?;
    let base = opf_path.rsplit_once('/').map(|(d, _)| format!("{d}/")).unwrap_or_default();
    let mut manifest = std::collections::HashMap::new();
    for tag in opf.split("<item ").skip(1).filter_map(|s| s.split('>').next()) {
        if let (Some(id), Some(href)) = (attr(tag, "id"), attr(tag, "href")) {
            manifest.insert(id.to_string(), href.to_string());
        }
    }
    let mut text = String::new();
    for tag in opf.split("<itemref ").skip(1).filter_map(|s| s.split('>').next()) {
        if let Some(href) = attr(tag, "idref").and_then(|id| manifest.get(id)) {
            let href = href.replace("%20", " ");
            if let Ok(html) = read_entry(&mut z, &format!("{base}{href}")) {
                text.push_str(&strip(&html));
                text.push(' ');
            }
        }
    }
    let (words, real, language) = measure(&text);
    let images = names.iter().filter(|n| [".jpg", ".jpeg", ".png", ".gif"].iter().any(|e| n.to_lowercase().ends_with(e))).count();
    let problem = if text.contains("evaluation copy of Aspose") {
        Some("conversão de demonstração (Aspose): o livro vem cortado".to_string())
    } else if kind != Kind::Comic && words < 200 && images > 10 {
        Some("só imagens de página (escaneado), sem texto".to_string())
    } else if kind != Kind::Comic && words >= 200 && real < MIN_REAL_RATIO {
        Some(format!("texto ilegível ({:.0}% de palavras de verdade)", real * 100.0))
    } else if kind == Kind::Book && words < MIN_WORDS_BOOK {
        Some(format!("curto demais para um livro ({words} palavras)"))
    } else {
        None
    };
    Ok(Verdict { problem, words, pages: None, language })
}

async fn pdf(path: &Path, kind: Kind) -> Result<Verdict> {
    let info = tokio::process::Command::new("pdfinfo").arg(path).output().await.context("pdfinfo")?;
    if !info.status.success() {
        bail!("PDF corrompido ou com senha: {}", String::from_utf8_lossy(&info.stderr).lines().next().unwrap_or(""));
    }
    let info = String::from_utf8_lossy(&info.stdout);
    let pages: u32 = info.lines().find_map(|l| l.strip_prefix("Pages:")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    let min_pages = match kind {
        Kind::Book => 40,
        Kind::Comic => 10,
        Kind::Article => 2,
    };
    let text = tokio::process::Command::new("pdftotext").args(["-l", "15"]).arg(path).arg("-").output().await.context("pdftotext")?;
    let (words, real, language) = measure(&String::from_utf8_lossy(&text.stdout));
    // texto quebrado não reprova um PDF (a página continua legível); só o idioma fica desconhecido
    let language = if real >= MIN_REAL_RATIO { language } else { None };
    let problem = (pages < min_pages).then(|| format!("curto demais ({pages} páginas)"));
    Ok(Verdict { problem, words, pages: Some(pages), language })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_epub(dir: &Path, name: &str, body: &str, extra: &[(&str, &str)]) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut z = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let opt = zip::write::SimpleFileOptions::default();
        z.start_file("mimetype", opt).unwrap();
        z.write_all(b"application/epub+zip").unwrap();
        z.start_file("META-INF/container.xml", opt).unwrap();
        z.write_all(br#"<container><rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles></container>"#).unwrap();
        z.start_file("OEBPS/content.opf", opt).unwrap();
        z.write_all(br#"<package><manifest><item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="c1"/></spine></package>"#).unwrap();
        z.start_file("OEBPS/c1.xhtml", opt).unwrap();
        z.write_all(format!("<html><body><p>{body}</p></body></html>").as_bytes()).unwrap();
        for (n, c) in extra {
            z.start_file(*n, opt).unwrap();
            z.write_all(c.as_bytes()).unwrap();
        }
        z.finish().unwrap();
        path
    }

    fn dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("quality-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn livro_bom_passa_e_detecta_portugues() {
        let body = "Durante todo o verão ele tocou alaúde em Southwark, mas não era para isso que estava ali. ".repeat(600);
        let v = check(&make_epub(&dir(), "bom.epub", &body, &[]), Kind::Book).await;
        assert!(v.ok(), "{v:?}");
        assert_eq!(v.language, Some("pt"));
    }

    #[tokio::test]
    async fn texto_de_fonte_quebrada_reprova() {
        let body = "+, 1 * &#xa0; +%&amp; * +%, * 7 @ 8 )@ 5 ".repeat(800);
        let v = check(&make_epub(&dir(), "lixo.epub", &body, &[]), Kind::Book).await;
        assert!(v.problem.unwrap().starts_with("texto ilegível"));
    }

    #[tokio::test]
    async fn demonstracao_do_aspose_e_amostra_curta_reprovam() {
        let aspose = "A biblioteca. Created with an evaluation copy of Aspose.Words. To remove all limitations";
        let v = check(&make_epub(&dir(), "aspose.epub", aspose, &[]), Kind::Book).await;
        assert!(v.problem.unwrap().contains("Aspose"));
        let v = check(&make_epub(&dir(), "curto.epub", &"uma palavra de verdade ".repeat(100), &[]), Kind::Book).await;
        assert!(v.problem.unwrap().starts_with("curto demais"));
    }

    #[tokio::test]
    async fn fonte_embaralhada_nao_e_drm_mas_texto_cifrado_e() {
        let body = "Case manteve os olhos fechados enquanto o trem disparava pelo túnel escuro. ".repeat(800);
        let fonts = r#"<encryption><EncryptedData><CipherData><CipherReference URI="OEBPS/fonts/a.otf"/></CipherData></EncryptedData></encryption>"#;
        let v = check(&make_epub(&dir(), "fonte.epub", &body, &[("META-INF/encryption.xml", fonts)]), Kind::Book).await;
        assert!(v.ok(), "{v:?}");
        let drm = r#"<encryption><EncryptedData><CipherData><CipherReference URI="OEBPS/c1.xhtml"/></CipherData></EncryptedData></encryption>"#;
        let v = check(&make_epub(&dir(), "drm.epub", &body, &[("META-INF/encryption.xml", drm)]), Kind::Book).await;
        assert_eq!(v.problem.as_deref(), Some("DRM"));
    }

    #[tokio::test]
    async fn arquivo_que_nao_e_epub_reprova() {
        let p = dir().join("falso.epub");
        std::fs::write(&p, b"<html>pagina de erro</html>").unwrap();
        assert!(check(&p, Kind::Book).await.problem.unwrap().starts_with("arquivo ilegível"));
    }
}
