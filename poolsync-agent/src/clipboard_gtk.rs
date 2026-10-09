//! GTK clipboard owner (systray thread).
//!
//! Images advertise image/png plus a real image/bmp (not GTK set_image).

use gtk::gdk;
use gtk::{Clipboard, TargetEntry, TargetFlags};
use poolsync_core::hash_bytes;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const INFO_PNG: u32 = 1;
const INFO_TEXT: u32 = 2;
const INFO_BMP: u32 = 3;
/// xrdp-chansrv often strips PNG within seconds — keep re-offering for local paste.
pub const IMAGE_CLAIM_KEEPALIVE: Duration = Duration::from_secs(45);

#[derive(Clone)]
pub enum ClipboardOffer {
    /// `mirror_primary` : recopier aussi le texte dans PRIMARY.
    ///
    /// Utile sur un bureau classique (Ctrl+V se rabat parfois sur PRIMARY),
    /// mais à proscrire sur les sessions xrdp : y réécrire PRIMARY entre en
    /// concurrence avec la sélection souris de l'utilisateur et avec
    /// xrdp-chansrv, ce qui fait vaciller la propriété de la sélection sous
    /// les doigts de Chromium — donc VSCode et Chrome qui se figent au collage.
    Text {
        text: String,
        mirror_primary: bool,
    },
    Rich {
        plain: String,
        html: String,
        mirror_primary: bool,
    },
    Image {
        mime: String,
        bytes: Vec<u8>,
    },
    /// Drop GTK ownership so the native X11 clipboard can work (PoolSync sync OFF).
    Release,
}

struct PendingOffer {
    offer: ClipboardOffer,
    guard: Option<SelectionGuard>,
    increment_on_apply: bool,
}

/// A repair must still refer to the selection inspected before its reads.
#[derive(Clone, Copy)]
pub struct SelectionGuard {
    owner: u32,
    epoch: Option<u64>,
    copy_epoch: Option<u64>,
    generation: u64,
}

impl SelectionGuard {
    pub fn capture() -> Self {
        Self {
            owner: current_clipboard_owner(),
            epoch: crate::clipboard_epoch::clipboard_current(),
            copy_epoch: crate::clipboard_epoch::clipboard_copy_current(),
            generation: offer_generation(),
        }
    }

    pub fn unchanged(self) -> bool {
        self.owner == current_clipboard_owner()
            && self.epoch == crate::clipboard_epoch::clipboard_current()
            && self.generation == offer_generation()
    }

    /// A closing application may release ownership after SAVE_TARGETS.
    /// Destruction is allowed; any intervening copy or queued offer is not.
    pub fn handoff_current(self) -> bool {
        self.unchanged()
            || (self.generation == offer_generation()
                && current_clipboard_owner() == 0
                && self.copy_epoch.is_some()
                && self.copy_epoch == crate::clipboard_epoch::clipboard_copy_current())
    }
}

static GTK_TX: OnceLock<glib::Sender<PendingOffer>> = OnceLock::new();
static LAST_PNG: Mutex<Option<Vec<u8>>> = Mutex::new(None);
static LAST_REOFFER: Mutex<Option<Instant>> = Mutex::new(None);
static LAST_IMAGE_CLAIM_AT: Mutex<Option<Instant>> = Mutex::new(None);
static IMAGE_OWNER: AtomicU32 = AtomicU32::new(0);
/// Fenêtre X11 propriétaire de CLIPBOARD après *notre* offre de texte.
/// Symétrique de `IMAGE_OWNER` : sans elle, l'agent relit son propre texte.
static TEXT_OWNER: AtomicU32 = AtomicU32::new(0);
// Delayed verification must never restore an offer superseded by a new copy.
static OFFER_GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn offer_generation() -> u64 {
    OFFER_GENERATION.load(Ordering::SeqCst)
}

pub fn text_offer_is_current(generation: u64) -> bool {
    offer_generation() == generation && owns_text_clipboard()
}
/// Dernière fois qu'une application nous a *demandé* le contenu de la
/// sélection. C'est le seul signal fiable qu'un collage est en cours : X11 ne
/// dit pas « je colle », mais il vient chercher la donnée chez le propriétaire.
static LAST_SERVE_AT: Mutex<Option<Instant>> = Mutex::new(None);
/// Nombre de lectures que l'agent fait lui-même en ce moment (xclip interne).
/// Nos propres lectures passent par le même rappel GTK que celles des autres
/// applications : sans ce compteur, l'agent se prend pour un collage en cours
/// et diffère ses écritures pour rien.
static INTERNAL_READS: AtomicU32 = AtomicU32::new(0);

#[cfg(test)]
pub(crate) static IMAGE_TEST_LOCK: Mutex<()> = Mutex::new(());

pub fn current_clipboard_owner() -> u32 {
    use x11rb::protocol::xproto::ConnectionExt;
    let Ok((conn, _)) = x11rb::connect(None) else {
        return 0;
    };
    let Ok(atom_cookie) = conn.intern_atom(false, b"CLIPBOARD") else {
        return 0;
    };
    let Ok(atom) = atom_cookie.reply() else {
        return 0;
    };
    let Ok(owner_cookie) = conn.get_selection_owner(atom.atom) else {
        return 0;
    };
    owner_cookie.reply().map(|r| r.owner).unwrap_or(0)
}

/// True only while the X11 selection is still the image offered by our GTK
/// clipboard. Unlike the 45s keepalive timer, this remains exact indefinitely.
/// Appelé depuis les rappels GTK quand un client vient lire notre sélection.
fn note_selection_served() {
    if INTERNAL_READS.load(Ordering::SeqCst) > 0 {
        return; // c'est nous qui lisons : ce n'est pas un collage utilisateur
    }
    if let Ok(mut g) = LAST_SERVE_AT.lock() {
        *g = Some(Instant::now());
    }
}

/// Garde RAII : marque la durée d'une lecture faite par l'agent lui-même.
pub struct InternalRead;

impl InternalRead {
    pub fn begin() -> Self {
        INTERNAL_READS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for InternalRead {
    fn drop(&mut self) {
        INTERNAL_READS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Une application a-t-elle lu notre sélection dans les `window` dernières ms ?
///
/// Chromium — donc VSCode, Cursor, Slack — fait cette lecture de façon
/// synchrone : lui retirer la sélection en plein transfert laisse sa fenêtre
/// bloquée (« CodeWindow unresponsive »). Tant que la réponse est fraîche, un
/// collage est probablement en cours et il ne faut pas toucher à la sélection.
pub fn selection_served_recently(window: Duration) -> bool {
    LAST_SERVE_AT
        .lock()
        .ok()
        .and_then(|g| *g)
        .is_some_and(|at| at.elapsed() < window)
}

pub fn owns_image_clipboard() -> bool {
    let expected = IMAGE_OWNER.load(Ordering::SeqCst);
    expected != 0 && current_clipboard_owner() == expected
}

/// Le texte actuellement dans CLIPBOARD est-il notre propre offre GTK ?
///
/// Notre propriétaire GTK annonce UTF8_STRING/STRING mais ne répond pas
/// toujours aux demandes de conversion venant de `xclip` : la lecture échoue
/// alors sur toutes les cibles texte, et seules les métadonnées (TARGETS,
/// TIMESTAMP) répondent encore. C'est ainsi que la sortie de nos propres
/// sondes s'est retrouvée diffusée dans tout le pool. On ne relit donc jamais
/// notre propre offre : on sait déjà ce qu'elle contient.
pub fn owns_text_clipboard() -> bool {
    let expected = TEXT_OWNER.load(Ordering::SeqCst);
    expected != 0 && current_clipboard_owner() == expected
}

pub fn owns_clipboard() -> bool {
    let owner = current_clipboard_owner();
    owner != 0
        && (owner == TEXT_OWNER.load(Ordering::SeqCst)
            || owner == IMAGE_OWNER.load(Ordering::SeqCst))
}

pub fn mark_image_claim() {
    if let Ok(mut t) = LAST_IMAGE_CLAIM_AT.lock() {
        *t = Some(Instant::now());
    }
}

pub fn clear_image_claim() {
    if let Ok(mut t) = LAST_IMAGE_CLAIM_AT.lock() {
        *t = None;
    }
}

/// Un vrai texte vient de remplacer l'image : aucun keepalive XRDP ne doit
/// pouvoir réoffrir l'ancien PNG après l'écriture texte.
pub fn discard_last_image() {
    IMAGE_OWNER.store(0, Ordering::SeqCst);
    if let Ok(mut last) = LAST_PNG.lock() {
        *last = None;
    }
    if let Ok(mut last) = LAST_REOFFER.lock() {
        *last = None;
    }
    clear_image_claim();
}

/// Refresh XRDP recovery without taking ownership from the copying application.
/// A native screenshot can already be pasteable, while the previous GTK image
/// is still cached. Recovering from a later BMP-only callback must use this copy.
pub fn remember_native_image(mime: &str, bytes: &[u8]) {
    OFFER_GENERATION.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut last) = LAST_PNG.lock() {
        *last = Some(ensure_png(mime, bytes));
    }
    if let Ok(mut last) = LAST_REOFFER.lock() {
        *last = None;
    }
    mark_image_claim();
}

pub fn recent_image_claim_active() -> bool {
    LAST_IMAGE_CLAIM_AT
        .lock()
        .ok()
        .and_then(|g| *g)
        .is_some_and(|t| t.elapsed() < IMAGE_CLAIM_KEEPALIVE)
}

pub fn image_claim_debug_state() -> (bool, usize) {
    let active = recent_image_claim_active();
    let bytes = LAST_PNG
        .lock()
        .ok()
        .and_then(|last| last.as_ref().map(Vec::len))
        .unwrap_or(0);
    (active, bytes)
}

#[cfg(test)]
mod internal_read_tests {
    use super::*;

    /// Ces tests manipulent des états globaux ; les sérialiser évite qu'ils se
    /// marchent dessus quand cargo les exécute en parallèle.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Nos propres lectures xclip passent par le même rappel GTK que celles des
    /// applications. Sans le compteur, l'agent se prenait lui-même pour un
    /// collage en cours et différait ses écritures pour rien (observé le 29/08 :
    /// « écriture après 909 ms d'attente — lectures continues »).
    #[test]
    fn our_own_reads_are_ignored_but_a_real_application_read_is_seen() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        *LAST_SERVE_AT.lock().unwrap() = None;

        {
            let _internal = InternalRead::begin();
            note_selection_served();
        }
        assert!(
            !selection_served_recently(Duration::from_secs(5)),
            "une lecture interne ne doit pas ressembler à un collage"
        );

        note_selection_served();
        assert!(
            selection_served_recently(Duration::from_secs(5)),
            "une vraie lecture applicative doit rester détectée"
        );
        *LAST_SERVE_AT.lock().unwrap() = None;
    }

    #[test]
    fn nested_internal_reads_restore_the_counter() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(INTERNAL_READS.load(Ordering::SeqCst), 0);
        {
            let _outer = InternalRead::begin();
            {
                let _inner = InternalRead::begin();
                assert_eq!(INTERNAL_READS.load(Ordering::SeqCst), 2);
            }
            assert_eq!(INTERNAL_READS.load(Ordering::SeqCst), 1);
        }
        assert_eq!(INTERNAL_READS.load(Ordering::SeqCst), 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD as B64, Engine};
    use image::ImageEncoder;

    #[test]
    fn text_discards_stale_image_keepalive() {
        let _serial = IMAGE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        *LAST_PNG.lock().unwrap() = Some(vec![1, 2, 3]);
        mark_image_claim();
        discard_last_image();
        assert!(LAST_PNG.lock().unwrap().is_none());
        assert!(!recent_image_claim_active());
    }

    #[test]
    fn consecutive_native_captures_replace_recovery_without_claiming_x11() {
        let _serial = IMAGE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let first = B64
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
            .unwrap();
        let mut second = Vec::new();
        image::codecs::png::PngEncoder::new(&mut second)
            .write_image(
                &[10, 20, 30, 255, 40, 50, 60, 255],
                2,
                1,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        let owner_before = IMAGE_OWNER.load(Ordering::SeqCst);

        remember_native_image("image/png", &first);
        remember_native_image("image/png", &second);

        assert_eq!(LAST_PNG.lock().unwrap().as_deref(), Some(second.as_slice()));
        assert_eq!(IMAGE_OWNER.load(Ordering::SeqCst), owner_before);
        assert!(recent_image_claim_active());
        discard_last_image();
    }

    #[test]
    fn valid_png_has_a_real_bmp_fallback() {
        let png = B64
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
            .unwrap();
        let bmp = png_to_bmp(&png).expect("a valid PNG must encode as BMP");
        assert!(bmp.starts_with(b"BM"));
        assert!(bmp.len() > 32);
    }

    #[test]
    fn bmp_fallback_has_a_legacy_header_and_preserves_opaque_pixels() {
        for width in [1, 3, 193] {
            let height = 2;
            let pixels: Vec<u8> = (0..width * height)
                .flat_map(|i| [(i * 31) as u8, (i * 17) as u8, (i * 7) as u8, 255])
                .collect();
            let mut png = Vec::new();
            image::codecs::png::PngEncoder::new(&mut png)
                .write_image(&pixels, width, height, image::ExtendedColorType::Rgba8)
                .unwrap();
            let bmp = png_to_bmp(&png).unwrap();
            let header_size = u32::from_le_bytes(bmp[14..18].try_into().unwrap());
            let pixel_offset = u32::from_le_bytes(bmp[10..14].try_into().unwrap());
            assert_eq!(
                header_size, 40,
                "legacy CF_DIB must not include a V4 header"
            );
            assert_eq!(pixel_offset, 54);
            assert_eq!(u16::from_le_bytes(bmp[28..30].try_into().unwrap()), 24);
            let decoded = image::load_from_memory(&bmp).unwrap().to_rgba8();
            assert_eq!(
                decoded.as_raw(),
                &pixels,
                "odd-width row padding must round-trip"
            );
        }
    }

    #[test]
    fn png_offer_keeps_transparent_pixels_and_original_bytes() {
        let pixels = [70, 241, 196, 0, 10, 20, 30, 128];
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&pixels, 2, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        assert_eq!(ensure_png("image/png", &png), png);
        assert_eq!(
            image::load_from_memory(&png).unwrap().to_rgba8().as_raw(),
            &pixels
        );
    }
}

/// À appeler sur le thread GTK après `gtk::init()`.
pub fn attach_gtk_handler() {
    #[allow(deprecated)]
    let (tx, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
    let _ = GTK_TX.set(tx);
    rx.attach(None, |pending| {
        if let Some(guard) = pending.guard {
            if !guard.unchanged() {
                tracing::debug!("clipboard repair cancelled after a newer selection");
                return glib::ControlFlow::Continue;
            }
            if pending.increment_on_apply {
                OFFER_GENERATION.fetch_add(1, Ordering::SeqCst);
            }
        }
        apply_offer(pending.offer);
        glib::ControlFlow::Continue
    });
}

pub fn try_offer(offer: ClipboardOffer) -> bool {
    OFFER_GENERATION.fetch_add(1, Ordering::SeqCst);
    let guard = Some(SelectionGuard::capture());
    GTK_TX
        .get()
        .and_then(|tx| {
            tx.send(PendingOffer {
                offer,
                guard,
                increment_on_apply: false,
            })
            .ok()
        })
        .is_some()
}

pub fn try_offer_if_unchanged(offer: ClipboardOffer, guard: SelectionGuard) -> bool {
    GTK_TX
        .get()
        .and_then(|tx| {
            tx.send(PendingOffer {
                offer,
                guard: Some(guard),
                increment_on_apply: true,
            })
            .ok()
        })
        .is_some()
}

/// Apply immediately when the caller is already running on the GTK main
/// thread (the tray/history menu).  Queuing from that same thread lets the
/// click handler report success before X11 ownership actually changes.
pub fn offer_now_from_gtk(offer: ClipboardOffer) {
    OFFER_GENERATION.fetch_add(1, Ordering::SeqCst);
    apply_offer(offer);
}

/// xrdp-chansrv often replaces a PNG offer with empty image/bmp. Put PNG back.
pub fn reoffer_last_image(guard: SelectionGuard) -> bool {
    if !guard.unchanged() {
        return false;
    }
    let min_ms = if recent_image_claim_active() {
        200
    } else {
        400
    };
    let too_soon = LAST_REOFFER
        .lock()
        .ok()
        .and_then(|g| *g)
        .is_some_and(|t| t.elapsed().as_millis() < min_ms);
    if too_soon {
        return false;
    }
    let png = LAST_PNG.lock().ok().and_then(|g| g.clone());
    let Some(png) = png else {
        return false;
    };
    if let Ok(mut t) = LAST_REOFFER.lock() {
        *t = Some(Instant::now());
    }
    try_offer_if_unchanged(
        ClipboardOffer::Image {
            mime: "image/png".into(),
            bytes: png,
        },
        guard,
    )
}

fn apply_offer(offer: ClipboardOffer) {
    let Some(display) = gdk::Display::default() else {
        tracing::warn!("gtk clipboard: no display");
        return;
    };
    let clip =
        Clipboard::default(&display).unwrap_or_else(|| Clipboard::get(&gdk::SELECTION_CLIPBOARD));
    let primary = Clipboard::get(&gdk::SELECTION_PRIMARY);
    match offer {
        ClipboardOffer::Text {
            text,
            mirror_primary,
        } => {
            IMAGE_OWNER.store(0, Ordering::SeqCst);
            if let Ok(mut last) = LAST_PNG.lock() {
                *last = None;
            }
            clear_image_claim();
            // xrdp / Chrome Ctrl+V: CLIPBOARD without UTF8 falls back to PRIMARY
            // (stale URL). Mirror text on both selections.
            let ok_clip = set_text_only(&clip, text.clone());
            if mirror_primary {
                let _ = set_text_only(&primary, text);
            }
            TEXT_OWNER.store(
                if ok_clip {
                    current_clipboard_owner()
                } else {
                    0
                },
                Ordering::SeqCst,
            );
            if !ok_clip {
                tracing::warn!("gtk clipboard set_with_data failed — text not owned by agent");
            }
        }
        ClipboardOffer::Rich {
            plain,
            html,
            mirror_primary,
        } => {
            IMAGE_OWNER.store(0, Ordering::SeqCst);
            if let Ok(mut last) = LAST_PNG.lock() {
                *last = None;
            }
            clear_image_claim();
            let ok_html = set_text_and_html(&clip, plain.clone(), html);
            TEXT_OWNER.store(
                if ok_html {
                    current_clipboard_owner()
                } else {
                    0
                },
                Ordering::SeqCst,
            );
            if !ok_html {
                tracing::warn!("gtk clipboard set_with_data failed — html not owned by agent");
            }
            if mirror_primary {
                let _ = set_text_only(&primary, plain);
            }
        }
        ClipboardOffer::Image { mime, bytes } => {
            TEXT_OWNER.store(0, Ordering::SeqCst);
            // Ctrl+V falls back to PRIMARY when CLIPBOARD has no UTF8 → old text.
            // A browser's selected text is separate user state. Clearing it
            // during bridge repair can freeze the tab; the PNG serves Ctrl+V.
            if !crate::clipboard::primary_owner_is_chromium_based() {
                primary.clear();
            }
            if !set_image_png_bmp(&clip, &mime, bytes) {
                tracing::warn!("gtk clipboard set_with_data failed — image not owned by agent");
            } else {
                IMAGE_OWNER.store(current_clipboard_owner(), Ordering::SeqCst);
            }
        }
        ClipboardOffer::Release => {
            // A delayed pause can arrive after an application made a private
            // copy. Release our own offer only; never clear that application's
            // selection when leaving the pool.
            let was_ours = owns_clipboard();
            TEXT_OWNER.store(0, Ordering::SeqCst);
            IMAGE_OWNER.store(0, Ordering::SeqCst);
            if let Ok(mut last) = LAST_PNG.lock() {
                *last = None;
            }
            clear_image_claim();
            if was_ours {
                clip.clear();
            }
        }
    }
}

/// Stop owning CLIPBOARD so Ctrl+C/Ctrl+V of the desktop session work again.
pub fn release_ownership() -> bool {
    try_offer(ClipboardOffer::Release)
}

fn set_text_and_html(clip: &Clipboard, plain: String, html: String) -> bool {
    const INFO_HTML: u32 = 3;
    let targets = [
        TargetEntry::new("text/html", TargetFlags::empty(), INFO_HTML),
        TargetEntry::new("UTF8_STRING", TargetFlags::empty(), INFO_TEXT),
        TargetEntry::new("STRING", TargetFlags::empty(), INFO_TEXT),
        TargetEntry::new("TEXT", TargetFlags::empty(), INFO_TEXT),
        TargetEntry::new("text/plain", TargetFlags::empty(), INFO_TEXT),
        TargetEntry::new("text/plain;charset=utf-8", TargetFlags::empty(), INFO_TEXT),
    ];
    clip.set_with_data(&targets, move |_cb, selection, info| {
        note_selection_served();
        if info == INFO_HTML {
            selection.set(&gdk::Atom::intern("text/html"), 8, html.as_bytes());
        } else if info == INFO_TEXT {
            selection.set_text(&plain);
        }
    })
}

fn set_text_only(clip: &Clipboard, text: String) -> bool {
    let targets = [
        TargetEntry::new("UTF8_STRING", TargetFlags::empty(), INFO_TEXT),
        TargetEntry::new("STRING", TargetFlags::empty(), INFO_TEXT),
        TargetEntry::new("TEXT", TargetFlags::empty(), INFO_TEXT),
        TargetEntry::new("text/plain", TargetFlags::empty(), INFO_TEXT),
        TargetEntry::new("text/plain;charset=utf-8", TargetFlags::empty(), INFO_TEXT),
    ];
    clip.set_with_data(&targets, move |_cb, selection, info| {
        note_selection_served();
        if info == INFO_TEXT {
            selection.set_text(&text);
        }
    })
}

fn set_image_png_bmp(clip: &Clipboard, mime: &str, bytes: Vec<u8>) -> bool {
    let png = ensure_png(mime, &bytes);
    // GDK's image clipboard consumer (including several Electron widgets)
    // asks for a bitmap target first.  Give it a *real* BMP: advertising an
    // empty BMP is what caused xrdp-chansrv / Chrome paste failures before.
    let bmp = png_to_bmp(&png);
    let hash = hash_bytes(&png);
    let trace = hash.get(..12.min(hash.len())).unwrap_or(&hash).to_string();
    if let Ok(mut last) = LAST_PNG.lock() {
        *last = Some(png.clone());
    }
    mark_image_claim();
    tracing::info!(
        "image-trace OFFER id={} mime=image/png bytes={} bmp_bytes={}",
        trace,
        png.len(),
        bmp.as_ref().map_or(0, Vec::len)
    );
    // Advertise only formats containing this image. XRDP 0.10.1 treats the
    // empty text targets as a text copy and FreeRDP never receives the bitmap.
    // Ordinary GTK image offers likewise omit text targets.
    let mut targets = vec![TargetEntry::new(
        "image/png",
        TargetFlags::empty(),
        INFO_PNG,
    )];
    if bmp.is_some() {
        targets.extend([
            TargetEntry::new("image/bmp", TargetFlags::empty(), INFO_BMP),
            TargetEntry::new("image/x-bmp", TargetFlags::empty(), INFO_BMP),
        ]);
    }
    clip.set_with_data(&targets, move |_cb, selection, info| {
        note_selection_served();
        if info == INFO_PNG {
            tracing::info!(
                "image-trace SERVE id={} target=image/png bytes={}",
                trace,
                png.len()
            );
            selection.set(&selection.target(), 8, &png);
        } else if info == INFO_BMP {
            if let Some(bmp) = bmp.as_ref() {
                tracing::info!(
                    "image-trace SERVE id={} target={} bytes={}",
                    trace,
                    selection.target().name(),
                    bmp.len()
                );
                selection.set(&selection.target(), 8, bmp);
            }
        }
    })
}

fn ensure_png(_mime: &str, bytes: &[u8]) -> Vec<u8> {
    if bytes.len() >= 4 && bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        return bytes.to_vec();
    }
    encode_png(bytes).unwrap_or_else(|| bytes.to_vec())
}

fn encode_png(bytes: &[u8]) -> Option<Vec<u8>> {
    use image::codecs::png::PngEncoder;
    use image::{ExtendedColorType, ImageEncoder, ImageReader};
    use std::io::Cursor;
    let img = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    let rgba = img.to_rgba8();
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some(out)
}

fn png_to_bmp(png: &[u8]) -> Option<Vec<u8>> {
    use image::codecs::bmp::BmpEncoder;
    use image::{ExtendedColorType, ImageEncoder, ImageReader};
    use std::io::Cursor;

    let image = ImageReader::new(Cursor::new(png))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    // Legacy native clipboard bridges consume CF_DIB with a 40-byte
    // BITMAPINFOHEADER. The RGBA encoder emits a V4 header; affected bridges
    // mistake its extra 68 bytes for 17 pixels on every clipboard round trip.
    // Keep PNG bytes (including alpha) intact, and use RGB only for this
    // compatibility fallback. Native RDP does not qualify transparent images.
    let rgb = image.to_rgb8();
    let mut bmp = Vec::new();
    BmpEncoder::new(&mut bmp)
        .write_image(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            ExtendedColorType::Rgb8,
        )
        .ok()?;
    Some(bmp)
}
