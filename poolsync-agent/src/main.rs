mod agent;
mod clip_cache;
mod clip_order;
mod clipboard;
mod clipboard_diag;
mod clipboard_epoch;
mod clipboard_gtk;
mod clipboard_history;
mod clipboard_incoming;
mod clipboard_manager;
mod clipboard_x11;
mod config_window;
mod crashlog;
mod cursor_ripple;
mod edge_flash;
mod hotkey;
mod hubless;
mod kvm;
mod kvm_input;
mod kvm_wayland;
mod kvm_x11;
mod local_control;
mod logs_viewer;
mod network;
mod notify_thumb;
mod notify_util;
mod participation;
mod peer_clip_transfer;
mod peer_mesh;
mod physical_input;
mod rdp_detect;
mod single;
mod state;
mod thumb;
mod topology_mosaic;
mod tray;

use agent::{clipboard_poll_loop, run_agent};
use anyhow::{Context, Result};
use clap::Parser;
use poolsync_core::AgentConfig;
use state::AgentState;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::time::{sleep, Duration};
use tracing::{info, warn};

const RECONNECT_INITIAL: Duration = Duration::from_secs(2);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

#[derive(Parser, Debug)]
#[command(
    name = "poolsync-agent",
    version,
    about = "PoolSync agent — client presse-papiers + KVM"
)]
struct Args {
    #[arg(long, default_value = "/etc/poolsync/agent.toml")]
    config: PathBuf,

    /// Temporarily leave (true) or rejoin (false) the pool, then exit.
    #[arg(long, value_name = "BOOL", action = clap::ArgAction::Set)]
    away: Option<bool>,

    #[arg(long)]
    no_tray: bool,

    /// Open the running agent configuration, or start an agent with it visible.
    #[arg(long)]
    open_window: bool,

    /// Start the actual agent with its configuration window visible.
    #[arg(long)]
    show_window: bool,
}

fn main() -> Result<()> {
    // The agent has both native/ring clients and an aws-lc rustls peer server.
    // Select one process provider explicitly before either TLS path is used.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    crashlog::install();
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "inconnu".into());
        let msg = match info.payload().downcast_ref::<&str>() {
            Some(s) => *s,
            None => match info.payload().downcast_ref::<String>() {
                Some(s) => &s[..],
                None => "panic sans message",
            },
        };
        tracing::error!("🔥 PANIC DETECTE [{location}]: {msg}");
        eprintln!("PANIC DETECTE [{location}]: {msg}");
    }));

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "poolsync_agent=info".into()),
        )
        .init();

    let mut args = Args::parse();

    // Diagnostic : ouvre la fenêtre à onglets sans systray ni verrou d'instance,
    // pour pouvoir la tester pendant que l'agent principal tourne.
    if let Some(away) = args.away {
        anyhow::ensure!(args.config.is_file(), "agent configuration does not exist");
        if let Err(error) = local_control::request_away(&args.config, away) {
            let _instance = single::InstanceLock::acquire().context(
                "running agent must confirm participation; restart it to enable local control",
            )?;
            tracing::debug!("no live agent participation control: {error:#}");
            participation::set_away(&args.config, away)?;
            println!("Local pool absence saved for next start: {away}");
        } else {
            println!("Local pool absence applied and saved: {away}");
        }
        return Ok(());
    }
    if args.open_window {
        if local_control::request_window(&args.config).is_ok() {
            return Ok(());
        }
        args.show_window = true;
    }

    let _instance = match single::InstanceLock::acquire() {
        Ok(lock) => lock,
        Err(error) => {
            if args.open_window {
                return Err(error)
                    .context("restart the running agent to enable native window control");
            }
            tracing::info!("poolsync-agent déjà actif — sortie");
            return Ok(());
        }
    };
    let raw = std::fs::read_to_string(&args.config)
        .with_context(|| format!("read config {}", args.config.display()))?;
    let cfg: AgentConfig = toml::from_str(&raw).context("parse agent config")?;
    let state = Arc::new(AgentState::new(cfg.clone(), args.config.clone()));

    info!(
        "starting agent node={} hub={} mode={:?}",
        cfg.node, cfg.hub_url, cfg.mode
    );

    hotkey::spawn_hotkey_listener(state.clone());
    // Recueille le presse-papiers des applications qui se ferment (X11
    // CLIPBOARD_MANAGER). S'abstient si un autre gestionnaire est déjà en place.
    if std::env::var("XDG_SESSION_TYPE")
        .is_ok_and(|session| !session.eq_ignore_ascii_case("wayland"))
    {
        clipboard_manager::spawn();
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("tokio runtime")?;

    let peer_tx = rt.block_on(async { peer_mesh::spawn(state.clone()) })?;
    if state.config.hubless && state.config.kvm_active() {
        let tx = peer_tx
            .clone()
            .context("hubless KVM requires direct peers")?;
        let kvm_state = state.clone();
        rt.spawn_blocking(move || crate::kvm::kvm_poll_loop(&kvm_state, tx));
    }

    // Le presse-papiers local appartient à l'agent, pas à la session hub. Le
    // hub n'est qu'un transport facultatif : lorsqu'il est absent, le sender
    // broadcast n'a aucun abonné et les copies continuent de partir sur le
    // mesh direct sans s'accumuler en mémoire jusqu'à la reconnexion.
    let (hub_clip_tx, _) = broadcast::channel::<String>(32);
    let state_clip = state.clone();
    let peer_tx_clip = peer_tx.clone();
    let hub_clip_tx_loop = hub_clip_tx.clone();
    let last_clip_hash = state.last_clip_hash_handle();
    rt.spawn(async move {
        clipboard_poll_loop(&state_clip, hub_clip_tx_loop, peer_tx_clip, last_clip_hash).await;
    });

    let state_agent = state.clone();
    let hub_clip_tx_agent = hub_clip_tx.clone();
    if !state.config.hubless {
        rt.spawn(async move {
            let mut backoff = RECONNECT_INITIAL;
            loop {
                match run_agent(state_agent.clone(), hub_clip_tx_agent.clone()).await {
                    Ok(()) => {
                        state_agent.set_error(None);
                        backoff = RECONNECT_INITIAL;
                        warn!("session hub terminée — reconnexion…");
                    }
                    Err(err) => {
                        state_agent.set_connected(false);
                        state_agent.set_error(Some(err.to_string()));
                        tracing::error!("agent session ended: {err:#}");
                    }
                }
                info!("nouvelle tentative hub dans {}s", backoff.as_secs());
                sleep(backoff).await;
                backoff = std::cmp::min(backoff * 2, RECONNECT_MAX);
            }
        });
    }

    let show_tray = !args.no_tray
        && (std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok());
    if show_tray {
        local_control::spawn(state.clone())?;
        info!("starting systray");
        if let Err(err) = tray::run_tray(state.clone(), args.show_window) {
            tracing::error!("systray failed ({err:#}), agent continues without tray");
            rt.block_on(async { std::future::pending::<()>().await });
        }
    } else {
        rt.block_on(async { std::future::pending::<()>().await });
    }

    Ok(())
}
