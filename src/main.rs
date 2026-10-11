//! Anubis — orquestrador da stack de mídia do FeuVault.
//!
//! Cuida do armazenamento (com o disco cheio, apaga os vídeos mais antigos:
//! Sonarr/Radarr + o torrent que semeia o mesmo dado) e oferece uma busca
//! unificada de filmes e séries, com página própria em `/`.
//!
//! Uso:
//!   anubis [--config ARQ] serve   daemon: API HTTP + agendador (padrão)
//!   anubis [--config ARQ] plan    mostra o que apagaria agora e sai
//!
//! Config padrão: $ANUBIS_CONFIG ou ~/.config/anubis/anubis.toml.

mod api;
mod autodub;
mod avail;
mod bitmagnet;
mod config;
mod discover;
mod drive;
mod estudo;
mod disk;
mod episode_avail;
mod dubsync;
mod external;
mod foryou;
mod gosto;
mod indexers;
mod kosync;
mod openlibrary;
mod leitura;
mod leitura_dl;
mod leitura_recs;
mod libgen;
mod orchestrator;
mod live;
mod mux;
mod planner;
mod quality;
mod prefs;
mod search;
mod season;
mod services;
mod stream;
mod subs;
mod telegram;
mod temas;
mod uninstall;
mod vitrine_livros;

use anyhow::{Context, Result, bail};
use orchestrator::Orchestrator;
use std::future::IntoFuture;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tracing::{error, info};

// uma thread basta: o trabalho é esperar rede e disco; mantém o consumo baixo
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "anubis=info".into()))
        .with_target(false)
        // logs no stderr: o stdout do `plan` fica só com o JSON
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr())) // cores só no terminal, não no journald
        .without_time() // o journald já carimba a hora
        .init();

    let mut args = std::env::args().skip(1);
    // `anubis verificar ARQ…`: o validador de EPUB/PDF, sem configuração
    if std::env::args().nth(1).as_deref() == Some("verificar") {
        for f in args.skip(1) {
            let v = quality::check(std::path::Path::new(&f), quality::Kind::Book).await;
            println!("{}\t{}\t{}\t{f}", v.problem.as_deref().unwrap_or("ok"), v.words, v.language.unwrap_or("?"));
        }
        return Ok(());
    }
    let mut config = None;
    let mut command = None;
    let mut rest: Vec<String> = vec![];
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" | "-c" => config = Some(PathBuf::from(args.next().context("--config precisa de um caminho")?)),
            "serve" | "plan" if command.is_none() => command = Some(a),
            // `anubis telegram …`: login e consultas da fonte Telegram
            "telegram" if command.is_none() => {
                command = Some(a);
                rest = args.by_ref().collect();
            }
            "-h" | "--help" => {
                println!("uso: anubis [--config ARQ] [serve|plan|telegram …] · anubis verificar ARQ…");
                return Ok(());
            }
            other => bail!("argumento desconhecido: {other} (veja --help)"),
        }
    }
    let config = config.or_else(|| std::env::var_os("ANUBIS_CONFIG").map(PathBuf::from)).unwrap_or_else(default_config);
    let mut cfg = config::Config::load(&config)?;
    if command.as_deref() == Some("telegram") {
        let tg = cfg.leitura.as_ref().and_then(|l| l.telegram.as_ref()).context("falta [leitura.telegram] na configuração")?;
        return telegram::cli(tg, &cfg.state_dir(), &rest).await;
    }
    let serving = command.as_deref().unwrap_or("serve") == "serve";
    let self_url = format!("http://{}", cfg.server.listen[0]);
    let streamer = match cfg.stream.as_mut() {
        Some(s) if serving => {
            let transcode = std::mem::take(&mut s.transcode);
            Some(Arc::new(stream::Streamer::new(s.state_dir.clone(), transcode, self_url).await?))
        }
        _ => None,
    };
    let orch = Arc::new(Orchestrator::new(cfg, streamer)?);
    if orch.streamer.is_some()
        && let Err(e) = orch.restore_streams().await
    {
        tracing::warn!("pedidos gravados não retomados: {e:#}");
    }
    if orch.streamer.is_some()
        && let Err(e) = orch.restore_muxes().await
    {
        tracing::warn!("dublagens em andamento não retomadas: {e:#}");
    }

    match command.as_deref().unwrap_or("serve") {
        "plan" => {
            let report = orch.cycle(false).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        _ => serve(orch).await,
    }
}

fn default_config() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".config/anubis/anubis.toml")
}

async fn serve(orch: Arc<Orchestrator>) -> Result<()> {
    let wake = Arc::new(Notify::new());
    tokio::spawn(scheduler(orch.clone(), wake.clone()));
    tokio::spawn(live::run(orch.clone()));
    tokio::spawn(avail::run(orch.clone()));
    tokio::spawn(indexers::run(orch.clone()));
    tokio::spawn(bitmagnet::run(orch.clone()));
    tokio::spawn(autodub::run(orch.clone()));
    // perfil do Sonarr para séries sem dublagem (quando se aceita o legendado)
    {
        let orch = orch.clone();
        tokio::spawn(async move {
            if let Err(e) = orch.setup_profiles().await {
                error!("perfil do Sonarr não preparado: {e:#}");
            }
        });
    }

    // um endereço que falha (placa ainda sem IP, sem ip_nonlocal_bind) não
    // impede os outros
    let mut listeners = Vec::new();
    for addr in &orch.cfg.server.listen {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => listeners.push((addr.clone(), l)),
            Err(e) => error!("não deu para escutar em {addr}: {e}"),
        }
    }
    if listeners.is_empty() {
        bail!("nenhum endereço de [server] listen disponível");
    }
    info!(
        "Anubis {} em {} — limpeza acima de {}%, até {}%{}",
        env!("CARGO_PKG_VERSION"),
        listeners.iter().map(|(a, _)| format!("http://{a}")).collect::<Vec<_>>().join(", "),
        orch.cfg.storage.start_percent,
        orch.cfg.storage.stop_percent,
        if orch.cfg.storage.dry_run { " (dry_run: só registra)" } else { "" }
    );
    let kosync = match &orch.cfg.kosync {
        Some(k) => Some(Arc::new(kosync::Kosync::load(k, orch.cfg.state_dir())?)),
        None => None,
    };
    let leitura = orch.cfg.leitura.as_ref().map(|l| {
        let ol = openlibrary::OpenLibrary::load(orch.cfg.state_dir())?;
        let bookshelf = l.bookshelf.as_ref().map(|b| leitura_dl::Bookshelf::new(&b.url, &b.api_key)).transpose()?;
        // sem login, o Telegram só fica fora da cadeia (avisa no log)
        let telegram = l.telegram.as_ref().map(|t| telegram::Telegram::new(t, &orch.cfg.state_dir())).transpose()?.map(Arc::new);
        let downloads = leitura_dl::Downloads::load(orch.clone(), bookshelf, telegram, l.drive.clone(), &orch.cfg.state_dir())?;
        let vitrine = vitrine_livros::Vitrine::load(l.flaresolverr.clone(), orch.cfg.state_dir())?;
        let estudo = estudo::Estudo::load(l.areas.clone(), l.openalex_api_key.clone(), l.openalex_campos.clone(), orch.cfg.state_dir())?;
        let leitura = Arc::new(leitura::Leitura::new(drive::Drive::new(l.drive.clone()), kosync.clone(), ol, downloads.clone(), orch.clone(), vitrine, estudo));
        leitura.start();
        // livro novo no Drive: a estante relista
        let me = leitura.clone();
        tokio::spawn(async move {
            loop {
                downloads.on_delivered.notified().await;
                me.drive.invalidate().await;
            }
        });
        anyhow::Ok(leitura)
    }).transpose()?;
    let app = api::router(api::AppState { orch, wake, kosync, leitura });
    let mut servers = tokio::task::JoinSet::new();
    for (_, l) in listeners {
        servers.spawn(axum::serve(l, app.clone()).into_future());
    }
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        Some(res) = servers.join_next() => res??,
    }
    Ok(())
}

/// Ciclo periódico + gatilhos (download concluído). Um erro num ciclo é
/// registrado e o próximo tenta de novo — o daemon não cai por isso.
async fn scheduler(orch: Arc<Orchestrator>, wake: Arc<Notify>) {
    let mut tick = tokio::time::interval(Duration::from_secs(orch.cfg.storage.interval_minutes * 60));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let why = tokio::select! {
            _ = tick.tick() => "periódico",
            _ = wake.notified() => "download concluído",
        };
        match orch.cycle(true).await {
            Ok(r) if r.plan.need == 0 => info!("ciclo ({why}): {:.1}% usado, nada a fazer", r.plan.used_percent),
            Ok(r) => info!("ciclo ({why}): {:.1}% usado, {} itens no plano", r.plan.used_percent, r.plan.deletions.len()),
            Err(e) => error!("ciclo ({why}) falhou: {e:#}"),
        }
    }
}
