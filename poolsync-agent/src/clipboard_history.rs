//! Historique presse-papiers pool (hub) — fenêtre GTK depuis le systray.

use crate::clipboard::write_clipboard_sync;
use crate::state::AgentState;
use crate::thumb::{thumb_b64_from_wire, LIST_THUMB_MAX_PX, PREVIEW_THUMB_MAX_PX};
use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use gtk::gdk::prelude::GdkContextExt;
use gtk::gdk_pixbuf::PixbufLoader;
use gtk::prelude::*;
use gtk::{
    Box as GtkBox, Button, CheckButton, Frame, Image, Label, ListBox, ListBoxRow, MessageDialog,
    MessageType, Orientation, Paned, ScrolledWindow, SearchEntry, Window,
};
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const HTTP_TIMEOUT: Duration = Duration::from_secs(8);
const TRAY_FETCH_TIMEOUT: Duration = Duration::from_secs(2);
const LIST_THUMB_PX: i32 = 160;
const PREVIEW_PX: i32 = 420;
const PAGE_SIZE: usize = 20;

#[derive(Debug, Clone, Deserialize)]
pub struct HistoryItem {
    pub hash: String,
    pub mime: String,
    pub preview: String,
    pub source_node: String,
    pub at: u64,
    pub is_image: bool,
    pub thumb_b64: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HistoryResponse {
    items: Vec<HistoryItem>,
}

#[derive(Debug, Deserialize)]
struct ItemResponse {
    mime: String,
    data: String,
}

pub struct HistoryWindow {
    weak: std::rc::Weak<HistoryWindow>,
    window: Window,
    state: Arc<AgentState>,
    list: ListBox,
    status: Label,
    search: SearchEntry,
    page_sidebar: GtkBox,
    preview_image: Image,
    preview_caption: Label,
    items: RefCell<Vec<HistoryItem>>,
    filtered: RefCell<Vec<HistoryItem>>,
    selected: RefCell<HashSet<String>>,
    page: RefCell<usize>,
    /// Vrai pendant `render_page` : empêche le timer de reconstruire la liste
    /// alors que GTK est encore en train de la dessiner.
    rendering: std::cell::Cell<bool>,
}

/// Construit la page « Presse-papiers » destinée au `Notebook` de la fenêtre
/// unique, et renvoie le widget racine accompagné du handle qui la pilote.
///
pub fn build_page(state: Arc<AgentState>) -> (gtk::Widget, Rc<HistoryWindow>) {
    crate::crashlog::set_context("clipboard_history::build_page");
    let win = HistoryWindow::new(state);
    win.reload();
    // La racine est montée dans l'onglet : on l'affiche sans jamais montrer la
    // fenêtre porteuse, qui ne sert que de conteneur temporaire.
    let page = win
        .window
        .child()
        .expect("fenêtre historique construite avec une racine");
    // Détache la racine de sa fenêtre porteuse pour la replacer dans l'onglet.
    win.window.remove(&page);
    // La fenêtre porteuse ne sert qu'à construire la page : sans ce retrait de la
    // hiérarchie GTK, elle reste vivante, tente de dessiner un contenu qu'elle n'a
    // plus, et segfaute dans Pango.
    unsafe {
        win.window.destroy();
    }
    page.show_all();
    (page, win)
}

/// Dialogue « vider l'historique » depuis le systray (sans ouvrir la fenêtre).
pub fn confirm_clear_from_tray(state: Arc<AgentState>) {
    let dialog = MessageDialog::new(
        None::<&gtk::Window>,
        gtk::DialogFlags::MODAL,
        MessageType::Question,
        gtk::ButtonsType::YesNo,
        "Vider tout l'historique presse-papiers du pool ?",
    );
    dialog.set_secondary_text(Some(
        "Cette action supprime l'historique sur le hub pour tous les nœuds.",
    ));
    dialog.connect_response(move |d, response| {
        if response == gtk::ResponseType::Yes {
            match clear_history(&state) {
                Ok(()) => tracing::info!("clipboard history cleared from tray"),
                Err(err) => tracing::warn!("clear history: {err:#}"),
            }
        }
        d.close();
    });
    dialog.show_all();
}

impl HistoryWindow {
    fn new(state: Arc<AgentState>) -> Rc<Self> {
        let win = Rc::new_cyclic(|weak: &std::rc::Weak<HistoryWindow>| {
            let window = Window::builder()
                .title(format!(
                    "PoolSync - historique presse-papiers ({})",
                    state.config.node
                ))
                .default_width(920)
                .default_height(580)
                .build();

            let root = GtkBox::new(Orientation::Vertical, 0);

            let toolbar = GtkBox::new(Orientation::Horizontal, 6);
            toolbar.set_margin_start(8);
            toolbar.set_margin_end(8);
            toolbar.set_margin_top(8);
            toolbar.set_margin_bottom(4);

            let refresh_btn = Button::with_label("Actualiser");
            let paste_btn = Button::with_label("Coller");
            let delete_btn = Button::with_label("Supprimer sélection");
            let select_all_btn = Button::with_label("Tout cocher");
            let clear_btn = Button::with_label("Vider tout");
            let close_btn = Button::with_label("Fermer");

            toolbar.pack_start(&refresh_btn, false, false, 0);
            toolbar.pack_start(&paste_btn, false, false, 0);
            toolbar.pack_start(&delete_btn, false, false, 0);
            toolbar.pack_start(&select_all_btn, false, false, 0);
            toolbar.pack_start(&clear_btn, false, false, 0);
            toolbar.pack_end(&close_btn, false, false, 0);

            let search = SearchEntry::new();
            search.set_placeholder_text(Some("Rechercher…"));
            search.set_margin_start(8);
            search.set_margin_end(8);
            search.set_margin_bottom(6);

            let body = GtkBox::new(Orientation::Horizontal, 0);

            let page_frame = Frame::new(Some("Pages"));
            page_frame.set_margin_start(8);
            page_frame.set_margin_bottom(8);
            let page_sidebar = GtkBox::new(Orientation::Vertical, 4);
            page_sidebar.set_margin_start(6);
            page_sidebar.set_margin_end(6);
            page_sidebar.set_margin_top(6);
            page_sidebar.set_margin_bottom(6);
            let page_scroll = ScrolledWindow::builder()
                .min_content_width(52)
                .max_content_width(64)
                .vexpand(true)
                .build();
            page_scroll.add(&page_sidebar);
            page_frame.add(&page_scroll);

            let list = ListBox::new();
            list.set_selection_mode(gtk::SelectionMode::Single);
            list.set_activate_on_single_click(false);

            let list_scroll = ScrolledWindow::builder()
                .vexpand(true)
                .hexpand(true)
                .min_content_width(360)
                .build();
            list_scroll.add(&list);

            let preview_frame = Frame::new(Some("Aperçu"));
            preview_frame.set_margin_end(8);
            preview_frame.set_margin_bottom(8);
            let preview_box = GtkBox::new(Orientation::Vertical, 8);
            preview_box.set_margin_start(10);
            preview_box.set_margin_end(10);
            preview_box.set_margin_top(10);
            preview_box.set_margin_bottom(10);
            let preview_image = Image::new();
            // Pas de `set_size_request` à la taille pleine : le Paned propagerait
            // cette largeur minimale dans `gtk_widget_get_preferred_width`, dont la
            // récursion finit par segfauter Pango. Un ScrolledWindow borne la
            // demande, l'aperçu garde sa taille réelle à l'intérieur.
            preview_image.set_size_request(-1, PREVIEW_PX);
            let preview_caption = Label::new(None);
            preview_caption.set_line_wrap(true);
            preview_caption.set_xalign(0.0);
            preview_caption.set_max_width_chars(32);
            preview_box.pack_start(&preview_image, false, false, 0);
            preview_box.pack_start(&preview_caption, false, false, 0);
            let preview_scroll = ScrolledWindow::builder()
                .min_content_width(220)
                .max_content_width(PREVIEW_PX + 40)
                .propagate_natural_width(false)
                .build();
            preview_scroll.add(&preview_box);
            preview_frame.add(&preview_scroll);

            let paned = Paned::new(Orientation::Horizontal);
            paned.pack1(&list_scroll, true, true);
            paned.pack2(&preview_frame, false, false);
            paned.set_position(480);

            body.pack_start(&page_frame, false, false, 0);
            body.pack_start(&paned, true, true, 0);

            let status = Label::new(None);
            status.set_halign(gtk::Align::Start);
            status.set_margin_start(8);
            status.set_margin_bottom(6);

            // Style CSS moderne et épuré pour la fenêtre d'historique GTK
            let css_provider = gtk::CssProvider::new();
            let _ = css_provider.load_from_data(
                b"window { background-color: #1e1e2e; color: #cdd6f4; }\n\
                  list { background-color: #181825; border-radius: 8px; }\n\
                  row { padding: 6px; border-bottom: 1px solid #313244; }\n\
                  row:selected { background-color: #45475a; color: #f5e0dc; }\n\
                  button { background-color: #313244; color: #cdd6f4; border-radius: 6px; font-weight: bold; padding: 4px 10px; }\n\
                  button:hover { background-color: #45475a; }\n\
                  entry { background-color: #181825; color: #cdd6f4; border-radius: 6px; padding: 6px; }\n"
            );
            window
                .style_context()
                .add_provider(&css_provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);

            root.pack_start(&toolbar, false, false, 0);
            root.pack_start(&search, false, false, 0);
            root.pack_start(&body, true, true, 0);
            root.pack_start(&status, false, false, 0);
            window.add(&root);

            // Rafraîchissement automatique en pseudo temps réel (toutes les 1.5s)
            let w_auto = weak.clone();
            // Rafraîchissement dynamique sûr : on ne reconstruit la liste que si son
            // contenu a changé ET que la fenêtre n'est pas en train d'être dessinée.
            // `render_page` détruit puis recrée toutes les lignes ; le faire pendant
            // un cycle de rendu GTK fait segfauter Pango (cf. crashlog, contexte
            // « render_page »). Un drapeau marque le rendu en cours.
            let mut last_signature = String::new();
            glib::timeout_add_local(Duration::from_millis(1200), move || {
                let Some(win) = w_auto.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                if win.rendering.get() {
                    return glib::ControlFlow::Continue;
                }
                let signature = match fetch_history(&win.state) {
                    Ok(items) => items
                        .iter()
                        .map(|i| i.hash.as_str())
                        .collect::<Vec<_>>()
                        .join(","),
                    Err(_) => return glib::ControlFlow::Continue,
                };
                if signature != last_signature {
                    last_signature = signature;
                    win.reload();
                }
                glib::ControlFlow::Continue
            });

            let w = weak.clone();
            refresh_btn.connect_clicked(move |_| {
                if let Some(v) = w.upgrade() {
                    v.reload();
                }
            });
            let w = weak.clone();
            paste_btn.connect_clicked(move |_| {
                if let Some(v) = w.upgrade() {
                    v.paste_selected();
                }
            });
            let w = weak.clone();
            delete_btn.connect_clicked(move |_| {
                if let Some(v) = w.upgrade() {
                    v.delete_selected();
                }
            });
            let w = weak.clone();
            select_all_btn.connect_clicked(move |_| {
                if let Some(v) = w.upgrade() {
                    v.set_all_checks(true);
                }
            });
            let w = weak.clone();
            clear_btn.connect_clicked(move |_| {
                if let Some(v) = w.upgrade() {
                    let dialog = MessageDialog::new(
                        Some(&v.window),
                        gtk::DialogFlags::MODAL,
                        MessageType::Question,
                        gtk::ButtonsType::YesNo,
                        "Vider tout l'historique du pool ?",
                    );
                    let w2 = w.clone();
                    dialog.connect_response(move |d, response| {
                        if response == gtk::ResponseType::Yes {
                            if let Some(win) = w2.upgrade() {
                                match clear_history(&win.state) {
                                    Ok(()) => {
                                        win.search.set_text("");
                                        win.selected.borrow_mut().clear();
                                        win.reload();
                                        win.set_status("Historique vidé");
                                    }
                                    Err(err) => win.set_status(&format!("Échec : {err}")),
                                }
                            }
                        }
                        d.close();
                    });
                    dialog.show_all();
                }
            });
            let w = weak.clone();
            list.connect_row_activated(move |_, row| {
                if let Some(v) = w.upgrade() {
                    if v.state.history_double_click_paste() {
                        v.paste_row(row);
                    }
                }
            });
            let w = weak.clone();
            list.connect_row_selected(move |_, row| {
                if let Some(v) = w.upgrade() {
                    if let Some(row) = row {
                        v.show_preview_for_row(row);
                    }
                }
            });
            let w = weak.clone();
            search.connect_search_changed(move |entry| {
                if let Some(v) = w.upgrade() {
                    *v.page.borrow_mut() = 0;
                    v.apply_filter(entry.text().as_ref());
                }
            });
            close_btn.connect_clicked(move |btn| {
                if let Some(w) = btn.toplevel() {
                    w.hide();
                }
            });

            HistoryWindow {
                weak: weak.clone(),
                window,
                state: state.clone(),
                list,
                status,
                search,
                page_sidebar,
                preview_image,
                preview_caption,
                items: RefCell::new(Vec::new()),
                filtered: RefCell::new(Vec::new()),
                selected: RefCell::new(HashSet::new()),
                page: RefCell::new(0),
                rendering: std::cell::Cell::new(false),
            }
        });

        win.window.present();
        win
    }

    fn set_status(&self, msg: &str) {
        self.status.set_text(msg);
    }

    fn reload(&self) {
        crate::crashlog::set_context("clipboard_history::reload");
        match fetch_history(&self.state) {
            Ok(items) => {
                *self.items.borrow_mut() = items;
                self.apply_filter(self.search.text().as_ref());
            }
            Err(err) => self.set_status(&format!("Erreur chargement : {err}")),
        }
    }

    fn apply_filter(&self, query: &str) {
        let all = self.items.borrow();
        let q = query.trim();
        let filtered: Vec<HistoryItem> = if q.is_empty() {
            all.clone()
        } else {
            let ql = q.to_lowercase();
            all.iter()
                .filter(|item| item_matches(item, &ql))
                .cloned()
                .collect()
        };
        drop(all);
        *self.filtered.borrow_mut() = filtered;
        self.rebuild_pages();
        self.render_page();
    }

    fn rebuild_pages(&self) {
        for child in self.page_sidebar.children() {
            self.page_sidebar.remove(&child);
        }
        let count = self.filtered.borrow().len();
        let pages = page_count(count);
        let w = self.weak.clone();
        for p in 0..pages {
            let btn = Button::with_label(&format!("{}", p + 1));
            btn.set_tooltip_text(Some(&format!(
                "Page {} - entrees {}-{}",
                p + 1,
                p * PAGE_SIZE + 1,
                ((p + 1) * PAGE_SIZE).min(count).max(p * PAGE_SIZE)
            )));
            let w2 = w.clone();
            let page_idx = p;
            btn.connect_clicked(move |_| {
                if let Some(win) = w2.upgrade() {
                    *win.page.borrow_mut() = page_idx;
                    win.render_page();
                }
            });
            self.page_sidebar.pack_start(&btn, false, false, 0);
        }
        self.page_sidebar.show_all();
    }

    fn render_page(&self) {
        crate::crashlog::set_context("clipboard_history::render_page");
        self.rendering.set(true);
        let filtered = self.filtered.borrow();
        let page = *self.page.borrow();
        let pages = page_count(filtered.len());
        let page = page.min(pages.saturating_sub(1));
        *self.page.borrow_mut() = page;

        let start = page * PAGE_SIZE;
        let slice: Vec<&HistoryItem> = filtered.iter().skip(start).take(PAGE_SIZE).collect();

        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }
        for item in &slice {
            self.list
                .add(&build_history_row(item, &self.state, &self.selected));
        }
        self.list.show_all();

        let total = self.items.borrow().len();
        let filt = filtered.len();
        let sel = self.selected.borrow().len();
        let q = self.search.text().to_string();
        let page_info = if pages > 1 {
            format!(" - page {}/{}", page + 1, pages)
        } else {
            String::new()
        };
        if q.trim().is_empty() {
            self.set_status(&format!(
                "{filt} entrée(s){page_info} - {sel} cochée(s) - {total} au total"
            ));
        } else {
            self.set_status(&format!(
                "{filt} / {total} - {}{page_info} - {sel} cochée(s)",
                q.trim()
            ));
        }
        self.rendering.set(false);
    }

    fn set_all_checks(&self, checked: bool) {
        let page = *self.page.borrow();
        let start = page * PAGE_SIZE;
        let hashes: Vec<String> = self
            .filtered
            .borrow()
            .iter()
            .skip(start)
            .take(PAGE_SIZE)
            .map(|i| i.hash.clone())
            .collect();
        {
            let mut sel = self.selected.borrow_mut();
            if checked {
                for h in &hashes {
                    sel.insert(h.clone());
                }
            } else {
                for h in &hashes {
                    sel.remove(h);
                }
            }
        }
        self.render_page();
    }

    fn delete_selected(&self) {
        let hashes: Vec<String> = self.selected.borrow().iter().cloned().collect();
        if hashes.is_empty() {
            self.set_status("Cochez des entrées à supprimer (case à gauche)");
            return;
        }
        let dialog = MessageDialog::new(
            Some(&self.window),
            gtk::DialogFlags::MODAL,
            MessageType::Question,
            gtk::ButtonsType::YesNo,
            &format!("Supprimer {} entrée(s) de l'historique ?", hashes.len()),
        );
        let w = self.weak.clone();
        dialog.connect_response(move |d, response| {
            if response == gtk::ResponseType::Yes {
                if let Some(win) = w.upgrade() {
                    match delete_hashes(&win.state, &hashes) {
                        Ok(()) => {
                            win.selected.borrow_mut().clear();
                            win.reload();
                            win.set_status(&format!("{} entrée(s) supprimée(s)", hashes.len()));
                        }
                        Err(err) => win.set_status(&format!("Échec suppression : {err}")),
                    }
                }
            }
            d.close();
        });
        dialog.show_all();
    }

    fn show_preview_for_row(&self, row: &ListBoxRow) {
        let hash = row.widget_name().to_string();
        let item = self
            .filtered
            .borrow()
            .iter()
            .find(|i| i.hash == hash)
            .cloned();
        let Some(item) = item else {
            return;
        };
        if item.is_image {
            if let Some(bytes) = thumb_bytes(&item, &self.state, PREVIEW_THUMB_MAX_PX) {
                if let Some(pixbuf) = pixbuf_from_bytes(&bytes, PREVIEW_PX) {
                    self.preview_image.set_from_pixbuf(Some(&pixbuf));
                    self.preview_caption.set_text(&format!(
                        "{}\n{} - {} - {}",
                        format_time(item.at),
                        item.source_node,
                        item.mime,
                        item.preview
                    ));
                } else {
                    self.preview_image.clear();
                    self.preview_caption
                        .set_text("Impossible de charger l'aperçu image");
                }
            } else {
                self.preview_image.clear();
                self.preview_caption
                    .set_text("Impossible de charger l'aperçu image");
            }
        } else {
            self.preview_image.clear();
            self.preview_caption.set_text(&format!(
                "Texte - {}\n{}\n\n{}",
                format_time(item.at),
                item.source_node,
                truncate_one_line(&item.preview, 500)
            ));
        }
    }

    fn paste_selected(&self) {
        let row = match self.list.selected_row() {
            Some(r) => r,
            None => {
                self.set_status("Sélectionnez une ligne puis Coller (ou double-clic si activé dans Agent local)");
                return;
            }
        };
        self.paste_row(&row);
    }

    fn paste_row(&self, row: &ListBoxRow) {
        let hash = row.widget_name().to_string();
        if hash.is_empty() {
            self.set_status("Entrée invalide");
            return;
        }
        match pick_and_paste(&self.state, &hash) {
            Ok(()) => self.set_status("Collé sur ce poste et diffusé au pool"),
            Err(err) => self.set_status(&format!("Échec collage : {err}")),
        }
    }
}

fn page_count(total: usize) -> usize {
    if total == 0 {
        1
    } else {
        total.div_ceil(PAGE_SIZE)
    }
}

fn item_matches(item: &HistoryItem, ql: &str) -> bool {
    item.preview.to_lowercase().contains(ql)
        || item.source_node.to_lowercase().contains(ql)
        || item.mime.to_lowercase().contains(ql)
        || item.hash.to_lowercase().contains(ql)
}

fn build_history_row(
    item: &HistoryItem,
    state: &AgentState,
    selected: &RefCell<HashSet<String>>,
) -> ListBoxRow {
    let row = ListBoxRow::new();
    row.set_widget_name(&item.hash);

    let hbox = GtkBox::new(Orientation::Horizontal, 8);
    hbox.set_margin_top(8);
    hbox.set_margin_bottom(8);
    hbox.set_margin_start(6);
    hbox.set_margin_end(8);

    // Poignée en ASCII : le caractère braille U+283F n'existe dans aucune police
    // installée, et la recherche de fallback correspondante segfaute dans
    // `pango_coverage_get` (Pango 1.52) au moment du rendu de la ligne.
    let grip = Label::new(Some("::"));
    grip.set_tooltip_text(Some("Poignée - cochez pour sélection multiple"));
    grip.set_width_chars(2);

    let check = CheckButton::new();
    check.set_active(selected.borrow().contains(&item.hash));
    let hash = item.hash.clone();
    let sel = selected.clone();
    check.connect_toggled(move |cb| {
        let mut s = sel.borrow_mut();
        if cb.is_active() {
            s.insert(hash.clone());
        } else {
            s.remove(&hash);
        }
    });

    let check_box = GtkBox::new(Orientation::Horizontal, 2);
    check_box.pack_start(&grip, false, false, 0);
    check_box.pack_start(&check, false, false, 0);
    hbox.pack_start(&check_box, false, false, 0);

    // Vignette rendue dans un DrawingArea de taille fixe : un `Image` GTK négocie sa
    // largeur avec le reste de la ligne, et cette négociation récursive
    // (`gtk_widget_get_preferred_width`) finit par segfauter Pango 1.52. Ici la
    // taille est imposée, donc plus aucune négociation.
    if item.is_image {
        if let Some(pixbuf) = list_thumb_pixbuf(item, state, LIST_THUMB_PX) {
            let area = gtk::DrawingArea::new();
            area.set_size_request(LIST_THUMB_PX, LIST_THUMB_PX);
            area.connect_draw(move |_, cr| {
                cr.set_source_pixbuf(&pixbuf, 0.0, 0.0);
                let _ = cr.paint();
                gtk::glib::Propagation::Proceed
            });
            hbox.pack_start(&area, false, false, 0);
        }
    }

    let kind = if item.is_image { "Image" } else { "Texte" };
    let body = if item.is_image {
        format!("{} - {}", item.source_node, item.mime)
    } else {
        format!(
            "{} - {} - {}",
            item.source_node,
            kind,
            truncate_one_line(&item.preview.replace('\n', " "), 160)
        )
    };
    let label = Label::new(Some(&format!("{}  -  {body}", format_time(item.at))));
    label.set_xalign(0.0);
    // Pas de `set_line_wrap` ni de `set_selectable` ici : combinés dans une ligne de
    // liste à largeur variable, ils font segfauter Pango 1.52 au calcul du retour à
    // la ligne. L'ellipse donne le même confort de lecture sans le crash.
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    // `width_chars` fixe la largeur minimale demandée : sans lui, GTK réclame la
    // largeur naturelle du texte entier et la récursion de `get_preferred_width`
    // finit par segfauter Pango.
    label.set_width_chars(20);
    label.set_max_width_chars(60);
    hbox.pack_start(&label, true, true, 0);

    row.add(&hbox);
    row.show_all();
    row
}

fn thumb_bytes(item: &HistoryItem, state: &AgentState, max_px: u32) -> Option<Vec<u8>> {
    let b64 = item.thumb_b64.clone().or_else(|| {
        let wire = fetch_item(state, &item.hash).ok()?;
        thumb_b64_from_wire(&wire.data, max_px).ok()
    })?;
    B64.decode(b64).ok()
}

fn pixbuf_from_bytes(bytes: &[u8], size: i32) -> Option<gtk::gdk_pixbuf::Pixbuf> {
    if bytes.is_empty() {
        return None;
    }
    let loader = PixbufLoader::new();
    loader.set_size(size, size);
    if loader.write(bytes).is_err() {
        let _ = loader.close();
        return None;
    }
    if loader.close().is_err() {
        return None;
    }
    loader.pixbuf()
}

fn list_thumb_pixbuf(
    item: &HistoryItem,
    state: &AgentState,
    px: i32,
) -> Option<gtk::gdk_pixbuf::Pixbuf> {
    let bytes = thumb_bytes(item, state, LIST_THUMB_MAX_PX)?;
    pixbuf_from_bytes(&bytes, px)
}

fn format_time(at: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let ago = now.saturating_sub(at);
    if ago < 60 {
        format!("{ago}s")
    } else if ago < 3600 {
        format!("{}min", ago / 60)
    } else {
        format!("{}h", ago / 3600)
    }
}

fn http_base(state: &AgentState) -> Result<String> {
    anyhow::ensure!(
        !state.config.hubless,
        "history is stored locally in hubless mode"
    );
    crate::network::hub_http_base(&state.config.hub_url)
}

fn fetch_history(state: &AgentState) -> Result<Vec<HistoryItem>> {
    let mut items = crate::clip_cache::list_recent(50);
    if items.len() < 50 {
        if let Ok(hub_items) = fetch_history_from_hub(state) {
            for item in hub_items {
                if items.iter().any(|i| i.hash == item.hash) {
                    continue;
                }
                items.push(item);
                if items.len() >= 50 {
                    break;
                }
            }
        }
    }
    merge_optimistic_tray(state, &mut items);
    Ok(items)
}

fn fetch_history_from_hub(state: &AgentState) -> Result<Vec<HistoryItem>> {
    let url = format!("{}/api/clipboard/history?limit=50", http_base(state)?);
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
    Ok(serde_json::from_str::<HistoryResponse>(&body)?.items)
}

/// Historique hub uniquement (sans cache local) — ex. après vidage distant.
pub fn fetch_history_hub_only(state: &AgentState) -> Result<Vec<HistoryItem>> {
    fetch_history_from_hub(state)
}

fn fetch_item(state: &AgentState, hash: &str) -> Result<ItemResponse> {
    if let Some((mime, data)) = crate::clip_cache::get(hash) {
        return Ok(ItemResponse { mime, data });
    }
    let url = format!("{}/api/clipboard/item?hash={hash}", http_base(state)?);
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

pub fn clear_history(state: &AgentState) -> Result<()> {
    if state.config.hubless {
        crate::clip_cache::clear_all();
        state.clear_optimistic_tray_all();
        state.mark_history_cleared();
        state.notify_tray_history_changed();
        return Ok(());
    }
    let url = format!("{}/api/clipboard/clear", http_base(state)?);
    ureq::post(&url)
        .timeout(HTTP_TIMEOUT)
        .set(
            "Authorization",
            &format!("Bearer {}", state.config.authentication_token()),
        )
        .set("X-PoolSync-Node", &state.config.node)
        .call()
        .map_err(|e| anyhow!("{e}"))?;
    crate::clip_cache::clear_all();
    state.clear_optimistic_tray_all();
    state.mark_history_cleared();
    state.notify_tray_history_changed();
    Ok(())
}

pub fn delete_hashes(state: &AgentState, hashes: &[String]) -> Result<()> {
    if state.config.hubless {
        crate::clip_cache::remove_hashes(hashes);
        state.notify_tray_history_changed();
        return Ok(());
    }
    if hashes.is_empty() {
        return Ok(());
    }
    let url = format!("{}/api/clipboard/delete", http_base(state)?);
    let body = serde_json::json!({ "hashes": hashes }).to_string();
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
    crate::clip_cache::remove_hashes(hashes);
    state.notify_tray_history_changed();
    Ok(())
}
/// Libellé court pour une entrée texte du menu systray.
pub fn tray_label(item: &HistoryItem) -> String {
    let ago = format_time(item.at);
    let body = truncate_one_line(&item.preview.replace('\n', " "), 48);
    format!("{ago} - {} - {body}", item.source_node)
}

fn truncate_one_line(s: &str, max: usize) -> String {
    let t: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        format!("{t}…")
    } else {
        t
    }
}

/// Entrées récentes pour le menu systray (léger, timeout court).
pub fn fetch_history_tray(state: &AgentState) -> Result<Vec<HistoryItem>> {
    let limit = state.config.tray_history_count.clamp(5, 50) as usize;
    let mut items = crate::clip_cache::list_recent(limit);
    if items.len() < limit {
        if let Ok(hub_items) = fetch_history_tray_from_hub(state, limit) {
            for item in hub_items {
                if items.iter().any(|i| i.hash == item.hash) {
                    continue;
                }
                items.push(item);
                if items.len() >= limit {
                    break;
                }
            }
        }
    }
    merge_optimistic_tray(state, &mut items);
    items.truncate(limit);
    Ok(items)
}

fn fetch_history_tray_from_hub(state: &AgentState, limit: usize) -> Result<Vec<HistoryItem>> {
    let url = format!("{}/api/clipboard/history?limit={limit}", http_base(state)?);
    let body = ureq::get(&url)
        .timeout(TRAY_FETCH_TIMEOUT)
        .set(
            "Authorization",
            &format!("Bearer {}", state.config.authentication_token()),
        )
        .set("X-PoolSync-Node", &state.config.node)
        .call()
        .map_err(|e| anyhow!("{e}"))?
        .into_string()?;
    Ok(serde_json::from_str::<HistoryResponse>(&body)?.items)
}

fn merge_optimistic_tray(state: &AgentState, items: &mut Vec<HistoryItem>) {
    let opt = state.optimistic_tray_item();
    let Some(head) = opt else { return };
    if items.first().is_some_and(|i| i.hash == head.hash) {
        state.clear_optimistic_tray(&head.hash);
        return;
    }
    items.retain(|i| i.hash != head.hash);
    items.insert(0, head);
}

/// Mise à jour systray immédiate après envoi local (sans attendre hub/WS).
pub fn notify_local_clipboard_sent(
    state: &AgentState,
    payload: &crate::clipboard::ClipboardPayload,
) {
    let preview = crate::state::clip_preview_mime(&payload.mime, &payload.wire_data);
    crate::clip_cache::store_payload(payload, &preview, &state.config.node);
    let thumb_b64 = payload
        .mime
        .starts_with("image/")
        .then(|| thumb_b64_from_wire(&payload.wire_data, crate::thumb::TRAY_MENU_SOURCE_PX).ok())
        .flatten();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    state.set_optimistic_tray_item(HistoryItem {
        hash: payload.hash.clone(),
        mime: payload.mime.clone(),
        preview,
        source_node: state.config.node.clone(),
        at: now,
        is_image: payload.mime.starts_with("image/"),
        thumb_b64,
    });
}

/// Colle localement depuis le cache ; relais hub en arrière-plan (si hub_clipboard).
pub fn pick_and_paste(state: &AgentState, hash: &str) -> Result<()> {
    let item = fetch_item(state, hash)?;
    write_clipboard_sync(&item.data, &item.mime)?;
    state.set_last_clip_hash(hash);
    state.mark_hub_clipboard_applied();
    if item.mime.starts_with("image/") {
        crate::clipboard::mark_image_clipboard_epoch();
    }
    if state.config.hubless || !state.config.hub_clipboard {
        return Ok(());
    }
    let hash = hash.to_string();
    let node = state.config.node.clone();
    let token = state.config.authentication_token().to_string();
    let hub_base = http_base(state)?;
    std::thread::spawn(move || {
        let url = format!("{hub_base}/api/clipboard/pick");
        let body = serde_json::json!({
            "hash": hash,
            "node": node,
        })
        .to_string();
        if let Err(err) = ureq::post(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {token}"))
            .set("X-PoolSync-Node", &node)
            .set("Content-Type", "application/json")
            .send_string(&body)
        {
            tracing::debug!("clipboard pick hub relay: {err}");
        }
    });
    Ok(())
}
