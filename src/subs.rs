//! Legendas para o player: arquivos ao lado do vídeo (os que o Bazarr baixa,
//! os que vêm no torrent) e faixas de texto embutidas no MKV, sempre
//! entregues em WebVTT, o formato do navegador.

use serde::Serialize;
use std::path::{Path, PathBuf};

const TEXT_EXT: [&str; 4] = ["srt", "vtt", "ass", "ssa"];
/// codecs de legenda em texto (as de imagem, PGS/VobSub, não viram WebVTT)
pub const TEXT_CODECS: [&str; 5] = ["subrip", "ass", "ssa", "webvtt", "mov_text"];

#[derive(Debug, Clone, Serialize)]
pub struct Sub {
    pub label: String,
    /// código de idioma para o <track srclang>
    pub lang: String,
    #[serde(skip)]
    pub source: SubSource,
}

#[derive(Debug, Clone)]
pub enum SubSource {
    File(PathBuf),
    /// índice entre as faixas de legenda do arquivo (0:s:N no ffmpeg)
    Embedded(usize),
}

/// Legendas externas: arquivos de legenda na pasta do vídeo cujo nome começa
/// pelo nome do vídeo (ex.: "Filme.pt-BR.srt"), ou qualquer um se a pasta
/// tem um vídeo só (torrent com "Portuguese.srt").
pub fn external(video: &Path) -> Vec<Sub> {
    let (Some(dir), Some(stem)) = (video.parent(), video.file_stem().and_then(|s| s.to_str())) else { return vec![] };
    let Ok(entries) = std::fs::read_dir(dir) else { return vec![] };
    let mut files: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    files.sort();
    let videos = files.iter().filter(|p| ext(p).is_some_and(|e| ["mkv", "mp4", "m4v", "avi", "mov", "webm"].contains(&e.as_str()))).count();
    let mut out = Vec::new();
    for p in files {
        if !ext(&p).is_some_and(|e| TEXT_EXT.contains(&e.as_str())) {
            continue;
        }
        let name = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        let tag = if let Some(rest) = name.strip_prefix(stem) {
            rest.trim_start_matches(['.', '_', '-', ' '])
        } else if videos <= 1 {
            name
        } else {
            continue;
        };
        let (label, lang) = language(tag);
        out.push(Sub { label, lang, source: SubSource::File(p) });
    }
    out
}

fn ext(p: &Path) -> Option<String> {
    p.extension().and_then(|e| e.to_str()).map(str::to_lowercase)
}

/// Rótulo legível e código do idioma a partir de "pt-BR", "pb", "por",
/// "eng.forced", "Portuguese"…
pub fn language(tag: &str) -> (String, String) {
    let t = tag.to_lowercase();
    let forced = t.contains("forced");
    let sdh = t.contains("sdh") || t.split(['.', '_', '-', ' ']).any(|x| x == "hi" || x == "cc");
    let base = t.split(['.', '_', ' ']).find(|x| !x.is_empty() && !["forced", "sdh", "hi", "cc"].contains(x)).unwrap_or("");
    let (label, lang) = match base {
        "pt-br" | "ptbr" | "pb" | "pob" | "brazilian" | "portuguese-br" => ("Português (BR)", "pt-BR"),
        "pt" | "por" | "portuguese" | "português" | "portugues" => ("Português", "pt"),
        "en" | "eng" | "english" | "inglês" => ("Inglês", "en"),
        "es" | "spa" | "spanish" | "espanhol" => ("Espanhol", "es"),
        "fr" | "fre" | "fra" | "french" => ("Francês", "fr"),
        "it" | "ita" | "italian" => ("Italiano", "it"),
        "de" | "ger" | "deu" | "german" => ("Alemão", "de"),
        "ja" | "jpn" | "japanese" => ("Japonês", "ja"),
        "" => ("Legenda", "und"),
        other => return (other.to_owned(), other.to_owned()),
    };
    let mut label = label.to_owned();
    if forced {
        label.push_str(" (forçada)");
    } else if sdh {
        label.push_str(" (SDH)");
    }
    (label, lang.to_owned())
}

/// Texto do arquivo: UTF-8 ou, se não for, Windows-1252 (comum em legendas BR).
pub fn decode(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_owned();
    }
    // 0x80–0x9F no Windows-1252; o resto coincide com Latin-1
    const CP1252: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž', '\u{8F}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ',
        '\u{9D}', 'ž', 'Ÿ',
    ];
    bytes.iter().map(|&b| if (0x80..0xA0).contains(&b) { CP1252[(b - 0x80) as usize] } else { b as char }).collect()
}

/// SRT → WebVTT (tempos com ponto, cabeçalho, sem os números das falas).
pub fn srt_to_vtt(srt: &str) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for block in srt.replace("\r\n", "\n").split("\n\n") {
        let lines: Vec<&str> = block.lines().filter(|l| !l.trim().is_empty()).collect();
        let Some(ti) = lines.iter().position(|l| l.contains("-->")) else { continue };
        out.push_str(&lines[ti].replace(',', "."));
        out.push('\n');
        for l in &lines[ti + 1..] {
            out.push_str(l);
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

fn parse_ts(t: &str) -> Option<f64> {
    let t = t.trim().replace(',', ".");
    let parts: Vec<&str> = t.split(':').collect();
    let (h, m, s) = match parts.as_slice() {
        [h, m, s] => (h.parse::<f64>().ok()?, m.parse::<f64>().ok()?, s.parse::<f64>().ok()?),
        [m, s] => (0.0, m.parse::<f64>().ok()?, s.parse::<f64>().ok()?),
        _ => return None,
    };
    Some(h * 3600.0 + m * 60.0 + s)
}

fn fmt_ts(t: f64) -> String {
    let ms = (t.max(0.0) * 1000.0).round() as u64;
    format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// Adianta todas as falas em `shift` segundos (o vídeo convertido começa no
/// meio do filme) e descarta as que já passaram.
pub fn shift_vtt(vtt: &str, shift: f64) -> String {
    if shift <= 0.0 {
        return vtt.to_owned();
    }
    let mut out = String::new();
    let mut skipping = false;
    for block in vtt.replace("\r\n", "\n").split("\n\n") {
        let mut lines = block.lines();
        let Some(pos) = block.lines().position(|l| l.contains("-->")) else {
            if !skipping || block.starts_with("WEBVTT") {
                out.push_str(block);
                out.push_str("\n\n");
            }
            continue;
        };
        let head: Vec<&str> = lines.by_ref().take(pos).collect();
        let timing = lines.next().unwrap_or_default();
        let rest: Vec<&str> = lines.collect();
        let mut it = timing.splitn(2, "-->");
        let (a, b_and_settings) = (it.next().unwrap_or_default(), it.next().unwrap_or_default());
        let mut bs = b_and_settings.trim().splitn(2, ' ');
        let (b, settings) = (bs.next().unwrap_or_default(), bs.next().unwrap_or_default());
        let (Some(a), Some(b)) = (parse_ts(a), parse_ts(b)) else { continue };
        skipping = b - shift <= 0.0;
        if skipping {
            continue;
        }
        for h in head {
            out.push_str(h);
            out.push('\n');
        }
        out.push_str(&format!("{} --> {}{}{}\n", fmt_ts(a - shift), fmt_ts(b - shift), if settings.is_empty() { "" } else { " " }, settings));
        for l in rest {
            out.push_str(l);
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_srt_to_vtt() {
        let srt = "1\r\n00:00:01,500 --> 00:00:03,000\r\nOlá!\r\n\r\n2\r\n00:01:00,000 --> 00:01:02,250\r\nSegunda\r\nlinha\r\n";
        assert_eq!(srt_to_vtt(srt), "WEBVTT\n\n00:00:01.500 --> 00:00:03.000\nOlá!\n\n00:01:00.000 --> 00:01:02.250\nSegunda\nlinha\n\n");
    }

    #[test]
    fn shifts_cues_and_drops_past_ones() {
        let vtt = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nantes\n\n00:01:05.000 --> 00:01:07.500 line:90%\ndepois\n";
        let s = shift_vtt(vtt, 60.0);
        assert!(!s.contains("antes"));
        assert!(s.contains("00:00:05.000 --> 00:00:07.500 line:90%\ndepois"));
        assert!(s.starts_with("WEBVTT"));
    }

    #[test]
    fn decodes_windows_1252() {
        assert_eq!(decode(b"n\xe3o \x93sei\x94"), "não “sei”");
        assert_eq!(decode("já é utf-8".as_bytes()), "já é utf-8");
    }

    #[test]
    fn labels_languages() {
        assert_eq!(language("pt-BR"), ("Português (BR)".into(), "pt-BR".into()));
        assert_eq!(language("pb.forced"), ("Português (BR) (forçada)".into(), "pt-BR".into()));
        assert_eq!(language("eng.sdh"), ("Inglês (SDH)".into(), "en".into()));
        assert_eq!(language(""), ("Legenda".into(), "und".into()));
    }
}
