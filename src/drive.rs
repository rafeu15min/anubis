//! `Meu Drive/Livros` pelo rclone do host: o que já está na estante.
//!
//! A primeira pasta é a categoria (Ficção, Técnicos, Artigos, Mangás e HQs,
//! RPG, Outros). Título e autor saem do nome do arquivo ("Título (Autor)
//! (Z-Library).pdf"). O livro no kosync é o md5 do nome que o arquivo tem no
//! Kindle (o plugin drivesync troca o que o FAT não aceita).

use anyhow::{Context, Result, bail};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Serialize)]
pub struct Book {
    /// caminho dentro de Livros
    pub path: String,
    pub category: String,
    pub title: String,
    pub author: Option<String>,
    pub ext: String,
    pub size: u64,
    /// quando entrou no Drive
    pub modified: String,
    pub drive_id: String,
    /// documento no kosync (md5 do nome no Kindle)
    pub doc: String,
}

pub struct Drive {
    remote: String,
    cache: Mutex<Option<(Instant, Arc<Vec<Book>>)>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Entry {
    path: String,
    name: String,
    size: i64,
    mod_time: String,
    #[serde(rename = "ID")]
    id: String,
}

impl Drive {
    pub fn new(remote: String) -> Self {
        Self { remote, cache: Mutex::new(None) }
    }

    /// Todos os livros (10 min em memória).
    pub async fn books(&self) -> Result<Arc<Vec<Book>>> {
        let mut cache = self.cache.lock().await;
        if let Some((at, books)) = cache.as_ref()
            && at.elapsed() < TTL
        {
            return Ok(books.clone());
        }
        let out = tokio::process::Command::new("rclone")
            .args(["lsjson", "-R", "--files-only", "--no-mimetype", &self.remote])
            .output()
            .await
            .context("rodando o rclone")?;
        if !out.status.success() {
            bail!("rclone lsjson: {}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or(""));
        }
        let entries: Vec<Entry> = serde_json::from_slice(&out.stdout).context("lendo a listagem do rclone")?;
        let books = Arc::new(entries.into_iter().filter_map(book).collect::<Vec<_>>());
        *cache = Some((Instant::now(), books.clone()));
        Ok(books)
    }

    /// A estante mudou (um download entrou): relista na próxima consulta.
    pub async fn invalidate(&self) {
        *self.cache.lock().await = None;
    }
}

fn book(e: Entry) -> Option<Book> {
    let (category, _) = e.path.split_once('/')?;
    let (stem, ext) = e.name.rsplit_once('.')?;
    let (title, author) = parse_name(stem);
    Some(Book {
        category: category.to_string(),
        title,
        author,
        ext: ext.to_lowercase(),
        size: e.size.max(0) as u64,
        modified: e.mod_time,
        drive_id: e.id,
        doc: kosync_doc(&e.name),
        path: e.path,
    })
}

/// "Título (Autor) (Z-Library)" → título e autor; os nomes com sublinhado do
/// Z-Library ("Capitães_da_Areia_Jorge_Amado_z_library_sk,_1lib_sk,") ficam só
/// com o título legível.
pub fn parse_name(stem: &str) -> (String, Option<String>) {
    let mut s = stem.trim().to_string();
    for tail in ["(Z-Library)", "(z-library.sk, 1lib.sk, z-lib.sk)", "(z-lib.org)"] {
        if let Some(rest) = s.strip_suffix(tail) {
            s = rest.trim_end().to_string();
        }
    }
    if let Some(i) = s.find("_z_library_sk") {
        s.truncate(i);
    }
    let mut author = None;
    if s.ends_with(')')
        && let Some(i) = s.rfind(" (")
    {
        let a = s[i + 2..s.len() - 1].trim().to_string();
        if i > 0 && !a.is_empty() {
            s.truncate(i);
            // "(1)" é cópia, não autor
            if !a.chars().all(|c| c.is_ascii_digit()) {
                author = Some(a);
            }
        }
    }
    if !s.contains(' ') {
        s = s.replace('_', " ");
    }
    (s.trim().to_string(), author)
}

/// O nome que o arquivo tem no Kindle: o FAT não aceita : ? " * < > | \ nem
/// espaço ou ponto no fim (mesma regra de `localRel` no drivesync.koplugin).
pub fn kindle_name(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| match c {
            ':' => '：',
            '?' => '？',
            '"' => '＂',
            '*' => '＊',
            '<' => '＜',
            '>' => '＞',
            '|' => '｜',
            '\\' => '＼',
            c => c,
        })
        .collect();
    mapped.trim_end_matches(|c: char| c.is_whitespace() || c == '.').to_string()
}

/// Como o KOReader identifica o livro no kosync ("nome do arquivo").
pub fn kosync_doc(name: &str) -> String {
    Md5::digest(kindle_name(name).as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titulo_e_autor_do_nome() {
        assert_eq!(parse_name("Crime e castigo (Fiódor Dostoiévski) (Z-Library)"), ("Crime e castigo".into(), Some("Fiódor Dostoiévski".into())));
        assert_eq!(parse_name("Fahrenheit 451"), ("Fahrenheit 451".into(), None));
        assert_eq!(
            parse_name("Sprawl #1 Neuromancer (William Gibson) (z-library.sk, 1lib.sk, z-lib.sk)"),
            ("Sprawl #1 Neuromancer".into(), Some("William Gibson".into()))
        );
        assert_eq!(parse_name("Capitães_da_Areia_Jorge_Amado_z_library_sk,_1lib_sk,_z_lib_sk"), ("Capitães da Areia Jorge Amado".into(), None));
        assert_eq!(parse_name("Suzana_Kasey_Garota_Interrompida (1)"), ("Suzana Kasey Garota Interrompida".into(), None));
    }

    #[test]
    fn nome_no_kindle_e_documento_do_kosync() {
        assert_eq!(kindle_name("Pentest: Introdução.pdf"), "Pentest： Introdução.pdf");
        assert_eq!(kindle_name("Cartas para imprimir "), "Cartas para imprimir");
        // o mesmo md5 que a semente gravou para o Fahrenheit (nome no Kindle)
        assert_eq!(kosync_doc("Fahrenheit 451.pdf"), format!("{:x}", Md5::digest(b"Fahrenheit 451.pdf")));
    }
}
