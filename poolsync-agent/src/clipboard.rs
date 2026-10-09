use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, GenericImageView, ImageEncoder, ImageReader};
use poolsync_core::{encode_message, encrypt_clipboard, hash_bytes, hash_text, Message};
use std::io::Cursor;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Instant;
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc::UnboundedSender};
use tokio::time::{timeout, Duration};

#[allow(dead_code)]
const IMAGE_MIMES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/jpg",
    "image/bmp",
    "image/x-xfce-ng",
    "image/x-qt-image",
];

const MIN_TEXT_SYNC_LEN: usize = 2;
/// Au-delà, Chromium parse le collage dans le DOM → « Page ne répond pas ».
const MAX_TEXT_BYTES: usize = 32 * 1024;
/// HTML trop gros = freeze Chrome. LibreOffice n'a besoin que d'un extrait.
const MAX_HTML_BYTES: usize = 16 * 1024;
const MAX_IMAGE_BYTES: usize = 12 * 1024 * 1024;
/// Timeout dur sur chaque lecture xclip : un propriétaire de sélection figé
/// (app morte, RDP…) ne doit jamais geler la boucle presse-papiers.
const XCLIP_READ_TIMEOUT: Duration = Duration::from_secs(2);
const XCLIP_TARGETS_TIMEOUT: Duration = Duration::from_millis(350);
const XCLIP_TEXT_TIMEOUT: Duration = Duration::from_millis(500);
const XCLIP_IMAGE_READ_TIMEOUT: Duration = Duration::from_secs(10);
const CLIPBOARD_PY_TIMEOUT: Duration = Duration::from_secs(3);
/// Ne pas relancer le secours GTK à chaque poll (évite python3 à 100 % CPU).
const GTK_READ_COOLDOWN: Duration = Duration::from_secs(3);
/// Après envoi d'une image, ignorer le texte résiduel sur le presse-papiers X11.
static LAST_IMAGE_SENT_AT: Mutex<Option<Instant>> = Mutex::new(None);
static LAST_GTK_READ_AT: Mutex<Option<Instant>> = Mutex::new(None);
static LAST_SELECTION_TS: Mutex<Option<String>> = Mutex::new(None);
static LAST_PAYLOAD_CACHE: Mutex<Option<ClipboardPayload>> = Mutex::new(None);
type ImageReadKey = (u32, Option<String>, Option<u64>);
static LAST_IMAGE_READ_KEY: Mutex<Option<ImageReadKey>> = Mutex::new(None);
static LAST_SELECTION_ID: Mutex<Option<(u32, Option<u64>)>> = Mutex::new(None);
static LAST_PRIMARY_TEXT: Mutex<Option<(String, Instant)>> = Mutex::new(None);
const PRIMARY_STABLE: Duration = Duration::from_millis(280);
static LAST_MIRROR: Mutex<Option<(String, Instant)>> = Mutex::new(None);
static LAST_MIRROR_TEXT: Mutex<Option<String>> = Mutex::new(None);
/// Last PRIMARY string actually pushed to CLIPBOARD. Never re-apply the same
/// selection — that stomps incoming copies and the user's Ctrl+C.
static LAST_PRIMARY_APPLIED: Mutex<Option<String>> = Mutex::new(None);
static XRDP_ACTIVE_CACHE: Mutex<Option<(bool, Instant)>> = Mutex::new(None);
const XRDP_CACHE_TTL: Duration = Duration::from_secs(30);
const MIRROR_DEDUPE: Duration = Duration::from_millis(150);
static LAST_LOCAL_IMG_HASH: Mutex<String> = Mutex::new(String::new());
static LAST_IMAGE_PROBE_AT: Mutex<Option<Instant>> = Mutex::new(None);
const IMAGE_PROBE_COOLDOWN: Duration = Duration::from_millis(800);

/// Mémorise le texte entrant appliqué pour que la boucle de réparation xrdp
/// n'écrase pas le collage avec une ancienne sélection PRIMARY locale.
pub fn record_incoming_applied(text: &str) {
    if let Ok(mut g) = LAST_PRIMARY_APPLIED.lock() {
        *g = Some(text.to_string());
    }
    if let Ok(mut g) = LAST_MIRROR_TEXT.lock() {
        *g = Some(text.to_string());
    }
    if let Ok(mut g) = LAST_PRIMARY_TEXT.lock() {
        *g = Some((text.to_string(), Instant::now()));
    }
}

/// Snapshot the PRIMARY selection before an incoming XRDP image is offered.
/// XRDP frequently leaves old text there; it must not be mistaken for a new
/// user Ctrl+C and evict the image a few milliseconds later.
pub async fn seed_primary_baseline() {
    let raw = match read_selection_bytes("primary", "UTF8_STRING").await {
        Ok(bytes) => bytes,
        Err(_) => return,
    };
    let text = sanitize_copied_text(&String::from_utf8_lossy(&raw));
    if !is_syncable_text(&text) || text_is_image_sidecar(&text) {
        return;
    }
    if let Ok(mut applied) = LAST_PRIMARY_APPLIED.lock() {
        *applied = Some(text.clone());
    }
    if let Ok(mut observed) = LAST_PRIMARY_TEXT.lock() {
        *observed = Some((text, Instant::now()));
    }
}

fn primary_differs_from_applied(text: &str) -> bool {
    LAST_PRIMARY_APPLIED
        .lock()
        .ok()
        .is_none_or(|applied| primary_is_newer_than_applied(applied.as_deref(), text))
}

fn primary_is_newer_than_applied(applied: Option<&str>, candidate: &str) -> bool {
    applied != Some(candidate)
}

/// Empty X11 selections are normal between copies — not an error state.
pub async fn selections_dead() -> bool {
    false
}

/// Stable PRIMARY text that is a real user copy (not screenshot path sidecar).
async fn primary_user_text_override() -> Option<String> {
    if xrdp_session_active() && primary_owner_is_chromium_based() {
        return None;
    }
    let text = stable_primary_text().await?;
    if is_syncable_text(&text) && !text_is_image_sidecar(&text) {
        Some(text)
    } else {
        None
    }
}

/// Local xrdp clipboard repair — runs even when network clipboard sync is OFF.
pub async fn maintain_xrdp_clipboard_fixup() {
    if !xrdp_session_active() {
        return;
    }
    // Rewriting CLIPBOARD while Chromium owns it freezes the tab ("Attendre / Quitter").
    if clipboard_owner_is_chromium_based() {
        return;
    }
    // Selecting text in Chrome changes PRIMARY without being a Ctrl+C. Never
    // mirror that selection into CLIPBOARD. A separate BMP-only CLIPBOARD
    // owned by the bridge still needs its own guarded image repair.
    if primary_owner_is_chromium_based() {
        let targets = clipboard_targets("clipboard").await.unwrap_or_default();
        recover_stripped_image(&targets).await;
        return;
    }
    // xrdp-chansrv strips image/png within seconds — keep PNG alive for Ctrl+V.
    if crate::clipboard_gtk::recent_image_claim_active() {
        // Ctrl+C lands on PRIMARY — do not let stale image keepalive block new text.
        if let Some(text) = primary_user_text_override()
            .await
            .filter(|text| primary_differs_from_applied(text))
        {
            crate::clipboard_gtk::clear_image_claim();
            clear_image_clipboard_epoch();
            if mirror_text_to_selections(&text) {
                tracing::info!(
                    "clipboard keepalive: PRIMARY text overrides stale image ({} chars)",
                    text.len()
                );
            }
            return;
        }
        let clip_targets = clipboard_targets("clipboard").await.unwrap_or_default();
        if recover_stripped_image(&clip_targets).await {
            tracing::info!("clipboard keepalive: chansrv stripped PNG — reoffer");
        }
        return;
    }
    mirror_primary_to_clipboard_if_needed().await;
    mirror_primary_image_if_needed().await;
    fix_local_clipboard_image().await;
}

/// True when this agent runs inside an xrdp session (chansrv owns clip sync).
pub fn xrdp_session_active_sync() -> bool {
    xrdp_session_active()
}

/// True when this agent runs inside an xrdp session (chansrv owns clip sync).
fn xrdp_session_active() -> bool {
    let now = Instant::now();
    if let Ok(mut g) = XRDP_ACTIVE_CACHE.lock() {
        if let Some((v, at)) = *g {
            if now.duration_since(at) < XRDP_CACHE_TTL {
                return v;
            }
        }
        let v = xrdp_session_active_scan();
        *g = Some((v, now));
        return v;
    }
    false
}

fn xrdp_session_active_scan() -> bool {
    let disp = std::env::var("DISPLAY").unwrap_or_default();
    let disp = disp.strip_suffix(".0").unwrap_or(&disp);
    if disp.is_empty() {
        return false;
    }
    let uid = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|u| u.parse::<u32>().ok())
        })
        .unwrap_or(0);
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let pid = entry
                .file_name()
                .to_string_lossy()
                .parse::<u32>()
                .ok()
                .unwrap_or(0);
            if pid == 0 {
                continue;
            }
            let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
            if comm.trim() != "xrdp-chansrv" {
                continue;
            }
            let owner = std::fs::read_to_string(format!("/proc/{pid}/status"))
                .ok()
                .and_then(|s| {
                    s.lines()
                        .find(|l| l.starts_with("Uid:"))
                        .and_then(|l| l.split_whitespace().nth(1))
                        .and_then(|u| u.parse::<u32>().ok())
                });
            if owner != Some(uid) {
                continue;
            }
            let ch_disp = std::fs::read(format!("/proc/{pid}/environ"))
                .ok()
                .and_then(|raw| {
                    for slice in raw.split(|b| *b == 0) {
                        if let Ok(s) = std::str::from_utf8(slice) {
                            if let Some(d) = s.strip_prefix("DISPLAY=") {
                                let d = d.strip_suffix(".0").unwrap_or(d);
                                return Some(d.to_string());
                            }
                        }
                    }
                    None
                })
                .unwrap_or_default();
            if ch_disp == disp {
                return true;
            }
        }
    }
    false
}

fn clipboard_owner_is_chromium_based() -> bool {
    selection_owner_is_chromium_based(b"CLIPBOARD")
}

/// An image repair may follow a stripped/empty image, never a genuine text copy.
pub fn image_recovery_allowed(targets: &[String]) -> bool {
    !targets_have_pasteable_image(targets) && !targets_advertise_text(targets)
}

pub(crate) fn primary_owner_is_chromium_based() -> bool {
    selection_owner_is_chromium_based(b"PRIMARY")
}

fn selection_owner_is_chromium_based(selection: &[u8]) -> bool {
    use x11rb::protocol::xproto::ConnectionExt;
    let Ok((conn, _)) = x11rb::connect(None) else {
        return false;
    };
    let Ok(cookie) = conn.intern_atom(false, selection) else {
        return false;
    };
    let Ok(atom) = cookie.reply() else {
        return false;
    };
    let Ok(owner_cookie) = conn.get_selection_owner(atom.atom) else {
        return false;
    };
    let Ok(owner) = owner_cookie.reply() else {
        return false;
    };
    if owner.owner == 0 {
        return false;
    }
    wm_class_is_chromium_based(&conn, owner.owner)
}

fn wm_class_is_chromium_based(
    conn: &x11rb::rust_connection::RustConnection,
    mut current: x11rb::protocol::xproto::Window,
) -> bool {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};
    for _ in 0..6 {
        if current == 0 {
            break;
        }
        let Ok(cookie) =
            conn.get_property(false, current, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
        else {
            break;
        };
        if let Ok(reply) = cookie.reply() {
            if chromium_based_identity(&reply.value) {
                return true;
            }
        }
        // Chromium's dedicated X11 selection owner is commonly a root child
        // named "Chromium clipboard" with no WM_CLASS at all.
        let Ok(name_cookie) =
            conn.get_property(false, current, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 256)
        else {
            break;
        };
        if let Ok(reply) = name_cookie.reply() {
            if chromium_based_identity(&reply.value) {
                return true;
            }
        }
        let Ok(tree) = conn.query_tree(current) else {
            break;
        };
        let Ok(reply) = tree.reply() else {
            break;
        };
        if reply.parent == 0 || reply.parent == current {
            break;
        }
        current = reply.parent;
    }
    false
}

/// Applications bâties sur Chromium dont le `WM_CLASS` ne contient ni « chrom »
/// ni « electron » : c'est le cas de VSCode (`code`) et de Cursor (`cursor`),
/// donc des deux éditeurs qui gelaient au collage. Comparaison jeton par jeton,
/// pour ne pas confondre « code » avec un « barcode-scanner ».
const CHROMIUM_BASED_CLASSES: &[&str] = &[
    "code", "code-oss", "codium", "vscodium", "cursor", "windsurf", "slack", "discord", "signal",
    "obsidian", "postman", "spotify",
];

fn chromium_based_identity(raw: &[u8]) -> bool {
    let name = String::from_utf8_lossy(raw).to_ascii_lowercase();
    if name.contains("chrom")
        || name.contains("firefox")
        || name.contains("navigator")
        || name.contains("brave")
        || name.contains("electron")
    {
        return true;
    }
    // WM_CLASS = deux chaînes séparées par NUL (« code\0Code\0 »).
    name.split(|c: char| c == '\0' || c.is_whitespace())
        .any(|token| CHROMIUM_BASED_CLASSES.contains(&token))
}

fn mirror_text_to_selections(text: &str) -> bool {
    // CLIPBOARD only — never PRIMARY. Rewriting PRIMARY while Chrome has a
    // selection freezes the page ("Page ne répond pas").
    if xrdp_session_active() && write_selection_text_sync("clipboard", text).is_ok() {
        if let Ok(mut g) = LAST_MIRROR_TEXT.lock() {
            *g = Some(text.to_string());
        }
        return true;
    }
    if crate::clipboard_gtk::try_offer(crate::clipboard_gtk::ClipboardOffer::Text {
        text: text.to_string(),
        mirror_primary: !xrdp_session_active(),
    }) {
        if let Ok(mut g) = LAST_MIRROR_TEXT.lock() {
            *g = Some(text.to_string());
        }
        return true;
    }
    false
}

/// xrdp/XFCE: copy lands on PRIMARY, CLIPBOARD stays empty → Ctrl+V fails.
pub async fn mirror_primary_to_clipboard_if_needed() {
    if xrdp_session_active() && primary_owner_is_chromium_based() {
        return;
    }
    let pri_text = if xrdp_session_active() {
        match stable_primary_text().await {
            Some(t) => t,
            None => return,
        }
    } else {
        let raw_pri =
            match read_selection_bytes_timeout("primary", "UTF8_STRING", XCLIP_TEXT_TIMEOUT)
                .await
                .ok()
            {
                Some(b) => b,
                None => return,
            };
        if looks_like_image_bytes(&raw_pri) {
            return;
        }
        let pri_text = sanitize_copied_text(&String::from_utf8_lossy(&raw_pri));
        if !is_syncable_text(&pri_text) {
            return;
        }
        pri_text
    };
    if text_is_image_sidecar(&pri_text) {
        return;
    }

    // Same PRIMARY we already pushed: do not write again (would erase a newer
    // CLIPBOARD copy from the user or from the pool).
    if let Ok(mut g) = LAST_PRIMARY_APPLIED.lock() {
        if g.is_none() {
            // Seed on startup: leftover PRIMARY must not replay over CLIPBOARD.
            *g = Some(pri_text);
            return;
        }
        if g.as_deref() == Some(pri_text.as_str()) {
            return;
        }
    }

    let clip_text = read_plain_text_timeout(XCLIP_TEXT_TIMEOUT).await;
    if clip_text.as_deref() == Some(pri_text.as_str()) {
        if let Ok(mut g) = LAST_PRIMARY_APPLIED.lock() {
            *g = Some(pri_text);
        }
        return;
    }

    // Incoming mesh copy or a prior mirror: do not replace with PRIMARY selection
    // noise while the user is about to Ctrl+V (that race freezes Chromium).
    if let Ok(g) = LAST_MIRROR_TEXT.lock() {
        if clip_text.as_deref() == g.as_deref() {
            return;
        }
    }

    // Même règle sous xrdp qu'ailleurs : PRIMARY est un secours, jamais un
    // remplaçant. Écraser un CLIPBOARD qui contient déjà du texte, c'était
    // rejouer indéfiniment un surlignage par-dessus les copies du pool — et
    // changer de propriétaire de sélection sous les doigts de l'utilisateur,
    // ce qui fige Chromium en plein Ctrl+V.
    if clip_text.is_some() {
        return;
    }
    // Une lecture qui *échoue* n'est pas un presse-papiers vide. Quand une
    // application colle en boucle, notre lecture expire — et croire le
    // CLIPBOARD vide à ce moment-là revient à y réinjecter un vieux surlignage
    // par-dessus la copie qui vient d'arriver (observé en test le 30/08 : la
    // copie d'desk-a atterrissait sur work-a, puis disparaissait aussitôt).
    // On ne mirroite donc que si la sélection n'annonce aucune cible texte.
    let clip_targets = clipboard_targets("clipboard").await.unwrap_or_default();
    if targets_advertise_text(&clip_targets) {
        tracing::debug!("clipboard mirror: CLIPBOARD illisible mais non vide — miroir annulé");
        return;
    }

    let now = Instant::now();
    if let Ok(mut g) = LAST_MIRROR.lock() {
        if let Some((prev, at)) = g.as_ref() {
            if prev == &pri_text && now.duration_since(*at) < MIRROR_DEDUPE {
                return;
            }
        }
        *g = Some((pri_text.clone(), now));
    }
    if mirror_text_to_selections(&pri_text) {
        if let Ok(mut g) = LAST_PRIMARY_APPLIED.lock() {
            *g = Some(pri_text.clone());
        }
        tracing::info!(
            "clipboard mirror PRIMARY→CLIPBOARD ({} chars)",
            pri_text.len()
        );
    } else {
        tracing::warn!("clipboard mirror PRIMARY→CLIPBOARD failed");
    }
}

async fn mirror_primary_image_if_needed() {
    let clip_targets = clipboard_targets("clipboard").await.unwrap_or_default();
    if targets_have_pasteable_image(&clip_targets) {
        return;
    }
    let pri_targets = clipboard_targets("primary").await.unwrap_or_default();
    if !targets_have_pasteable_image(&pri_targets) {
        return;
    }
    for mime in ["image/png", "image/jpeg", "image/jpg"] {
        if let Ok(bytes) = read_selection_bytes("primary", mime).await {
            if bytes.len() >= 8 {
                let hash = hash_bytes(&bytes);
                let mime_used = if mime == "image/jpg" {
                    "image/jpeg"
                } else {
                    mime
                };
                let nbytes = bytes.len();
                offer_local_image_if_needed(mime_used, bytes, &hash);
                tracing::info!(
                    "clipboard mirror PRIMARY image→CLIPBOARD mime={} bytes={}",
                    mime_used,
                    nbytes
                );
                return;
            }
        }
    }
}

/// Only the actual XRDP clipboard bridge may recover a stripped offer.
/// A BMP target is read first: it can be a genuinely new screenshot.
pub async fn recover_stripped_image(targets: &[String]) -> bool {
    if !image_recovery_allowed(targets) || !clipboard_owner_is_chansrv() {
        return false;
    }
    let guard = crate::clipboard_gtk::SelectionGuard::capture();
    let recovery = recover_current_bridge_image(guard);
    tokio::pin!(recovery);
    loop {
        tokio::select! {
            result = &mut recovery => return result,
            _ = tokio::time::sleep(Duration::from_millis(40)) => {
                if !guard.unchanged() {
                    // Dropping the conversion future cancels its native request
                    // window. A slow old owner must not block a fresh copy.
                    tracing::debug!("bridge image recovery cancelled after a newer selection");
                    return false;
                }
            }
        }
    }
}

async fn recover_current_bridge_image(guard: crate::clipboard_gtk::SelectionGuard) -> bool {
    // The caller's TARGETS may precede a new copy. Recheck metadata under the
    // same owner/epoch/generation guard used by the eventual GTK offer.
    let Ok(current_targets) = clipboard_targets("clipboard").await else {
        return false;
    };
    if !guard.unchanged()
        || !image_recovery_allowed(&current_targets)
        || !clipboard_owner_is_chansrv()
    {
        return false;
    }
    if is_rdp_bmp_only(&current_targets) {
        // A valid BMP is not a browser-compatible offer. Convert the CURRENT
        // pixels even when publication deduplication has already consumed this
        // image. Never replace them with the previously cached PNG.
        if let Ok(bytes) = read_selection_bytes("clipboard", "image/bmp").await {
            if let Ok(payload) = image_payload_from_bytes(&bytes) {
                // First let the ordinary source reader capture this epoch for
                // publication. Taking ownership before that read would make
                // it skip our own PNG and swallow a new native bridge copy.
                // Once captured, repair is independent of publication/hash
                // deduplication and still uses the current guarded pixels.
                let key = (
                    crate::clipboard_gtk::current_clipboard_owner(),
                    clipboard_timestamp().await,
                    crate::clipboard_epoch::current(),
                );
                let captured = LAST_IMAGE_READ_KEY
                    .lock()
                    .ok()
                    .is_some_and(|last| last.as_ref() == Some(&key));
                if !captured || !guard.unchanged() {
                    return false;
                }
                let Ok(png) = B64.decode(&payload.wire_data) else {
                    return false;
                };
                return crate::clipboard_gtk::try_offer_if_unchanged(
                    crate::clipboard_gtk::ClipboardOffer::Image {
                        mime: payload.mime,
                        bytes: png,
                    },
                    guard,
                );
            }
        }
    }
    crate::clipboard_gtk::recent_image_claim_active()
        && crate::clipboard_gtk::reoffer_last_image(guard)
}

fn clipboard_owner_is_chansrv() -> bool {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};
    let Ok((conn, _)) = x11rb::connect(None) else {
        return false;
    };
    let owner = crate::clipboard_gtk::current_clipboard_owner();
    if owner == 0 {
        return false;
    }
    // chansrv sets this property on its otherwise unnamed clipboard window.
    let Ok(atom) = conn.intern_atom(true, b"XRDP_GET_TIME_ATOM") else {
        return false;
    };
    let Ok(atom) = atom.reply() else {
        return false;
    };
    if atom.atom == 0 {
        return false;
    }
    conn.get_property(false, owner, atom.atom, AtomEnum::ANY, 0, 0)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .is_some_and(|property| property.type_ != u32::from(AtomEnum::NONE))
}

async fn fix_local_clipboard_image() {
    let guard = crate::clipboard_gtk::SelectionGuard::capture();
    let targets = clipboard_targets("clipboard").await.unwrap_or_default();
    if !is_rdp_bmp_only(&targets) {
        return;
    }
    if let Some(payload) = read_image_payload(&targets).await {
        if let Ok(bytes) = B64.decode(&payload.wire_data) {
            crate::clipboard_gtk::try_offer_if_unchanged(
                crate::clipboard_gtk::ClipboardOffer::Image {
                    mime: payload.mime,
                    bytes,
                },
                guard,
            );
        }
    } else {
        recover_stripped_image(&targets).await;
    }
}

fn offer_local_image_if_needed(mime: &str, bytes: Vec<u8>, hash: &str) {
    if let Ok(mut g) = LAST_LOCAL_IMG_HASH.lock() {
        if g.as_str() == hash {
            return;
        }
        *g = hash.to_string();
    }
    crate::clipboard_gtk::try_offer(crate::clipboard_gtk::ClipboardOffer::Image {
        mime: mime.to_string(),
        bytes,
    });
}

async fn stable_primary_text() -> Option<String> {
    let raw = match read_text_selection_bytes("primary", "UTF8_STRING", XCLIP_TEXT_TIMEOUT).await {
        Ok(b) => b,
        Err(_) => match read_text_selection_bytes("primary", "STRING", XCLIP_TEXT_TIMEOUT).await {
            Ok(b) => b,
            Err(_) => read_text_selection_bytes("primary", "TEXT", XCLIP_TEXT_TIMEOUT)
                .await
                .ok()?,
        },
    };
    if raw.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        return None;
    }
    let text = sanitize_copied_text(&String::from_utf8_lossy(&raw));
    if !is_syncable_text(&text) {
        return None;
    }
    let now = Instant::now();
    let mut guard = LAST_PRIMARY_TEXT.lock().ok()?;
    match guard.as_ref() {
        Some((prev, at)) if prev == &text => {
            if now.duration_since(*at) >= PRIMARY_STABLE {
                Some(text)
            } else {
                None
            }
        }
        _ => {
            *guard = Some((text, now));
            None
        }
    }
}

pub fn mark_image_clipboard_epoch() {
    if let Ok(mut guard) = LAST_IMAGE_SENT_AT.lock() {
        *guard = Some(Instant::now());
    }
}

fn clear_image_clipboard_epoch() {
    if let Ok(mut guard) = LAST_IMAGE_SENT_AT.lock() {
        *guard = None;
    }
}

fn gtk_read_allowed() -> bool {
    LAST_GTK_READ_AT
        .lock()
        .ok()
        .and_then(|g| *g)
        .is_none_or(|t| t.elapsed() >= GTK_READ_COOLDOWN)
}

fn note_gtk_read_attempt() {
    if let Ok(mut guard) = LAST_GTK_READ_AT.lock() {
        *guard = Some(Instant::now());
    }
}

/// Read a single correlated native target with a bounded, cancellable lifetime.
async fn selection_read_timeout(args: &[&str], limit: Duration) -> Result<std::process::Output> {
    use std::os::unix::process::ExitStatusExt;
    let _internal = crate::clipboard_gtk::InternalRead::begin();
    anyhow::ensure!(
        args.len() == 5 && args[0] == "-selection" && args[2] == "-t" && args[4] == "-o",
        "invalid native selection request"
    );
    let (status, stdout) = match crate::clipboard_x11::read(args[1], args[3], limit).await {
        Ok(bytes) => (0, bytes),
        Err(error) if error.is::<crate::clipboard_x11::UnsupportedTarget>() => (1 << 8, Vec::new()),
        Err(error) => return Err(error),
    };
    Ok(std::process::Output {
        status: std::process::ExitStatus::from_raw(status),
        stdout,
        stderr: Vec::new(),
    })
}

/// TIMESTAMP X11 (petit) — pas le contenu. Si inchangé, on ne relit pas l'image.
/// Dernière horloge X11 observée, avec l'instant local correspondant.
/// Sert d'ancre quand la lecture directe de TIMESTAMP échoue — ce qui arrive
/// justement sur les sélections dégradées, là où le garde-fou est nécessaire.
static LAST_SERVER_CLOCK: Mutex<Option<(u64, Instant)>> = Mutex::new(None);

fn note_server_clock(ts: &str) {
    let Ok(value) = ts.trim().parse::<u64>() else {
        return;
    };
    if let Ok(mut g) = LAST_SERVER_CLOCK.lock() {
        *g = Some((value, Instant::now()));
    }
}

/// Estimation de l'horloge du serveur X maintenant, extrapolée depuis la
/// dernière valeur lue. `None` tant qu'aucune valeur n'a jamais été observée.
fn estimated_server_clock() -> Option<u64> {
    let (value, at) = (*LAST_SERVER_CLOCK.lock().ok()?)?;
    Some(value.saturating_add(at.elapsed().as_millis() as u64))
}

async fn clipboard_timestamp() -> Option<String> {
    if let Some(timestamp) = crate::clipboard_epoch::selection_timestamp() {
        let value = timestamp.to_string();
        note_server_clock(&value);
        return Some(value);
    }
    // Some native owners return their PNG for unsupported targets. A timed
    // metadata probe can then abandon an INCR transfer and strand subsequent
    // reads. Ask only for metadata the owner actually advertises.
    if !clipboard_targets("clipboard")
        .await
        .ok()?
        .iter()
        .any(|t| t == "TIMESTAMP")
    {
        return None;
    }
    let output = selection_read_timeout(
        &["-selection", "clipboard", "-t", "TIMESTAMP", "-o"],
        Duration::from_millis(400),
    )
    .await
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = output.stdout;
    if raw.len() > 24 || raw.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        return None;
    }
    let s = String::from_utf8_lossy(&raw).trim().to_string();
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    note_server_clock(&s);
    Some(s)
}

fn payload_cache_hit(ts: &str) -> Option<ClipboardPayload> {
    let last_ts = LAST_SELECTION_TS.lock().ok()?;
    if last_ts.as_deref() != Some(ts) {
        return None;
    }
    LAST_PAYLOAD_CACHE.lock().ok()?.clone()
}

fn remember_payload(ts: Option<String>, payload: Option<ClipboardPayload>) {
    if let (Ok(mut t), Some(ts)) = (LAST_SELECTION_TS.lock(), ts) {
        *t = Some(ts);
    }
    if let Ok(mut c) = LAST_PAYLOAD_CACHE.lock() {
        *c = payload;
    }
}

pub fn invalidate_payload_cache() {
    if let Ok(mut t) = LAST_SELECTION_TS.lock() {
        *t = None;
    }
    if let Ok(mut c) = LAST_PAYLOAD_CACHE.lock() {
        *c = None;
    }
}

pub struct ClipboardPayload {
    pub mime: String,
    pub wire_data: String,
    pub hash: String,
}

/// GTK/X11 remains the default on the existing fleet. On a native Wayland
/// session, wl-clipboard provides a compositor-neutral data-control backend.
fn wayland_session() -> bool {
    std::env::var("XDG_SESSION_TYPE").is_ok_and(|value| value.eq_ignore_ascii_case("wayland"))
        && std::env::var_os("WAYLAND_DISPLAY").is_some()
}

async fn wl_paste(args: &[&str], limit: Duration) -> Result<std::process::Output> {
    let mut command = Command::new("wl-paste");
    command.args(args).kill_on_drop(true);
    timeout(limit, command.output())
        .await
        .context("wl-paste timeout")?
        .context("spawn wl-paste")
}

async fn read_wayland_clipboard(
    allow_images: bool,
    keep_formatting: bool,
) -> Result<Option<ClipboardPayload>> {
    let types = wl_paste(&["--list-types"], XCLIP_TARGETS_TIMEOUT).await?;
    if !types.status.success() {
        return Ok(None);
    }
    let types_text = String::from_utf8_lossy(&types.stdout);
    let types: Vec<&str> = types_text.lines().map(str::trim).collect();

    let plain_type = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"]
        .into_iter()
        .find(|mime| types.contains(mime));
    if keep_formatting && types.contains(&"text/html") {
        let output = wl_paste(&["--no-newline", "--type", "text/html"], XCLIP_READ_TIMEOUT).await?;
        if output.status.success()
            && !output.stdout.is_empty()
            && output.stdout.len() <= MAX_HTML_BYTES
        {
            let html = String::from_utf8_lossy(&output.stdout).into_owned();
            if looks_like_markup(&html) {
                return Ok(Some(ClipboardPayload {
                    hash: hash_text(&html),
                    mime: "text/html".into(),
                    wire_data: html,
                }));
            }
        }
    }
    if let Some(mime) = plain_type {
        let output = wl_paste(&["--no-newline", "--type", mime], XCLIP_READ_TIMEOUT).await?;
        if output.status.success() && output.stdout.len() <= MAX_TEXT_BYTES {
            let text = sanitize_copied_text(&String::from_utf8_lossy(&output.stdout));
            if is_syncable_text(&text) {
                return Ok(Some(ClipboardPayload {
                    hash: hash_text(&text),
                    mime: "text/plain".into(),
                    wire_data: text,
                }));
            }
        }
    }
    if allow_images {
        if let Some(mime) = ["image/png", "image/bmp", "image/jpeg"]
            .into_iter()
            .find(|mime| types.contains(mime))
        {
            let output = wl_paste(&["--type", mime], XCLIP_IMAGE_READ_TIMEOUT).await?;
            if output.status.success() && !output.stdout.is_empty() {
                return image_payload_from_bytes(&output.stdout).map(Some);
            }
        }
    }
    Ok(None)
}

fn write_wayland_clipboard_sync(data: &str, mime: &str) -> Result<()> {
    let bytes = if mime.starts_with("image/") {
        B64.decode(data)
            .with_context(|| format!("decode base64 image ({mime})"))?
    } else if mime == "text/html" || mime == "text/plain" {
        data.as_bytes().to_vec()
    } else {
        anyhow::bail!("unsupported Wayland clipboard mime: {mime}");
    };
    let mut child = std::process::Command::new("wl-copy")
        .args(["--type", mime])
        .stdin(Stdio::piped())
        .spawn()
        .context("spawn wl-copy")?;
    use std::io::Write as _;
    child
        .stdin
        .take()
        .context("wl-copy stdin")?
        .write_all(&bytes)
        .context("write wl-copy stdin")?;
    let status = child.wait().context("wait wl-copy")?;
    if !status.success() {
        anyhow::bail!("wl-copy failed with {status}");
    }
    Ok(())
}

impl Clone for ClipboardPayload {
    fn clone(&self) -> Self {
        Self {
            mime: self.mime.clone(),
            wire_data: self.wire_data.clone(),
            hash: self.hash.clone(),
        }
    }
}

/// Atomes que le serveur X annonce dans `TARGETS` — jamais du contenu copié.
const X11_TARGET_ATOMS: &[&str] = &[
    "TIMESTAMP",
    "TARGETS",
    "MULTIPLE",
    "SAVE_TARGETS",
    "UTF8_STRING",
    "STRING",
    "TEXT",
    "COMPOUND_TEXT",
    "DELETE",
    "INSERT_SELECTION",
    "INSERT_PROPERTY",
    "ATOM",
    "ATOM_PAIR",
    "INCR",
    "NULL",
    "PIXMAP",
    "OWNER_OS",
    "HOST_NAME",
    "USER",
];

/// Cibles texte que l'on a le droit de demander pour obtenir du *contenu*.
/// Toute autre cible (TIMESTAMP, TARGETS…) renvoie des métadonnées : les lire
/// comme du texte, c'est diffuser nos propres sondes dans le pool.
const X11_TEXT_TARGETS: &[&str] = &[
    "UTF8_STRING",
    "STRING",
    "TEXT",
    "COMPOUND_TEXT",
    "text/plain",
    "text/plain;charset=utf-8",
    "text/html",
];

fn is_x11_target_atom(line: &str) -> bool {
    let l = line.trim();
    if X11_TARGET_ATOMS.iter().any(|a| a.eq_ignore_ascii_case(l)) {
        return true;
    }
    // Un type MIME annoncé comme cible (`image/png`, `text/plain;charset=utf-8`).
    !l.is_empty() && !l.contains(char::is_whitespace) && l.contains('/') && l.len() < 64
}

/// `xclip -t TARGETS -o` renvoie la liste des cibles, une par ligne. Sur un
/// propriétaire de sélection à moitié mort (annonce des cibles texte mais
/// refuse de les servir), c'est la seule sonde qui répond — et sa sortie a
/// déjà été diffusée à tout le pool comme si l'utilisateur l'avait copiée.
///
/// Require recognizable metadata atoms, or TARGETS with actual MIME targets.
/// Owners may omit TIMESTAMP/MULTIPLE, leaving a minimal TARGETS+image list.
/// A user list containing only paths or MIME types remains ordinary text.
pub fn is_target_list_dump(text: &str) -> bool {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.len() < 2 {
        return false;
    }
    if !lines.iter().all(|l| is_x11_target_atom(l)) {
        return false;
    }
    let known = lines
        .iter()
        .filter(|l| X11_TARGET_ATOMS.iter().any(|a| a.eq_ignore_ascii_case(l)))
        .count();
    known >= 2
        || (lines
            .iter()
            .any(|line| line.eq_ignore_ascii_case("TARGETS"))
            && lines.iter().any(|line| {
                line.starts_with("image/")
                    || line.starts_with("text/")
                    || line.starts_with("application/")
            }))
}

/// La sélection annonce-t-elle au moins une cible texte ?
pub fn targets_advertise_text(targets: &[String]) -> bool {
    targets
        .iter()
        .any(|t| X11_TEXT_TARGETS.iter().any(|k| k.eq_ignore_ascii_case(t)))
}

pub fn targets_have_image(targets: &[String]) -> bool {
    targets.iter().any(|t| t.starts_with("image/"))
}

pub fn targets_have_pasteable_image(targets: &[String]) -> bool {
    targets.iter().any(|t| {
        let l = t.to_ascii_lowercase();
        l == "image/png" || l == "image/jpeg" || l == "image/jpg"
    })
}

/// xrdp CF_DIB: TARGETS lists image/bmp only (often empty). Chrome cannot paste.
pub fn is_rdp_bmp_only(targets: &[String]) -> bool {
    targets_have_image(targets) && !targets_have_pasteable_image(targets)
}

pub async fn clipboard_targets(selection: &str) -> Result<Vec<String>> {
    let output = selection_read_timeout(
        &["-selection", selection, "-t", "TARGETS", "-o"],
        XCLIP_TARGETS_TIMEOUT,
    )
    .await?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

pub async fn read_clipboard_payload_filtered(
    allow_images: bool,
    keep_formatting: bool,
) -> Result<Option<ClipboardPayload>> {
    if wayland_session() {
        return read_wayland_clipboard(allow_images, keep_formatting).await;
    }
    let identity = (
        crate::clipboard_gtk::current_clipboard_owner(),
        crate::clipboard_epoch::current(),
    );
    if let Ok(mut last) = LAST_SELECTION_ID.lock() {
        if *last != Some(identity) {
            invalidate_payload_cache();
            *last = Some(identity);
        }
    }
    let ts = clipboard_timestamp().await;
    if let Some(ref ts) = ts {
        if let Some(cached) = payload_cache_hit(ts) {
            if cached.mime.starts_with("image/") && allow_images {
                return Ok(Some(cached));
            }
            // Never reuse cached text: Chrome can copy a new email while X11
            // TIMESTAMP stays the same (we still own CLIPBOARD) → old junk pasted.
        }
    }

    let read = read_clipboard_payload_uncached(allow_images, keep_formatting);
    tokio::pin!(read);
    let payload = loop {
        tokio::select! {
            payload = &mut read => break payload?,
            _ = tokio::time::sleep(Duration::from_millis(40)) => {
                if identity != (
                    crate::clipboard_gtk::current_clipboard_owner(),
                    crate::clipboard_epoch::current(),
                ) {
                    // Dropping the read cancels its native request window.
                    // A hung owner cannot delay a fresh native selection.
                    invalidate_payload_cache();
                    if let Ok(mut last) = LAST_IMAGE_READ_KEY.lock() { *last = None; }
                    tracing::debug!("clipboard read cancelled after a newer selection");
                    return Ok(None);
                }
            }
        }
    };
    // A native copy may replace the selection while an asynchronous read is
    // running. Do not publish an older image under the newer copy's identity.
    if identity
        != (
            crate::clipboard_gtk::current_clipboard_owner(),
            crate::clipboard_epoch::current(),
        )
    {
        invalidate_payload_cache();
        // The reader may already have remembered the new owner while the
        // outer operation was still bound to the old selection. Retry it.
        if let Ok(mut last) = LAST_IMAGE_READ_KEY.lock() {
            *last = None;
        }
        return Ok(None);
    }
    // Deuxième sonde diffusée par erreur dans le pool le 29/08 : la valeur de
    // `xclip -t TIMESTAMP -o`. La comparaison est exacte, donc un vrai numéro
    // copié par l'utilisateur (téléphone, référence) n'est jamais rejeté — il
    // faudrait qu'il soit égal à l'horloge X11 de la sélection au même instant.
    // La lecture directe de TIMESTAMP échoue précisément sur les sélections
    // dégradées : se rabattre sur l'horloge estimée, sinon le garde-fou saute
    // exactement dans le cas qu'il doit couvrir.
    if let Some(p) = payload.as_ref() {
        let reference = ts
            .as_deref()
            .and_then(|t| t.trim().parse::<u64>().ok())
            .or_else(estimated_server_clock);
        if let Some(reference) = reference {
            if is_server_clock_echo(&p.mime, &p.wire_data, reference) {
                tracing::warn!(
                    "clipboard: horloge X11 lue comme du texte ({}, horloge ≈ {reference}) — ignoré",
                    p.wire_data.trim()
                );
                return Ok(None);
            }
        }
    }
    if payload
        .as_ref()
        .is_some_and(|p| p.mime.starts_with("image/"))
    {
        remember_payload(ts, payload.clone());
        let guard = crate::clipboard_gtk::SelectionGuard::capture();
        if is_rdp_bmp_only(&clipboard_targets("clipboard").await.unwrap_or_default()) {
            if let Some(p) = &payload {
                if let Ok(bytes) = B64.decode(&p.wire_data) {
                    crate::clipboard_gtk::try_offer_if_unchanged(
                        crate::clipboard_gtk::ClipboardOffer::Image {
                            mime: p.mime.clone(),
                            bytes,
                        },
                        guard,
                    );
                }
            }
        }
    }
    Ok(payload)
}

/// Écart toléré entre le texte lu et l'horodatage de la sélection.
///
/// Les deux valeurs viennent de deux appels `xclip` successifs : l'horloge du
/// serveur X avance entre les deux, donc une égalité stricte laissait passer la
/// quasi-totalité des cas (c'est ce qui a laissé la tempête du 29/08 continuer).
const SELECTION_CLOCK_SLACK_MS: u64 = 60_000;

/// Le texte lu est-il l'horodatage X11 de la sélection plutôt qu'un contenu ?
///
/// Un vrai nombre copié par l'utilisateur (téléphone, référence, montant) n'est
/// pas concerné : il faudrait qu'il tombe à moins de 30 s de l'horloge
/// millisecondes du serveur X au moment précis de la lecture.
fn is_server_clock_echo(mime: &str, text: &str, reference_clock: u64) -> bool {
    if mime.starts_with("image/") {
        return false;
    }
    let t = text.trim();
    if t.len() < 6 || t.len() > 12 || !t.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let Ok(value) = t.parse::<u64>() else {
        return false;
    };
    value.abs_diff(reference_clock) <= SELECTION_CLOCK_SLACK_MS
}

async fn read_plain_text_timeout(limit: Duration) -> Option<String> {
    let raw = match read_text_selection_bytes("clipboard", "UTF8_STRING", limit).await {
        Ok(b) => b,
        Err(_) => match read_text_selection_bytes("clipboard", "STRING", limit).await {
            Ok(b) => b,
            Err(_) => read_text_selection_bytes("clipboard", "TEXT", limit)
                .await
                .ok()?,
        },
    };
    if looks_like_image_bytes(&raw) {
        return None;
    }
    let text = sanitize_copied_text(&String::from_utf8_lossy(&raw));
    if is_syncable_text(&text) {
        Some(text)
    } else {
        None
    }
}

async fn read_plain_text() -> Option<String> {
    read_plain_text_timeout(XCLIP_TEXT_TIMEOUT).await
}

async fn read_html_fragment() -> Option<String> {
    let raw = read_selection_bytes("clipboard", "text/html").await.ok()?;
    if raw.len() > MAX_HTML_BYTES || raw.is_empty() {
        return None;
    }
    let html = String::from_utf8_lossy(&raw).into_owned();
    if !looks_like_markup(&html) {
        return None;
    }
    Some(html)
}

pub fn html_to_visible_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"");
    sanitize_copied_text(&out)
}

fn sanitize_copied_text(text: &str) -> String {
    text.trim().replace("\u{00a0}", " ").to_string()
}

fn compact_ws(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Chrome copies a 20-char email plus a whole-page `text/html`. LibreOffice
/// HTML is the selection only — visible text matches UTF8_STRING.
fn html_is_same_selection(plain: Option<&str>, html: &str) -> bool {
    if html.len() > MAX_HTML_BYTES {
        return false;
    }
    let vis = html_to_visible_text(html);
    let compact_v = compact_ws(&vis);
    if compact_v.len() < MIN_TEXT_SYNC_LEN {
        return false;
    }
    let Some(p) = plain else {
        return vis.len() <= 8 * 1024;
    };
    let compact_p = compact_ws(p);
    if compact_p.len() < MIN_TEXT_SYNC_LEN {
        return false;
    }
    compact_v == compact_p
        || (compact_v.contains(&compact_p) && compact_v.len() <= compact_p.len().saturating_add(80))
}

/// Incoming HTML with formatting off (or HTML that is a whole document).
pub fn local_write_text(data: &str, mime: &str, keep_formatting: bool) -> (String, String) {
    if mime == "text/html" {
        // Never inject HTML into X11 on xrdp — Chromium parses it into the DOM.
        if keep_formatting
            && !xrdp_session_active()
            && data.len() <= MAX_HTML_BYTES
            && looks_like_markup(data)
        {
            (data.to_string(), "text/html".into())
        } else {
            (html_to_visible_text(data), "text/plain".into())
        }
    } else {
        (sanitize_copied_text(data), "text/plain".into())
    }
}

/// Dernier contenu connu du presse-papiers, gardé dans notre propre mémoire.
///
/// Sous X11, le presse-papiers n'est pas un stockage central : le contenu reste
/// dans la mémoire de l'application qui a copié, et meurt avec elle. D'où le
/// symptôme classique — fermer une application fait perdre ce qu'on y avait
/// copié. Garder une copie ici permet de la resservir quand le propriétaire
/// disparaît, sans jamais avoir à lui voler la sélection de son vivant.
#[derive(Clone)]
struct RememberedSelection {
    payload: ClipboardPayload,
    copy_epoch: Option<u64>,
}

static LAST_KNOWN_PAYLOAD: Mutex<Option<RememberedSelection>> = Mutex::new(None);
/// Copie confiée explicitement par une application au `CLIPBOARD_MANAGER`.
/// Elle doit entrer dans le chemin réseau même si l'application disparaît
/// avant le prochain sondage (cas reproductible avec XFCE Screenshooter).
static MANAGER_PENDING_PAYLOAD: Mutex<
    Option<(ClipboardPayload, crate::clipboard_gtk::SelectionGuard)>,
> = Mutex::new(None);
/// Le sondage du propriétaire ouvre une connexion X11 : ne pas le faire à
/// chaque tour de boucle (jusqu'à 20 fois par seconde sur certains nœuds).
static LAST_ORPHAN_CHECK: Mutex<Option<Instant>> = Mutex::new(None);
const ORPHAN_CHECK_INTERVAL: Duration = Duration::from_millis(1_000);

/// Mémorise le contenu courant, quelle que soit son origine (locale ou réseau).
pub fn remember_clipboard_content(mime: &str, wire_data: &str, hash: &str) {
    if let Ok(mut g) = LAST_KNOWN_PAYLOAD.lock() {
        *g = Some(RememberedSelection {
            payload: ClipboardPayload {
                mime: mime.to_string(),
                wire_data: wire_data.to_string(),
                hash: hash.to_string(),
            },
            copy_epoch: crate::clipboard_epoch::clipboard_copy_current(),
        });
    }
}

fn remember_clipboard_manager_payload(
    payload: ClipboardPayload,
    guard: crate::clipboard_gtk::SelectionGuard,
) {
    if !guard.handoff_current() {
        return;
    }
    if let Ok(mut pending) = MANAGER_PENDING_PAYLOAD.lock() {
        *pending = Some((payload, guard));
    }
}

/// Sauvegarde une image remise par une application qui va se fermer.
pub(crate) fn remember_clipboard_manager_image(
    bytes: &[u8],
    guard: crate::clipboard_gtk::SelectionGuard,
) -> Result<(&'static str, String)> {
    let payload = image_payload_from_bytes(bytes)?;
    let mime = if payload.mime == "image/jpeg" {
        "image/jpeg"
    } else {
        "image/png"
    };
    let hash = payload.hash.clone();
    remember_clipboard_manager_payload(payload, guard);
    Ok((mime, hash))
}

/// Sauvegarde un texte remis par une application qui va se fermer.
pub(crate) fn remember_clipboard_manager_text(
    text: String,
    guard: crate::clipboard_gtk::SelectionGuard,
) {
    let payload = ClipboardPayload {
        hash: hash_text(&text),
        mime: "text/plain".into(),
        wire_data: text,
    };
    remember_clipboard_manager_payload(payload, guard);
}

/// Retire la copie confiée au gestionnaire pour qu'elle soit diffusée une fois.
pub(crate) fn take_clipboard_manager_payload() -> Option<ClipboardPayload> {
    MANAGER_PENDING_PAYLOAD
        .lock()
        .ok()
        .and_then(|mut pending| pending.take())
        .and_then(|(payload, guard)| guard.handoff_current().then_some(payload))
}

/// Reprend la sélection quand son propriétaire a disparu.
///
/// C'est le rôle d'un gestionnaire de presse-papiers : l'application qui a
/// copié garde la sélection tant qu'elle vit — on ne la dérange pas, donc
/// aucune course avec un collage en cours — et on ne prend le relais qu'au
/// moment où elle s'en va, avec le contenu qu'on avait mis de côté.
pub async fn reclaim_orphaned_selection() -> bool {
    // Wayland compositors persist the selection independently; X11 ownership
    // probes would falsely report an orphan and rewrite it on every poll.
    if wayland_session() {
        return false;
    }
    {
        let Ok(mut last) = LAST_ORPHAN_CHECK.lock() else {
            return false;
        };
        if last.is_some_and(|at| at.elapsed() < ORPHAN_CHECK_INTERVAL) {
            return false;
        }
        *last = Some(Instant::now());
    }
    // Zéro = plus aucun propriétaire : l'application a été fermée. C'est le
    // seul cas sans ambiguïté, et le seul où l'on intervient.
    if crate::clipboard_gtk::current_clipboard_owner() != 0 {
        return false;
    }
    let Some(remembered) = LAST_KNOWN_PAYLOAD.lock().ok().and_then(|g| g.clone()) else {
        return false;
    };
    // A newer application may have copied and closed before its data was read.
    // Empty is safer than restoring the previously remembered capture.
    if remembered.copy_epoch.is_none()
        || remembered.copy_epoch != crate::clipboard_epoch::clipboard_copy_current()
    {
        return false;
    }
    let payload = remembered.payload;
    match write_clipboard(&payload.wire_data, &payload.mime).await {
        Ok(()) => {
            tracing::info!(
                "clipboard: propriétaire disparu — contenu restauré depuis la mémoire de l'agent (mime={} bytes={})",
                payload.mime,
                payload.wire_data.len()
            );
            true
        }
        Err(err) => {
            tracing::warn!("clipboard: restauration après disparition du propriétaire: {err:#}");
            false
        }
    }
}

/// L'image déjà présente dans CLIPBOARD est-elle collable telle quelle ?
///
/// Si l'application qui vient de copier (Flameshot, GIMP, un navigateur…)
/// expose un PNG ou un JPEG et le sert, il n'y a aucune raison de lui prendre
/// la sélection : la reprendre au moment où l'utilisateur colle laisse sa
/// demande partir vers un propriétaire qui n'existe plus, et l'application
/// se fige. On ne revendique que ce qui n'est pas utilisable — le cas
/// xrdp/chansrv, qui n'annonce qu'un BMP vide.
pub async fn local_image_is_already_pasteable() -> bool {
    if crate::clipboard_gtk::owns_image_clipboard() {
        return true; // c'est déjà notre offre : rien à reprendre
    }
    let targets = clipboard_targets("clipboard").await.unwrap_or_default();
    targets_have_pasteable_image(&targets)
}

/// Secours xrdp : sous xrdp, Ctrl+C n'atteint pas toujours CLIPBOARD, et la
/// copie se retrouve seulement dans PRIMARY (la sélection à la souris).
///
/// Ce secours ne doit s'appliquer QUE si CLIPBOARD n'a rien d'utilisable.
/// Le faire primer, comme c'était le cas, laisse un simple surlignage écraser
/// une copie arrivée du réseau : les deux machines se renvoient alors leurs
/// sélections respectives sans fin (boucle observée le 30/08 entre work-a et
/// le reste du pool, une notification par seconde et Chrome figé au collage).
async fn xrdp_primary_fallback_payload() -> Option<ClipboardPayload> {
    if !xrdp_session_active() || primary_owner_is_chromium_based() {
        return None;
    }
    let text = stable_primary_text().await?;
    if text_is_image_sidecar(&text) {
        return None;
    }
    let already = LAST_PRIMARY_APPLIED.lock().ok().and_then(|g| g.clone());
    if already.as_deref() == Some(text.as_str()) {
        return None;
    }
    tracing::debug!("clipboard read source=xrdp-primary-fallback");
    Some(ClipboardPayload {
        mime: "text/plain".into(),
        wire_data: text.clone(),
        hash: hash_text(&text),
    })
}

async fn read_clipboard_payload_uncached(
    allow_images: bool,
    keep_formatting: bool,
) -> Result<Option<ClipboardPayload>> {
    if let Some(text) = primary_user_text_override()
        .await
        .filter(|text| primary_differs_from_applied(text))
    {
        if crate::clipboard_gtk::recent_image_claim_active() {
            crate::clipboard_gtk::clear_image_claim();
            clear_image_clipboard_epoch();
            tracing::debug!("clipboard read source=primary-override");
            return Ok(Some(ClipboardPayload {
                mime: "text/plain".into(),
                wire_data: text.clone(),
                hash: hash_text(&text),
            }));
        }
    }
    let targets = clipboard_targets("clipboard").await.unwrap_or_default();
    // Some X11 applications (notably screenshot tools) put an image only in
    // PRIMARY.  It must enter the normal poll path so it is cached in the
    // PoolSync buffer and sent to peers, not merely re-offered locally.
    if should_read_primary_image(allow_images, &targets) {
        if let Some(payload) = read_primary_image_payload().await {
            tracing::info!(
                "clipboard read: primary-only image mime={} bytes={}",
                payload.mime,
                payload.wire_data.len()
            );
            return Ok(Some(payload));
        }
    }
    // Idem pour le texte : notre propre offre GTK n'a pas à être relue. Sans
    // ce garde-fou, une offre qui n'honore pas les demandes de conversion fait
    // échouer toutes les cibles texte et l'agent se rabat sur les métadonnées.
    if crate::clipboard_gtk::owns_text_clipboard() {
        return Ok(None);
    }
    // We already own this image through the GTK offer. Reading it again calls
    // our own image/png callback every poll (320 KiB × 20/s on a desktop), creating a
    // SERVE→CAPTURE loop that races xrdp-chansrv and freezes Chromium.
    if crate::clipboard_gtk::owns_image_clipboard() && targets_have_pasteable_image(&targets) {
        return Ok(None);
    }
    if is_rdp_bmp_only(&targets) {
        if allow_images {
            if let Some(payload) = read_image_payload(&targets).await {
                return Ok(Some(payload));
            }
        }
        recover_stripped_image(&targets).await;
        return Ok(None);
    }
    let has_image = targets_have_pasteable_image(&targets);
    // Pure PNG offers cannot serve text. Probing three missing text targets
    // first adds unnecessary waits to every screenshot transfer.
    let plain = if targets_advertise_text(&targets) || !has_image {
        read_plain_text().await
    } else {
        None
    };
    // Propriétaire de sélection à moitié mort : il annonce des cibles texte et
    // refuse de les servir. Seules les sondes (TARGETS, TIMESTAMP) répondent
    // encore — c'est l'état qui a inondé le pool. Le tracer explicitement, une
    // ligne de log suffit alors à l'identifier.
    if plain.is_none() && !has_image && targets_advertise_text(&targets) {
        note_broken_text_owner(&targets);
    }
    let mut image_payload = if allow_images && has_image {
        read_image_payload(&targets).await
    } else {
        None
    };
    // Gimp on the older XRDP/X11 stack can serve a valid image/png while its
    // TARGETS advertises only UTF8_STRING. Probe only when that text is not
    // usable and rate-limit it, so normal text is never stalled.
    if image_payload.is_none()
        && should_probe_unadvertised_image(allow_images, has_image, plain.as_deref())
    {
        image_payload = read_unadvertised_image_payload().await;
    }

    // Screenshot / « copy image » : PNG + texte reliquat (email, chemin, URL).
    // Copie Chrome d'un champ : text/html + vrai texte → garder le texte.
    let prefer_image =
        image_payload.is_some() && !is_chrome_style_text_copy(&targets, plain.as_deref());

    if image_payload.is_some() && !prefer_image {
        tracing::warn!(
            "clipboard read: image présente mais texte préféré (plain {} chars)",
            plain.as_ref().map(|s| s.len()).unwrap_or(0)
        );
    }
    if has_image && image_payload.is_none() {
        tracing::warn!(
            "clipboard read: cible image sur CLIPBOARD mais lecture échouée targets={}",
            targets.len()
        );
    }

    if keep_formatting && !prefer_image {
        if let Some(html) = read_html_fragment().await {
            if html_is_same_selection(plain.as_deref(), &html) {
                return Ok(Some(ClipboardPayload {
                    mime: "text/html".into(),
                    wire_data: html.clone(),
                    hash: hash_text(&html),
                }));
            }
        }
    }
    if plain.is_none() && image_payload.is_none() {
        if let Some(payload) = xrdp_primary_fallback_payload().await {
            return Ok(Some(payload));
        }
    }
    if !prefer_image {
        if let Some(text) = plain {
            if allow_images {
                if let Some(bytes) = image_bytes_from_path_text(&text) {
                    match image_payload_from_bytes(&bytes) {
                        Ok(p) => {
                            tracing::info!(
                                "clipboard read: image from path ({} bytes)",
                                bytes.len()
                            );
                            return Ok(Some(p));
                        }
                        Err(err) => tracing::warn!("clipboard path image skipped: {err:#}"),
                    }
                }
            }
            // A valid text copy always supersedes the previous image, including
            // text published by the XRDP bridge. Session type proves no staleness.
            tracing::debug!("clipboard read source=primary-mirror");
            return Ok(Some(ClipboardPayload {
                mime: "text/plain".into(),
                wire_data: text.clone(),
                hash: hash_text(&text),
            }));
        }
    }

    if let Some(img) = image_payload {
        return Ok(Some(img));
    }
    if let Some(text) = plain {
        tracing::debug!("clipboard read source=clipboard-plain");
        return Ok(Some(ClipboardPayload {
            mime: "text/plain".into(),
            wire_data: text.clone(),
            hash: hash_text(&text),
        }));
    }
    if !has_image {
        if let Some(text) = stable_primary_text().await {
            tracing::debug!("clipboard read source=html-fragment");
            return Ok(Some(ClipboardPayload {
                mime: "text/plain".into(),
                wire_data: text.clone(),
                hash: hash_text(&text),
            }));
        }
    }
    Ok(None)
}

fn should_read_primary_image(allow_images: bool, clipboard_targets: &[String]) -> bool {
    allow_images && !targets_have_pasteable_image(clipboard_targets)
}

fn should_probe_unadvertised_image(
    allow_images: bool,
    has_advertised_image: bool,
    plain_text: Option<&str>,
) -> bool {
    allow_images && !has_advertised_image && plain_text.is_none()
}

async fn read_unadvertised_image_payload() -> Option<ClipboardPayload> {
    let too_soon = LAST_IMAGE_PROBE_AT
        .lock()
        .ok()
        .and_then(|t| *t)
        .is_some_and(|at| at.elapsed() < IMAGE_PROBE_COOLDOWN);
    if too_soon {
        return None;
    }
    if let Ok(mut at) = LAST_IMAGE_PROBE_AT.lock() {
        *at = Some(Instant::now());
    }
    for mime in ["image/png", "image/jpeg", "image/jpg"] {
        // Incomplete metadata does not make image conversion cheaper. Native
        // GTK owners can take over 500 ms to encode a full-HD PNG. Timing it
        // out and accepting the faster JPEG permanently loses source pixels.
        // The enclosing read still cancels within 40 ms of a newer selection.
        let output = match selection_read_timeout(
            &["-selection", "clipboard", "-t", mime, "-o"],
            XCLIP_IMAGE_READ_TIMEOUT,
        )
        .await
        {
            Ok(output) => output,
            Err(error) => {
                tracing::debug!("unadvertised image read deferred mime={mime}: {error:#}");
                return None;
            }
        };
        // Only an explicit unsupported target permits a different format.
        // A slow or malformed PNG response is not proof that PNG is absent.
        if !output.status.success() {
            continue;
        }
        let bytes = output.stdout;
        if let Ok(payload) = image_payload_from_bytes(&bytes) {
            tracing::info!(
                "clipboard read: image recovered despite incomplete TARGETS mime={} bytes={}",
                payload.mime,
                bytes.len()
            );
            return Some(payload);
        }
        return None;
    }
    None
}

fn looks_like_image_bytes(raw: &[u8]) -> bool {
    raw.starts_with(&[0x89, 0x50, 0x4E, 0x47])
        || (raw.len() >= 3 && raw[0] == 0xFF && raw[1] == 0xD8 && raw[2] == 0xFF)
}

async fn read_primary_image_payload() -> Option<ClipboardPayload> {
    let targets = clipboard_targets("primary").await.ok()?;
    if !targets_have_pasteable_image(&targets) {
        return None;
    }
    for mime in preferred_image_mimes(&targets) {
        let Ok(bytes) = read_selection_bytes("primary", &mime).await else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        if let Ok(payload) = image_payload_from_bytes(&bytes) {
            return Some(payload);
        }
    }
    None
}

/// Preserve source pixels when an owner advertises a lossless representation.
/// A transient failed conversion must be retried, rather than cached as JPEG.
fn preferred_image_mimes(targets: &[String]) -> Vec<String> {
    let mut lossless = Vec::new();
    let mut lossy = Vec::new();
    for target in targets {
        match target.to_ascii_lowercase().as_str() {
            "image/png" | "image/bmp" => lossless.push(target.clone()),
            "image/jpeg" | "image/jpg" => lossy.push(target.clone()),
            _ => {}
        }
    }
    if lossless.is_empty() {
        return lossy;
    }
    lossless.sort_by_key(|mime| !mime.eq_ignore_ascii_case("image/png"));
    lossless
}

async fn read_image_payload(targets: &[String]) -> Option<ClipboardPayload> {
    // xrdp-chansrv takes ownership about 10s after our GTK offer, while
    // preserving exactly the same PNG and often exposing no TIMESTAMP. Read
    // once per selection epoch, not every 50ms. XFixes distinguishes copies
    // even when an application reuses its owner and omits TIMESTAMP.
    let owner = crate::clipboard_gtk::current_clipboard_owner();
    let timestamp = clipboard_timestamp().await;
    let key = (owner, timestamp, crate::clipboard_epoch::current());
    if owner != 0 && (key.1.is_some() || key.2.is_some()) {
        let duplicate_owner = LAST_IMAGE_READ_KEY
            .lock()
            .ok()
            .map(|last| last.as_ref() == Some(&key))
            .unwrap_or(false);
        if duplicate_owner {
            return None;
        }
    }
    let image_mimes = preferred_image_mimes(targets);
    let lossless_advertised = image_mimes.iter().any(|mime| {
        mime.eq_ignore_ascii_case("image/png") || mime.eq_ignore_ascii_case("image/bmp")
    });
    for mime in &image_mimes {
        if let Ok(bytes) = read_selection_bytes("clipboard", mime).await {
            if !bytes.is_empty() {
                match image_payload_from_bytes(&bytes) {
                    Ok(p) => {
                        tracing::info!(
                            "image-trace CAPTURE id={} mime={} bytes={} reader=native-x11",
                            trace_id(&p.hash),
                            p.mime,
                            bytes.len()
                        );
                        if let Ok(mut last) = LAST_IMAGE_READ_KEY.lock() {
                            *last = Some(key.clone());
                        }
                        return Some(p);
                    }
                    Err(err) => tracing::warn!("screenshot/image skipped: {err:#}"),
                }
            }
        }
    }
    // GTK's generic image conversion can choose JPEG after a failed PNG.
    // Keep lossless failures uncached so the next native poll retries them.
    if !lossless_advertised && targets_have_image(targets) && gtk_read_allowed() {
        note_gtk_read_attempt();
        if let Ok(bytes) = read_image_via_gtk().await {
            if !bytes.is_empty() {
                match image_payload_from_bytes(&bytes) {
                    Ok(payload) => {
                        tracing::info!(
                            "image-trace CAPTURE id={} mime={} bytes={} reader=gtk",
                            trace_id(&payload.hash),
                            payload.mime,
                            bytes.len()
                        );
                        if let Ok(mut last) = LAST_IMAGE_READ_KEY.lock() {
                            *last = Some(key.clone());
                        }
                        return Some(payload);
                    }
                    Err(err) => tracing::warn!("screenshot/image skipped: {err:#}"),
                }
            }
        }
    }
    None
}

pub fn trace_id(hash: &str) -> &str {
    hash.get(..12.min(hash.len())).unwrap_or(hash)
}

fn is_chrome_style_text_copy(targets: &[String], plain: Option<&str>) -> bool {
    let has_html = targets
        .iter()
        .any(|t| t.to_ascii_lowercase().contains("text/html"));
    let Some(p) = plain else {
        return false;
    };
    if !has_html {
        return false;
    }
    if looks_like_url(p) || text_is_image_sidecar(p) {
        return false;
    }
    true
}

fn looks_like_url(text: &str) -> bool {
    let l = text.trim().to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://") || l.starts_with("www.")
}

/// XFCE screenshooter sometimes puts only a saved file path on the clipboard (xrdp).
fn image_bytes_from_path_text(text: &str) -> Option<Vec<u8>> {
    let t = text.trim();
    let lower = t.to_ascii_lowercase();
    if !(lower.ends_with(".png")
        || lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".webp"))
    {
        return None;
    }
    let path = if let Some(rest) = t.strip_prefix("file://") {
        std::path::PathBuf::from(rest)
    } else if let Some(rest) = t.strip_prefix("~/") {
        let home = std::env::var("HOME").ok()?;
        std::path::PathBuf::from(home).join(rest)
    } else if t.starts_with('/') {
        std::path::PathBuf::from(t)
    } else {
        return None;
    };
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAX_IMAGE_BYTES as u64 {
        return None;
    }
    std::fs::read(&path).ok()
}

/// XFCE/Chrome often advertise UTF8_STRING next to a screenshot (file path).
fn text_is_image_sidecar(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return true;
    }
    let lower = t.to_ascii_lowercase();
    if looks_like_url(t) || lower.starts_with("file://") {
        return true;
    }
    let pathish = t.starts_with('/') || lower.starts_with("~/");
    if pathish {
        return true;
    }
    lower.contains("screenshot")
        || lower.contains("capture d")
        || lower.contains("capture-ecran")
        || lower.contains("captures d")
}

/// Adopte le contenu déjà présent au démarrage comme référence, sans l'émettre.
///
/// Sans cela, un agent qui redémarre re-détecte le presse-papiers hérité comme
/// une copie neuve : il lui attribue une horloge courante, donc supérieure à
/// celle d'une copie faite juste avant ailleurs, et le pool régresse vers un
/// contenu périmé.
pub async fn seed_local_baseline(last_clip_hash: &Mutex<String>, keep_formatting: bool) {
    let Ok(Some(payload)) = read_clipboard_payload_filtered(true, keep_formatting).await else {
        return;
    };
    if let Ok(mut last) = last_clip_hash.lock() {
        *last = payload.hash.clone();
    }
    tracing::info!(
        "clipboard: état hérité adopté sans diffusion (mime={} bytes={})",
        payload.mime,
        payload.wire_data.len()
    );
}

/// Détection locale : met à jour le hash, enregistre le cache, retourne true si nouveau contenu.
pub fn prepare_local_clipboard(payload: &ClipboardPayload, last_clip_hash: &Mutex<String>) -> bool {
    if payload.mime == "text/plain" && !is_syncable_text(&payload.wire_data) {
        return false;
    }
    if payload.mime == "text/html" && payload.wire_data.len() > MAX_HTML_BYTES {
        return false;
    }
    // Texte local = intention utilisateur : toujours autoriser.
    if payload.mime == "text/plain" || payload.mime == "text/html" {
        crate::clipboard_gtk::discard_last_image();
        clear_image_clipboard_epoch();
    }
    let mut last = match last_clip_hash.lock() {
        Ok(guard) => guard,
        Err(_) => return false,
    };
    if *last == payload.hash {
        return false;
    }
    *last = payload.hash.clone();
    drop(last);
    remember_clipboard_content(&payload.mime, &payload.wire_data, &payload.hash);
    if payload.mime.starts_with("image/") {
        if let Ok(bytes) = B64.decode(&payload.wire_data) {
            crate::clipboard_gtk::remember_native_image(&payload.mime, &bytes);
        }
    }
    true
}

/// Envoi réseau (hub optionnel + peer) — après cache local, ne bloque pas l'usage local.
pub fn send_payload_network(
    payload: &ClipboardPayload,
    hub_tx: &broadcast::Sender<String>,
    peer_tx: &Option<UnboundedSender<String>>,
    relay_hub: bool,
    origin: &str,
    seq: u64,
    e2e_key: Option<&str>,
) -> bool {
    let plain = Message::Clipboard {
        msg_id: uuid::Uuid::new_v4().to_string(),
        hash: payload.hash.clone(),
        mime: payload.mime.clone(),
        data: payload.wire_data.clone(),
        origin: origin.to_string(),
        seq,
    };
    let message = match e2e_key {
        Some(key) => match encrypt_clipboard(&plain, key) {
            Ok(message) => message,
            Err(err) => {
                tracing::error!("clipboard E2E encryption failed: {err:#}");
                return false;
            }
        },
        None => plain,
    };
    if let Ok(encoded) = encode_message(&message) {
        let mut sent = false;
        if relay_hub {
            sent |= hub_tx.send(encoded.clone()).is_ok();
        }
        if let Some(peer) = peer_tx {
            sent |= peer.send(encoded).is_ok();
        }
        if !sent {
            return false;
        }
        if payload.mime.starts_with("image/") {
            mark_image_clipboard_epoch();
            tracing::info!(
                "image-trace DISPATCH id={} hub={} peer={} wire_bytes={}",
                trace_id(&payload.hash),
                relay_hub,
                peer_tx.is_some(),
                payload.wire_data.len()
            );
        }
        return true;
    }
    false
}

pub async fn write_clipboard(data: &str, mime: &str) -> Result<()> {
    write_clipboard_if(data, mime, || true).await.map(|_| ())
}

pub async fn write_clipboard_if(
    data: &str,
    mime: &str,
    current: impl Fn() -> bool,
) -> Result<bool> {
    let selection = (!wayland_session()).then(|| {
        (
            crate::clipboard_gtk::current_clipboard_owner(),
            crate::clipboard_epoch::clipboard_current(),
        )
    });
    defer_write_while_paste_in_flight(mime).await;
    if !current() {
        return Ok(false);
    }
    if let Some(before) = selection {
        let after = (
            crate::clipboard_gtk::current_clipboard_owner(),
            crate::clipboard_epoch::clipboard_current(),
        );
        if before != after && !crate::clipboard_gtk::owns_clipboard() {
            tracing::debug!("clipboard deferred write cancelled after a native copy");
            return Ok(false);
        }
    }
    invalidate_payload_cache();
    if wayland_session() {
        let data = data.to_string();
        let mime = mime.to_string();
        return tokio::task::spawn_blocking(move || write_wayland_clipboard_sync(&data, &mime))
            .await
            .context("Wayland clipboard task")?
            .map(|_| true);
    }
    if mime == "text/plain" || mime == "text/html" {
        offer_text_payload(data, mime)?;
        let data = data.to_string();
        let mime = mime.to_string();
        let generation = crate::clipboard_gtk::offer_generation();
        tokio::spawn(async move {
            ensure_text_is_actually_served(&data, &mime, generation).await;
        });
        Ok(true)
    } else if mime.starts_with("image/") {
        let bytes = B64
            .decode(data)
            .with_context(|| format!("decode base64 image ({mime})"))?;
        // Own the PNG through the GTK loop first.  Its image-only targets are
        // the XRDP-safe path; xclip is solely a fallback when GTK is absent.
        if crate::clipboard_gtk::try_offer(crate::clipboard_gtk::ClipboardOffer::Image {
            mime: mime.to_string(),
            bytes,
        }) {
            Ok(true)
        } else {
            write_image_to_clipboard_async(&B64.decode(data)?, mime)
                .await
                .map(|_| true)
        }
    } else {
        anyhow::bail!("unsupported clipboard mime: {mime}");
    }
}

fn offer_text_payload(data: &str, mime: &str) -> Result<()> {
    // Texte utilisateur ou distant prioritaire sur toute ancienne image. Sur
    // X11, l'écriture xclip ne traverse pas apply_offer(Text), donc il faut
    // invalider explicitement le keepalive GTK avant de rendre la sélection.
    crate::clipboard_gtk::discard_last_image();
    // Autrefois, les sessions xrdp étaient traitées à part : on y écrivait avec
    // xclip pour éviter que l'offre GTK ne réécrive aussi PRIMARY, ce qui fait
    // vaciller la propriété de la sélection et fige Chromium au collage.
    //
    // Mais c'est le *miroir PRIMARY* qui posait problème, pas GTK. Passer par
    // xclip privait ces sessions de tout ce que l'offre GTK apporte — dont la
    // détection des collages en cours, qui est justement le garde-fou contre
    // ces gels. On garde donc GTK partout, et on n'écrit PRIMARY que là où
    // c'est utile : sur un bureau classique.
    let mirror_primary = !xrdp_session_active();
    if mime == "text/html" {
        if data.len() > MAX_HTML_BYTES {
            anyhow::bail!("html too large to paste safely ({} bytes)", data.len());
        }
        let plain = html_to_visible_text(data);
        // Le mime n'arrive ici en text/html que si `local_write_text` a décidé
        // de garder le formatage (option `keep_formatting`). Offrir seulement
        // le texte aplati reviendrait à ignorer l'option en silence : on offre
        // les deux cibles, et l'application collante choisit.
        if crate::clipboard_gtk::try_offer(crate::clipboard_gtk::ClipboardOffer::Rich {
            plain: plain.clone(),
            html: data.to_string(),
            mirror_primary,
        }) {
            if let Ok(mut g) = LAST_MIRROR_TEXT.lock() {
                *g = Some(plain.clone());
            }
            return Ok(());
        }
        if crate::clipboard_gtk::try_offer(crate::clipboard_gtk::ClipboardOffer::Text {
            text: plain.clone(),
            mirror_primary,
        }) {
            if let Ok(mut g) = LAST_MIRROR_TEXT.lock() {
                *g = Some(plain.clone());
            }
            return Ok(());
        }
        write_selection_text_sync("clipboard", &plain)
    } else {
        if data.len() > MAX_TEXT_BYTES {
            anyhow::bail!("text too large to paste safely ({} bytes)", data.len());
        }
        if crate::clipboard_gtk::try_offer(crate::clipboard_gtk::ClipboardOffer::Text {
            text: data.to_string(),
            mirror_primary,
        }) {
            if let Ok(mut g) = LAST_MIRROR_TEXT.lock() {
                *g = Some(data.to_string());
            }
            return Ok(());
        }
        write_selection_text_sync("clipboard", data)
    }
}

/// Fenêtre pendant laquelle une lecture récente signale un collage en cours.
const PASTE_IN_FLIGHT_WINDOW: Duration = Duration::from_millis(300);
/// Report maximal avant d'écrire quand même : une copie entrante doit finir par
/// arriver, même si l'application d'en face lit la sélection en continu.
const PASTE_DEFER_MAX: Duration = Duration::from_millis(900);
const PASTE_DEFER_STEP: Duration = Duration::from_millis(100);

/// Diffère la prise de sélection tant qu'une application est en train de lire.
///
/// Le 29/08, VSCode s'est figé (« CodeWindow unresponsive ») au moment précis
/// où une copie venue d'desk-a faisait changer le propriétaire de CLIPBOARD :
/// Chromium lit la sélection de façon synchrone, et se la faire retirer en
/// plein transfert bloque sa fenêtre. GTK nous signale ces lectures, puisque
/// c'est nous qu'on vient servir : tant qu'elles sont fraîches, on attend.
///
/// L'attente est bornée : passé `PASTE_DEFER_MAX`, on écrit quand même, sinon
/// une application qui lit en boucle empêcherait toute synchronisation.
async fn defer_write_while_paste_in_flight(mime: &str) {
    if !crate::clipboard_gtk::selection_served_recently(PASTE_IN_FLIGHT_WINDOW) {
        return;
    }
    let started = Instant::now();
    while started.elapsed() < PASTE_DEFER_MAX {
        tokio::time::sleep(PASTE_DEFER_STEP).await;
        if !crate::clipboard_gtk::selection_served_recently(PASTE_IN_FLIGHT_WINDOW) {
            tracing::info!(
                "clipboard: écriture différée de {} ms — collage terminé (mime={mime})",
                started.elapsed().as_millis()
            );
            return;
        }
    }
    tracing::info!(
        "clipboard: écriture après {} ms d'attente — lectures continues (mime={mime})",
        started.elapsed().as_millis()
    );
}

/// Vérifie que l'offre GTK sert réellement le texte, sinon repasse par xclip.
///
/// `try_offer` ne dit que « la demande est partie sur la boucle GTK ». Sur
/// certaines sessions, l'offre prend bien la sélection puis n'honore aucune
/// demande de conversion : les applications collent du vide, et l'agent
/// lui-même ne peut plus relire que TARGETS et TIMESTAMP. Un propriétaire
/// `xclip` détaché, lui, sert le contenu de façon fiable.
async fn ensure_text_is_actually_served(data: &str, mime: &str, generation: u64) {
    // L'offre est appliquée sur la boucle GTK, et sur certaines sessions elle
    // sert correctement pendant quelques secondes *puis* se dégrade : elle
    // garde la sélection en n'honorant plus aucune conversion. Une seule
    // vérification immédiate ne verrait donc rien. On revient plusieurs fois.
    let mut degraded = false;
    for delay_ms in [250_u64, 1_500, 4_000] {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        if !crate::clipboard_gtk::text_offer_is_current(generation) {
            return;
        }
        let targets = clipboard_targets("clipboard").await.unwrap_or_default();
        if !targets_advertise_text(&targets) {
            return; // Une image ou un autre propriétaire a pris la main.
        }
        if read_plain_text_timeout(XCLIP_TEXT_TIMEOUT).await.is_none() {
            degraded = true;
            break;
        }
    }
    if !degraded {
        return; // L'offre sert bien le texte.
    }
    let plain = if mime == "text/html" {
        html_to_visible_text(data)
    } else {
        data.to_string()
    };
    tracing::warn!(
        "clipboard: l'offre GTK a cessé de servir le texte — bascule sur xclip ({} octets)",
        plain.len()
    );
    // Check again on the GTK thread after all asynchronous reads. A new
    // same-owner GTK offer or a native application copy cancels this fallback.
    glib::MainContext::default().invoke(move || {
        if !crate::clipboard_gtk::text_offer_is_current(generation) {
            return;
        }
        crate::clipboard_gtk::offer_now_from_gtk(crate::clipboard_gtk::ClipboardOffer::Release);
        if let Err(err) = write_selection_text_sync("clipboard", &plain) {
            tracing::warn!("clipboard: text fallback failed: {err:#}");
        }
    });
}

/// Écriture presse-papiers depuis le thread GTK (systray / historique).
pub fn write_clipboard_sync(data: &str, mime: &str) -> Result<()> {
    if wayland_session() {
        return write_wayland_clipboard_sync(data, mime);
    }
    invalidate_payload_cache();
    if mime == "text/plain" || mime == "text/html" {
        offer_text_payload(data, mime)
    } else if mime.starts_with("image/") {
        let bytes = B64
            .decode(data)
            .with_context(|| format!("decode base64 image ({mime})"))?;
        // This entry point is called by the GTK history/tray click handler:
        // take X11 ownership now, not on a later main-loop iteration.
        crate::clipboard_gtk::offer_now_from_gtk(crate::clipboard_gtk::ClipboardOffer::Image {
            mime: mime.to_string(),
            bytes,
        });
        Ok(())
    } else {
        anyhow::bail!("unsupported clipboard mime: {mime}");
    }
}

fn write_selection_text_sync(selection: &str, text: &str) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("xclip")
        .args(["-selection", selection, "-t", "UTF8_STRING"])
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("xclip -selection {selection} UTF8_STRING"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes()).context("xclip stdin")?;
    }
    // stdin fermé : xclip possède la sélection. On le laisse vivre pour la servir
    // aux collages suivants — cf. detach_selection_owner_sync.
    detach_selection_owner_sync(child);
    Ok(())
}

/// Laisse `xclip` vivre en arrière-plan après l'écriture.
///
/// Sous X11 la sélection n'est pas un stockage central : le processus qui l'écrit
/// en reste **propriétaire** et sert le contenu aux applications qui collent. Si on
/// l'attend avec `wait()`, il rend la sélection en mourant et le contenu disparaît —
/// le copier fonctionne, le coller ne trouve plus rien. On le détache donc, et on
/// se contente de le moissonner pour ne pas laisser de zombies : le prochain
/// propriétaire de la sélection (nouvelle copie locale ou distante) le terminera.
fn detach_selection_owner_sync(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

fn detect_image_mime(bytes: &[u8]) -> &'static str {
    if bytes.len() >= 4 && bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        "image/png"
    } else if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        "image/jpeg"
    } else {
        "image/png"
    }
}

fn write_image_to_clipboard(bytes: &[u8], mime: &str) -> Result<()> {
    let mime = if mime.starts_with("image/") {
        mime
    } else {
        detect_image_mime(bytes)
    };
    // Écriture multi-mimes : certaines applications X11 (Qt/Chromium) attendent image/png ou TARGETS précises.
    let res_xclip = write_selection_bytes_sync("clipboard", mime, bytes)
        .or_else(|_| write_selection_bytes_sync("clipboard", "image/png", bytes));

    // Le fallback GTK est un *repli*, pas un complément : il prend lui aussi la
    // sélection et évincerait le xclip détaché qui la conserve. Deux propriétaires
    // successifs laissaient une image tronquée que l'agent relisait comme une
    // nouvelle copie — l'image repartait alors sur le réseau en se dégradant à
    // chaque tour. On ne l'exécute donc que si xclip a réellement échoué.
    if res_xclip.is_ok() {
        return Ok(());
    }
    anyhow::bail!("échec de l'écriture image dans le presse-papiers X11 (xclip)")
}

fn write_selection_bytes_sync(selection: &str, mime: &str, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("xclip")
        .args(["-selection", selection, "-t", mime])
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("xclip -selection {selection} -t {mime}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(bytes).context("xclip stdin")?;
    }
    // xclip doit rester propriétaire de la sélection (cf. detach_selection_owner_sync).
    // On ne peut donc pas juger l'écriture sur son code de sortie : on lui laisse un
    // court instant pour échouer bruyamment (mime refusé, DISPLAY absent), et s'il
    // est toujours là c'est qu'il sert la sélection.
    std::thread::sleep(std::time::Duration::from_millis(120));
    match child.try_wait().context("xclip try_wait")? {
        Some(status) if !status.success() => {
            anyhow::bail!("xclip write {selection} {mime} failed");
        }
        Some(_) => Ok(()),
        None => {
            detach_selection_owner_sync(child);
            Ok(())
        }
    }
}

async fn write_image_to_clipboard_async(bytes: &[u8], mime: &str) -> Result<()> {
    let owned = bytes.to_vec();
    let mime = mime.to_string();
    tokio::task::spawn_blocking(move || write_image_to_clipboard(&owned, &mime))
        .await
        .context("image clipboard task")?
}

fn read_image_via_gtk_sync() -> Result<Vec<u8>> {
    let output = run_clipboard_py_script_output("read-image-clipboard.py", &[])?;
    if output.status.success() && !output.stdout.is_empty() {
        Ok(output.stdout)
    } else {
        anyhow::bail!("gtk clipboard read failed");
    }
}

async fn read_image_via_gtk() -> Result<Vec<u8>> {
    tokio::task::spawn_blocking(read_image_via_gtk_sync)
        .await
        .context("gtk read task")?
}

fn clipboard_py_script(name: &str) -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let path = std::path::PathBuf::from(home).join(".local/bin").join(name);
    if path.is_file() {
        Some(path)
    } else {
        None
    }
}

fn run_clipboard_py_script_output(name: &str, stdin_bytes: &[u8]) -> Result<std::process::Output> {
    let script = clipboard_py_script(name).context("clipboard py script missing")?;
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("timeout")
        .args([&CLIPBOARD_PY_TIMEOUT.as_secs().to_string(), "python3"])
        .arg(&script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn clipboard py")?;
    if !stdin_bytes.is_empty() {
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(stdin_bytes).context("clipboard py stdin")?;
        }
    }
    child.wait_with_output().context("clipboard py wait")
}

/// Avertissement limité à une fois par minute : l'état peut durer des heures.
fn note_broken_text_owner(targets: &[String]) {
    static LAST_WARN: Mutex<Option<Instant>> = Mutex::new(None);
    let Ok(mut last) = LAST_WARN.lock() else {
        return;
    };
    if last.is_some_and(|at| at.elapsed() < Duration::from_secs(60)) {
        return;
    }
    *last = Some(Instant::now());
    tracing::warn!(
        "clipboard: propriétaire de sélection cassé — cibles annoncées ({}) mais aucun texte lisible",
        targets.join(",")
    );
}

fn is_syncable_text(text: &str) -> bool {
    let t = text.trim();
    if t.len() < MIN_TEXT_SYNC_LEN {
        return false;
    }
    if is_target_list_dump(t) {
        tracing::warn!(
            "clipboard: sortie de sonde X11 ignorée ({} octets) — propriétaire de sélection cassé",
            t.len()
        );
        return false;
    }
    if text.len() > MAX_TEXT_BYTES {
        tracing::info!(
            "clipboard text skipped ({} bytes > {MAX_TEXT_BYTES}) — évite freeze navigateur",
            text.len()
        );
        return false;
    }
    if looks_like_markup(text) {
        tracing::info!(
            "clipboard HTML/markup skipped ({} bytes) — Chrome colle ça dans le DOM",
            text.len()
        );
        return false;
    }
    let b = text.as_bytes();
    // PNG/JPEG lu par erreur via xclip texte — ne jamais synchroniser comme text/plain.
    if b.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        return false;
    }
    if b.len() >= 3 && b[0] == 0xFF && b[1] == 0xD8 && b[2] == 0xFF {
        return false;
    }
    if b.contains(&0) {
        return false;
    }
    // Trop de non-texte → probablement binaire.
    let non_text = b
        .iter()
        .filter(|&&c| c < 0x09 || (c > 0x0d && c < 0x20) || c == 0x7f)
        .count();
    if !b.is_empty() && non_text * 10 > b.len() {
        return false;
    }
    true
}

fn looks_like_markup(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("<html") || lower.contains("<!doctype") || lower.contains("<body") {
        return true;
    }
    let tags = text.as_bytes().iter().filter(|&&c| c == b'<').count();
    tags >= 8 && (lower.contains("<div") || lower.contains("<span") || lower.contains("<p "))
}

fn image_payload_from_bytes(bytes: &[u8]) -> Result<ClipboardPayload> {
    if bytes.len() > MAX_IMAGE_BYTES {
        anyhow::bail!("image too large ({} bytes)", bytes.len());
    }
    // Fast path: keep native PNG/JPEG in clipboard without re-encoding (local-first).
    if bytes.len() >= 4 && bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        return Ok(ClipboardPayload {
            mime: "image/png".into(),
            wire_data: B64.encode(bytes),
            hash: hash_bytes(bytes),
        });
    }
    if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return Ok(ClipboardPayload {
            mime: "image/jpeg".into(),
            wire_data: B64.encode(bytes),
            hash: hash_bytes(bytes),
        });
    }
    let img = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .context("guess image format")?
        .decode()
        .context("decode image")?;
    let (w, h) = img.dimensions();
    let rgba = img.to_rgba8();
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(rgba.as_raw(), w, h, ExtendedColorType::Rgba8)
        .context("encode png")?;
    Ok(ClipboardPayload {
        mime: "image/png".into(),
        wire_data: B64.encode(&png),
        hash: hash_bytes(&png),
    })
}

async fn read_selection_bytes(selection: &str, mime: &str) -> Result<Vec<u8>> {
    let limit = if mime.starts_with("image/") {
        XCLIP_IMAGE_READ_TIMEOUT
    } else if mime == "UTF8_STRING" || mime.starts_with("text/") {
        XCLIP_TEXT_TIMEOUT
    } else {
        XCLIP_READ_TIMEOUT
    };
    read_selection_bytes_timeout(selection, mime, limit).await
}

/// Lecture de *contenu* texte. Refuse toute cible non textuelle : c'est ce
/// garde-fou qui rend structurellement impossible de renvoyer la sortie d'une
/// sonde (TARGETS, TIMESTAMP) comme si c'était le presse-papiers.
async fn read_text_selection_bytes(
    selection: &str,
    target: &str,
    limit: Duration,
) -> Result<Vec<u8>> {
    if !X11_TEXT_TARGETS
        .iter()
        .any(|t| t.eq_ignore_ascii_case(target))
    {
        anyhow::bail!("refus de lire {target} comme du texte");
    }
    read_selection_bytes_timeout(selection, target, limit).await
}

async fn read_selection_bytes_timeout(
    selection: &str,
    mime: &str,
    limit: Duration,
) -> Result<Vec<u8>> {
    let output =
        selection_read_timeout(&["-selection", selection, "-t", mime, "-o"], limit).await?;
    if !output.status.success() {
        anyhow::bail!("native read {selection} {mime} failed");
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lossless_image_targets_exclude_jpeg_even_when_the_owner_lists_jpeg_first() {
        assert_eq!(
            preferred_image_mimes(&targets(&["image/jpeg", "image/bmp", "image/png"])),
            vec!["image/png", "image/bmp"]
        );
        assert_eq!(
            preferred_image_mimes(&targets(&["image/jpeg", "image/bmp"])),
            vec!["image/bmp"]
        );
        assert_eq!(
            preferred_image_mimes(&targets(&["image/jpg", "image/jpeg"])),
            vec!["image/jpg", "image/jpeg"]
        );
    }

    #[test]
    fn minimal_image_target_metadata_is_never_published_as_copied_text() {
        let metadata = "TARGETS\nimage/png\nimage/jpeg\n";
        assert!(is_target_list_dump(metadata));
        assert!(!is_syncable_text(metadata));
        assert!(is_syncable_text("image/png\nimage/jpeg"));
        assert!(is_syncable_text("/tmp/first.png\n/tmp/second.png"));
    }

    #[test]
    fn a_native_text_copy_cancels_delayed_image_recovery() {
        let targets = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(!image_recovery_allowed(&targets(&[
            "TARGETS",
            "UTF8_STRING"
        ])));
        assert!(!image_recovery_allowed(&targets(&["image/png"])));
        assert!(image_recovery_allowed(&targets(&["image/bmp"])));
        assert!(image_recovery_allowed(&[]));
    }

    fn targets(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_string()).collect()
    }

    fn text_payload(text: &str) -> ClipboardPayload {
        ClipboardPayload {
            mime: "text/plain".into(),
            wire_data: text.into(),
            hash: poolsync_core::hash_text(text),
        }
    }

    fn sent_clipboard(raw: &str) -> (String, u64, String) {
        match poolsync_core::decode_message(raw).unwrap() {
            Message::Clipboard {
                origin, seq, data, ..
            } => (origin, seq, data),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// L'ordre du mesh ne tient que si l'émetteur estampille réellement chaque
    /// copie : sans `origin`/`seq` sur le fil, tout retombe en mode legacy.
    #[test]
    fn a_sent_payload_carries_its_origin_and_clock_to_the_hub_and_the_peers() {
        let (hub_tx, mut hub_rx) = tokio::sync::broadcast::channel(8);
        let (peer_tx, mut peer_rx) = tokio::sync::mpsc::unbounded_channel();

        assert!(send_payload_network(
            &text_payload("bonjour"),
            &hub_tx,
            &Some(peer_tx),
            true,
            "desk-a",
            4_242,
            None,
        ));

        for raw in [hub_rx.try_recv().unwrap(), peer_rx.try_recv().unwrap()] {
            let (origin, seq, data) = sent_clipboard(&raw);
            assert_eq!(origin, "desk-a");
            assert_eq!(seq, 4_242);
            assert_eq!(data, "bonjour");
        }
    }

    /// `hub_clipboard = false` (pas d'upload de blob vers le VPS) ne doit pas
    /// désactiver le lien direct entre voisins.
    #[test]
    fn with_the_hub_relay_off_the_payload_still_reaches_the_peers() {
        let (hub_tx, mut hub_rx) = tokio::sync::broadcast::channel(8);
        let (peer_tx, mut peer_rx) = tokio::sync::mpsc::unbounded_channel();

        assert!(send_payload_network(
            &text_payload("bonjour"),
            &hub_tx,
            &Some(peer_tx),
            false,
            "desk-a",
            7,
            None,
        ));

        assert!(hub_rx.try_recv().is_err());
        assert_eq!(sent_clipboard(&peer_rx.try_recv().unwrap()).1, 7);
    }

    /// Une session hub absente ne doit jamais neutraliser le transport pair.
    #[test]
    fn with_no_hub_session_the_payload_still_reaches_the_peers() {
        let (hub_tx, hub_rx) = tokio::sync::broadcast::channel(8);
        drop(hub_rx);
        let (peer_tx, mut peer_rx) = tokio::sync::mpsc::unbounded_channel();

        assert!(send_payload_network(
            &text_payload("hors hub"),
            &hub_tx,
            &Some(peer_tx),
            true,
            "desk-a",
            8,
            None,
        ));

        let (origin, seq, data) = sent_clipboard(&peer_rx.try_recv().unwrap());
        assert_eq!(
            (origin.as_str(), seq, data.as_str()),
            ("desk-a", 8, "hors hub")
        );
    }

    #[test]
    fn sending_fails_when_no_transport_is_left() {
        let (hub_tx, hub_rx) = tokio::sync::broadcast::channel(8);
        drop(hub_rx);
        assert!(!send_payload_network(
            &text_payload("bonjour"),
            &hub_tx,
            &None,
            true,
            "desk-a",
            7,
            None,
        ));
    }

    /// Charge réellement capturée dans le pool le 29/08/2026 : la sortie de
    /// `xclip -t TARGETS -o` d'desk-b, diffusée à tous les nœuds comme du texte.
    /// Seconde charge capturée le 29/08 : la valeur de `xclip -t TIMESTAMP -o`,
    /// qui change à chaque sonde et générait donc du contenu neuf à l'infini.
    #[test]
    fn the_selection_timestamp_is_never_synced_as_text() {
        assert!(is_server_clock_echo("text/plain", "452550525", 452550525));
        assert!(is_server_clock_echo(
            "text/plain",
            " 452550525\n",
            452550525
        ));
    }

    /// Cas réel du 29/08 : l'horloge avait avancé de 1,3 s entre la lecture de
    /// TIMESTAMP et celle du texte. L'égalité stricte laissait tout passer.
    #[test]
    fn the_clock_moving_between_the_two_xclip_calls_is_still_caught() {
        assert!(is_server_clock_echo("text/plain", "455772428", 455_771_100));
        assert!(is_server_clock_echo("text/plain", "455772428", 455802428));
    }

    /// La mémoire tampon de l'agent : sous X11 le contenu copié meurt avec
    /// l'application qui l'a copié. En garder une copie permet de le resservir
    /// quand elle se ferme, sans jamais lui prendre la sélection de son vivant.
    #[test]
    fn the_agent_keeps_its_own_copy_of_the_clipboard() {
        remember_clipboard_content("text/plain", "contenu à conserver", "h1");
        let kept = LAST_KNOWN_PAYLOAD
            .lock()
            .unwrap()
            .clone()
            .expect("mémorisé");
        assert_eq!(kept.payload.wire_data, "contenu à conserver");
        assert_eq!(kept.payload.mime, "text/plain");

        // Une copie plus récente remplace la précédente.
        remember_clipboard_content("text/plain", "plus récent", "h2");
        assert_eq!(
            LAST_KNOWN_PAYLOAD
                .lock()
                .unwrap()
                .clone()
                .unwrap()
                .payload
                .wire_data,
            "plus récent"
        );
    }

    /// La reprise n'a lieu que lorsqu'il n'y a plus AUCUN propriétaire : tant
    /// qu'une application sert la sélection, on ne la dérange pas — c'est ce
    /// qui évite la course qui figeait Chrome et VSCode au collage.
    #[test]
    fn the_takeover_only_targets_an_ownerless_selection() {
        let source = include_str!("clipboard.rs");
        let f = source
            .split("pub async fn reclaim_orphaned_selection")
            .nth(1)
            .expect("fonction présente");
        let body = f.split("\n/// ").next().unwrap_or(f);
        assert!(
            body.contains("current_clipboard_owner() != 0"),
            "on ne doit intervenir que sans propriétaire"
        );
        assert!(
            body.contains("ORPHAN_CHECK_INTERVAL"),
            "le sondage ouvre une connexion X11 : il doit être limité en fréquence"
        );
    }

    /// Sous xrdp on ne doit jamais réécrire PRIMARY : c'est ce miroir, et non
    /// GTK lui-même, qui faisait vaciller la sélection sous Chromium. L'offre
    /// GTK, elle, doit être utilisée partout — c'est elle qui nous signale les
    /// collages en cours, donc le garde-fou contre les gels.
    #[test]
    fn the_gtk_offer_is_used_everywhere_and_primary_is_mirrored_only_off_xrdp() {
        let source = include_str!("clipboard.rs");
        let offer = source
            .split("fn offer_text_payload")
            .nth(1)
            .expect("fonction présente");
        let body = offer.split("\nfn ").next().unwrap_or(offer);
        assert!(
            body.contains("let mirror_primary = !xrdp_session_active();"),
            "le miroir PRIMARY doit être conditionné à l'absence de xrdp"
        );
        assert!(
            !body.contains("return write_selection_text_sync(\"clipboard\""),
            "plus de court-circuit xclip sous xrdp : GTK est utilisé partout"
        );
    }

    /// Une lecture en échec n'est pas un presse-papiers vide : sous la pression
    /// des collages, croire l'inverse réinjectait un vieux surlignage par-dessus
    /// la copie qui venait d'arriver.
    #[test]
    fn an_unreadable_clipboard_is_not_treated_as_empty() {
        let source = include_str!("clipboard.rs");
        let mirror = source
            .split("pub async fn mirror_primary_to_clipboard_if_needed")
            .nth(1)
            .expect("fonction présente");
        let body = mirror.split("\nasync fn ").next().unwrap_or(mirror);
        assert!(
            body.contains("targets_advertise_text(&clip_targets)"),
            "le miroir doit vérifier les cibles avant de conclure au vide"
        );
    }

    /// La boucle du 30/08 : sur xrdp, un surlignage souris (PRIMARY) écrasait
    /// une copie arrivée du réseau, qui était aussitôt rediffusée, et les deux
    /// machines se renvoyaient leurs sélections sans fin. PRIMARY ne doit servir
    /// que quand CLIPBOARD n'a rien d'utilisable.
    #[test]
    fn primary_is_a_fallback_never_an_override() {
        // Le miroir PRIMARY→CLIPBOARD s'arrête dès que CLIPBOARD a du texte,
        // et cette règle ne dépend plus du fait d'être sous xrdp.
        let source = include_str!("clipboard.rs");
        let mirror = source
            .split("pub async fn mirror_primary_to_clipboard_if_needed")
            .nth(1)
            .expect("fonction présente");
        let body = mirror.split("\nasync fn ").next().unwrap_or(mirror);
        assert!(
            body.contains("if clip_text.is_some() {\n        return;"),
            "le miroir doit renoncer dès que CLIPBOARD contient du texte"
        );
        assert!(
            !body.contains("clip_text.is_some() && clipboard_owner_is_chromium_based()"),
            "l'ancienne exception xrdp laissait PRIMARY écraser CLIPBOARD"
        );
    }

    /// Le trou qui a laissé VSCode geler : son WM_CLASS ne contient ni
    /// « chrom » ni « electron », il échappait donc à toutes les protections.
    #[test]
    fn vscode_and_cursor_are_recognised_as_chromium_based() {
        assert!(chromium_based_identity(b"code\0Code\0"));
        assert!(chromium_based_identity(b"cursor\0Cursor\0"));
        assert!(chromium_based_identity(b"Code\0Code\0"));
    }

    #[test]
    fn the_families_already_covered_stay_covered() {
        assert!(chromium_based_identity(b"google-chrome\0Google-chrome\0"));
        assert!(chromium_based_identity(b"Navigator\0Firefox\0"));
        assert!(chromium_based_identity(b"Chromium clipboard"));
        assert!(chromium_based_identity(b"electron\0Electron\0"));
    }

    /// La comparaison est faite jeton par jeton : un nom qui *contient* « code »
    /// sans être Chromium ne doit pas être pris pour un éditeur.
    #[test]
    fn a_class_merely_containing_a_short_name_is_not_matched() {
        assert!(!chromium_based_identity(
            b"barcode-scanner\0Barcode-scanner\0"
        ));
        assert!(!chromium_based_identity(b"xterm\0XTerm\0"));
        assert!(!chromium_based_identity(b"Thunar\0Thunar\0"));
        assert!(!chromium_based_identity(
            b"poolsync-agent\0Poolsync-agent\0"
        ));
        assert!(!chromium_based_identity(b""));
    }

    /// Le report est borné : une application qui lit en boucle ne doit pas
    /// pouvoir bloquer indéfiniment la synchronisation du pool.
    #[test]
    fn the_paste_defer_is_bounded() {
        assert!(PASTE_DEFER_MAX <= Duration::from_secs(1));
        assert!(PASTE_DEFER_STEP < PASTE_DEFER_MAX);
        // La fenêtre de détection doit être plus courte que l'attente totale,
        // sinon la condition de sortie ne peut jamais devenir fausse.
        assert!(PASTE_IN_FLIGHT_WINDOW < PASTE_DEFER_MAX);
    }

    /// Une fois l'état hérité adopté, la même charge n'est plus vue comme une
    /// copie : c'est ce qui empêche un redémarrage de faire régresser le pool.
    #[test]
    fn an_adopted_baseline_is_not_re_detected_as_a_local_copy() {
        let _serial = crate::clipboard_gtk::IMAGE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let payload = text_payload("contenu hérité du démarrage");
        let last = Mutex::new(String::new());
        // Ce que seed_local_baseline fait du hash.
        *last.lock().unwrap() = payload.hash.clone();
        assert!(!prepare_local_clipboard(&payload, &last));
        // Une vraie copie ultérieure passe toujours.
        assert!(prepare_local_clipboard(
            &text_payload("nouvelle copie"),
            &last
        ));
    }

    /// Le cas qui a laissé la tempête continuer : sur une sélection dégradée,
    /// la lecture de TIMESTAMP échoue, donc le garde-fou n'avait aucune
    /// référence et laissait passer l'horloge. L'ancre extrapolée la fournit.
    #[test]
    fn the_estimated_clock_takes_over_when_timestamp_cannot_be_read() {
        note_server_clock("700000000");
        let estimated = estimated_server_clock().expect("ancre posée");
        assert!(estimated >= 700_000_000);
        assert!(
            estimated < 700_060_000,
            "extrapolation aberrante: {estimated}"
        );
        assert!(is_server_clock_echo("text/plain", "700000000", estimated));
    }

    /// Au-delà de la tolérance, c'est un nombre comme un autre : on synchronise.
    #[test]
    fn a_number_far_from_the_selection_clock_is_synced_normally() {
        assert!(!is_server_clock_echo("text/plain", "455772428", 455900000));
        assert!(!is_server_clock_echo("text/plain", "12345", 12345678));
        assert!(!is_server_clock_echo(
            "text/plain",
            "4557724281234",
            455772428
        ));
        assert!(!is_server_clock_echo(
            "text/plain",
            "455772428abc",
            455772428
        ));
    }

    /// Un vrai nombre copié par l'utilisateur ne doit pas être confondu : il
    /// faudrait qu'il soit égal à l'horloge X11 de la sélection au même instant.
    #[test]
    fn a_number_copied_by_the_user_is_still_synced() {
        assert!(!is_server_clock_echo("text/plain", "451432216", 452550525));
        assert!(!is_server_clock_echo("text/plain", "0612345678", 452550525));
        // Une image dont le base64 coïnciderait n'est pas concernée.
        assert!(!is_server_clock_echo("image/png", "452550525", 452550525));
    }

    #[test]
    fn the_targets_dump_that_flooded_the_pool_is_recognised() {
        let fixture = "TIMESTAMP\nTARGETS\nUTF8_STRING\nSTRING";
        assert!(is_target_list_dump(fixture));
        assert!(!is_syncable_text(fixture));
    }

    #[test]
    fn a_longer_targets_dump_with_mime_targets_is_recognised_too() {
        let dump = "TIMESTAMP\nTARGETS\nMULTIPLE\nUTF8_STRING\nSTRING\nTEXT\ntext/plain\ntext/plain;charset=utf-8";
        assert!(is_target_list_dump(dump));
    }

    /// Le garde-fou ne doit pas manger du vrai contenu : deux chemins, une
    /// liste de types MIME écrite par l'utilisateur, une seule ligne.
    #[test]
    fn ordinary_multiline_text_is_never_taken_for_a_probe_dump() {
        assert!(!is_target_list_dump("src/main.rs\nsrc/lib.rs"));
        assert!(!is_target_list_dump("image/png\nimage/jpeg"));
        assert!(!is_target_list_dump("TARGETS"));
        assert!(!is_target_list_dump("bonjour\nle monde"));
        assert!(!is_target_list_dump(""));
    }

    /// Un seul atome connu ne suffit pas : « STRING » peut apparaître dans du
    /// code copié à côté d'un chemin.
    #[test]
    fn a_single_known_atom_next_to_a_path_is_not_a_probe_dump() {
        assert!(!is_target_list_dump("STRING\nsrc/main.rs"));
    }

    #[test]
    fn only_text_targets_may_be_read_as_content() {
        for good in ["UTF8_STRING", "STRING", "TEXT", "text/plain"] {
            assert!(X11_TEXT_TARGETS
                .iter()
                .any(|t| t.eq_ignore_ascii_case(good)));
        }
        // Les deux sondes qui ont inondé le pool ne sont pas des cibles texte.
        for probe in ["TIMESTAMP", "TARGETS"] {
            assert!(!X11_TEXT_TARGETS
                .iter()
                .any(|t| t.eq_ignore_ascii_case(probe)));
        }
    }

    #[test]
    fn a_half_dead_owner_advertising_text_is_detected() {
        let advertised = targets(&["TIMESTAMP", "TARGETS", "UTF8_STRING", "STRING"]);
        assert!(targets_advertise_text(&advertised));
        // Une sélection qui n'annonce que des métadonnées n'a rien de textuel.
        assert!(!targets_advertise_text(&targets(&["TIMESTAMP", "TARGETS"])));
    }

    #[test]
    fn png_and_jpeg_are_pasteable_images_but_bmp_only_is_not() {
        assert!(targets_have_pasteable_image(&targets(&["image/png"])));
        assert!(targets_have_pasteable_image(&targets(&["image/jpeg"])));
        assert!(is_rdp_bmp_only(&targets(&["TARGETS", "image/bmp"])));
        assert!(!is_rdp_bmp_only(&targets(&["image/bmp", "image/png"])));
    }

    #[test]
    fn primary_only_image_is_collected_for_the_buffer() {
        assert!(should_read_primary_image(true, &targets(&["UTF8_STRING"])));
        assert!(!should_read_primary_image(true, &targets(&["image/png"])));
        assert!(!should_read_primary_image(
            false,
            &targets(&["UTF8_STRING"])
        ));
    }

    #[test]
    fn incomplete_gimp_targets_trigger_a_safe_image_probe() {
        assert!(should_probe_unadvertised_image(true, false, None));
        assert!(!should_probe_unadvertised_image(true, true, None));
        assert!(!should_probe_unadvertised_image(
            true,
            false,
            Some("real text")
        ));
        assert!(!should_probe_unadvertised_image(false, false, None));
    }

    #[test]
    fn chromium_clipboard_window_without_wm_class_is_detected() {
        assert!(chromium_based_identity(b"Chromium clipboard"));
        assert!(chromium_based_identity(b"google-chrome"));
        assert!(chromium_based_identity(b"Electron clipboard"));
        assert!(!chromium_based_identity(b"xfce4-terminal"));
    }

    #[test]
    fn html_is_converted_to_safe_visible_text() {
        assert_eq!(
            html_to_visible_text("<p>Bonjour&nbsp;<b>PoolSync</b> &amp; Cursor</p>"),
            "Bonjour PoolSync & Cursor"
        );
    }

    #[test]
    fn incoming_html_without_formatting_becomes_plain_text() {
        let (data, mime) = local_write_text("<div>Hello <b>world</b></div>", "text/html", false);
        assert_eq!(mime, "text/plain");
        assert_eq!(data, "Hello world");
    }

    /// `keep_formatting = true` doit laisser passer le balisage : c'est lui que
    /// `offer_text_payload` publie ensuite comme cible `text/html`.
    #[test]
    fn incoming_html_with_formatting_on_keeps_its_markup() {
        let html = "<html><body><p>Hello <b>world</b></p></body></html>";
        let (data, mime) = local_write_text(html, "text/html", true);
        if xrdp_session_active() {
            // Sous xrdp on aplatit toujours : Chromium interprète le HTML.
            assert_eq!(mime, "text/plain");
        } else {
            assert_eq!(mime, "text/html");
            assert_eq!(data, html);
        }
    }

    /// Garde-fou de `looks_like_markup` : un fragment isolé (une puce, un mot
    /// en gras) ne justifie pas d'injecter du HTML dans X11, on l'aplatit.
    #[test]
    fn a_tiny_html_fragment_is_flattened_even_with_formatting_on() {
        let (data, mime) = local_write_text("<div>Hello <b>world</b></div>", "text/html", true);
        assert_eq!(mime, "text/plain");
        assert_eq!(data, "Hello world");
    }

    /// Du texte brut n'est jamais promu en HTML, quelle que soit l'option.
    #[test]
    fn plain_text_is_never_turned_into_html_by_the_formatting_option() {
        let (data, mime) = local_write_text("juste du texte", "text/plain", true);
        assert_eq!(mime, "text/plain");
        assert_eq!(data, "juste du texte");
    }

    /// Un « HTML » sans balise n'a rien à formater : il part en texte brut.
    #[test]
    fn html_without_any_markup_is_flattened_even_with_formatting_on() {
        let (_, mime) = local_write_text("pas de balise ici", "text/html", true);
        assert_eq!(mime, "text/plain");
    }

    #[test]
    fn image_sidecars_never_replace_an_image_with_old_text() {
        assert!(text_is_image_sidecar("file:///tmp/Screenshot.png"));
        assert!(text_is_image_sidecar(
            "/home/demo-user/Images/capture-ecran.png"
        ));
        assert!(text_is_image_sidecar("https://example.test/image.png"));
        assert!(!text_is_image_sidecar("nouveau texte utilisateur"));
    }

    #[test]
    fn chrome_text_copy_is_distinguished_from_copy_image() {
        let chrome = targets(&["UTF8_STRING", "text/html", "image/png"]);
        assert!(is_chrome_style_text_copy(
            &chrome,
            Some("adresse@example.test")
        ));
        assert!(!is_chrome_style_text_copy(
            &chrome,
            Some("https://example.test/a.png")
        ));
    }

    #[test]
    fn menu_and_notifications_use_short_hash_without_panicking() {
        assert_eq!(trace_id("abc"), "abc");
        assert_eq!(trace_id("123456789012abcdef"), "123456789012");
    }

    #[test]
    fn copied_text_is_sanitized_and_bounded() {
        assert_eq!(sanitize_copied_text("  a\u{00a0}b  "), "a b");
        assert!(is_syncable_text("texte normal"));
        assert!(!is_syncable_text("x"));
        assert!(!is_syncable_text(&"x".repeat(MAX_TEXT_BYTES + 1)));
    }

    #[test]
    fn even_bmp_bridge_targets_cannot_replace_a_valid_new_text_copy() {
        assert!(!image_recovery_allowed(&targets(&[
            "image/bmp",
            "UTF8_STRING"
        ])));
        assert!(!image_recovery_allowed(&targets(&[
            "UTF8_STRING",
            "text/plain"
        ])));
    }

    #[test]
    fn reoffer_is_needed_only_when_no_pasteable_image_target_remains() {
        assert!(targets_have_pasteable_image(&targets(&["image/png"])));
        assert!(!targets_have_pasteable_image(&targets(&[
            "UTF8_STRING",
            "STRING",
        ])));
    }

    #[test]
    fn primary_baseline_distinguishes_old_and_new_copies() {
        assert!(!primary_is_newer_than_applied(Some("old"), "old"));
        assert!(primary_is_newer_than_applied(Some("old"), "new"));
        assert!(primary_is_newer_than_applied(None, "new"));
    }
}
