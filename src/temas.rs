//! Temas de busca de cada aba da Leitura (Livros, Artigos, Mangás e HQs).
//!
//! O sistema propõe os temas (as áreas de estudo da configuração, os temas
//! do que está no Drive, o gosto do que se assiste e lê) e a pessoa edita:
//! adiciona os seus e tira os que não quer. Tirado por ela, um tema do
//! sistema não volta. A edição fica em `state_dir/leitura-temas.json`.
//! (Diferente dos vídeos, onde o gosto vem só do sistema.)

use crate::estudo::{Area, Origem};
use crate::gosto::fold;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::Mutex;

/// peso de um tema posto pela pessoa (as áreas da configuração valem 1)
const USER_WEIGHT: f64 = 2.5;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Edits {
    /// os da pessoa, na ordem em que entraram
    added: Vec<String>,
    /// os do sistema que ela tirou (comparáveis: `key`)
    removed: Vec<String>,
}

pub struct Temas {
    path: PathBuf,
    edits: Mutex<HashMap<String, Edits>>,
}

fn key(nome: &str) -> String {
    fold(nome).chars().filter(|c| c.is_alphanumeric()).collect()
}

/// Um tema posto pela pessoa: o nome é o termo de busca.
pub fn user_theme(nome: &str) -> Area {
    Area::new(nome.to_string(), vec![nome.to_string()], USER_WEIGHT, Origem::Voce)
}

impl Temas {
    pub fn load(state_dir: PathBuf) -> Self {
        let path = state_dir.join("leitura-temas.json");
        let edits = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { path, edits: Mutex::new(edits) }
    }

    async fn save(&self) {
        crate::disk::save_json_or_warn(&self.path, &*self.edits.lock().await).await;
    }

    /// Os temas da aba: os do sistema sem os tirados, mais os da pessoa
    /// (que vêm primeiro: foram pedidos).
    pub async fn apply(&self, tab: &str, system: Vec<Area>) -> Vec<Area> {
        let e = self.edits.lock().await.get(tab).cloned().unwrap_or_default();
        let mut out: Vec<Area> = e.added.iter().map(|n| user_theme(n)).collect();
        for a in system {
            let k = key(&a.nome);
            if !e.removed.contains(&k) && !out.iter().any(|o| key(&o.nome) == k) {
                out.push(a);
            }
        }
        out
    }

    pub async fn add(&self, tab: &str, nome: &str) -> Result<()> {
        let nome = nome.trim();
        if key(nome).len() < 2 || nome.chars().count() > 60 {
            bail!("tema vazio ou longo demais");
        }
        {
            let mut all = self.edits.lock().await;
            let e = all.entry(tab.to_string()).or_default();
            let k = key(nome);
            e.removed.retain(|r| *r != k);
            if !e.added.iter().any(|a| key(a) == k) {
                e.added.push(nome.to_string());
            }
        }
        self.save().await;
        Ok(())
    }

    pub async fn remove(&self, tab: &str, nome: &str) {
        {
            let mut all = self.edits.lock().await;
            let e = all.entry(tab.to_string()).or_default();
            let k = key(nome);
            let before = e.added.len();
            e.added.retain(|a| key(a) != k);
            // não era da pessoa: era do sistema, que não o traz de volta
            if e.added.len() == before && !e.removed.contains(&k) {
                e.removed.push(k);
            }
        }
        self.save().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pessoa_adiciona_e_tira() {
        let dir = std::env::temp_dir().join(format!("anubis-temas-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let t = Temas::load(dir.clone());
        let sys = || vec![Area::new("Cibersegurança".into(), vec!["cybersecurity".into()], 1.0, Origem::Config), Area::new("cyberpunk".into(), vec!["cyberpunk".into()], 1.0, Origem::Gosto)];
        t.add("livros", "Criptografia").await.unwrap();
        t.remove("livros", "cyberpunk").await;
        let names: Vec<String> = t.apply("livros", sys()).await.into_iter().map(|a| a.nome).collect();
        assert_eq!(names, ["Criptografia", "Cibersegurança"]);
        // tirado pela pessoa, o do sistema não volta (mesmo recarregando)
        let t2 = Temas::load(dir.clone());
        assert!(!t2.apply("livros", sys()).await.iter().any(|a| a.nome == "cyberpunk"));
        // e volta se ela o puser de novo
        t2.add("livros", "Cyberpunk").await.unwrap();
        assert!(t2.apply("livros", sys()).await.iter().any(|a| a.nome == "Cyberpunk"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
