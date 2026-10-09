//! Application d'un collage entrant (hub ou peer mesh).

use crate::clip_cache;
use crate::clipboard::{
    clipboard_targets, local_write_text, recover_stripped_image, write_clipboard_if,
};
use crate::state::{clip_preview_mime, AgentState};
use tracing::info;

fn applied_text_hash(data: &str) -> String {
    poolsync_core::hash_text(data)
}

/// Applique un collage reçu (hub ou voisin direct).
///
/// `origin` / `seq` = horodatage logique de la copie d'origine (cf.
/// `clip_order`). Ils remplacent les anciennes fenêtres de grâce : l'ordre est
/// total, donc identique sur tous les nœuds quelle que soit la latence.
#[allow(clippy::too_many_arguments)]
pub async fn apply_incoming_clipboard(
    state: &AgentState,
    hash: &str,
    data: &str,
    mime: &str,
    source_node: &str,
    from_hub: bool,
    origin: &str,
    seq: u64,
) -> anyhow::Result<()> {
    let participation_epoch = state.participation_epoch();
    // An absent laptop must not accumulate office clipboard contents.
    if state.pool_away() {
        return Ok(());
    }
    if mime.starts_with("image/") {
        let via = if from_hub { "hub" } else { "peer" };
        info!(
            "image-trace RECEIVE id={} source={} via={} mime={} wire_bytes={}",
            crate::clipboard::trace_id(hash),
            source_node,
            via,
            mime,
            data.len()
        );
    }
    // Synchro coupée ou agent en pause : on ne touche pas à la sélection X11 —
    // c'est le copier-coller natif de la session qui fait foi. Mais « coupé »
    // ne veut pas dire « sourd » : ce que le pool partage est quand même gardé
    // dans le tampon et l'historique, donc récupérable à la demande.
    // The native RDP channel owns the client's clipboard during a session.
    // Keep receiving and relaying pool history without competing for X11 ownership.
    let rdp_paused =
        state.config.pause_clipboard_when_rdp && crate::rdp_detect::rdp_client_active().await;
    let touch_selection =
        state.clipboard_sync_enabled() && state.local_poolsync_active() && !rdp_paused;
    // Un pair encore sur l'ancien binaire peut diffuser la sortie de ses propres
    // sondes X11 (liste de cibles) comme si c'était une copie. Ne jamais
    // l'appliquer : sinon un nœud corrigé se fait re-polluer par le pool.
    if !mime.starts_with("image/") && crate::clipboard::is_target_list_dump(data) {
        tracing::warn!(
            "ignore sortie de sonde X11 reçue de {source_node} ({} octets)",
            data.len()
        );
        return Ok(());
    }
    // Ordre total : rejette d'un coup le message périmé (une copie locale ou
    // distante plus récente est déjà appliquée), notre propre écho revenu par
    // le mesh, et le doublon hub + pair — sans aucune minuterie.
    if !state.clip_order().accept_incoming(origin, seq) {
        tracing::info!(
            "ignore out-of-order clipboard from {source_node} (origin={origin} seq={seq})"
        );
        return Ok(());
    }

    let last_clip_hash = state.last_clip_hash_handle();
    {
        let mut last = last_clip_hash
            .lock()
            .map_err(|_| anyhow::anyhow!("clip hash lock"))?;
        if *last == hash {
            return Ok(());
        }
        *last = hash.to_string();
    }

    if !touch_selection {
        let preview = clip_preview_mime(mime, data);
        state.record_clip_received(preview.clone());
        clip_cache::store_received(hash, mime, data, &preview, source_node);
        crate::clipboard::remember_clipboard_content(mime, data, hash);
        state.notify_tray_history_changed();
        tracing::info!(
            "synchro coupée sur ce nœud : copie de {source_node} gardée dans le tampon, sélection non touchée (sync={} local_active={})",
            state.clipboard_sync_enabled(),
            state.local_poolsync_active()
        );
        return Ok(());
    }
    let (write_data, write_mime) = if mime.starts_with("image/") {
        crate::clipboard::seed_primary_baseline().await;
        (data.to_string(), mime.to_string())
    } else {
        local_write_text(data, mime, state.keep_formatting())
    };
    // Garder aussi en mémoire ce qui vient du réseau : si l'application qui
    // l'affiche est fermée ensuite, on pourra le resservir.
    let context = format!("incoming-{source_node}");
    match write_clipboard_if(&write_data, &write_mime, || {
        !state.pool_away()
            && state.participation_epoch() == participation_epoch
            && state.clip_order().is_current(origin, seq)
    })
    .await
    {
        Ok(false) => return Ok(()),
        Ok(true) => {
            crate::clipboard::remember_clipboard_content(&write_mime, &write_data, hash);
            if write_mime.starts_with("image/") {
                info!(
                    "image-trace APPLY id={} source={} mime={}",
                    crate::clipboard::trace_id(hash),
                    source_node,
                    write_mime
                );
                // GTK applies the queued offer on its main loop. Let it settle
                // before checking TARGETS, otherwise diagnostics report empty.
                tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                // xrdp-chansrv can take CLIPBOARD immediately after a PNG
                // offer, then expose only text targets. Re-offer while the
                // same image claim is still active; a later text copy clears
                // that claim, so it is never overwritten by an old image.
                let generation = crate::clipboard_gtk::offer_generation();
                tokio::spawn(async move {
                    for delay_ms in [200_u64, 700, 1_500] {
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                        if crate::clipboard_gtk::offer_generation() != generation {
                            break;
                        }
                        let (claim_active, cached_png_bytes) =
                            crate::clipboard_gtk::image_claim_debug_state();
                        if !claim_active {
                            tracing::info!(
                                "clipboard incoming image: no active claim after {delay_ms}ms (cached_png={cached_png_bytes})"
                            );
                            break;
                        }
                        let targets = clipboard_targets("clipboard").await.unwrap_or_default();
                        let reoffered = recover_stripped_image(&targets).await;
                        tracing::info!(
                            "clipboard incoming image: xrdp check after {delay_ms}ms claim={} cached_png={} targets={} reoffered={}",
                            claim_active,
                            cached_png_bytes,
                            targets.join(","),
                            reoffered
                        );
                        if reoffered {
                            tracing::info!(
                                "clipboard incoming image: PNG reoffer after xrdp takeover"
                            );
                        }
                    }
                });
            }
            crate::clipboard_diag::log_post_write(&write_mime, &context, true).await;
        }
        Err(e) => {
            crate::clipboard_diag::log_post_write(&write_mime, &context, false).await;
            tracing::warn!("clipboard write failed ({write_mime} from {source_node}): {e:#}");
            return Err(e);
        }
    }
    if write_mime == "text/plain" || write_mime == "text/html" {
        crate::clipboard::record_incoming_applied(&write_data);
        // `write_clipboard` puts text on the GTK main loop. Reading it back
        // immediately races that loop and used to store the *previous* hash,
        // causing every peer to re-broadcast old text. Hash the exact text we
        // wrote instead; this is also correct when incoming HTML was flattened.
        if let Ok(mut last) = last_clip_hash.lock() {
            *last = applied_text_hash(&write_data);
        }
    }
    // Do not xclip-read an image we just offered: we own CLIPBOARD on the
    // GTK thread; a same-process xclip -o deadlocks and leaves image/bmp empty.
    // Ne pas mark_image_clipboard_epoch ici : ça bloquerait le texte distant
    // pendant 4s après chaque image reçue (copier-coller texte mort après image).
    state.mark_incoming_clipboard_applied(mime);

    let preview = clip_preview_mime(mime, data);
    state.record_clip_received(preview.clone());
    clip_cache::store_received(hash, mime, data, &preview, source_node);

    let via = if from_hub { "hub" } else { "peer" };
    info!(
        "clipboard synced via {via} ({mime}, {} bytes wire)",
        data.len()
    );

    if state.should_notify(hash, &preview) {
        let preview = preview.clone();
        let mime = mime.to_string();
        let data = data.to_string();
        tokio::spawn(async move {
            crate::agent::show_clip_notification("PoolSync — Reçu", &preview, &mime, &data).await;
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applied_text_hash_tracks_the_transformed_text_not_the_wire_html() {
        let wire_html = "<p>Hello <b>PoolSync</b></p>";
        let (written, mime) = local_write_text(wire_html, "text/html", false);
        assert_eq!(mime, "text/plain");
        assert_eq!(written, "Hello PoolSync");
        assert_eq!(
            applied_text_hash(&written),
            poolsync_core::hash_text("Hello PoolSync")
        );
        assert_ne!(
            applied_text_hash(&written),
            poolsync_core::hash_text(wire_html)
        );
    }
}
