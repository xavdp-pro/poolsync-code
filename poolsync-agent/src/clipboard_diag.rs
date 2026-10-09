//! Rate-limited clipboard diagnostics for xrdp / PoolSync debugging.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::{info, warn};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};

use crate::clipboard::{
    clipboard_targets, is_rdp_bmp_only, selections_dead, targets_have_pasteable_image,
};

static LAST_SNAPSHOT: Mutex<Option<Instant>> = Mutex::new(None);
static LAST_OWNERS: Mutex<Option<(u32, u32)>> = Mutex::new(None);
static LAST_OWNER_CHECK: Mutex<Option<Instant>> = Mutex::new(None);

fn selection_owner(conn: &x11rb::rust_connection::RustConnection, name: &[u8]) -> Option<u32> {
    let atom = conn.intern_atom(false, name).ok()?.reply().ok()?.atom;
    Some(conn.get_selection_owner(atom).ok()?.reply().ok()?.owner)
}

fn owner_description(conn: &x11rb::rust_connection::RustConnection, owner: u32) -> String {
    if owner == 0 {
        return "none".into();
    }
    let class = conn
        .get_property(false, owner, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| {
            String::from_utf8_lossy(&reply.value)
                .replace('\0', "/")
                .trim_end_matches('/')
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "?".into());
    format!("0x{owner:x}:{class}")
}

/// Logs only X11 selection-owner transitions. This is cheap and does not read
/// clipboard contents, so it can stay enabled while reproducing browser hangs.
pub fn log_owner_transition() {
    let now = Instant::now();
    let due = LAST_OWNER_CHECK
        .lock()
        .ok()
        .map(|mut last| {
            let due = last
                .map(|at| now.duration_since(at) >= Duration::from_millis(500))
                .unwrap_or(true);
            if due {
                *last = Some(now);
            }
            due
        })
        .unwrap_or(false);
    if !due {
        return;
    }
    let Ok((conn, _)) = x11rb::connect(None) else {
        return;
    };
    let primary = selection_owner(&conn, b"PRIMARY").unwrap_or(0);
    let clipboard = selection_owner(&conn, b"CLIPBOARD").unwrap_or(0);
    let changed = LAST_OWNERS
        .lock()
        .ok()
        .map(|mut last| {
            let changed = *last != Some((primary, clipboard));
            if changed {
                *last = Some((primary, clipboard));
            }
            changed
        })
        .unwrap_or(false);
    if changed {
        info!(
            "clipboard owners primary={} clipboard={}",
            owner_description(&conn, primary),
            owner_description(&conn, clipboard)
        );
    }
}

fn summarize_targets(targets: &[String]) -> String {
    if targets.is_empty() {
        return "empty".into();
    }
    let mut kinds: Vec<&str> = Vec::new();
    if targets_have_pasteable_image(targets) {
        kinds.push("pasteable-image");
    }
    if is_rdp_bmp_only(targets) {
        kinds.push("bmp-only");
    }
    if targets
        .iter()
        .any(|t| t.to_ascii_lowercase().contains("utf8_string") || t.contains("text/plain"))
    {
        kinds.push("text");
    }
    if targets.iter().any(|t| t.starts_with("image/")) {
        kinds.push("image-target");
    }
    let sample: String = targets
        .iter()
        .take(6)
        .map(|t| t.as_str())
        .collect::<Vec<_>>()
        .join(",");
    format!("{}t [{}] ({})", targets.len(), kinds.join("+"), sample)
}

fn display_env() -> String {
    std::env::var("DISPLAY").unwrap_or_else(|_| "?".into())
}

/// Periodic snapshot while debugging (max once per 30s unless `force`).
pub async fn maybe_snapshot(reason: &str, force: bool) {
    let now = Instant::now();
    let due = if force {
        true
    } else {
        LAST_SNAPSHOT
            .lock()
            .ok()
            .map(|mut g| {
                let due = g
                    .map(|t| now.duration_since(t) >= Duration::from_secs(30))
                    .unwrap_or(true);
                if due {
                    *g = Some(now);
                }
                due
            })
            .unwrap_or(false)
    };
    if !due {
        return;
    }
    let clip = clipboard_targets("clipboard").await.unwrap_or_default();
    let pri = clipboard_targets("primary").await.unwrap_or_default();
    let dead = selections_dead().await;
    info!(
        "clipboard snap [{reason}] display={} dead={} clip={} primary={}",
        display_env(),
        dead,
        summarize_targets(&clip),
        summarize_targets(&pri)
    );
}

/// After a write, verify CLIPBOARD still looks pasteable.
pub async fn log_post_write(mime: &str, context: &str, ok: bool) {
    if !ok {
        warn!("clipboard write FAILED context={context} mime={mime}");
        maybe_snapshot("write-failed", true).await;
        return;
    }
    let clip = clipboard_targets("clipboard").await.unwrap_or_default();
    let summary = summarize_targets(&clip);
    if mime.starts_with("image/") && !targets_have_pasteable_image(&clip) {
        warn!(
            "clipboard write OK but no pasteable image on CLIPBOARD context={context} clip={summary}"
        );
    } else if mime.starts_with("text/") {
        let has_text = clip
            .iter()
            .any(|t| t.to_ascii_lowercase().contains("utf8_string") || t.contains("text/plain"));
        if !has_text {
            warn!("clipboard write OK but no text on CLIPBOARD context={context} clip={summary}");
        } else {
            info!("clipboard write OK context={context} mime={mime} clip={summary}");
        }
    } else {
        info!("clipboard write OK context={context} mime={mime} clip={summary}");
    }
}

/// Déclenche un rapport de diagnostic complet dans les logs et affiche une notification.
pub async fn trigger_full_diag(state: &std::sync::Arc<crate::state::AgentState>) {
    info!("================ [POLLSYNC DIAGNOSTIC DÉBUT] ================");
    info!(
        "Nœud: {} | Mode: {:?} | Sync clipboard: {} | Local active: {}",
        state.config.node,
        state.config.mode,
        state.clipboard_sync_enabled(),
        state.local_poolsync_active(),
    );
    info!(
        "Hub: {} | Connecté: {} | Master: {}",
        state.hub_display(),
        state.is_connected(),
        state.master_node(),
    );
    let clip = clipboard_targets("clipboard").await.unwrap_or_default();
    let pri = clipboard_targets("primary").await.unwrap_or_default();
    info!("Cibles X11 CLIPBOARD: {}", summarize_targets(&clip));
    info!("Cibles X11 PRIMARY:   {}", summarize_targets(&pri));
    if let Ok((conn, _)) = x11rb::connect(None) {
        let primary = selection_owner(&conn, b"PRIMARY").unwrap_or(0);
        let clipboard = selection_owner(&conn, b"CLIPBOARD").unwrap_or(0);
        info!(
            "Propriétaires X11: primary={} clipboard={}",
            owner_description(&conn, primary),
            owner_description(&conn, clipboard),
        );
    }
    info!(
        "Dernier hash: {}",
        state
            .last_clip_hash_handle()
            .lock()
            .map(|h| h.clone())
            .unwrap_or_default()
    );
    info!("================ [POLLSYNC DIAGNOSTIC FIN] ==================");
    crate::notify_util::notify_local(
        "Diagnostic PoolSync exécuté",
        "Rapport de diagnostic enregistré dans les logs (consultez 'Voir les logs').",
    );
}
