use glib::MainContext;
use poolsync_core::{AgentConfig, PoolTopology, TopologyNode};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

type PrivateClipboardEpoch = Option<(u32, Option<u64>)>;

type LayoutRequest = (
    PoolTopology,
    std::sync::mpsc::SyncSender<Result<(), String>>,
);

#[derive(Clone)]
pub struct AgentState {
    pub config: AgentConfig,
    pub config_path: PathBuf,
    connected: Arc<AtomicBool>,
    clipboard_sync: Arc<AtomicBool>,
    keep_formatting: Arc<AtomicBool>,
    history_double_click_paste: Arc<AtomicBool>,
    notify_on_receive: Arc<AtomicBool>,
    /// Optional debug toast when KVM master changes (off by default — noisy).
    notify_master: Arc<AtomicBool>,
    kvm_enabled: Arc<AtomicBool>,
    layout_kvm_allowed: Arc<AtomicBool>,
    /// Pause locale (raccourci clavier) — n'affecte que cette machine.
    local_active: Arc<AtomicBool>,
    pool_away: Arc<AtomicBool>,
    participation_epoch: Arc<AtomicU64>,
    pool_away_transition: Arc<Mutex<()>>,
    clipboard_baseline_reset: Arc<AtomicBool>,
    private_clipboard_epoch: Arc<Mutex<PrivateClipboardEpoch>>,
    /// Ctrl+Alt+Shift+M : la boucle KVM doit reprendre le master sur ce nœud.
    master_claim_requested: Arc<AtomicBool>,
    master_node: Arc<RwLock<String>>,
    kvm_focus: Arc<RwLock<String>>,
    kvm_input_node: Arc<RwLock<String>>,
    topology: Arc<RwLock<Option<PoolTopology>>>,
    peer_monitors: Arc<RwLock<std::collections::HashMap<String, Vec<poolsync_core::MonitorInfo>>>>,
    pending_layout: Arc<Mutex<Option<LayoutRequest>>>,
    config_window_requested: Arc<AtomicBool>,
    last_clip_preview: Arc<RwLock<String>>,
    last_clip_at: Arc<RwLock<Option<Instant>>>,
    last_error: Arc<RwLock<Option<String>>>,
    last_notified_hash: Arc<RwLock<String>>,
    last_notified_preview: Arc<RwLock<String>>,
    last_notify_at: Arc<RwLock<Option<Instant>>>,
    /// Dernier collage entrant (hub ou peer) — évite reboucle poll / double hub+peer.
    last_incoming_apply_at: Arc<RwLock<Option<Instant>>>,
    last_incoming_mime: Arc<RwLock<String>>,
    /// Ordre total du presse-papiers sur le mesh (Lamport) — remplace les
    /// anciennes fenêtres de grâce « priorité copie locale ».
    clip_order: Arc<crate::clip_order::ClipOrder>,
    /// Dernière position injectée (ignore le warp KVM, pas l'activité physique).
    kvm_inject_pos: Arc<RwLock<Option<(i32, i32, Instant)>>>,
    /// Court délai après injection clavier synthétique (xtest).
    kvm_inject_key_until: Arc<RwLock<Option<Instant>>>,
    /// Court délai après injection clic synthétique.
    kvm_inject_button_until: Arc<RwLock<Option<Instant>>>,
    /// Après un SwitchTo local (warp curseur) — ne pas confondre avec activité physique.
    kvm_switch_enter_until: Arc<RwLock<Option<Instant>>>,
    /// Révision hub de l'historique presse-papiers (rafraîchit le menu systray).
    tray_history_revision: Arc<AtomicU64>,
    /// Bascule locale (raccourci) — rafraîchit titre/icône systray.
    tray_status_revision: Arc<AtomicU64>,
    /// Entrée locale envoyée au hub — affichée tout de suite dans le systray (sans attendre le WS retour).
    optimistic_tray: Arc<RwLock<Option<crate::clipboard_history::HistoryItem>>>,
    /// Dernier hash clipboard connu (évite reboucles poll / echo hub après pick).
    last_clip_hash: Arc<Mutex<String>>,
    /// Après vidage historique : ne pas ré-uploader le presse-papiers local tout de suite.
    history_clear_until: Arc<RwLock<Option<Instant>>>,
    /// Dernier positionnement KVM local (évite doublons SwitchTo).
    last_kvm_enter: Arc<RwLock<Option<(i32, i32, Instant)>>>,
}

fn incoming_grace_elapsed_less_than(
    state: &AgentState,
    grace_for: impl Fn(&str) -> std::time::Duration,
) -> bool {
    let Some(started) = state.last_incoming_apply_at.read().ok().and_then(|t| *t) else {
        return false;
    };
    let mime = state
        .last_incoming_mime
        .read()
        .map(|m| m.clone())
        .unwrap_or_default();
    started.elapsed() < grace_for(&mime)
}

fn persist_config_bool(path: &PathBuf, key: &str, value: bool) {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return;
    };
    let line = format!("{key} = {value}");
    let mut out = String::new();
    let mut replaced = false;
    for (i, l) in raw.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if l.trim_start().starts_with(key) {
            out.push_str(&line);
            replaced = true;
        } else {
            out.push_str(l);
        }
    }
    if !replaced {
        if !out.ends_with('\n') && !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&line);
        out.push('\n');
    } else if raw.ends_with('\n') {
        out.push('\n');
    }
    let _ = std::fs::write(path, out);
}

fn persist_keep_formatting(path: &PathBuf, value: bool) {
    persist_config_bool(path, "keep_formatting", value);
}

fn persist_history_double_click_paste(path: &PathBuf, value: bool) {
    persist_config_bool(path, "history_double_click_paste", value);
}

/// URL du tableau de bord, déduite de l'adresse WebSocket du hub.
///
/// `ws://192.0.2.20:9470/ws` → `http://192.0.2.20:9470/`. Évite d'ajouter un
/// réglage que l'utilisateur devrait tenir à jour en double.
pub fn hub_dashboard_url(hub_ws_url: &str) -> String {
    let base = hub_ws_url
        .strip_prefix("wss://")
        .map(|rest| format!("https://{rest}"))
        .or_else(|| {
            hub_ws_url
                .strip_prefix("ws://")
                .map(|rest| format!("http://{rest}"))
        })
        .unwrap_or_else(|| hub_ws_url.to_string());
    let base = base.strip_suffix("/ws").unwrap_or(&base);
    format!("{}/", base.trim_end_matches('/'))
}

impl AgentState {
    pub fn new(config: AgentConfig, config_path: PathBuf) -> Self {
        let away = crate::participation::is_away(&config_path);
        let kvm_default = config.kvm_active();
        let local_node = config.node.clone();
        let order_node = local_node.clone();
        let keep_formatting = config.keep_formatting;
        let history_double_click_paste = config.history_double_click_paste;
        Self {
            config,
            config_path,
            connected: Arc::new(AtomicBool::new(false)),
            clipboard_sync: Arc::new(AtomicBool::new(true)),
            keep_formatting: Arc::new(AtomicBool::new(keep_formatting)),
            history_double_click_paste: Arc::new(AtomicBool::new(history_double_click_paste)),
            notify_on_receive: Arc::new(AtomicBool::new(true)),
            notify_master: Arc::new(AtomicBool::new(false)),
            kvm_enabled: Arc::new(AtomicBool::new(kvm_default)),
            layout_kvm_allowed: Arc::new(AtomicBool::new(true)),
            local_active: Arc::new(AtomicBool::new(true)),
            pool_away: Arc::new(AtomicBool::new(away)),
            participation_epoch: Arc::new(AtomicU64::new(0)),
            pool_away_transition: Arc::new(Mutex::new(())),
            clipboard_baseline_reset: Arc::new(AtomicBool::new(false)),
            private_clipboard_epoch: Arc::new(Mutex::new(None)),
            master_claim_requested: Arc::new(AtomicBool::new(false)),
            master_node: Arc::new(RwLock::new(String::from("—"))),
            kvm_focus: Arc::new(RwLock::new(local_node.clone())),
            kvm_input_node: Arc::new(RwLock::new(local_node)),
            topology: Arc::new(RwLock::new(None)),
            pending_layout: Arc::new(Mutex::new(None)),
            config_window_requested: Arc::new(AtomicBool::new(false)),
            peer_monitors: Arc::new(RwLock::new(Default::default())),
            last_clip_preview: Arc::new(RwLock::new(String::new())),
            last_clip_at: Arc::new(RwLock::new(None)),
            last_error: Arc::new(RwLock::new(None)),
            last_notified_hash: Arc::new(RwLock::new(String::new())),
            last_notified_preview: Arc::new(RwLock::new(String::new())),
            last_notify_at: Arc::new(RwLock::new(None)),
            last_incoming_apply_at: Arc::new(RwLock::new(None)),
            last_incoming_mime: Arc::new(RwLock::new(String::new())),
            clip_order: Arc::new(crate::clip_order::ClipOrder::new(order_node)),
            kvm_inject_pos: Arc::new(RwLock::new(None)),
            kvm_inject_key_until: Arc::new(RwLock::new(None)),
            kvm_inject_button_until: Arc::new(RwLock::new(None)),
            kvm_switch_enter_until: Arc::new(RwLock::new(None)),
            tray_history_revision: Arc::new(AtomicU64::new(0)),
            tray_status_revision: Arc::new(AtomicU64::new(0)),
            optimistic_tray: Arc::new(RwLock::new(None)),
            last_clip_hash: Arc::new(Mutex::new(String::new())),
            history_clear_until: Arc::new(RwLock::new(None)),
            last_kvm_enter: Arc::new(RwLock::new(None)),
        }
    }

    pub fn last_clip_hash_handle(&self) -> Arc<Mutex<String>> {
        Arc::clone(&self.last_clip_hash)
    }

    pub fn set_last_clip_hash(&self, hash: &str) {
        if let Ok(mut guard) = self.last_clip_hash.lock() {
            *guard = hash.to_string();
        }
    }

    /// Après vidage historique hub : aligner le hash local sans ré-envoyer au hub.
    pub fn mark_history_cleared(&self) {
        const SUPPRESS_MS: u64 = 8000;
        if let Ok(mut until) = self.history_clear_until.write() {
            *until = Some(Instant::now() + std::time::Duration::from_millis(SUPPRESS_MS));
        }
    }

    pub fn history_clear_suppress_active(&self) -> bool {
        self.history_clear_until
            .read()
            .ok()
            .and_then(|u| *u)
            .is_some_and(|deadline| Instant::now() < deadline)
    }

    /// Ignore un SwitchTo local identique reçu en double (hub / nœuds voisins).
    pub fn should_skip_kvm_enter(&self, x: i32, y: i32) -> bool {
        const DEDUP_MS: u128 = 250;
        let now = Instant::now();
        if let Ok(mut slot) = self.last_kvm_enter.write() {
            if let Some((lx, ly, t)) = *slot {
                if lx == x && ly == y && now.duration_since(t).as_millis() < DEDUP_MS {
                    return true;
                }
            }
            *slot = Some((x, y, now));
        }
        false
    }

    /// Affiche immédiatement la copie locale en tête du menu systray.
    pub fn set_optimistic_tray_item(&self, item: crate::clipboard_history::HistoryItem) {
        if let Ok(mut slot) = self.optimistic_tray.write() {
            *slot = Some(item);
        }
        self.notify_tray_history_changed();
    }

    /// Incrémente le compteur local — ne jamais écraser avec la révision hub (évite régressions).
    pub fn notify_tray_history_changed(&self) {
        self.tray_history_revision.fetch_add(1, Ordering::SeqCst);
    }

    pub fn clear_optimistic_tray(&self, hash: &str) {
        if let Ok(mut slot) = self.optimistic_tray.write() {
            if slot.as_ref().is_some_and(|i| i.hash == hash) {
                *slot = None;
            }
        }
    }

    pub fn clear_optimistic_tray_all(&self) {
        if let Ok(mut slot) = self.optimistic_tray.write() {
            *slot = None;
        }
    }

    pub fn optimistic_tray_item(&self) -> Option<crate::clipboard_history::HistoryItem> {
        self.optimistic_tray.read().ok().and_then(|g| g.clone())
    }

    pub fn tray_history_revision(&self) -> u64 {
        self.tray_history_revision.load(Ordering::SeqCst)
    }

    pub fn notify_tray_status_changed(&self) {
        self.tray_status_revision.fetch_add(1, Ordering::SeqCst);
    }

    pub fn tray_status_revision(&self) -> u64 {
        self.tray_status_revision.load(Ordering::SeqCst)
    }

    pub fn mark_kvm_inject_at(&self, x: i32, y: i32) {
        if let Ok(mut pos) = self.kvm_inject_pos.write() {
            *pos = Some((x, y, Instant::now()));
        }
    }

    pub fn mark_kvm_inject_key(&self) {
        const GRACE: std::time::Duration = std::time::Duration::from_millis(40);
        if let Ok(mut until) = self.kvm_inject_key_until.write() {
            *until = Some(Instant::now() + GRACE);
        }
    }

    pub fn mark_kvm_inject_button(&self) {
        const GRACE: std::time::Duration = std::time::Duration::from_millis(50);
        if let Ok(mut until) = self.kvm_inject_button_until.write() {
            *until = Some(Instant::now() + GRACE);
        }
    }

    pub fn mark_kvm_switch_enter(&self) {
        const GRACE: std::time::Duration = std::time::Duration::from_millis(450);
        if let Ok(mut until) = self.kvm_switch_enter_until.write() {
            *until = Some(Instant::now() + GRACE);
        }
    }

    pub fn switch_enter_grace_active(&self) -> bool {
        self.kvm_switch_enter_until
            .read()
            .ok()
            .and_then(|u| *u)
            .is_some_and(|t| t > Instant::now())
    }

    /// Pilotage KVM distant actif (injections récentes sur cette machine).
    pub fn remote_drive_active(&self) -> bool {
        const GRACE: std::time::Duration = std::time::Duration::from_millis(350);
        self.kvm_inject_pos
            .read()
            .ok()
            .and_then(|p| *p)
            .map(|(_, _, t)| t.elapsed() < GRACE)
            .unwrap_or(false)
    }

    /// Le mouvement souris peut reprendre le master (pas un warp / injection KVM).
    pub fn motion_claim_allowed(&self, local: &str, px: i32, py: i32) -> bool {
        if self.remote_drive_active() {
            return false;
        }
        // A different device's focus does not suppress this device's physical
        // mouse. Only our own active remote-grab session blocks a local claim.
        if self.kvm_focus() != local && self.is_input_owner() {
            return false;
        }
        if self.switch_enter_grace_active() {
            return false;
        }
        const INJECT_GRACE: std::time::Duration = std::time::Duration::from_millis(300);
        const DIST_MIN: i32 = 20;
        if let Ok(pos) = self.kvm_inject_pos.read() {
            if let Some((ix, iy, t)) = *pos {
                if t.elapsed() < INJECT_GRACE {
                    let dist = (px - ix).abs() + (py - iy).abs();
                    if dist < DIST_MIN {
                        return false;
                    }
                }
            }
        }
        true
    }

    pub fn inject_blocks_key_claim(&self) -> bool {
        self.kvm_inject_key_until
            .read()
            .ok()
            .and_then(|u| *u)
            .is_some_and(|t| t > Instant::now())
    }

    pub fn inject_blocks_button_claim(&self) -> bool {
        self.kvm_inject_button_until
            .read()
            .ok()
            .and_then(|u| *u)
            .is_some_and(|t| t > Instant::now())
    }

    pub fn note_kvm_inject(&self, kind: &poolsync_core::InputKind) {
        use poolsync_core::InputKind;
        match kind {
            InputKind::MouseMove { x, y } | InputKind::MouseWheel { x, y, .. } => {
                self.mark_kvm_inject_at(*x, *y);
            }
            InputKind::MouseButton { x, y, .. } => {
                self.mark_kvm_inject_at(*x, *y);
                self.mark_kvm_inject_button();
            }
            InputKind::Key { .. } => self.mark_kvm_inject_key(),
            InputKind::MouseMoveRelative { .. } => {}
        }
    }

    pub fn mark_incoming_clipboard_applied(&self, mime: &str) {
        if let Ok(mut t) = self.last_incoming_apply_at.write() {
            *t = Some(Instant::now());
        }
        if let Ok(mut m) = self.last_incoming_mime.write() {
            *m = mime.to_string();
        }
    }

    /// Ordre total des presse-papiers du pool (voir `clip_order`).
    pub fn clip_order(&self) -> &crate::clip_order::ClipOrder {
        &self.clip_order
    }

    /// Alias RDP / legacy.
    pub fn mark_hub_clipboard_applied(&self) {
        self.mark_incoming_clipboard_applied("text/plain");
    }

    /// Après collage entrant : ne pas relayer immédiatement ce qu'on vient d'écrire.
    pub fn incoming_poll_suppress_active(&self) -> bool {
        incoming_grace_elapsed_less_than(self, |mime| {
            if mime.starts_with("image/") {
                // Long enough for xclip/GTK settle + peer echo without re-send.
                std::time::Duration::from_millis(4000)
            } else {
                std::time::Duration::from_millis(900)
            }
        })
    }

    pub fn incoming_offer_settling(&self) -> bool {
        incoming_grace_elapsed_less_than(self, |_| std::time::Duration::from_millis(100))
    }

    /// Hub + peer livrent parfois la même image deux fois (hash différent).
    pub fn incoming_duplicate_suppress_active(&self) -> bool {
        incoming_grace_elapsed_less_than(self, |mime| {
            if mime.starts_with("image/") {
                std::time::Duration::from_millis(5000)
            } else {
                std::time::Duration::from_millis(1200)
            }
        })
    }

    pub fn set_connected(&self, value: bool) {
        let changed = self.connected.swap(value, Ordering::SeqCst) != value;
        if value {
            if let Ok(mut err) = self.last_error.write() {
                *err = None;
            }
        }
        if changed {
            self.notify_tray_status_changed();
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    pub fn clipboard_sync_enabled(&self) -> bool {
        self.clipboard_sync.load(Ordering::SeqCst)
    }

    pub fn set_clipboard_sync(&self, value: bool) {
        let was = self.clipboard_sync.swap(value, Ordering::SeqCst);
        if was != value {
            // Tracé au niveau info : un nœud « sourd » sans trace dans le
            // journal a coûté une matinée de diagnostic (work-a, 02/09).
            tracing::info!(
                "synchro presse-papiers {} sur {} (menu systray)",
                if value { "ACTIVÉE" } else { "COUPÉE" },
                self.config.node
            );
        }
    }

    pub fn toggle_clipboard_sync(&self) -> bool {
        let new = !self.clipboard_sync_enabled();
        self.set_clipboard_sync(new);
        new
    }

    pub fn keep_formatting(&self) -> bool {
        self.keep_formatting.load(Ordering::SeqCst)
    }

    pub fn set_keep_formatting(&self, value: bool) {
        self.keep_formatting.store(value, Ordering::SeqCst);
        persist_keep_formatting(&self.config_path, value);
    }

    pub fn history_double_click_paste(&self) -> bool {
        self.history_double_click_paste.load(Ordering::SeqCst)
    }

    pub fn set_history_double_click_paste(&self, value: bool) {
        self.history_double_click_paste
            .store(value, Ordering::SeqCst);
        persist_history_double_click_paste(&self.config_path, value);
    }

    pub fn notify_enabled(&self) -> bool {
        self.notify_on_receive.load(Ordering::SeqCst)
    }

    pub fn set_notify(&self, value: bool) {
        self.notify_on_receive.store(value, Ordering::SeqCst);
    }

    pub fn toggle_notify(&self) -> bool {
        let new = !self.notify_enabled();
        self.set_notify(new);
        new
    }

    pub fn notify_master_enabled(&self) -> bool {
        self.notify_master.load(Ordering::SeqCst)
    }

    pub fn set_notify_master(&self, value: bool) {
        self.notify_master.store(value, Ordering::SeqCst);
    }

    pub fn toggle_notify_master(&self) -> bool {
        let new = !self.notify_master_enabled();
        self.set_notify_master(new);
        new
    }

    pub fn kvm_enabled(&self) -> bool {
        self.kvm_enabled.load(Ordering::SeqCst)
    }

    pub fn set_kvm_enabled(&self, value: bool) {
        self.kvm_enabled.store(value, Ordering::SeqCst);
    }

    pub fn local_poolsync_active(&self) -> bool {
        self.local_active.load(Ordering::SeqCst) && !self.pool_away()
    }

    pub fn pool_away(&self) -> bool {
        self.pool_away.load(Ordering::SeqCst)
    }

    /// Fence queued/partial clipboard transfers across even brief departures.
    pub fn participation_epoch(&self) -> u64 {
        self.participation_epoch.load(Ordering::SeqCst)
    }

    pub fn set_pool_away(&self, away: bool) -> std::io::Result<()> {
        let _transition = self
            .pool_away_transition
            .lock()
            .map_err(|_| std::io::Error::other("pool participation transition unavailable"))?;
        crate::participation::set_away(&self.config_path, away)?;
        self.apply_pool_away(away);
        Ok(())
    }

    /// Observe the latest marker under the same lock as explicit commands.
    /// An older polling observation must never rewrite a newer user command.
    pub fn refresh_pool_away(&self) -> std::io::Result<()> {
        let _transition = self
            .pool_away_transition
            .lock()
            .map_err(|_| std::io::Error::other("pool participation transition unavailable"))?;
        self.apply_pool_away(crate::participation::is_away(&self.config_path));
        Ok(())
    }

    fn apply_pool_away(&self, away: bool) {
        if away == self.pool_away() {
            return;
        }
        if !away && self.pool_away() {
            if let Ok(mut private) = self.private_clipboard_epoch.lock() {
                *private = Some((
                    crate::clipboard_gtk::current_clipboard_owner(),
                    crate::clipboard_epoch::clipboard_copy_current(),
                ));
            }
            self.clipboard_baseline_reset.store(true, Ordering::SeqCst);
        }
        self.participation_epoch.fetch_add(1, Ordering::SeqCst);
        self.pool_away.store(away, Ordering::SeqCst);
        self.set_local_poolsync_active(!away);
    }

    /// Returning to the pool excludes the private selection itself, including
    /// a delayed SAVE_TARGETS payload whose owner has already closed. A newly
    /// copied selection is publishable even when it contains identical bytes.
    pub fn clipboard_publication_allowed(&self, identity: (u32, Option<u64>)) -> bool {
        let Ok(mut private) = self.private_clipboard_epoch.lock() else {
            return false;
        };
        let Some((owner, copy_epoch)) = *private else {
            return true;
        };
        let excluded = if copy_epoch.is_some() {
            identity.1 == copy_epoch
        } else {
            identity.0 == owner || identity.0 == 0
        };
        if excluded {
            return false;
        }
        *private = None;
        true
    }

    pub fn take_clipboard_baseline_reset(&self) -> bool {
        self.clipboard_baseline_reset.swap(false, Ordering::SeqCst)
    }

    pub fn set_local_poolsync_active(&self, value: bool) {
        let was = self.local_poolsync_active();
        if self.local_active.swap(value, Ordering::SeqCst) != value {
            self.participation_epoch.fetch_add(1, Ordering::SeqCst);
        }
        self.notify_tray_status_changed();
        if !value {
            // Leave remote-grab state so resume does not keep driving another screen.
            self.set_kvm_focus(&self.config.node);
            // Mettre PoolSync en pause doit rendre le presse-papiers à la
            // session : garder la propriété de la sélection sans plus la
            // synchroniser laisse l'utilisateur avec un copier-coller mort,
            // et empêche le mécanisme natif (XFCE, xrdp) de reprendre la main.
            crate::clipboard_gtk::clear_image_claim();
            crate::clipboard_gtk::release_ownership();
        } else if !was && self.kvm_enabled() {
            // Resume: this keyboard/mouse must own the pool again (edge switching).
            self.request_master_claim();
        }
    }

    /// Bascule KVM + presse-papiers sur cette machine (raccourci global).
    pub fn toggle_local_poolsync(&self) -> bool {
        let new = !self.local_poolsync_active();
        self.set_local_poolsync_active(new);
        new
    }

    pub fn toggle_kvm(&self) -> bool {
        let new = !self.kvm_enabled();
        self.set_kvm_enabled(new);
        new
    }

    /// Demande à la boucle KVM de reprendre le master sur cette machine.
    pub fn request_master_claim(&self) {
        self.master_claim_requested.store(true, Ordering::SeqCst);
    }

    pub fn take_master_claim_request(&self) -> bool {
        self.master_claim_requested.swap(false, Ordering::SeqCst)
    }

    pub fn set_topology(&self, topology: PoolTopology) {
        let focus = self.kvm_focus();
        if focus != self.config.node
            && !topology
                .nodes
                .get(&focus)
                .is_some_and(|node| node.kvm_enabled)
        {
            self.set_kvm_focus(&self.config.node);
            self.set_kvm_input_node(&self.config.node);
        }
        if let Ok(mut t) = self.topology.write() {
            *t = Some(topology);
        }
    }

    pub fn request_config_window(&self) {
        self.config_window_requested.store(true, Ordering::SeqCst);
    }

    pub fn take_config_window_request(&self) -> bool {
        self.config_window_requested.swap(false, Ordering::SeqCst)
    }

    pub fn request_layout(&self, topology: PoolTopology) -> anyhow::Result<()> {
        let (ack, received) = std::sync::mpsc::sync_channel(1);
        {
            let mut pending = self
                .pending_layout
                .lock()
                .map_err(|_| anyhow::anyhow!("layout queue unavailable"))?;
            anyhow::ensure!(pending.is_none(), "a layout edit is already pending");
            *pending = Some((topology, ack));
        }
        received
            .recv_timeout(std::time::Duration::from_secs(2))
            .map_err(|_| anyhow::anyhow!("layout save not confirmed; check the agent status"))?
            .map_err(anyhow::Error::msg)
    }

    pub fn take_layout_request(&self) -> Option<LayoutRequest> {
        self.pending_layout.lock().ok().and_then(|mut p| p.take())
    }

    pub fn topology(&self) -> Option<PoolTopology> {
        self.topology.read().ok().and_then(|t| t.clone())
    }

    pub fn set_peer_monitors(
        &self,
        monitors: std::collections::HashMap<String, Vec<poolsync_core::MonitorInfo>>,
    ) {
        if let Ok(mut current) = self.peer_monitors.write() {
            *current = monitors;
        }
    }

    pub fn clamp_peer_pointer(&self, node: &str, x: i32, y: i32) -> (i32, i32) {
        self.peer_monitors
            .read()
            .ok()
            .and_then(|peers| {
                peers
                    .get(node)
                    .map(|monitors| poolsync_core::clamp_pointer_to_monitors(monitors, x, y))
            })
            .unwrap_or((x, y))
    }

    pub fn topology_node(&self, name: &str) -> Option<TopologyNode> {
        self.topology().and_then(|t| t.nodes.get(name).cloned())
    }

    pub fn set_layout_kvm_allowed(&self, allowed: bool) {
        self.layout_kvm_allowed.store(allowed, Ordering::SeqCst);
    }

    pub fn kvm_effective(&self) -> bool {
        self.kvm_enabled()
            && self.local_poolsync_active()
            && self.layout_kvm_allowed.load(Ordering::SeqCst)
    }

    pub fn target_kvm_enabled(&self, node: &str) -> bool {
        if node == self.config.node {
            return self.kvm_effective();
        }
        self.topology_node(node)
            .map(|n| n.kvm_enabled)
            // Sécurité d'entrée : sans topologie connue, ne jamais considérer
            // par défaut une cible comme injectable. Le hub/cache pourra la
            // rendre explicitement KVM, mais un nœud clipboard_only reste sûr.
            .unwrap_or(false)
    }

    pub fn set_kvm_focus(&self, node: &str) {
        if let Ok(mut f) = self.kvm_focus.write() {
            *f = node.to_string();
        }
    }

    pub fn kvm_focus(&self) -> String {
        self.kvm_focus
            .read()
            .map(|f| f.clone())
            .unwrap_or_else(|_| self.config.node.clone())
    }

    pub fn set_kvm_input_node(&self, node: &str) {
        if let Ok(mut n) = self.kvm_input_node.write() {
            *n = node.to_string();
        }
    }

    pub fn kvm_input_node(&self) -> String {
        self.kvm_input_node
            .read()
            .map(|n| n.clone())
            .unwrap_or_else(|_| self.config.node.clone())
    }

    pub fn is_input_owner(&self) -> bool {
        self.kvm_input_node() == self.config.node
    }

    /// Notifie à la réception si le hash clipboard est nouveau (anti-rafale hub+peer).
    pub fn should_notify(&self, hash: &str, preview: &str) -> bool {
        if !self.notify_enabled() {
            return false;
        }
        const MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2000);
        if let Ok(last_at) = self.last_notify_at.read() {
            if last_at.is_some_and(|t| t.elapsed() < MIN_INTERVAL) {
                return false;
            }
        }
        let Ok(mut last_hash) = self.last_notified_hash.write() else {
            return false;
        };
        if *last_hash == hash {
            return false;
        }
        *last_hash = hash.to_string();
        if let Ok(mut last_preview) = self.last_notified_preview.write() {
            *last_preview = preview.to_string();
        }
        if let Ok(mut last_at) = self.last_notify_at.write() {
            *last_at = Some(Instant::now());
        }
        true
    }

    pub fn set_master(&self, node: &str) {
        let prev = self.master_node();
        if prev == node {
            return;
        }
        if let Ok(mut m) = self.master_node.write() {
            *m = node.to_string();
        }
        self.maybe_notify_master_change(node);
    }

    fn maybe_notify_master_change(&self, node: &str) {
        if !self.notify_master_enabled() {
            return;
        }
        if node.is_empty() || node == "—" {
            return;
        }
        let node = node.to_string();
        let here = self.config.node.clone();
        MainContext::default().invoke(move || {
            crate::notify_util::notify_master_changed(&here, &node);
        });
    }

    pub fn master_node(&self) -> String {
        self.master_node
            .read()
            .map(|m| m.clone())
            .unwrap_or_else(|_| "—".into())
    }

    pub fn set_error(&self, msg: Option<String>) {
        if let Ok(mut err) = self.last_error.write() {
            *err = msg;
        }
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.read().ok().and_then(|e| e.clone())
    }

    pub fn record_clip_received(&self, preview: String) {
        if let Ok(mut p) = self.last_clip_preview.write() {
            *p = preview;
        }
        if let Ok(mut t) = self.last_clip_at.write() {
            *t = Some(Instant::now());
        }
    }

    pub fn status_line(&self) -> String {
        let clip = if self.clipboard_sync_enabled() {
            "clip ON"
        } else {
            "clip OFF"
        };
        if !self.local_poolsync_active() {
            format!("● Suspendu localement ({clip})")
        } else if self.config.hubless {
            let count = self
                .topology()
                .map(|t| t.nodes.values().filter(|n| n.kvm_enabled).count())
                .unwrap_or(0);
            format!(
                "● Sans hub — {} — {count} postes KVM ({clip})",
                if self.is_connected() {
                    "pairs connectés"
                } else {
                    "local"
                }
            )
        } else if self.is_connected() {
            format!("● Connecté — {clip}")
        } else if self.config.peer_direct_clipboard {
            format!("● Hub hors ligne — P2P disponible ({clip})")
        } else if let Some(err) = self.last_error() {
            format!("● Reconnexion… — {err}")
        } else {
            format!("● Reconnexion VPN/hub… ({clip})")
        }
    }

    pub fn hub_display(&self) -> String {
        self.config
            .hub_url
            .trim_start_matches("ws://")
            .trim_start_matches("wss://")
            .trim_end_matches("/ws")
            .to_string()
    }
}

pub fn clip_preview(text: &str) -> String {
    let one_line: String = text.chars().take(80).collect();
    if text.len() > 80 {
        format!("{one_line}…")
    } else {
        one_line
    }
}

pub fn clip_preview_mime(mime: &str, data: &str) -> String {
    if mime.starts_with("image/") {
        let label = mime.strip_prefix("image/").unwrap_or(mime);
        let bytes = data.len().saturating_mul(3) / 4;
        let size = if bytes >= 1024 * 1024 {
            format!("{:.1} Mo", bytes as f64 / (1024.0 * 1024.0))
        } else if bytes >= 1024 {
            format!("{} Ko", bytes / 1024)
        } else {
            format!("{bytes} o")
        };
        format!("[Image {label} — {size}]")
    } else if mime == "text/html" {
        clip_preview(&crate::clipboard::html_to_visible_text(data))
    } else {
        clip_preview(data)
    }
}
#[cfg(test)]
mod dashboard_url_tests {
    use super::{hub_dashboard_url, AgentState};
    use poolsync_core::AgentConfig;
    use std::path::PathBuf;

    fn test_state() -> AgentState {
        let config: AgentConfig = toml::from_str(
            r#"
node = "desk-a"
hub_url = "wss://hub.invalid/ws"
token = "test"
mode = "full"
peer_direct_clipboard = true

[screen]
width = 100
height = 100
"#,
        )
        .unwrap();
        AgentState::new(config, PathBuf::from("/tmp/poolsync-test-agent.toml"))
    }

    #[test]
    fn another_devices_focus_does_not_disable_physical_takeover() {
        let state = test_state();
        state.set_kvm_focus("other");
        state.set_kvm_input_node("other");
        assert!(state.motion_claim_allowed("desk-a", 400, 300));
        state.set_kvm_input_node("desk-a");
        assert!(!state.motion_claim_allowed("desk-a", 400, 300));
    }

    #[test]
    fn derives_the_dashboard_url_from_the_hub_websocket() {
        assert_eq!(
            hub_dashboard_url("ws://192.0.2.20:9470/ws"),
            "http://192.0.2.20:9470/"
        );
        assert_eq!(
            hub_dashboard_url("wss://hub.example.net/ws"),
            "https://hub.example.net/"
        );
        // Une adresse sans /ws ni schéma connu reste utilisable telle quelle.
        assert_eq!(hub_dashboard_url("http://hub:9470"), "http://hub:9470/");
    }

    #[test]
    fn disconnected_hub_does_not_claim_that_peer_clipboard_is_down() {
        let state = test_state();
        assert_eq!(
            state.status_line(),
            "● Hub hors ligne — P2P disponible (clip ON)"
        );
    }

    #[test]
    fn connection_changes_refresh_the_tray_and_unknown_kvm_targets_fail_closed() {
        let state = test_state();
        let before = state.tray_status_revision();
        state.set_connected(true);
        assert!(state.tray_status_revision() > before);
        assert_eq!(state.status_line(), "● Connecté — clip ON");
        assert!(!state.target_kvm_enabled("unknown-or-clipboard-only"));
    }
}
