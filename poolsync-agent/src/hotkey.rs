//! Global local hotkeys:
//! - Ctrl+Alt+Shift+P — pause / resume PoolSync on this machine
//! - Ctrl+Alt+Shift+M — claim KVM master on this machine
//! - Ctrl+Alt+Shift+C — center the pointer on the current monitor
//! - Ctrl+Alt+Shift+L — locate the pointer (ripple + node name)
use crate::notify_util;
use crate::state::AgentState;
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// P = PoolSync pause.
pub const HOTKEY_LABEL: &str = "Ctrl+Alt+Shift+P";
/// M = Master.
pub const HOTKEY_MASTER_LABEL: &str = "Ctrl+Alt+Shift+M";
/// C = Center cursor.
pub const HOTKEY_CENTER_LABEL: &str = "Ctrl+Alt+Shift+C";
/// L = Locate cursor (ripple).
pub const HOTKEY_LOCATE_LABEL: &str = "Ctrl+Alt+Shift+L";

/// Ignore Pressed+Released / auto-repeat doubles.
const TOGGLE_DEBOUNCE: Duration = Duration::from_millis(600);
const CENTER_DEBOUNCE: Duration = Duration::from_millis(200);

fn mods_cas() -> Option<Modifiers> {
    Some(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SHIFT)
}

fn toggle_hotkey() -> HotKey {
    HotKey::new(mods_cas(), Code::KeyP)
}

fn master_hotkey() -> HotKey {
    HotKey::new(mods_cas(), Code::KeyM)
}

fn center_hotkey() -> HotKey {
    HotKey::new(mods_cas(), Code::KeyC)
}

fn locate_hotkey() -> HotKey {
    HotKey::new(mods_cas(), Code::KeyL)
}

pub fn spawn_hotkey_listener(state: Arc<AgentState>) {
    thread::Builder::new()
        .name("poolsync-hotkey".into())
        .spawn(move || hotkey_loop(state))
        .map(|_| {
            info!(
                "raccourcis globaux {HOTKEY_LABEL} / {HOTKEY_MASTER_LABEL} / {HOTKEY_CENTER_LABEL} / {HOTKEY_LOCATE_LABEL} — écoute"
            )
        })
        .unwrap_or_else(|err| warn!("impossible de lancer le thread hotkey: {err}"));
}

fn register(manager: &GlobalHotKeyManager, key: HotKey, label: &str, ok_msg: &str) {
    if let Err(err) = manager.register(key) {
        warn!("enregistrement {label} échoué: {err:#}");
    } else {
        info!("{ok_msg}");
    }
}

fn hotkey_loop(state: Arc<AgentState>) {
    let manager = match GlobalHotKeyManager::new() {
        Ok(m) => m,
        Err(err) => {
            warn!("raccourci global indisponible: {err:#}");
            return;
        }
    };
    let toggle = toggle_hotkey();
    let master = master_hotkey();
    let center = center_hotkey();
    let locate = locate_hotkey();
    register(
        &manager,
        toggle,
        HOTKEY_LABEL,
        &format!("raccourci {HOTKEY_LABEL} enregistré (toggle PoolSync local)"),
    );
    register(
        &manager,
        master,
        HOTKEY_MASTER_LABEL,
        &format!("raccourci {HOTKEY_MASTER_LABEL} enregistré (réclamer master KVM)"),
    );
    register(
        &manager,
        center,
        HOTKEY_CENTER_LABEL,
        &format!("raccourci {HOTKEY_CENTER_LABEL} enregistré (centrer le curseur)"),
    );
    register(
        &manager,
        locate,
        HOTKEY_LOCATE_LABEL,
        &format!("raccourci {HOTKEY_LOCATE_LABEL} enregistré (localiser le curseur)"),
    );

    let receiver = GlobalHotKeyEvent::receiver();
    let past = Instant::now()
        .checked_sub(TOGGLE_DEBOUNCE)
        .unwrap_or_else(Instant::now);
    let mut last_toggle = past;
    let mut last_master = past;
    let mut last_center = past;
    let mut last_locate = past;
    loop {
        if let Ok(event) = receiver.try_recv() {
            if event.state != HotKeyState::Pressed {
                continue;
            }
            let now = Instant::now();
            if event.id == toggle.id() {
                if now.duration_since(last_toggle) >= TOGGLE_DEBOUNCE {
                    last_toggle = now;
                    on_toggle(&state);
                }
            } else if event.id == master.id() {
                if now.duration_since(last_master) >= TOGGLE_DEBOUNCE {
                    last_master = now;
                    on_master_claim(&state);
                }
            } else if event.id == center.id() {
                if now.duration_since(last_center) >= CENTER_DEBOUNCE {
                    last_center = now;
                    on_center_cursor();
                }
            } else if event.id == locate.id() && now.duration_since(last_locate) >= CENTER_DEBOUNCE
            {
                last_locate = now;
                on_locate_cursor(&state);
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn on_toggle(state: &AgentState) {
    let active = state.toggle_local_poolsync();
    let node = state.config.node.clone();
    if active {
        info!("PoolSync activé localement ({node}) via {HOTKEY_LABEL}");
    } else {
        info!("PoolSync désactivé localement ({node}) via {HOTKEY_LABEL}");
        crate::kvm_x11::set_cursor_visible_best_effort(true);
    }
    glib::MainContext::default().invoke(move || {
        notify_util::notify_poolsync_toggle(active, &node);
    });
}

fn on_master_claim(state: &AgentState) {
    let node = state.config.node.clone();
    if !state.kvm_enabled() || state.pool_away() {
        info!("master claim ignoré ({node}) : KVM inactif");
        glib::MainContext::default().invoke(move || {
            notify_util::notify_master_claim(&node, false);
        });
        return;
    }
    if !state.local_poolsync_active() {
        state.set_local_poolsync_active(true);
    }
    state.request_master_claim();
    info!("master KVM réclamé localement ({node}) via {HOTKEY_MASTER_LABEL}");
    glib::MainContext::default().invoke(move || {
        notify_util::notify_master_claim(&node, true);
    });
}

pub fn on_center_cursor() {
    crate::kvm_x11::set_cursor_visible_best_effort(true);
    match crate::kvm_x11::center_pointer_on_current_monitor() {
        Ok((x, y)) => info!("curseur centré via {HOTKEY_CENTER_LABEL} → ({x},{y})"),
        Err(err) => warn!("centrage curseur échoué: {err:#}"),
    }
}

pub fn on_locate_cursor(state: &AgentState) {
    crate::kvm_x11::set_cursor_visible_best_effort(true);
    let node = state.config.node.clone();
    info!("localiser curseur via {HOTKEY_LOCATE_LABEL} sur {node}");
    glib::MainContext::default().invoke(move || {
        crate::cursor_ripple::locate_cursor(&node);
    });
}
