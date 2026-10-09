use gtk::prelude::*;
use gtk::{Box as GtkBox, Button, CheckButton, Orientation, ScrolledWindow, TextView, Window};
use std::cell::RefCell;
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

const UNIT: &str = "poolsync-agent.service";
const LOG_LINES: &str = "400";

thread_local! {
    static OPEN_WINDOW: RefCell<Option<Rc<LogsWindow>>> = const { RefCell::new(None) };
}

struct LogsWindow {
    window: Window,
    view: TextView,
    follow_btn: CheckButton,
    follow_timer: RefCell<Option<glib::SourceId>>,
}

pub fn show(node: &str) {
    OPEN_WINDOW.with(|slot| {
        if let Some(existing) = slot.borrow().as_ref() {
            existing.window.present();
            existing.refresh();
            return;
        }
        let viewer = LogsWindow::new(node);
        viewer.refresh();
        *slot.borrow_mut() = Some(viewer);
    });
}

impl LogsWindow {
    fn new(node: &str) -> Rc<Self> {
        let viewer = Rc::new_cyclic(|weak: &std::rc::Weak<LogsWindow>| {
            let window = Window::builder()
                .title(format!("PoolSync — logs ({node})"))
                .default_width(780)
                .default_height(500)
                .build();

            let root = GtkBox::new(Orientation::Vertical, 0);

            let toolbar = GtkBox::new(Orientation::Horizontal, 6);
            toolbar.set_margin_start(8);
            toolbar.set_margin_end(8);
            toolbar.set_margin_top(8);
            toolbar.set_margin_bottom(4);

            let refresh_btn = Button::with_label("Actualiser");
            let follow_btn = CheckButton::with_label("Suivi en direct");
            follow_btn.set_active(true);
            let copy_btn = Button::with_label("Copier tout");
            let close_btn = Button::with_label("Fermer");

            toolbar.pack_start(&refresh_btn, false, false, 0);
            toolbar.pack_start(&follow_btn, false, false, 0);
            toolbar.pack_end(&close_btn, false, false, 0);
            toolbar.pack_end(&copy_btn, false, false, 0);

            let scrolled = ScrolledWindow::builder()
                .vexpand(true)
                .hexpand(true)
                .margin_start(8)
                .margin_end(8)
                .margin_bottom(8)
                .build();

            let view = TextView::builder()
                .editable(false)
                .cursor_visible(true)
                .monospace(true)
                .wrap_mode(gtk::WrapMode::Word)
                .left_margin(12)
                .right_margin(12)
                .top_margin(10)
                .bottom_margin(10)
                .build();

            // Style CSS sombre moderne pour le journal de logs (appliqué localement à la vue)
            let css_provider = gtk::CssProvider::new();
            let _ = css_provider.load_from_data(
                b"textview text { background-color: #1e1e2e; color: #cdd6f4; font-family: 'JetBrains Mono', 'Fira Code', 'Monospace'; font-size: 11pt; }\n\
                  button { border-radius: 6px; font-weight: bold; padding: 4px 10px; }\n"
            );
            view.style_context()
                .add_provider(&css_provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);

            scrolled.add(&view);

            root.pack_start(&toolbar, false, false, 0);
            root.pack_start(&scrolled, true, true, 0);
            window.add(&root);

            let w = weak.clone();
            refresh_btn.connect_clicked(move |_| {
                if let Some(v) = w.upgrade() {
                    v.refresh();
                }
            });

            let w = weak.clone();
            copy_btn.connect_clicked(move |_| {
                if let Some(v) = w.upgrade() {
                    v.copy_all();
                }
            });

            let w = weak.clone();
            close_btn.connect_clicked(move |_| {
                if let Some(v) = w.upgrade() {
                    v.window.close();
                }
            });

            let w = weak.clone();
            follow_btn.connect_toggled(move |_| {
                if let Some(v) = w.upgrade() {
                    v.update_follow_timer();
                }
            });

            let w = weak.clone();
            window.connect_delete_event(move |_, _| {
                if let Some(v) = w.upgrade() {
                    v.stop_follow_timer();
                }
                glib::Propagation::Proceed
            });

            window.connect_destroy(|_| {
                OPEN_WINDOW.with(|slot| *slot.borrow_mut() = None);
            });

            LogsWindow {
                window,
                view,
                follow_btn,
                follow_timer: RefCell::new(None),
            }
        });

        viewer.update_follow_timer();
        viewer.window.show_all();
        viewer
    }

    fn refresh(&self) {
        let text = fetch_journal_logs();
        let buffer = self.view.buffer().expect("text buffer");
        buffer.set_text(&text);
        if self.follow_btn.is_active() {
            scroll_to_top(&self.view);
        }
    }

    fn copy_all(&self) {
        let buffer = self.view.buffer().expect("text buffer");
        let (start, end) = buffer.bounds();
        if let Some(text) = buffer.text(&start, &end, true) {
            let clipboard = gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD);
            clipboard.set_text(&text);
        }
    }

    fn update_follow_timer(&self) {
        if self.follow_btn.is_active() {
            self.start_follow_timer();
        } else {
            self.stop_follow_timer();
        }
    }

    fn start_follow_timer(&self) {
        if self.follow_timer.borrow().is_some() {
            return;
        }
        let view = self.view.clone();
        let id = glib::timeout_add_local(Duration::from_secs(2), move || {
            let text = fetch_journal_logs();
            let buffer = view.buffer().expect("text buffer");
            buffer.set_text(&text);
            scroll_to_top(&view);
            glib::ControlFlow::Continue
        });
        *self.follow_timer.borrow_mut() = Some(id);
    }

    fn stop_follow_timer(&self) {
        if let Some(id) = self.follow_timer.borrow_mut().take() {
            id.remove();
        }
    }
}

fn scroll_to_top(view: &TextView) {
    let buffer = view.buffer().expect("text buffer");
    let mut start = buffer.start_iter();
    view.scroll_to_iter(&mut start, 0.0, false, 0.0, 0.0);
}

pub(crate) fn fetch_journal_logs() -> String {
    let runtime = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));

    match Command::new("journalctl")
        .args([
            "--user",
            "-u",
            UNIT,
            "--no-pager",
            "-n",
            LOG_LINES,
            "-o",
            "short-iso",
            "-r", // Inversé : évènement le plus récent en premier !
        ])
        .env("XDG_RUNTIME_DIR", runtime)
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            format!("journalctl a échoué (code {:?})\n{err}", out.status.code())
        }
        Err(err) => format!("Impossible de lancer journalctl: {err}"),
    }
}
