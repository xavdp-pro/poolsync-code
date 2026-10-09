//! Matérialise les bords KVM d'un nœud à l'écran, à la demande du hub.
//!
//! Enregistrer une topologie ne dit pas si elle correspond au terrain : jusqu'ici
//! on le découvrait en promenant la souris jusqu'à un bord, en espérant qu'elle
//! bascule. Ce module dessine une bande lumineuse sur chaque bord qui possède un
//! voisin, avec son nom, pendant quelques secondes.

use std::cell::RefCell;
use std::time::Duration;

use gtk::cairo;
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use gtk::{DrawingArea, Window, WindowType};
use poolsync_core::Direction;

use crate::state::AgentState;

/// Épaisseur de la bande, assez large pour être vue sans masquer le travail.
const BAND_PX: i32 = 56;

thread_local! {
    static LIVE: RefCell<Vec<Window>> = const { RefCell::new(Vec::new()) };
}

/// Affiche les bords ayant un voisin, pendant `duration`.
pub fn show(state: &AgentState, duration: Duration) {
    close_all();

    let display = match crate::kvm_x11::kvm_display() {
        Ok(d) => d,
        Err(err) => {
            tracing::warn!("bords : géométrie écran indisponible: {err:#}");
            return;
        }
    };

    let mut shown = 0;
    for neighbor in &state.config.neighbors {
        let (x, y, w, h) = match neighbor.direction {
            Direction::Left => (display.x, display.y, BAND_PX, display.height as i32),
            Direction::Right => (
                display.x + display.width as i32 - BAND_PX,
                display.y,
                BAND_PX,
                display.height as i32,
            ),
            Direction::Up => (display.x, display.y, display.width as i32, BAND_PX),
            Direction::Down => (
                display.x,
                display.y + display.height as i32 - BAND_PX,
                display.width as i32,
                BAND_PX,
            ),
        };
        spawn_band(&neighbor.node, neighbor.direction, x, y, w, h);
        shown += 1;
    }

    if shown == 0 {
        tracing::info!("bords : aucun voisin configuré sur ce nœud");
        return;
    }
    tracing::info!("bords : {shown} bord(s) affiché(s) pendant {:?}", duration);

    glib::timeout_add_local_once(duration, close_all);
}

fn close_all() {
    LIVE.with(|slot| {
        for win in slot.borrow_mut().drain(..) {
            win.close();
        }
    });
}

fn spawn_band(neighbor: &str, dir: Direction, x: i32, y: i32, w: i32, h: i32) {
    let win = Window::new(WindowType::Popup);
    win.set_decorated(false);
    win.set_accept_focus(false);
    win.set_skip_taskbar_hint(true);
    win.set_skip_pager_hint(true);
    win.set_keep_above(true);
    win.set_app_paintable(true);
    win.set_type_hint(gdk::WindowTypeHint::Notification);
    win.set_default_size(w, h);

    if let Some(screen) = gtk::prelude::WidgetExt::screen(&win) {
        if let Some(visual) = screen.rgba_visual() {
            win.set_visual(Some(&visual));
        }
    }

    let label = format!("{} {}", arrow(dir), neighbor);
    let area = DrawingArea::new();
    area.set_size_request(w, h);
    area.connect_draw(move |widget, cr| {
        let width = widget.allocated_width() as f64;
        let height = widget.allocated_height() as f64;
        cr.set_source_rgba(0.29, 0.34, 0.94, 0.55);
        let _ = cr.paint();
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        cr.set_font_size(18.0);
        if let Ok(ext) = cr.text_extents(&label) {
            cr.move_to((width - ext.width()) / 2.0, (height + ext.height()) / 2.0);
            let _ = cr.show_text(&label);
        }
        gtk::glib::Propagation::Proceed
    });
    win.add(&area);
    win.move_(x, y);

    // Sans cela la bande volerait le clic : elle doit être décorative.
    win.connect_realize(|w| {
        if let Some(gdk_win) = w.window() {
            gdk_win.set_override_redirect(true);
            let region = cairo::Region::create();
            gdk_win.input_shape_combine_region(&region, 0, 0);
        }
    });

    win.show_all();
    LIVE.with(|slot| slot.borrow_mut().push(win));
}

fn arrow(dir: Direction) -> &'static str {
    match dir {
        Direction::Left => "←",
        Direction::Right => "→",
        Direction::Up => "↑",
        Direction::Down => "↓",
    }
}
