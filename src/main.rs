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
mod config;
mod discover;
mod disk;
mod foryou;
mod orchestrator;
mod live;
mod mux;
mod planner;
mod prefs;
mod search;
mod services;
mod stream;
mod subs;

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
    let mut config = None;
    let mut command = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" | "-c" => config = Some(PathBuf::from(args.next().context("--config precisa de um caminho")?)),
            "serve" | "plan" if command.is_none() => command = Some(a),
            "-h" | "--help" => {
                println!("uso: anubis [--config ARQ] [serve|plan]");
                return Ok(());
            }
            other => bail!("argumento desconhecido: {other} (veja --help)"),
        }
    }
    let config = config.or_else(|| std::env::var_os("ANUBIS_CONFIG").map(PathBuf::from)).unwrap_or_else(default_config);
    let mut cfg = config::Config::load(&config)?;
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
    let app = api::router(api::AppState { orch, wake });
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
