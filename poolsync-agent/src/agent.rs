use crate::clipboard::{
    prepare_local_clipboard, read_clipboard_payload_filtered, send_payload_network, write_clipboard,
};
use crate::clipboard_history;
use crate::kvm::{detect_kvm_desktop, detect_screen, inject_input, kvm_poll_loop};
use crate::kvm_x11;
use crate::network::{hub_tcp_endpoint, hub_tcp_reachable, wait_for_hub};
use crate::notify_thumb::notify_thumbnail_path;
use crate::rdp_detect::rdp_client_active;
use crate::state::AgentState;
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use poolsync_core::{decode_message, decrypt_clipboard, encode_message, Message};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::{
    process::Command,
    sync::{broadcast, mpsc},
    time::{interval, sleep, timeout, Duration},
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        client::IntoClientRequest,
        http::{header::AUTHORIZATION, HeaderValue},
        Message as WsMessage,
    },
};
use tracing::{info, warn};

const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const HUB_LINK_CHECK: Duration = Duration::from_secs(10);

pub async fn run_agent(
    state: Arc<AgentState>,
    hub_clip_tx: broadcast::Sender<String>,
) -> Result<()> {
    let cfg = &state.config;
    let mut request = cfg.hub_url.clone().into_client_request()?;
    request.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", cfg.authentication_token()))?,
    );
    request
        .headers_mut()
        .insert("x-poolsync-node", HeaderValue::from_str(&cfg.node)?);
    let (hub_host, hub_port) = hub_tcp_endpoint(&cfg.hub_url)?;

    state.set_connected(false);
    wait_for_hub(&hub_host, hub_port).await;

    let (ws, _) = timeout(WS_CONNECT_TIMEOUT, connect_async(request))
        .await
        .context("délai connexion hub dépassé (VPN/réseau?)")?
        .context("connect hub websocket")?;
    let (mut write, mut read) = ws.split();

    let screen = detect_screen().await.unwrap_or(cfg.screen);
    let kvm_desktop = detect_kvm_desktop().await.unwrap_or_default();
    if screen.width != cfg.screen.width || screen.height != cfg.screen.height {
        info!(
            "écran détecté {}x{} (config {}x{})",
            screen.width, screen.height, cfg.screen.width, cfg.screen.height
        );
    }

    write
        .send(WsMessage::Text(
            encode_message(&Message::Hello {
                node: cfg.node.clone(),
                mode: cfg.mode,
                screen,
                neighbors: cfg.neighbors.clone(),
                kvm_enabled: state.kvm_effective(),
                kvm_desktop,
                clipboard_sync: state.clipboard_sync_enabled(),
                local_active: state.local_poolsync_active(),
                monitors: crate::kvm_x11::described_monitors().unwrap_or_default(),
            })?
            .into(),
        ))
        .await?;

    state.set_connected(true);
    state.set_kvm_focus(&cfg.node);
    state.set_kvm_input_node(&cfg.node);
    info!("connected to hub");

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let last_clip_hash = state.last_clip_hash_handle();
    // S'abonner seulement une fois le hub connecté. Ainsi, les copies faites
    // pendant une coupure restent P2P et ne forment pas une file périmée qui
    // serait rejouée en rafale au retour du hub.
    let mut hub_clip_rx = hub_clip_tx.subscribe();

    let state_in = state.clone();
    let out_tx_in = out_tx.clone();
    let kvm_task = cfg
        .kvm_active()
        .then(|| tokio::task::spawn_blocking(move || kvm_poll_loop(&state_in, out_tx_in)));

    let mut link_check = interval(HUB_LINK_CHECK);
    // Le hub doit refléter l'état réel du nœud (synchro coupée, pause locale,
    // écrans branchés). L'agent le lui redit dès que ça bouge : sans ça, un
    // nœud « sourd » ou un écran ajouté restent invisibles dans l'interface.
    let mut state_check = interval(Duration::from_secs(3));
    let mut last_state = node_state_snapshot(&state);

    let result: Result<()> = async {
        loop {
            tokio::select! {
                maybe_out = out_rx.recv() => {
                    if let Some(payload) = maybe_out {
                        write.send(WsMessage::Text(payload.into())).await?;
                    } else {
                        break;
                    }
                }
                hub_clip = hub_clip_rx.recv() => {
                    match hub_clip {
                        Ok(payload) => write.send(WsMessage::Text(payload.into())).await?,
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!("relais hub clipboard en retard: {skipped} message(s) ignoré(s)");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                maybe_in = read.next() => {
                    match maybe_in {
                        Some(Ok(WsMessage::Text(text))) => {
                            handle_incoming(&state, &text, &last_clip_hash).await?;
                        }
                        Some(Ok(WsMessage::Ping(payload))) => {
                            write.send(WsMessage::Pong(payload)).await?;
                        }
                        Some(Ok(WsMessage::Close(_))) | None => break,
                        Some(Err(err)) => return Err(err.into()),
                        _ => {}
                    }
                }
                _ = link_check.tick() => {
                    if !hub_tcp_reachable(&hub_host, hub_port).await {
                        warn!("liaison hub perdue ({hub_host}:{hub_port}) — reconnexion");
                        break;
                    }
                }
                _ = state_check.tick() => {
                    let now = node_state_snapshot(&state);
                    if now != last_state {
                        last_state = now;
                        send_hello(&state, &out_tx);
                    }
                }
            }
        }

        Ok(())
    }
    .await;

    state.set_connected(false);
    state.set_kvm_focus(&cfg.node);
    state.set_kvm_input_node(&cfg.node);
    if let Some(task) = kvm_task {
        task.abort();
    }
    result
}

/// Ce que le hub doit savoir de l'état du nœud, pour détecter un changement.
fn node_state_snapshot(state: &AgentState) -> (bool, bool, bool, Vec<poolsync_core::MonitorInfo>) {
    (
        state.clipboard_sync_enabled(),
        state.local_poolsync_active(),
        state.kvm_effective(),
        crate::kvm_x11::described_monitors().unwrap_or_default(),
    )
}

/// Renvoie un Hello au hub avec l'état courant (écrans, synchro, pause).
///
/// Relit la géométrie réelle plutôt que d'envoyer une valeur par défaut :
/// le hub s'en sert pour la topologie, et lui transmettre des zéros
/// effacerait la position des écrans qu'il a mémorisée.
fn send_hello(state: &AgentState, out_tx: &mpsc::UnboundedSender<String>) {
    let monitors = crate::kvm_x11::described_monitors().unwrap_or_default();
    let screen = crate::kvm_x11::kvm_display()
        .map(|d| poolsync_core::ScreenInfo {
            width: d.width,
            height: d.height,
        })
        .unwrap_or(state.config.screen);
    let kvm_desktop = crate::kvm_x11::kvm_layout_snapshot().unwrap_or_default();
    match encode_message(&Message::Hello {
        node: state.config.node.clone(),
        mode: state.config.mode,
        screen,
        neighbors: state.config.neighbors.clone(),
        kvm_enabled: state.kvm_effective(),
        kvm_desktop,
        clipboard_sync: state.clipboard_sync_enabled(),
        local_active: state.local_poolsync_active(),
        monitors: monitors.clone(),
    }) {
        Ok(payload) => {
            if out_tx.send(payload).is_ok() {
                info!(
                    "état renvoyé au hub (synchro={} pause={} écrans={})",
                    state.clipboard_sync_enabled(),
                    !state.local_poolsync_active(),
                    monitors.len()
                );
            }
        }
        Err(err) => tracing::warn!("encode Hello (état): {err:#}"),
    }
}

pub(crate) async fn handle_incoming(
    state: &AgentState,
    text: &str,
    _last_clip_hash: &Mutex<String>,
) -> Result<()> {
    let mut msg = decode_message(text)?;
    if state.pool_away()
        && matches!(
            msg,
            Message::Clipboard { .. } | Message::EncryptedClipboard { .. }
        )
    {
        return Ok(());
    }
    if matches!(msg, Message::EncryptedClipboard { .. }) {
        let key = state
            .config
            .e2e_key
            .as_deref()
            .context("encrypted clipboard received but e2e_key is not configured")?;
        msg = decrypt_clipboard(&msg, key)?;
    } else if state.config.e2e_key.is_some() && matches!(msg, Message::Clipboard { .. }) {
        anyhow::bail!("unencrypted clipboard rejected while e2e_key is configured");
    }
    match msg {
        Message::Clipboard {
            hash,
            data,
            mime,
            origin,
            seq,
            ..
        } => {
            crate::clipboard_incoming::apply_incoming_clipboard(
                state, &hash, &data, &mime, "hub", true, &origin, seq,
            )
            .await?;
        }
        Message::MasterChanged { node } => {
            state.set_master(&node);
            // Ne pas modifier kvm_input_node / focus ici : sinon la machine
            // distante « vole » le clavier et les bords ne répondent plus localement.
            info!("primary KVM → {node}");
        }
        Message::ShowEdges { duration_ms } => {
            let st = state.clone();
            // Les fenêtres GTK ne se créent que sur la boucle GTK.
            glib::MainContext::default().invoke(move || {
                crate::edge_flash::show(&st, Duration::from_millis(duration_ms.clamp(300, 15_000)));
            });
        }
        Message::TopologyUpdate { topology } => {
            state.set_topology(topology);
            info!("topology updated from hub");
        }
        Message::ClipboardHistoryUpdated { .. } => {
            // Un autre nœud a vidé / modifié l'historique hub — purger le cache local si hub vide.
            if let Ok(items) = clipboard_history::fetch_history_hub_only(state) {
                if items.is_empty() {
                    crate::clip_cache::clear_all();
                    state.clear_optimistic_tray_all();
                    state.mark_history_cleared();
                }
            }
            state.notify_tray_history_changed();
        }
        Message::Input { kind, .. }
            if matches!(state.config.mode, poolsync_core::AgentMode::Full)
                && state.kvm_enabled()
                && state.local_poolsync_active() =>
        {
            state.note_kvm_inject(&kind);
            inject_input(&kind).await?;
        }
        Message::SwitchTo {
            node,
            x,
            y,
            input_node,
        } if state.kvm_enabled() => {
            state.set_kvm_focus(&node);
            let owner = if input_node.is_empty() {
                state.config.node.clone()
            } else {
                input_node.clone()
            };
            state.set_kvm_input_node(&owner);
            if !input_node.is_empty() {
                state.set_master(&input_node);
            }
            if !state.local_poolsync_active() {
                return Ok(());
            }
            if node == state.config.node {
                if state.should_skip_kvm_enter(x, y) {
                    return Ok(());
                }
                let edge = state.config.edge_px as i32;
                let whole_desktop = state.config.hubless;
                // Protect the warp before another thread can observe its result.
                state.mark_kvm_switch_enter();
                let (x, y) = tokio::task::spawn_blocking(move || {
                    let (x, y) = kvm_x11::nudge_kvm_enter(x, y, edge, whole_desktop)?;
                    kvm_x11::move_mouse_absolute(x, y)?;
                    Ok::<_, anyhow::Error>((x, y))
                })
                .await??;
                state.mark_kvm_inject_at(x, y);
                state.mark_kvm_switch_enter();
                info!("KVM cursor enter → {} ({x},{y})", state.config.node);
            }
        }
        _ => {}
    }
    Ok(())
}

pub async fn show_clip_notification(title: &str, preview: &str, mime: &str, wire_data: &str) {
    crate::notify_util::ensure_notify_daemon();
    let body = if preview.is_empty() {
        if mime.starts_with("image/") {
            "Image copiée".to_string()
        } else {
            "Nouveau contenu dans le presse-papiers".to_string()
        }
    } else {
        preview.to_string()
    };
    let icon = if mime.starts_with("image/") {
        match notify_thumbnail_path(mime, wire_data) {
            Ok(path) => path,
            Err(err) => {
                warn!("miniature notification: {err:#}");
                notify_icon_path()
            }
        }
    } else {
        notify_icon_path()
    };
    let base_args = [
        "-a",
        "com.xavdp.poolsync",
        "-i",
        &icon,
        "-t",
        "4000",
        "-u",
        "normal",
        title,
        &body,
    ];
    let with_replace = {
        let mut args = vec!["-r", "87001"];
        args.extend_from_slice(&base_args);
        args
    };
    if run_notify_send(&with_replace).await {
        info!("notification envoyée ({title})");
        return;
    }
    if run_notify_send(&base_args).await {
        info!("notification envoyée ({title})");
    } else {
        warn!("notify-send échoué");
    }
}

async fn run_notify_send(args: &[&str]) -> bool {
    // Hard timeout: if xfce4-notifyd is dead/zombie, bare notify-send hangs forever.
    let mut cmd = Command::new("timeout");
    cmd.arg("3").arg("notify-send").args(args);
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    for key in [
        "DISPLAY",
        "DBUS_SESSION_BUS_ADDRESS",
        "XAUTHORITY",
        "XDG_RUNTIME_DIR",
        "XDG_CURRENT_DESKTOP",
    ] {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }
    match cmd.status().await {
        Ok(s) if s.success() => true,
        Ok(s) => {
            if let Some(code) = s.code() {
                warn!("notify-send exit {code}");
            }
            false
        }
        Err(err) => {
            warn!("notify-send: {err}");
            false
        }
    }
}

fn notify_icon_path() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    format!("{home}/.local/share/poolsync/poolsync-tray.png")
}

pub(crate) async fn clipboard_poll_loop(
    state: &AgentState,
    hub_tx: broadcast::Sender<String>,
    peer_tx: Option<mpsc::UnboundedSender<String>>,
    last_clip_hash: Arc<Mutex<String>>,
) {
    let poll = Duration::from_millis(state.config.clipboard_poll_ms);
    // Au démarrage, le presse-papiers contient déjà quelque chose : c'est un
    // état hérité, pas une copie que l'utilisateur vient de faire. Le diffuser
    // lui donnerait une horloge fraîche, donc prioritaire, et il écraserait sur
    // tout le pool une copie réellement plus récente (observé sur work-b après
    // un redémarrage). On l'adopte comme référence, sans rien émettre.
    let mut observed_native_epoch = crate::clipboard_epoch::clipboard_copy_current();
    if !state.pool_away() {
        crate::clipboard::seed_local_baseline(&last_clip_hash, state.keep_formatting()).await;
    }

    let mut observed_native_hash = last_clip_hash
        .lock()
        .ok()
        .map(|h| h.clone())
        .filter(|h| !h.is_empty());
    let mut participation_probe = std::time::Instant::now();
    let mut last_native_probe = std::time::Instant::now();
    let mut last_selection_epoch = None;
    loop {
        if participation_probe.elapsed() >= Duration::from_secs(1) {
            if let Err(error) = state.refresh_pool_away() {
                warn!("cannot update local pool absence: {error}");
            }
            participation_probe = std::time::Instant::now();
        }
        // Returning a laptop must not promote a private copy made while away.
        if state.take_clipboard_baseline_reset() {
            observed_native_epoch = crate::clipboard_epoch::clipboard_copy_current();
            crate::clipboard::seed_local_baseline(&last_clip_hash, state.keep_formatting()).await;
            observed_native_hash = last_clip_hash
                .lock()
                .ok()
                .map(|h| h.clone())
                .filter(|h| !h.is_empty());
        }
        if crate::clipboard::xrdp_session_active_sync() {
            crate::clipboard_diag::log_owner_transition();
        }
        // Hands off X11 when PoolSync clipboard is OFF — otherwise we steal
        // CLIPBOARD from the apps and native Ctrl+V dies (xrdp session).
        // The RDP server agent distributes local copies to the pool. Reading
        // them again on its client would create a second clipboard transport,
        // promote RDP echoes to new copies, and overwrite newer screenshots.
        let rdp_paused = state.config.pause_clipboard_when_rdp && rdp_client_active().await;
        if state.clipboard_sync_enabled() && state.local_poolsync_active() && !rdp_paused {
            let manager_payload = crate::clipboard::take_clipboard_manager_payload();
            let selection_epoch = crate::clipboard_epoch::current();
            // XFixes observes both CLIPBOARD and PRIMARY changes. Keep the
            // configured fast response to a new copy, but avoid repeatedly
            // converting an unchanged native selection while idle. A one-
            // second fallback still checks owners which silently change data.
            if manager_payload.is_none()
                && selection_epoch.is_some()
                && selection_epoch == last_selection_epoch
                && last_native_probe.elapsed() < Duration::from_secs(1)
            {
                sleep(poll).await;
                continue;
            }
            last_selection_epoch = selection_epoch;
            last_native_probe = std::time::Instant::now();
            crate::clipboard::maintain_xrdp_clipboard_fixup().await;
            // Consume explicit application handoffs before orphan recovery
            // can change their selection generation.
            if manager_payload.is_none() {
                crate::clipboard::reclaim_orphaned_selection().await;
            }

            let skip_echo = state.incoming_offer_settling()
                || ((state.incoming_poll_suppress_active()
                    || state.incoming_duplicate_suppress_active())
                    && crate::clipboard_gtk::owns_clipboard())
                || state.history_clear_suppress_active();
            // Une application qui se ferme peut confier son contenu au
            // CLIPBOARD_MANAGER puis disparaître avant le prochain sondage.
            // Ce contenu est une vraie copie locale et doit être traité même
            // pendant une courte fenêtre anti-écho.
            // GTK/X11 transfers clipboard ownership asynchronously.  Reading
            // during this short settle window can still return the previous
            // text; treating it as a local copy creates an old-text echo that
            // overwrites the user's next paste on another node.
            if skip_echo && manager_payload.is_none() {
                sleep(poll).await;
                continue;
            }
            // Always read images: a local screenshot must enter the queue even
            // on clipboard_only (incoming images still skip X11 write).
            let publication_identity = (
                crate::clipboard_gtk::current_clipboard_owner(),
                crate::clipboard_epoch::clipboard_copy_current(),
            );
            let payload = match manager_payload {
                Some(payload) => Some(payload),
                None => read_clipboard_payload_filtered(true, state.keep_formatting())
                    .await
                    .ok()
                    .flatten(),
            };
            if let Some(payload) = payload {
                // An incoming copy can update the shared hash before its GTK
                // offer replaces the native owner. The unchanged previously
                // observed selection is not a new local copy in that interval.
                let fresh_native = publication_identity.1.is_none()
                    || observed_native_epoch != publication_identity.1
                    || observed_native_hash
                        .as_ref()
                        .is_some_and(|h| h != &payload.hash);
                if !fresh_native || !state.clipboard_publication_allowed(publication_identity) {
                    sleep(poll).await;
                    continue;
                }
                observed_native_epoch = publication_identity.1;
                observed_native_hash = Some(payload.hash.clone());
                if prepare_local_clipboard(&payload, &last_clip_hash) {
                    // Ne jamais journaliser le contenu : un presse-papiers peut
                    // contenir un mot de passe, un jeton ou une clef privée.
                    info!(
                        "clipboard local: mime={} bytes={} id={}",
                        payload.mime,
                        payload.wire_data.len(),
                        crate::clipboard::trace_id(&payload.hash)
                    );
                    // Horloge logique de cette copie : elle domine tout ce que
                    // ce nœud a déjà vu, donc un message plus ancien encore en
                    // vol ne pourra plus l'écraser.
                    let seq = state.clip_order().next_local_seq();
                    // Local-first: cache + systray avant tout envoi réseau (legacy hub / peer).
                    clipboard_history::notify_local_clipboard_sent(state, &payload);
                    if payload.mime.starts_with("image/") {
                        // Existing PRIMARY text predates this screenshot; it must
                        // not override the newly refreshed image recovery claim.
                        crate::clipboard::seed_primary_baseline().await;
                        info!(
                            "image-trace LOCAL id={} mime={} wire_bytes={}",
                            crate::clipboard::trace_id(&payload.hash),
                            payload.mime,
                            payload.wire_data.len()
                        );
                        // Reprendre la sélection d'une image que l'application
                        // d'origine sert déjà correctement ne sert à rien, et
                        // c'est dangereux : si l'utilisateur colle à cet
                        // instant précis, sa demande part vers l'ancien
                        // propriétaire qui vient de la perdre, et l'application
                        // reste bloquée (Flameshot → WhatsApp, 31/08).
                        //
                        // On ne revendique donc que si l'image n'est pas
                        // collable en l'état — le cas xrdp/chansrv, où le
                        // propriétaire n'expose qu'un BMP vide ou rien du tout.
                        if crate::clipboard::local_image_is_already_pasteable().await {
                            info!("clipboard image locale déjà collable — sélection laissée à l'application");
                        } else if let Err(err) =
                            write_clipboard(&payload.wire_data, &payload.mime).await
                        {
                            tracing::warn!("claim local clipboard: {err:#}");
                            crate::clipboard_diag::log_post_write(
                                &payload.mime,
                                "local-claim",
                                false,
                            )
                            .await;
                        } else {
                            crate::clipboard_diag::log_post_write(
                                &payload.mime,
                                "local-claim",
                                true,
                            )
                            .await;
                        }
                    }
                    let preview =
                        crate::state::clip_preview_mime(&payload.mime, &payload.wire_data);
                    if state.should_notify(&payload.hash, &preview) {
                        let mime = payload.mime.clone();
                        let wire = payload.wire_data.clone();
                        tokio::spawn(async move {
                            show_clip_notification("PoolSync — Copié", &preview, &mime, &wire)
                                .await;
                        });
                    }
                    let hub_tx_net = hub_tx.clone();
                    let peer_tx_net = peer_tx.clone();
                    let payload_net = payload.clone();
                    let relay_hub = state.config.hub_clipboard;
                    let origin = state.config.node.clone();
                    let e2e_key = state.config.e2e_key.clone();
                    tokio::spawn(async move {
                        if send_payload_network(
                            &payload_net,
                            &hub_tx_net,
                            &peer_tx_net,
                            relay_hub,
                            &origin,
                            seq,
                            e2e_key.as_deref(),
                        ) && payload_net.mime.starts_with("image/")
                        {
                            let approx_bytes = payload_net.wire_data.len().saturating_mul(3) / 4;
                            info!(
                                "clipboard image relayed ({}, ~{approx_bytes} bytes)",
                                payload_net.mime
                            );
                        }
                    });
                }
            }
        }
        let idle = !state.local_poolsync_active() || !state.clipboard_sync_enabled() || rdp_paused;
        sleep(if idle {
            poll.max(Duration::from_millis(250))
        } else {
            poll
        })
        .await;
    }
}
