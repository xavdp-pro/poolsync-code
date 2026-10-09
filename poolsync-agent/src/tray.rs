use crate::clipboard_history;
use crate::config_window;
use crate::notify_util;
use crate::state::AgentState;
use anyhow::{Context, Result};
use glib::translate::FromGlibPtrFull;
use gtk::prelude::*;
use std::cell::RefCell;
use std::ffi::CString;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

/// Nombre de copier-coller récents affichés directement dans le systray.
const RECENT_IN_TRAY: usize = 12;

thread_local! {
    /// Entrées de copier-coller du menu racine, créées une seule fois puis
    /// simplement renommées. Le menu vit dans le processus du panel (dbusmenu) :
    /// y détruire des widgets pendant qu'il les dessine fait segfauter Pango.
    static ITEM_SLOTS: std::cell::RefCell<Vec<gtk::MenuItem>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Lance le systray. **Doit être appelé depuis le thread principal** : GTK/GDK
/// n'est pas thread-safe, et les fenêtres ouvertes depuis le menu (historique,
/// configuration, logs) construisent des widgets et des `Pixbuf` qui segfaultent
/// si la boucle GTK ne tourne pas sur le thread qui a fait `gtk::init()`.
pub fn run_tray(state: Arc<AgentState>, show_window: bool) -> Result<()> {
    run_tray_gtk(state, show_window)
}

fn run_tray_gtk(state: Arc<AgentState>, show_window: bool) -> Result<()> {
    gtk::init().map_err(|e| anyhow::anyhow!("gtk init: {e}"))?;
    crate::clipboard_gtk::attach_gtk_handler();

    let (icon_dir, icon_path) = install_icon_png()?;

    if let Some(theme) = gtk::IconTheme::default() {
        theme.append_search_path(&icon_dir);
        theme.rescan_if_needed();
    }

    // Menu des options : il est réservé au clic droit.
    let menu = gtk::Menu::new();

    // ── 1. En-tête & Informations Nœud ──────────────────────────────────
    let title_label = format!(
        "● PoolSync v{} — {} ({})",
        env!("CARGO_PKG_VERSION"),
        state.config.node,
        if state.config.kvm_active() {
            "KVM + Clip"
        } else {
            "Clipboard Only"
        }
    );
    let title_item = gtk::MenuItem::with_label(&title_label);
    title_item.set_sensitive(false);
    menu.append(&title_item);

    let status_item = gtk::MenuItem::with_label(&format!("Statut : {}", state.status_line()));
    status_item.set_sensitive(false);
    menu.append(&status_item);

    let hub_item = gtk::MenuItem::with_label(&format!("Hub : {}", state.hub_display()));
    hub_item.set_sensitive(false);
    menu.append(&hub_item);

    let master_item = if state.config.kvm_active() || state.config.kvm_enabled.is_some() {
        let mi = gtk::MenuItem::with_label(&format!("Maître KVM : {}", state.master_node()));
        mi.set_sensitive(false);
        menu.append(&mi);
        Some(mi)
    } else {
        None
    };

    menu.append(&gtk::SeparatorMenuItem::new());

    // Local absence survives restarts and preserves layout and credentials.
    let away_item = gtk::CheckMenuItem::with_label("Machine temporairement à l’écart du pool");
    away_item.set_active(state.pool_away());
    away_item.set_tooltip_text(Some("À emporter : aucun partage automatique ni nouvelles copies du pool. Décochez au retour ; vos positions sont conservées."));
    let state_away = state.clone();
    let away_handler = away_item.connect_toggled(move |item| {
        if let Err(error) = state_away.set_pool_away(item.is_active()) {
            tracing::warn!("cannot persist local absence: {error}");
            item.set_active(state_away.pool_away());
        }
    });
    menu.append(&away_item);

    // ── 2. Bascules & Actions Principales ────────────────────────────────
    let clip_item =
        gtk::CheckMenuItem::with_label(&clip_sync_label(state.clipboard_sync_enabled()));
    clip_item.set_active(state.clipboard_sync_enabled());
    let state_clip = state.clone();
    clip_item.connect_toggled(move |item| {
        // Suivre l'état réel de la case plutôt que basculer à l'aveugle :
        // une émission parasite de « toggled » inversait la synchro sans
        // que la case le reflète, et personne ne pouvait le savoir.
        let wanted = item.is_active();
        if wanted != state_clip.clipboard_sync_enabled() {
            let on = apply_clipboard_sync_toggle(&state_clip);
            item.set_label(&clip_sync_label(on));
        }
    });
    menu.append(&clip_item);

    let fmt_item = gtk::CheckMenuItem::with_label("Garder le formatage (HTML / RTF)");
    fmt_item.set_active(state.keep_formatting());
    let state_fmt = state.clone();
    fmt_item.connect_toggled(move |item| {
        state_fmt.set_keep_formatting(item.is_active());
    });
    menu.append(&fmt_item);

    if state.config.kvm_active() || state.config.kvm_enabled.is_some() {
        let kvm_item = gtk::CheckMenuItem::with_label("Clavier / souris KVM");
        kvm_item.set_active(state.kvm_enabled());
        let state_kvm = state.clone();
        kvm_item.connect_toggled(move |_| {
            state_kvm.toggle_kvm();
        });
        menu.append(&kvm_item);

        let claim_item = gtk::MenuItem::with_label(&format!(
            "Devenir maître KVM ({})",
            crate::hotkey::HOTKEY_MASTER_LABEL
        ));
        let state_claim = state.clone();
        claim_item.connect_activate(move |_| {
            if !state_claim.kvm_enabled() || state_claim.pool_away() {
                return;
            }
            if !state_claim.local_poolsync_active() {
                state_claim.set_local_poolsync_active(true);
            }
            state_claim.request_master_claim();
        });
        menu.append(&claim_item);

        let center_item = gtk::MenuItem::with_label(&format!(
            "Centrer le curseur ({})",
            crate::hotkey::HOTKEY_CENTER_LABEL
        ));
        center_item.connect_activate(move |_| {
            crate::hotkey::on_center_cursor();
        });
        menu.append(&center_item);

        let locate_item = gtk::MenuItem::with_label(&format!(
            "Localiser le curseur ({})",
            crate::hotkey::HOTKEY_LOCATE_LABEL
        ));
        let state_loc = state.clone();
        locate_item.connect_activate(move |_| {
            crate::hotkey::on_locate_cursor(&state_loc);
        });
        menu.append(&locate_item);
    }

    menu.append(&gtk::SeparatorMenuItem::new());

    // ── 3. Fenêtres & Gestion ───────────────────────────────────────────
    let history_item = gtk::MenuItem::with_label("Ouvrir PoolSync (Historique & Config)…");
    let state_hist = state.clone();
    history_item.connect_activate(move |_| {
        config_window::show(state_hist.clone());
    });
    menu.append(&history_item);

    // Le tableau de bord du hub est l'interface complète (état du pool,
    // historique, mosaïque). Le menu local y renvoie plutôt que de dupliquer.
    if !state.config.hubless {
        let dash_item = gtk::MenuItem::with_label("Tableau de bord du pool (navigateur)…");
        let dash_url = crate::state::hub_dashboard_url(&state.config.hub_url);
        dash_item.connect_activate(move |_| {
            let url = dash_url.clone();
            std::thread::spawn(move || {
                let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
            });
        });
        menu.append(&dash_item);
    }

    let logs_item = gtk::MenuItem::with_label("Voir les logs en direct (Debug)…");
    let node_for_logs = state.config.node.clone();
    logs_item.connect_activate(move |_| {
        crate::logs_viewer::show(&node_for_logs);
    });
    menu.append(&logs_item);

    let diag_item = gtk::MenuItem::with_label("Diagnostic presse-papiers & réseau…");
    let state_diag = state.clone();
    diag_item.connect_activate(move |_| {
        let s = state_diag.clone();
        tokio::spawn(async move {
            crate::clipboard_diag::trigger_full_diag(&s).await;
        });
    });
    menu.append(&diag_item);

    let clear_item = gtk::MenuItem::with_label("Vider l'historique…");
    let state_clear = state.clone();
    clear_item.connect_activate(move |_| {
        clipboard_history::confirm_clear_from_tray(state_clear.clone());
    });
    menu.append(&clear_item);

    menu.append(&gtk::SeparatorMenuItem::new());

    // Menu historique séparé : il est réservé au clic gauche et ne contient
    // que les éléments réellement présents dans le buffer PoolSync.
    let history_menu = gtk::Menu::new();

    let mut slots = Vec::new();
    for _ in 0..RECENT_IN_TRAY {
        let item = gtk::MenuItem::with_label("");
        let state_pick = state.clone();
        let slot_hash: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let hash_for_click = slot_hash.clone();
        item.connect_activate(move |_| {
            let hash = hash_for_click.borrow().clone();
            if let Some(hash) = hash {
                if let Err(err) = clipboard_history::pick_and_paste(&state_pick, &hash) {
                    tracing::warn!("tray pick: {err:#}");
                }
            }
        });
        history_menu.append(&item);
        slots.push((item, slot_hash));
    }

    // Ce réglage appartient au menu droit, pas à la liste gauche.
    let dblclick_item = gtk::CheckMenuItem::with_label("Double-clic → presse-papiers");
    dblclick_item.set_active(state.history_double_click_paste());
    let state_dbl = state.clone();
    dblclick_item.connect_toggled(move |item| {
        state_dbl.set_history_double_click_paste(item.is_active());
    });
    menu.append(&dblclick_item);

    menu.append(&gtk::SeparatorMenuItem::new());

    // ── 5. Options Secondaires & Débogage ───────────────────────────────
    let notify_item = gtk::CheckMenuItem::with_label("Notifier copie / réception");
    notify_item.set_active(state.notify_enabled());
    let state_notif = state.clone();
    notify_item.connect_toggled(move |_| {
        state_notif.toggle_notify();
    });
    menu.append(&notify_item);

    let master_notif_item = gtk::CheckMenuItem::with_label("Notifier changement de master KVM");
    master_notif_item.set_active(state.notify_master_enabled());
    let state_mn = state.clone();
    master_notif_item.connect_toggled(move |_| {
        state_mn.toggle_notify_master();
    });
    menu.append(&master_notif_item);

    menu.append(&gtk::SeparatorMenuItem::new());

    // ── 6. Redémarrer & Quitter ─────────────────────────────────────────
    // Entrée volontairement active (texte noir dans XFCE) : les informations
    // désactivées de l'en-tête sont trop faciles à prendre pour du texte masqué.
    let version_item = gtk::MenuItem::with_label(&version_label());
    menu.append(&version_item);

    menu.append(&gtk::SeparatorMenuItem::new());

    let restart_item = gtk::MenuItem::with_label("Redémarrer PoolSync");
    restart_item.connect_activate(|_| {
        std::thread::spawn(|| {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "restart", "poolsync-agent.service"])
                .status();
        });
    });
    menu.append(&restart_item);

    let quit_item = gtk::MenuItem::with_label("Quitter PoolSync");
    quit_item.connect_activate(|_| {
        std::thread::spawn(|| {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "stop", "poolsync-agent.service"])
                .status();
        });
    });
    menu.append(&quit_item);

    menu.show_all();
    history_menu.show_all();

    // GtkStatusIcon est déprécié par GTK mais reste le seul protocole pris en
    // charge par XFCE qui distingue réellement activate (gauche) de
    // popup-menu (droite). gtk-rs ne génère plus son wrapper : on conserve
    // l'objet GObject et on branche ses deux signaux par leur nom stable.
    let icon_file = CString::new(icon_path.to_string_lossy().as_bytes())?;
    let raw_status = unsafe { gtk::ffi::gtk_status_icon_new_from_file(icon_file.as_ptr()) };
    if raw_status.is_null() {
        anyhow::bail!("création de l'icône systray GTK impossible");
    }
    let status_icon: glib::Object =
        unsafe { FromGlibPtrFull::from_glib_full(raw_status as *mut glib::gobject_ffi::GObject) };
    unsafe { gtk::ffi::gtk_status_icon_set_visible(raw_status, glib::ffi::GTRUE) };
    apply_tray_title(&status_icon, &state);

    let history_popup = history_menu.clone();
    status_icon.connect_local("activate", false, move |_| {
        history_popup.popup_easy(1, gtk::current_event_time());
        None
    });

    let options_popup = menu.clone();
    status_icon.connect_local("popup-menu", false, move |values| {
        let button = values.get(1).and_then(|v| v.get::<u32>().ok()).unwrap_or(3);
        let at = values
            .get(2)
            .and_then(|v| v.get::<u32>().ok())
            .unwrap_or_else(gtk::current_event_time);
        options_popup.popup_easy(button, at);
        None
    });

    ITEM_SLOTS.with(|s| *s.borrow_mut() = slots.iter().map(|(i, _)| i.clone()).collect());
    let slots = Rc::new(slots);
    refresh_item_labels(&slots, &state);

    let items_slots = slots.clone();
    let state_tick = state.clone();
    let status_icon_tick = status_icon.clone();
    let status_item_tick = status_item.clone();
    let hub_item_tick = hub_item.clone();
    let master_item_tick = master_item.clone();
    let mut last_revision = state.tray_history_revision();
    let mut last_status_revision = state.tray_status_revision();

    glib::timeout_add_local(std::time::Duration::from_millis(2500), move || {
        if away_item.is_active() != state_tick.pool_away() {
            // Reflect a CLI command without generating a second user command.
            // The live state can change concurrently with this GTK refresh.
            away_item.block_signal(&away_handler);
            away_item.set_active(state_tick.pool_away());
            away_item.unblock_signal(&away_handler);
        }
        let revision = state_tick.tray_history_revision();
        if revision != last_revision {
            last_revision = revision;
            refresh_item_labels(&items_slots, &state_tick);
        }
        let status_rev = state_tick.tray_status_revision();
        if status_rev != last_status_revision {
            last_status_revision = status_rev;
            apply_tray_title(&status_icon_tick, &state_tick);
            status_item_tick.set_label(&format!("Statut : {}", state_tick.status_line()));
            hub_item_tick.set_label(&format!("Hub : {}", state_tick.hub_display()));
            if let Some(ref mi) = master_item_tick {
                mi.set_label(&format!("Maître KVM : {}", state_tick.master_node()));
            }
        }
        glib::ControlFlow::Continue
    });

    tracing::info!(
        "systray ready — clic gauche=buffer, clic droit=options ({})",
        state.config.node
    );
    if show_window {
        crate::config_window::show(state.clone());
    }
    let window_state = state.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
        if window_state.take_config_window_request() {
            crate::config_window::show(window_state.clone());
        }
        glib::ControlFlow::Continue
    });
    gtk::main();
    Ok(())
}

/// Nettoie un libellé destiné au menu systray.
///
/// Le presse-papiers peut contenir n'importe quoi : caractères de contrôle, texte
/// vide, séquences trop longues. Passés tels quels au
/// panel via dbusmenu, ils font segfauter son rendu Pango.
fn safe_menu_label(raw: &str) -> String {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "(vide)".to_string()
    } else {
        cleaned.chars().take(80).collect()
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TraySurface {
    Buffer,
    Options,
    Ignore,
}

#[cfg(test)]
fn tray_surface_for_button(button: u32) -> TraySurface {
    match button {
        1 => TraySurface::Buffer,
        3 => TraySurface::Options,
        _ => TraySurface::Ignore,
    }
}

/// Met à jour les libellés des entrées de copier-coller, sans créer ni détruire
/// aucun widget : c'est ce qui rend le rafraîchissement sûr alors que le menu est
/// affiché par le panel.
type ItemSlot = (gtk::MenuItem, Rc<RefCell<Option<String>>>);

fn refresh_item_labels(slots: &[ItemSlot], state: &Arc<AgentState>) {
    let items = clipboard_history::fetch_history_tray(state).unwrap_or_default();

    for (idx, (item, hash_slot)) in slots.iter().enumerate() {
        match items.get(idx) {
            Some(entry) => {
                let text = safe_menu_label(&clipboard_history::tray_label(entry));
                if let Some(label) = item.child().and_then(|c| c.downcast::<gtk::Label>().ok()) {
                    let cur = label.text().to_string();
                    if cur != text {
                        label.set_text(&text);
                        label.set_xalign(0.0);
                    }
                }
                *hash_slot.borrow_mut() = Some(entry.hash.clone());
                item.set_sensitive(true);
                item.show();
            }
            None => {
                // Emplacement inutilisé : masqué, jamais supprimé.
                *hash_slot.borrow_mut() = None;
                item.hide();
            }
        }
    }

    if items.is_empty() {
        if let Some((item, _)) = slots.first() {
            if let Some(label) = item.child().and_then(|c| c.downcast::<gtk::Label>().ok()) {
                label.set_text("(aucun copier-coller récent)");
            }
            item.set_sensitive(false);
            item.show();
        }
    }
}

/// Installe l'icône dans le thème `hicolor` de l'utilisateur.
///
/// Un `IconThemePath` pointant vers un dossier plat ne suffit pas : GTK y cherche
/// une arborescence de thème (`<taille>/apps/…` + `index.theme`) et n'y trouve
/// rien, donc le panel enregistre l'item sans jamais dessiner d'icône. On écrit
/// donc dans `~/.local/share/icons/hicolor/<taille>/apps/` puis on rafraîchit le
/// cache, seul chemin que le panel résout de façon fiable après un redémarrage.
fn install_icon_png() -> Result<(PathBuf, PathBuf)> {
    const ICON: &[u8] = include_bytes!("../icons/poolsync-tray.png");
    const SIZES: [&str; 5] = ["16x16", "22x22", "24x24", "48x48", "scalable"];

    let home = std::env::var("HOME").context("HOME absent")?;
    let theme_dir = PathBuf::from(&home).join(".local/share/icons/hicolor");

    let mut installed = None;
    for size in SIZES {
        let dir = theme_dir.join(size).join("apps");
        if fs::create_dir_all(&dir).is_err() {
            continue;
        }
        let path = dir.join("poolsync-tray.png");
        if fs::write(&path, ICON).is_ok() && installed.is_none() {
            installed = Some(path);
        }
    }
    let icon_path = installed.context("écriture icône hicolor")?;

    // Sans cache à jour, le panel peut ignorer une icône fraîchement installée.
    let _ = std::process::Command::new("gtk-update-icon-cache")
        .args(["-f", "-t", "-q"])
        .arg(&theme_dir)
        .status();

    Ok((theme_dir, icon_path))
}

fn clip_sync_label(enabled: bool) -> String {
    if enabled {
        "Presse-papiers PoolSync : activé".into()
    } else {
        "Presse-papiers PoolSync : désactivé".into()
    }
}

fn version_label() -> String {
    format!(
        "Version : PoolSync v{} — build {}",
        env!("CARGO_PKG_VERSION"),
        env!("POOLSYNC_BUILD_TIME")
    )
}

fn apply_tray_title(status_icon: &glib::Object, state: &AgentState) {
    let title = if state.local_poolsync_active() {
        format!("PoolSync — {} — {}", state.config.node, state.status_line())
    } else {
        format!("PoolSync — {} — OFF", state.config.node)
    };
    if let Ok(title) = CString::new(title) {
        unsafe {
            gtk::ffi::gtk_status_icon_set_tooltip_text(
                status_icon.as_ptr() as *mut gtk::ffi::GtkStatusIcon,
                title.as_ptr(),
            );
        }
    }
}

/// Bascule le sync clipboard sur ce nœud + notification ; OFF = clipboard système seul.
fn apply_clipboard_sync_toggle(state: &AgentState) -> bool {
    let on = state.toggle_clipboard_sync();
    let node = &state.config.node;
    if on {
        notify_util::notify_local(
            "Presse-papiers PoolSync activé",
            &format!("Copier-coller synchronisé avec le pool sur {node}."),
        );
    } else {
        state.mark_hub_clipboard_applied();
        crate::clipboard_gtk::clear_image_claim();
        crate::clipboard_gtk::release_ownership();
        notify_util::notify_local(
            "Sync presse-papiers désactivé",
            &format!(
                "PoolSync ne touche plus au presse-papiers sur {node}.\n\
                 Copier-coller = celui de la session (XFCE / xrdp)."
            ),
        );
    }
    on
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_clicks_are_strictly_separated() {
        assert_eq!(tray_surface_for_button(1), TraySurface::Buffer);
        assert_eq!(tray_surface_for_button(3), TraySurface::Options);
        assert_eq!(tray_surface_for_button(2), TraySurface::Ignore);
    }

    #[test]
    fn clipboard_labels_are_safe_for_panel_rendering() {
        assert_eq!(safe_menu_label("a\n\tb\0c"), "abc");
        assert_eq!(safe_menu_label("\n\t"), "(vide)");
        assert_eq!(safe_menu_label(&"x".repeat(100)).chars().count(), 80);
    }

    #[test]
    fn clipboard_toggle_label_is_explicit() {
        assert!(clip_sync_label(true).contains("activé"));
        assert!(clip_sync_label(false).contains("désactivé"));
    }

    #[test]
    fn version_label_identifies_the_release_and_build() {
        let label = version_label();
        assert!(label.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))));
        assert!(label.contains("build"));
        assert!(label.contains("UTC"));
    }
}
