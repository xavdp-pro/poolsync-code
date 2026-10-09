//! Fenêtre GTK native : configuration du pool servie par le hub + agent local.
//!
//! Chaque agent peut ouvrir cette fenêtre depuis le systray pour :
//!  - consulter/éditer la topologie du hub (voisins, KVM par nœud, écran) via
//!    l'API HTTP du hub (`GET`/`POST /api/topology`), avec le token de l'agent ;
//!  - éditer sa config locale `~/.config/poolsync/agent.toml` et se redémarrer ;
//!  - lire ses logs (`journalctl`).
//!
//! Remplace le dashboard web pour la configuration au quotidien.

use crate::logs_viewer::fetch_journal_logs;
use crate::state::AgentState;
use crate::topology_mosaic::TopologyMosaic;
use anyhow::{anyhow, Result};
use gtk::prelude::*;
use gtk::{
    Align, Box as GtkBox, Button, CheckButton, ComboBoxText, Entry, Expander, Grid, Label,
    Notebook, Orientation, ScrolledWindow, SpinButton, TextView, Window,
};
use poolsync_core::{
    infer_neighbors, AgentConfig, AgentMode, Direction, PoolTopology, TopologyNode,
    DEFAULT_EDGE_TOLERANCE_PX,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const DIRS: [&str; 4] = ["left", "right", "up", "down"];

thread_local! {
    static OPEN_WINDOW: RefCell<Option<Rc<ConfigWindow>>> = const { RefCell::new(None) };
}

/// Ouvre (ou ré-affiche) la fenêtre de configuration. À appeler sur le thread GTK.
pub fn show(state: Arc<AgentState>) {
    crate::crashlog::set_context("config_window::show");
    OPEN_WINDOW.with(|slot| {
        if let Some(existing) = slot.borrow().as_ref() {
            existing.window.present();
            existing.load();
            return;
        }
        let win = ConfigWindow::new(state);
        win.load();
        *slot.borrow_mut() = Some(win);
    });
}

/// Widgets d'édition d'un nœud dans la topologie (position conservée telle quelle).
struct NodeRow {
    id: String,
    x: i32,
    y: i32,
    kvm: CheckButton,
    width: SpinButton,
    height: SpinButton,
    neighbors: Vec<(String, Label)>,
}

/// Formulaire d'édition de l'agent.toml local.
struct AgentForm {
    hub_url: Entry,
    token: Entry,
    mode: ComboBoxText,
    kvm: ComboBoxText,
    edge_px: SpinButton,
    clip_poll: SpinButton,
    input_poll: SpinButton,
    tray_history: SpinButton,
    pause_rdp: CheckButton,
    keep_formatting: CheckButton,
    history_double_click_paste: CheckButton,
    display: Entry,
    status: Label,
}

struct ConfigWindow {
    window: Window,
    /// Maintient en vie la page presse-papiers (timers de rafraîchissement inclus).
    #[allow(dead_code)]
    clip_handle: Rc<crate::clipboard_history::HistoryWindow>,
    state: Arc<AgentState>,
    mosaic: Rc<TopologyMosaic>,
    nodes_box: GtkBox,
    status: Label,
    rows: RefCell<Vec<NodeRow>>,
    loaded_topology: RefCell<Option<PoolTopology>>,
    agent_form: AgentForm,
    logs_view: TextView,
}

impl ConfigWindow {
    fn new(state: Arc<AgentState>) -> Rc<Self> {
        let win = Rc::new_cyclic(|weak: &std::rc::Weak<ConfigWindow>| {
            let window = Window::builder()
                .title(format!("PoolSync — {}", state.config.node))
                .default_width(1000)
                .default_height(680)
                .build();

            let notebook = Notebook::new();

            // --- Onglet 1 : presse-papiers (l'usage quotidien) --------------
            let (clip_page, clip_handle) = crate::clipboard_history::build_page(state.clone());
            notebook.append_page(&clip_page, Some(&Label::new(Some("Presse-papiers"))));

            // --- Onglet 2 : écrans (drag & drop des positions relatives) ----
            let (config_page, mosaic, nodes_box, status) = build_topology_page(weak);
            notebook.append_page(&config_page, Some(&Label::new(Some("Écrans du pool"))));

            // --- Onglet 3 : agent.toml local --------------------------------
            let (agent_page, agent_form) = build_agent_page(&state, weak);
            notebook.append_page(&agent_page, Some(&Label::new(Some("Agent local"))));

            // --- Onglet 4 : logs --------------------------------------------
            let (logs_page, logs_view) = build_logs_page(weak);
            notebook.append_page(&logs_page, Some(&Label::new(Some("Logs"))));

            window.add(&notebook);
            window.connect_destroy(|_| {
                tracing::info!("native configuration window closed");
                OPEN_WINDOW.with(|slot| *slot.borrow_mut() = None);
            });

            ConfigWindow {
                window,
                state: state.clone(),
                clip_handle,
                mosaic,
                nodes_box,
                status,
                rows: RefCell::new(Vec::new()),
                loaded_topology: RefCell::new(None),
                agent_form,
                logs_view,
            }
        });

        win.fill_agent_form();
        win.refresh_logs();
        win.window.show_all();
        win
    }

    /// Récupère la topologie du hub et (re)construit l'éditeur.
    fn load(&self) {
        for child in self.nodes_box.children() {
            self.nodes_box.remove(&child);
        }
        self.rows.borrow_mut().clear();

        let topo = match fetch_topology(&self.state) {
            Ok(topo) => topo,
            Err(err) => {
                self.set_status(&format!("Disposition indisponible : {err}"), true);
                return;
            }
        };
        *self.loaded_topology.borrow_mut() = Some(topo.clone());

        if topo.nodes.is_empty() {
            let empty = Label::new(Some(
                "Aucun ordinateur dans la disposition du pool.\n\
                 Ouvrez les agents autorisés pour les retrouver ici.",
            ));
            empty.set_halign(Align::Start);
            self.nodes_box.pack_start(&empty, false, false, 0);
            self.nodes_box.show_all();
            self.set_status("Topologie vide", false);
            return;
        }

        let mut ids: Vec<String> = topo.nodes.keys().cloned().collect();
        ids.sort_by(|a, b| {
            let na = &topo.nodes[a];
            let nb = &topo.nodes[b];
            na.x.cmp(&nb.x).then_with(|| a.cmp(b))
        });

        for id in &ids {
            let node = &topo.nodes[id];
            let row = self.build_node_frame(id, node);
            self.rows.borrow_mut().push(row);
        }
        self.nodes_box.show_all();
        self.mosaic.set_local_node(&self.state.config.node);
        self.mosaic.rebuild(&topo);
        self.set_status(&format!("{} ordinateur(s) dans le pool", ids.len()), false);
    }

    /// Applique une topologie recalculée depuis la mosaïque (positions + voisins).
    fn apply_topology_layout(&self, topo: PoolTopology) {
        {
            let mut rows = self.rows.borrow_mut();
            for row in rows.iter_mut() {
                if let Some(n) = topo.nodes.get(&row.id) {
                    row.x = n.x;
                    row.y = n.y;
                    for (dir, label) in &row.neighbors {
                        let active = n.neighbors.get(dir).map(|s| s.as_str()).unwrap_or("");
                        label.set_text(if active.is_empty() { "—" } else { active });
                    }
                }
            }
        }
        *self.loaded_topology.borrow_mut() = Some(topo.clone());
        self.mosaic.rebuild(&topo);
        self.set_status("Mosaïque — voisins recalculés depuis les positions", false);
    }

    fn collect_topology_from_rows(&self) -> PoolTopology {
        // The editor's saved geometry includes offline devices and monitor
        // offsets. The agent's runtime graph may exclude them or be empty.
        let existing = self.loaded_topology.borrow();
        let mut nodes = HashMap::new();
        for row in self.rows.borrow().iter() {
            let prev = existing.as_ref().and_then(|t| t.nodes.get(&row.id));
            nodes.insert(
                row.id.clone(),
                TopologyNode {
                    x: row.x,
                    y: row.y,
                    width: row.width.value_as_int() as u32,
                    height: row.height.value_as_int() as u32,
                    kvm_enabled: row.kvm.is_active(),
                    neighbors: prev.map(|n| n.neighbors.clone()).unwrap_or_default(),
                    monitor_x: prev.map(|n| n.monitor_x).unwrap_or(0),
                    monitor_y: prev.map(|n| n.monitor_y).unwrap_or(0),
                    desktop_x: prev.map(|n| n.desktop_x).unwrap_or(0),
                    desktop_y: prev.map(|n| n.desktop_y).unwrap_or(0),
                    desktop_width: prev.map(|n| n.desktop_width).unwrap_or(0),
                    desktop_height: prev.map(|n| n.desktop_height).unwrap_or(0),
                },
            );
        }
        PoolTopology { nodes }
    }

    fn recalc_neighbors(&self) {
        let topo = infer_neighbors(
            &self.collect_topology_from_rows(),
            DEFAULT_EDGE_TOLERANCE_PX,
        );
        self.apply_topology_layout(topo);
    }

    /// Construit le cadre d'un nœud et renvoie les widgets pour lecture au save.
    fn build_node_frame(&self, id: &str, node: &TopologyNode) -> NodeRow {
        // Keep the map and every device visible; expand only the settings
        // currently needed instead of stacking every machine's form.
        let frame = Expander::new(Some(id));
        let grid = Grid::new();
        grid.set_row_spacing(6);
        grid.set_column_spacing(10);
        grid.set_margin_start(10);
        grid.set_margin_end(10);
        grid.set_margin_top(8);
        grid.set_margin_bottom(8);

        let kvm = CheckButton::with_label("KVM actif (clavier / souris)");
        kvm.set_active(node.kvm_enabled);
        grid.attach(&kvm, 0, 0, 4, 1);

        let screen_label = Label::new(Some("Écran"));
        screen_label.set_halign(Align::Start);
        grid.attach(&screen_label, 0, 1, 1, 1);
        let width = SpinButton::with_range(1.0, 32768.0, 1.0);
        width.set_value(node.width as f64);
        let height = SpinButton::with_range(1.0, 32768.0, 1.0);
        height.set_value(node.height as f64);
        grid.attach(&width, 1, 1, 1, 1);
        grid.attach(&Label::new(Some("×")), 2, 1, 1, 1);
        grid.attach(&height, 3, 1, 1, 1);

        let mut neighbors = Vec::new();
        for (i, dir) in DIRS.iter().enumerate() {
            let dir_label = Label::new(Some(dir_fr(dir)));
            dir_label.set_halign(Align::Start);
            grid.attach(&dir_label, 0, 2 + i as i32, 1, 1);

            let current = node.neighbors.get(*dir).map(String::as_str).unwrap_or("");
            let label = Label::new(Some(if current.is_empty() { "—" } else { current }));
            label.set_halign(Align::Start);
            label.set_tooltip_text(Some(
                "Voisin calculé depuis la position des écrans dans la mosaïque.",
            ));
            grid.attach(&label, 1, 2 + i as i32, 3, 1);
            neighbors.push((dir.to_string(), label));
        }

        frame.add(&grid);
        self.nodes_box.pack_start(&frame, false, false, 0);

        NodeRow {
            id: id.to_string(),
            x: node.x,
            y: node.y,
            kvm,
            width,
            height,
            neighbors,
        }
    }

    /// Reconstruit la topologie depuis les widgets et l'envoie au hub.
    fn save(&self) {
        let topo = infer_neighbors(
            &self.collect_topology_from_rows(),
            DEFAULT_EDGE_TOLERANCE_PX,
        );
        self.apply_topology_layout(topo.clone());

        if topo.nodes.is_empty() {
            self.set_status("Rien à enregistrer", true);
            return;
        }

        match post_topology(&self.state, &topo) {
            Ok(()) => self.set_status(
                if self.state.config.hubless {
                    "Topologie enregistrée ; diffusion directe en cours ✓"
                } else {
                    "Topologie enregistrée et diffusée aux agents ✓"
                },
                false,
            ),
            Err(err) => self.set_status(&format!("Échec enregistrement : {err}"), true),
        }
    }

    /// Remplit le formulaire agent.toml depuis la config chargée au démarrage.
    fn fill_agent_form(&self) {
        let cfg = &self.state.config;
        let f = &self.agent_form;
        f.hub_url.set_text(&cfg.hub_url);
        f.token.set_text(&cfg.token);
        f.mode.set_active_id(Some(mode_id(cfg.mode)));
        f.kvm.set_active_id(Some(match cfg.kvm_enabled {
            None => "auto",
            Some(true) => "true",
            Some(false) => "false",
        }));
        f.edge_px.set_value(cfg.edge_px as f64);
        f.clip_poll.set_value(cfg.clipboard_poll_ms as f64);
        f.input_poll.set_value(cfg.input_poll_ms as f64);
        f.tray_history
            .set_value(cfg.tray_history_count.clamp(5, 50) as f64);
        f.pause_rdp.set_active(cfg.pause_clipboard_when_rdp);
        f.keep_formatting.set_active(self.state.keep_formatting());
        f.history_double_click_paste
            .set_active(self.state.history_double_click_paste());
        f.display
            .set_text(cfg.display.as_deref().unwrap_or_default());
    }

    /// Écrit l'agent.toml local depuis le formulaire.
    fn save_agent(&self) {
        let f = &self.agent_form;
        let mut cfg = self.state.config.clone();
        cfg.hub_url = f.hub_url.text().to_string();
        cfg.token = f.token.text().to_string();
        cfg.mode = match f.mode.active_id().as_deref() {
            Some("clipboard_only") => AgentMode::ClipboardOnly,
            _ => AgentMode::Full,
        };
        cfg.kvm_enabled = match f.kvm.active_id().as_deref() {
            Some("true") => Some(true),
            Some("false") => Some(false),
            _ => None,
        };
        cfg.edge_px = f.edge_px.value_as_int() as u32;
        cfg.clipboard_poll_ms = f.clip_poll.value_as_int() as u64;
        cfg.input_poll_ms = f.input_poll.value_as_int() as u64;
        cfg.tray_history_count = f.tray_history.value_as_int() as u32;
        cfg.pause_clipboard_when_rdp = f.pause_rdp.is_active();
        cfg.keep_formatting = f.keep_formatting.is_active();
        self.state.set_keep_formatting(cfg.keep_formatting);
        cfg.history_double_click_paste = f.history_double_click_paste.is_active();
        self.state
            .set_history_double_click_paste(cfg.history_double_click_paste);
        let disp = f.display.text().to_string();
        cfg.display = if disp.trim().is_empty() {
            None
        } else {
            Some(disp)
        };

        let path = &self.state.config_path;
        match std::fs::write(path, render_agent_toml(&cfg)) {
            Ok(()) => f.status.set_markup(&status_markup(
                &format!("Écrit dans {} — redémarrer pour appliquer", path.display()),
                false,
            )),
            Err(err) => f
                .status
                .set_markup(&status_markup(&format!("Échec écriture : {err}"), true)),
        }
    }

    /// Redémarre le service systemd user de l'agent (applique l'agent.toml).
    fn restart_agent(&self) {
        let runtime = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
        let res = std::process::Command::new("systemctl")
            .args(["--user", "restart", "poolsync-agent.service"])
            .env("XDG_RUNTIME_DIR", runtime)
            .spawn();
        match res {
            Ok(_) => self
                .agent_form
                .status
                .set_markup(&status_markup("Redémarrage de l'agent demandé…", false)),
            Err(err) => self
                .agent_form
                .status
                .set_markup(&status_markup(&format!("Échec redémarrage : {err}"), true)),
        }
    }

    fn refresh_logs(&self) {
        if let Some(buffer) = self.logs_view.buffer() {
            buffer.set_text(&fetch_journal_logs());
        }
    }

    fn set_status(&self, msg: &str, is_error: bool) {
        self.status.set_markup(&status_markup(msg, is_error));
    }
}

// --- Construction des pages (hors impl pour garder new() lisible) -----------

fn build_topology_page(
    weak: &std::rc::Weak<ConfigWindow>,
) -> (GtkBox, Rc<TopologyMosaic>, GtkBox, Label) {
    let page = GtkBox::new(Orientation::Vertical, 0);
    let toolbar = toolbar_box();
    let reload_btn = Button::with_label("Recharger");
    let recalc_btn = Button::with_label("Recalculer voisins");
    let save_btn = Button::with_label("Enregistrer dans le pool");
    let status = Label::new(None);
    status.set_halign(Align::Start);
    toolbar.pack_start(&reload_btn, false, false, 0);
    toolbar.pack_start(&recalc_btn, false, false, 0);
    toolbar.pack_start(&save_btn, false, false, 0);
    toolbar.pack_start(&status, false, false, 8);

    let w_mosaic = weak.clone();
    let mosaic = Rc::new(TopologyMosaic::new(Rc::new(move |topo| {
        if let Some(v) = w_mosaic.upgrade() {
            v.apply_topology_layout(topo);
        }
    })));

    let scrolled = scrolled_area();
    let nodes_box = GtkBox::new(Orientation::Vertical, 10);
    nodes_box.set_margin_top(4);
    nodes_box.set_margin_bottom(4);
    scrolled.add(&nodes_box);

    page.pack_start(&toolbar, false, false, 0);
    page.pack_start(mosaic.widget(), false, false, 8);
    page.pack_start(&scrolled, true, true, 0);

    let w = weak.clone();
    reload_btn.connect_clicked(move |_| {
        if let Some(v) = w.upgrade() {
            v.load();
        }
    });
    let w = weak.clone();
    recalc_btn.connect_clicked(move |_| {
        if let Some(v) = w.upgrade() {
            v.recalc_neighbors();
        }
    });
    let w = weak.clone();
    save_btn.connect_clicked(move |_| {
        if let Some(v) = w.upgrade() {
            v.save();
        }
    });

    (page, mosaic, nodes_box, status)
}

fn build_agent_page(state: &AgentState, weak: &std::rc::Weak<ConfigWindow>) -> (GtkBox, AgentForm) {
    let page = GtkBox::new(Orientation::Vertical, 0);

    let toolbar = toolbar_box();
    let save_btn = Button::with_label("Enregistrer agent.toml");
    let restart_btn = Button::with_label("Redémarrer l'agent");
    let status = Label::new(None);
    status.set_halign(Align::Start);
    toolbar.pack_start(&save_btn, false, false, 0);
    toolbar.pack_start(&restart_btn, false, false, 0);
    toolbar.pack_start(&status, false, false, 8);

    let grid = Grid::new();
    grid.set_row_spacing(8);
    grid.set_column_spacing(10);
    grid.set_margin_start(12);
    grid.set_margin_end(12);
    grid.set_margin_top(10);

    let node_label = Label::new(Some(&format!(
        "Nœud : {}  (non modifiable ici)",
        state.config.node
    )));
    node_label.set_halign(Align::Start);
    grid.attach(&node_label, 0, 0, 2, 1);

    let hub_url = Entry::new();
    let token = Entry::new();
    token.set_visibility(false);
    if state.config.hubless {
        let direct = Label::new(Some("Communication directe chiffrée — sans hub"));
        direct.set_halign(Align::Start);
        grid.attach(&direct, 0, 1, 2, 1);
    } else {
        attach_field(&grid, 1, "Hub URL", &hub_url);
        attach_field(&grid, 2, "Token", &token);
    }

    let mode = ComboBoxText::new();
    mode.append(Some("full"), "Complet (clip + KVM)");
    mode.append(Some("clipboard_only"), "Presse-papiers seul");
    attach_field(&grid, 3, "Mode", &mode);

    let kvm = ComboBoxText::new();
    kvm.append(Some("auto"), "Auto (selon mode)");
    kvm.append(Some("true"), "Activé");
    kvm.append(Some("false"), "Désactivé");
    attach_field(&grid, 4, "KVM", &kvm);

    let edge_px = SpinButton::with_range(0.0, 200.0, 1.0);
    attach_field(&grid, 5, "Bord (px)", &edge_px);
    let clip_poll = SpinButton::with_range(50.0, 5000.0, 10.0);
    attach_field(&grid, 6, "Poll presse-papiers (ms)", &clip_poll);
    let input_poll = SpinButton::with_range(1.0, 1000.0, 1.0);
    attach_field(&grid, 7, "Poll souris (ms)", &input_poll);

    let tray_history = SpinButton::with_range(5.0, 50.0, 1.0);
    attach_field(&grid, 8, "Entrées menu systray", &tray_history);

    let pause_rdp = CheckButton::with_label("Pause presse-papiers pendant RDP actif");
    grid.attach(&pause_rdp, 1, 9, 1, 1);

    let keep_formatting =
        CheckButton::with_label("Garder le formatage (HTML) — décoché = texte brut uniquement");
    keep_formatting.set_tooltip_text(Some(
        "Désactivé : copie seulement le texte visible (email dans un champ).\n\
         Activé : gras/styles pour LibreOffice. Chrome peut alors coller du HTML.",
    ));
    grid.attach(&keep_formatting, 1, 10, 1, 1);

    let history_double_click_paste =
        CheckButton::with_label("Double-clic historique → coller dans le presse-papiers");
    history_double_click_paste.set_tooltip_text(Some(
        "Désactivé (défaut) : double-clic sur une ligne = aperçu seulement.\n\
         Utilisez le bouton Coller ou le menu systray.",
    ));
    grid.attach(&history_double_click_paste, 1, 11, 1, 1);

    let display = Entry::new();
    display.set_placeholder_text(Some("ex. :10 (vide = auto)"));
    attach_field(&grid, 12, "Display X11", &display);

    page.pack_start(&toolbar, false, false, 0);
    page.pack_start(&grid, false, false, 0);

    let w = weak.clone();
    save_btn.connect_clicked(move |_| {
        if let Some(v) = w.upgrade() {
            v.save_agent();
        }
    });
    let w = weak.clone();
    restart_btn.connect_clicked(move |_| {
        if let Some(v) = w.upgrade() {
            v.restart_agent();
        }
    });

    let form = AgentForm {
        hub_url,
        token,
        mode,
        kvm,
        edge_px,
        clip_poll,
        input_poll,
        tray_history,
        pause_rdp,
        keep_formatting,
        history_double_click_paste,
        display,
        status,
    };
    (page, form)
}

fn build_logs_page(weak: &std::rc::Weak<ConfigWindow>) -> (GtkBox, TextView) {
    let page = GtkBox::new(Orientation::Vertical, 0);
    let toolbar = toolbar_box();
    let refresh = Button::with_label("Actualiser");
    toolbar.pack_start(&refresh, false, false, 0);

    let scrolled = scrolled_area();
    let view = TextView::builder()
        .editable(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::Word)
        .left_margin(8)
        .right_margin(8)
        .top_margin(6)
        .bottom_margin(6)
        .build();
    scrolled.add(&view);

    page.pack_start(&toolbar, false, false, 0);
    page.pack_start(&scrolled, true, true, 0);

    let w = weak.clone();
    refresh.connect_clicked(move |_| {
        if let Some(v) = w.upgrade() {
            v.refresh_logs();
        }
    });

    (page, view)
}

fn toolbar_box() -> GtkBox {
    let toolbar = GtkBox::new(Orientation::Horizontal, 6);
    toolbar.set_margin_start(8);
    toolbar.set_margin_end(8);
    toolbar.set_margin_top(8);
    toolbar.set_margin_bottom(4);
    toolbar
}

fn scrolled_area() -> ScrolledWindow {
    ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .margin_start(8)
        .margin_end(8)
        .margin_bottom(8)
        .build()
}

fn attach_field<W: IsA<gtk::Widget>>(grid: &Grid, row: i32, label: &str, widget: &W) {
    let l = Label::new(Some(label));
    l.set_halign(Align::Start);
    grid.attach(&l, 0, row, 1, 1);
    widget.set_hexpand(true);
    grid.attach(widget, 1, row, 1, 1);
}

fn dir_fr(dir: &str) -> &'static str {
    match dir {
        "left" => "← Gauche",
        "right" => "→ Droite",
        "up" => "↑ Haut",
        "down" => "↓ Bas",
        _ => "?",
    }
}

fn mode_id(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Full => "full",
        AgentMode::ClipboardOnly => "clipboard_only",
    }
}

fn status_markup(msg: &str, is_error: bool) -> String {
    let color = if is_error { "#b91c1c" } else { "#475569" };
    format!("<span foreground=\"{color}\">{}</span>", escape_markup(msg))
}

fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Sérialise l'agent.toml à la main : scalaires d'abord, puis tables (TOML valide).
fn render_agent_toml(cfg: &AgentConfig) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "node = {:?}", cfg.node);
    let _ = writeln!(s, "hubless = {}", cfg.hubless);
    let _ = writeln!(s, "hub_url = {:?}", cfg.hub_url);
    let _ = writeln!(s, "token = {:?}", cfg.token);
    if let Some(token) = &cfg.node_token {
        let _ = writeln!(s, "node_token = {token:?}");
    }
    if let Some(key) = &cfg.e2e_key {
        let _ = writeln!(s, "e2e_key = {key:?}");
    }
    let _ = writeln!(s, "mode = {:?}", mode_id(cfg.mode));
    if let Some(k) = cfg.kvm_enabled {
        let _ = writeln!(s, "kvm_enabled = {k}");
    }
    if let Some(c) = cfg.kvm_capture {
        let _ = writeln!(s, "kvm_capture = {c}");
    }
    let _ = writeln!(s, "edge_px = {}", cfg.edge_px);
    let _ = writeln!(s, "clipboard_poll_ms = {}", cfg.clipboard_poll_ms);
    let _ = writeln!(s, "input_poll_ms = {}", cfg.input_poll_ms);
    let _ = writeln!(s, "tray_history_count = {}", cfg.tray_history_count);
    let _ = writeln!(
        s,
        "pause_clipboard_when_rdp = {}",
        cfg.pause_clipboard_when_rdp
    );
    let _ = writeln!(s, "peer_listen_port = {}", cfg.peer_listen_port);
    if let Some(cert) = &cfg.peer_tls_cert {
        let _ = writeln!(s, "peer_tls_cert = {cert:?}");
    }
    if let Some(key) = &cfg.peer_tls_key {
        let _ = writeln!(s, "peer_tls_key = {key:?}");
    }
    let _ = writeln!(s, "peer_direct_clipboard = {}", cfg.peer_direct_clipboard);
    let _ = writeln!(s, "hub_clipboard = {}", cfg.hub_clipboard);
    let _ = writeln!(s, "keep_formatting = {}", cfg.keep_formatting);
    let _ = writeln!(
        s,
        "history_double_click_paste = {}",
        cfg.history_double_click_paste
    );
    if let Some(d) = &cfg.display {
        let _ = writeln!(s, "display = {d:?}");
    }
    if !cfg.peer_tokens.is_empty() {
        let _ = writeln!(s, "\n[peer_tokens]");
        for (node, token) in &cfg.peer_tokens {
            let _ = writeln!(s, "{node:?} = {token:?}");
        }
    }
    if !cfg.previous_peer_tokens.is_empty() {
        let _ = writeln!(s, "\n[previous_peer_tokens]");
        for (node, token) in &cfg.previous_peer_tokens {
            let _ = writeln!(s, "{node:?} = {token:?}");
        }
    }
    let _ = writeln!(s, "\n[screen]");
    let _ = writeln!(s, "width = {}", cfg.screen.width);
    let _ = writeln!(s, "height = {}", cfg.screen.height);
    for n in &cfg.neighbors {
        let _ = writeln!(s, "\n[[neighbors]]");
        let _ = writeln!(s, "direction = {:?}", dir_id(n.direction));
        let _ = writeln!(s, "node = {:?}", n.node);
        if let Some(url) = &n.peer_url {
            let _ = writeln!(s, "peer_url = {url:?}");
        }
        if let Some(url) = &n.peer_url_vpn {
            let _ = writeln!(s, "peer_url_vpn = {url:?}");
        }
        if let Some(token) = &n.auth_token {
            let _ = writeln!(s, "auth_token = {token:?}");
        }
        if let Some(token) = &n.previous_auth_token {
            let _ = writeln!(s, "previous_auth_token = {token:?}");
        }
    }
    s
}

fn dir_id(dir: Direction) -> &'static str {
    match dir {
        Direction::Left => "left",
        Direction::Right => "right",
        Direction::Up => "up",
        Direction::Down => "down",
    }
}

fn http_base(state: &AgentState) -> Result<String> {
    crate::network::hub_http_base(&state.config.hub_url)
}

fn fetch_topology(state: &AgentState) -> Result<PoolTopology> {
    if state.config.hubless {
        let mut saved = crate::hubless::load_layout(state).topology;
        if let Some(live) = state.topology() {
            for (name, mut node) in live.nodes {
                if let Some(existing) = saved.nodes.get(&name) {
                    // Absence is runtime state, not a saved capability change.
                    // Opening the editor during reconnect must keep the tile
                    // and must never persist an offline peer as clipboard-only.
                    node.kvm_enabled = existing.kvm_enabled;
                }
                saved.nodes.insert(name, node);
            }
        }
        return Ok(saved);
    }
    let url = format!("{}/api/topology", http_base(state)?);
    let body = ureq::get(&url)
        .timeout(HTTP_TIMEOUT)
        .set(
            "Authorization",
            &format!("Bearer {}", state.config.authentication_token()),
        )
        .set("X-PoolSync-Node", &state.config.node)
        .call()
        .map_err(|e| anyhow!("{e}"))?
        .into_string()?;
    Ok(serde_json::from_str(&body)?)
}

fn post_topology(state: &AgentState, topo: &PoolTopology) -> Result<()> {
    if state.config.hubless {
        return state.request_layout(topo.clone());
    }
    let url = format!("{}/api/topology", http_base(state)?);
    let body = serde_json::to_string(topo)?;
    ureq::post(&url)
        .timeout(HTTP_TIMEOUT)
        .set(
            "Authorization",
            &format!("Bearer {}", state.config.authentication_token()),
        )
        .set("X-PoolSync-Node", &state.config.node)
        .set("Content-Type", "application/json")
        .send_string(&body)
        .map_err(|e| anyhow!("{e}"))?;
    Ok(())
}
