//! Mosaïque drag-and-drop des écrans (style Barrier) pour la fenêtre GTK de config.

use gtk::prelude::*;
use gtk::{DrawingArea, EventBox, Fixed, Label, Orientation, ScrolledWindow};
use poolsync_core::{
    infer_neighbors, layout_scale, snap_position, PoolTopology, TopologyNode,
    DEFAULT_EDGE_TOLERANCE_PX, DEFAULT_SNAP_GRID_PX,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

const PAD: i32 = 24;
const MAX_CANVAS_W: f64 = 680.0;
const MAX_CANVAS_H: f64 = 320.0;

pub struct TopologyMosaic {
    root: gtk::Box,
    canvas: Fixed,
    scale: RefCell<f64>,
    origin: RefCell<(i32, i32)>,
    // Press, motion and release callbacks must observe the same positions.
    canvas_pos: Rc<RefCell<HashMap<String, (i32, i32)>>>,
    on_layout: Rc<dyn Fn(PoolTopology)>,
    /// Écran en cours de déplacement, pour le mettre en évidence au dessin.
    dragging: Rc<RefCell<Option<String>>>,
    /// Nom du nœud local, signalé sur sa vignette.
    local_node: RefCell<Option<String>>,
}

impl TopologyMosaic {
    pub fn new(on_layout: Rc<dyn Fn(PoolTopology)>) -> Self {
        let root = gtk::Box::new(Orientation::Vertical, 4);
        let hint = Label::new(None);
        hint.set_markup(
            "<small>Faites <b>glisser</b> une vignette pour placer l'écran. \
             Les voisins sont recalculés au relâchement, puis <b>Enregistrer</b>.</small>",
        );
        hint.set_halign(gtk::Align::Start);
        hint.set_margin_start(4);
        hint.set_margin_bottom(2);
        let scrolled = ScrolledWindow::builder()
            .vexpand(false)
            .hexpand(true)
            .height_request(340)
            .build();
        let canvas = Fixed::new();
        canvas.set_size_request(400, 200);
        scrolled.add(&canvas);
        root.pack_start(&hint, false, false, 0);
        root.pack_start(&scrolled, false, false, 0);

        TopologyMosaic {
            root,
            canvas,
            scale: RefCell::new(0.2),
            origin: RefCell::new((0, 0)),
            canvas_pos: Rc::new(RefCell::new(HashMap::new())),
            on_layout,
            dragging: Rc::new(RefCell::new(None)),
            local_node: RefCell::new(None),
        }
    }

    /// Nom du nœud local, pour le signaler sur sa vignette.
    pub fn set_local_node(&self, node: &str) {
        *self.local_node.borrow_mut() = Some(node.to_string());
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn rebuild(&self, full_topo: &PoolTopology) {
        for child in self.canvas.children() {
            self.canvas.remove(&child);
        }
        self.canvas_pos.borrow_mut().clear();

        // Seuls les nœuds avec KVM actif figurent sur la mosaïque : les nœuds
        // clip-only (session RDP, etc.) ne participent pas à la bascule
        // clavier/souris, les positionner n'aurait aucun effet.
        let kvm_only = PoolTopology {
            nodes: full_topo
                .nodes
                .iter()
                .filter(|(_, n)| n.kvm_enabled)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        };
        let topo = &kvm_only;

        if topo.nodes.is_empty() {
            return;
        }

        let origin = (
            topo.nodes.values().map(|n| n.x).min().unwrap_or(0),
            topo.nodes.values().map(|n| n.y).min().unwrap_or(0),
        );
        *self.origin.borrow_mut() = origin;
        let scale = layout_scale(&topo.nodes, MAX_CANVAS_W, MAX_CANVAS_H);
        *self.scale.borrow_mut() = scale;

        let mut max_x = 0i32;
        let mut max_y = 0i32;
        for n in topo.nodes.values() {
            max_x = max_x.max(n.x + n.width as i32);
            max_y = max_y.max(n.y + n.height as i32);
        }
        let cw = ((max_x - origin.0) as f64 * scale) as i32 + PAD * 2;
        let ch = ((max_y - origin.1) as f64 * scale) as i32 + PAD * 2;
        self.canvas.set_size_request(cw.max(320), ch.max(160));

        let mut rendered = topo.clone();
        for node in rendered.nodes.values_mut() {
            node.x -= origin.0;
            node.y -= origin.1;
        }
        let lines = connection_lines(&rendered, scale);
        if !lines.is_empty() {
            let area = DrawingArea::new();
            area.set_size_request(cw, ch);
            // La zone de dessin couvre tout le canvas et serait posée *au-dessus*
            // des écrans : sans ce masque d'entrée vide elle avale les clics et le
            // drag ne démarre jamais. On la rend transparente aux événements.
            area.set_sensitive(false);
            let lines_rc = Rc::new(lines);
            area.connect_draw(move |_area, cr| {
                cr.set_source_rgb(0.45, 0.48, 0.95);
                cr.set_line_width(2.0);
                for ln in lines_rc.iter() {
                    cr.move_to(ln.0, ln.1);
                    cr.line_to(ln.2, ln.3);
                    cr.stroke().ok();
                }
                gtk::glib::Propagation::Stop
            });
            self.canvas.put(&area, 0, 0);
            area.show();
        }

        let mut ids: Vec<_> = topo.nodes.keys().cloned().collect();
        ids.sort_by(|a, b| {
            let na = &topo.nodes[a];
            let nb = &topo.nodes[b];
            na.x.cmp(&nb.x).then_with(|| a.cmp(b))
        });

        for id in ids {
            let node = topo.nodes.get(&id).expect("node");
            self.add_screen(&id, node, full_topo.clone());
        }
        self.canvas.show_all();
    }

    fn add_screen(&self, id: &str, node: &TopologyNode, topo: PoolTopology) {
        let scale = *self.scale.borrow();
        let w = (node.width as f64 * scale).max(64.0) as i32;
        let h = (node.height as f64 * scale).max(40.0) as i32;
        let origin = *self.origin.borrow();
        let px = PAD + ((node.x - origin.0) as f64 * scale) as i32;
        let py = PAD + ((node.y - origin.1) as f64 * scale) as i32;

        self.canvas_pos
            .borrow_mut()
            .insert(id.to_string(), (px, py));

        let event_box = EventBox::new();
        event_box.set_size_request(w, h);
        // Sans fenêtre propre placée au-dessus du Frame et des Label, ce sont eux
        // qui reçoivent le clic et le drag ne démarre pas.
        event_box.set_visible_window(true);
        event_box.set_above_child(true);
        // Vignette dessinée plutôt qu'un Frame GTK : coins arrondis, teinte propre
        // à la machine et liseré plus marqué pendant le déplacement, pour qu'on
        // voie ce qu'on déplace.
        let tile = DrawingArea::new();
        tile.set_size_request(w, h);
        let (hue_r, hue_g, hue_b) = node_tint(id);
        let label = id.to_string();
        let dims = format!("{} × {}", node.width, node.height);
        let is_local = self.local_node.borrow().as_deref() == Some(id);
        let dragging = self.dragging.clone();
        let id_draw = id.to_string();
        tile.connect_draw(move |area, cr| {
            let w = area.allocated_width() as f64;
            let h = area.allocated_height() as f64;
            let active = dragging.borrow().as_deref() == Some(id_draw.as_str());
            rounded_rect(cr, 1.5, 1.5, w - 3.0, h - 3.0, 8.0);
            cr.set_source_rgba(hue_r, hue_g, hue_b, if active { 0.38 } else { 0.20 });
            cr.fill_preserve().ok();
            if active {
                cr.set_source_rgb(hue_r * 0.7, hue_g * 0.7, hue_b * 0.7);
                cr.set_line_width(2.5);
            } else {
                cr.set_source_rgba(hue_r * 0.6, hue_g * 0.6, hue_b * 0.6, 0.85);
                cr.set_line_width(1.4);
            }
            cr.stroke().ok();

            cr.set_source_rgb(0.13, 0.14, 0.18);
            cr.select_font_face(
                "Sans",
                gtk::cairo::FontSlant::Normal,
                gtk::cairo::FontWeight::Bold,
            );
            cr.set_font_size(13.0);
            let te = cr.text_extents(&label).ok();
            if let Some(te) = te {
                cr.move_to((w - te.width()) / 2.0, h / 2.0 - 2.0);
                cr.show_text(&label).ok();
            }
            cr.select_font_face(
                "Sans",
                gtk::cairo::FontSlant::Normal,
                gtk::cairo::FontWeight::Normal,
            );
            cr.set_font_size(10.0);
            cr.set_source_rgba(0.13, 0.14, 0.18, 0.65);
            if let Ok(te) = cr.text_extents(&dims) {
                cr.move_to((w - te.width()) / 2.0, h / 2.0 + 14.0);
                cr.show_text(&dims).ok();
            }
            if is_local {
                cr.set_source_rgba(hue_r * 0.6, hue_g * 0.6, hue_b * 0.6, 0.9);
                cr.set_font_size(9.0);
                if let Ok(te) = cr.text_extents("cette machine") {
                    cr.move_to((w - te.width()) / 2.0, h - 8.0);
                    cr.show_text("cette machine").ok();
                }
            }
            gtk::glib::Propagation::Stop
        });
        event_box.add(&tile);

        self.canvas.put(&event_box, px, py);
        event_box.show_all();
        self.wire_drag(id, &event_box, topo);
    }

    fn wire_drag(&self, id: &str, widget: &EventBox, topo: PoolTopology) {
        widget.add_events(
            gtk::gdk::EventMask::BUTTON_PRESS_MASK
                | gtk::gdk::EventMask::BUTTON_RELEASE_MASK
                | gtk::gdk::EventMask::BUTTON_MOTION_MASK
                | gtk::gdk::EventMask::POINTER_MOTION_MASK
                | gtk::gdk::EventMask::ENTER_NOTIFY_MASK
                | gtk::gdk::EventMask::LEAVE_NOTIFY_MASK,
        );

        let canvas = self.canvas.clone();
        let scale = *self.scale.borrow();
        let on_layout = self.on_layout.clone();
        let positions = self.canvas_pos.clone();
        let drag = Rc::new(RefCell::new(None::<(f64, f64, i32, i32)>));
        let id = id.to_string();

        let d0 = drag.clone();
        let id0 = id.clone();
        let pos0 = positions.clone();
        let dragging0 = self.dragging.clone();
        widget.connect_button_press_event(move |w, event| {
            if event.button() != 1 {
                return gtk::glib::Propagation::Proceed;
            }
            let (ox, oy) = pos0.borrow().get(&id0).copied().unwrap_or((0, 0));
            *d0.borrow_mut() = Some((event.root().0, event.root().1, ox, oy));
            *dragging0.borrow_mut() = Some(id0.clone());
            set_cursor(w, "grabbing");
            w.queue_draw();
            w.grab_add();
            gtk::glib::Propagation::Stop
        });

        // Curseur « main » au survol : sans ce signal rien n'indique que la
        // vignette se déplace.
        widget.connect_enter_notify_event(move |w, _| {
            set_cursor(w, "grab");
            gtk::glib::Propagation::Proceed
        });

        let d1 = drag.clone();
        let c1 = canvas.clone();
        let w1 = widget.clone();
        let id1 = id.clone();
        let pos1 = positions.clone();
        widget.connect_motion_notify_event(move |_w, event| {
            let Some((rx0, ry0, ox, oy)) = *d1.borrow() else {
                return gtk::glib::Propagation::Proceed;
            };
            let dx = (event.root().0 - rx0) as i32;
            let dy = (event.root().1 - ry0) as i32;
            let nx = ox + dx;
            let ny = oy + dy;
            c1.move_(&w1, nx, ny);
            pos1.borrow_mut().insert(id1.clone(), (nx, ny));
            gtk::glib::Propagation::Stop
        });

        let d2 = drag;
        let dragging2 = self.dragging.clone();
        let id2 = id.clone();
        let pos2 = positions.clone();
        let on_layout2 = on_layout.clone();
        let scale2 = scale;
        let origin = *self.origin.borrow();
        let topo2 = topo.clone();
        widget.connect_button_release_event(move |w, event| {
            if event.button() != 1 {
                return gtk::glib::Propagation::Proceed;
            }
            w.grab_remove();
            set_cursor(w, "grab");
            if d2.borrow().is_none() {
                return gtk::glib::Propagation::Stop;
            }
            *d2.borrow_mut() = None;
            *dragging2.borrow_mut() = None;
            w.queue_draw();

            let (alloc_x, alloc_y) = pos2.borrow().get(&id2).copied().unwrap_or((0, 0));
            let tx = ((alloc_x - PAD) as f64 / scale2).round() as i32;
            let ty = ((alloc_y - PAD) as f64 / scale2).round() as i32;
            let (sx, sy) = snap_position(tx + origin.0, ty + origin.1, DEFAULT_SNAP_GRID_PX);

            let mut nodes = topo2.nodes.clone();
            if let Some(n) = nodes.get_mut(&id2) {
                n.x = sx;
                n.y = sy;
            }
            let inferred = infer_neighbors(&PoolTopology { nodes }, DEFAULT_EDGE_TOLERANCE_PX);
            on_layout2(inferred);
            gtk::glib::Propagation::Stop
        });
    }
}

/// Tracé d'un rectangle à coins arrondis.
fn rounded_rect(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::PI;
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}

/// Teinte stable dérivée du nom : chaque machine garde la même couleur d'une
/// ouverture à l'autre, sans table de correspondance à maintenir.
fn node_tint(id: &str) -> (f64, f64, f64) {
    let mut h: u32 = 2166136261;
    for b in id.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(16777619);
    }
    // Palette fixe : des teintes lisibles sur fond clair comme sombre.
    const PALETTE: [(f64, f64, f64); 6] = [
        (0.36, 0.52, 0.94),
        (0.30, 0.72, 0.55),
        (0.90, 0.60, 0.25),
        (0.72, 0.45, 0.88),
        (0.92, 0.44, 0.50),
        (0.28, 0.70, 0.80),
    ];
    PALETTE[(h % PALETTE.len() as u32) as usize]
}

/// Curseur de la fenêtre du widget ("grab" / "grabbing").
fn set_cursor(w: &EventBox, name: &str) {
    if let Some(win) = w.window() {
        let display = gtk::prelude::WidgetExt::display(w);
        if let Some(c) = gtk::gdk::Cursor::from_name(&display, name) {
            win.set_cursor(Some(&c));
        }
    }
}

fn connection_lines(topo: &PoolTopology, scale: f64) -> Vec<(f64, f64, f64, f64)> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let pad = PAD as f64;

    for (id, n) in &topo.nodes {
        for (dir, other) in &n.neighbors {
            if !topo.nodes.contains_key(other) {
                continue;
            }
            let mut pair = [id.as_str(), other.as_str()];
            pair.sort();
            let key = pair.join("|");
            if seen.contains(&key) {
                continue;
            }
            seen.insert(key);
            let o = &topo.nodes[other];
            let (x1, y1) = edge_point(n, dir, scale, pad);
            let opp = opposite(dir);
            let (x2, y2) = edge_point(o, opp, scale, pad);
            out.push((x1, y1, x2, y2));
        }
    }
    out
}

fn edge_point(n: &TopologyNode, dir: &str, scale: f64, pad: f64) -> (f64, f64) {
    let cx = pad + (n.x as f64 + n.width as f64 / 2.0) * scale;
    let cy = pad + (n.y as f64 + n.height as f64 / 2.0) * scale;
    match dir {
        "left" => (pad + n.x as f64 * scale, cy),
        "right" => (pad + (n.x as f64 + n.width as f64) * scale, cy),
        "up" => (cx, pad + n.y as f64 * scale),
        "down" => (cx, pad + (n.y as f64 + n.height as f64) * scale),
        _ => (cx, cy),
    }
}

fn opposite(dir: &str) -> &str {
    match dir {
        "left" => "right",
        "right" => "left",
        "up" => "down",
        "down" => "up",
        _ => "left",
    }
}
