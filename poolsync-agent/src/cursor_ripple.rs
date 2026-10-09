//! Transient GTK overlay: expanding rings around the pointer + node name.
use gtk::cairo;
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use gtk::{DrawingArea, Window, WindowType};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

const OVERLAY_PX: i32 = 320;
const ANIM_MS: u64 = 1100;
const TICK_MS: u64 = 16;

thread_local! {
    static LIVE: RefCell<Option<Window>> = const { RefCell::new(None) };
}

/// Show a ripple at the current pointer and a notification with the computer name.
/// Must run on the GTK main thread.
pub fn locate_cursor(node: &str) {
    crate::kvm_x11::set_cursor_visible_best_effort(true);
    let (px, py) = crate::kvm_x11::mouse_location().unwrap_or((0, 0));
    let monitor = crate::kvm_x11::active_monitors()
        .ok()
        .and_then(|ms| {
            ms.into_iter()
                .find(|m| m.contains(px, py))
                .map(|m| format!("{}×{}", m.width, m.height))
        })
        .unwrap_or_else(|| "local screen".into());

    crate::notify_util::notify_cursor_locate(node, &monitor);
    spawn_overlay(node, px, py);
}

fn spawn_overlay(node: &str, px: i32, py: i32) {
    LIVE.with(|slot| {
        if let Some(old) = slot.borrow_mut().take() {
            old.close();
        }
    });

    let win = Window::new(WindowType::Popup);
    win.set_decorated(false);
    win.set_accept_focus(false);
    win.set_skip_taskbar_hint(true);
    win.set_skip_pager_hint(true);
    win.set_keep_above(true);
    win.set_resizable(false);
    win.set_app_paintable(true);
    win.set_default_size(OVERLAY_PX, OVERLAY_PX);
    win.set_type_hint(gdk::WindowTypeHint::Notification);

    if let Some(screen) = gtk::prelude::WidgetExt::screen(&win) {
        if let Some(visual) = screen.rgba_visual() {
            win.set_visual(Some(&visual));
        }
    }

    let label = node.to_string();
    let progress = Rc::new(Cell::new(0.0_f64));
    let area = DrawingArea::new();
    area.set_size_request(OVERLAY_PX, OVERLAY_PX);
    let p = progress.clone();
    area.connect_draw(move |_, cr| {
        draw_ripple(cr, &label, p.get());
        gtk::glib::Propagation::Proceed
    });
    win.add(&area);

    win.move_(px - OVERLAY_PX / 2, py - OVERLAY_PX / 2);

    win.connect_realize(|w| {
        if let Some(gdk_win) = w.window() {
            gdk_win.set_override_redirect(true);
            let region = cairo::Region::create();
            gdk_win.input_shape_combine_region(&region, 0, 0);
        }
    });

    win.show_all();
    win.present();

    let started = Instant::now();
    let win_tick = win.clone();
    let area_tick = area.clone();
    glib::timeout_add_local(Duration::from_millis(TICK_MS), move || {
        let t = started.elapsed().as_secs_f64() / (ANIM_MS as f64 / 1000.0);
        progress.set(t.clamp(0.0, 1.0));
        area_tick.queue_draw();
        if t >= 1.0 {
            win_tick.close();
            LIVE.with(|slot| {
                *slot.borrow_mut() = None;
            });
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });

    LIVE.with(|slot| {
        *slot.borrow_mut() = Some(win);
    });
}

fn draw_ripple(cr: &cairo::Context, node: &str, t: f64) {
    let size = f64::from(OVERLAY_PX);
    let cx = size / 2.0;
    let cy = size / 2.0;
    cr.set_operator(cairo::Operator::Source);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
    let _ = cr.paint();
    cr.set_operator(cairo::Operator::Over);

    for i in 0..3 {
        let phase = (t - f64::from(i) * 0.16).clamp(0.0, 1.0);
        if phase <= 0.0 {
            continue;
        }
        let radius = 18.0 + phase * (size / 2.0 - 22.0);
        let alpha = (1.0 - phase) * 0.9;
        cr.set_source_rgba(0.15, 0.82, 1.0, alpha);
        cr.set_line_width(5.0 - phase * 2.0);
        cr.arc(cx, cy, radius, 0.0, std::f64::consts::TAU);
        let _ = cr.stroke();
    }

    let dot_a = (1.0 - t * 0.4).clamp(0.25, 1.0);
    cr.set_source_rgba(1.0, 1.0, 1.0, dot_a);
    cr.arc(cx, cy, 7.0, 0.0, std::f64::consts::TAU);
    let _ = cr.fill();
    cr.set_source_rgba(0.05, 0.45, 0.75, dot_a);
    cr.arc(cx, cy, 4.0, 0.0, std::f64::consts::TAU);
    let _ = cr.fill();

    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(20.0);
    if let Ok(ext) = cr.text_extents(node) {
        let tx = cx - ext.width() / 2.0 - ext.x_bearing();
        let ty = cy + 42.0;
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.45 * (1.0 - t * 0.3));
        cr.move_to(tx + 1.5, ty + 1.5);
        let _ = cr.show_text(node);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.95 * (1.0 - t * 0.25));
        cr.move_to(tx, ty);
        let _ = cr.show_text(node);
    }
}
