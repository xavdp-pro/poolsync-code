//! Cache disque local du presse-papiers — lecture immédiate sans legacy hub.

use crate::clipboard::ClipboardPayload;
use crate::clipboard_history::HistoryItem;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_ENTRIES: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedClip {
    hash: String,
    mime: String,
    data: String,
    preview: String,
    source_node: String,
    at: u64,
    is_image: bool,
}

pub fn cache_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".cache/poolsync/clipboard")
}

fn entry_path(hash: &str) -> PathBuf {
    cache_dir().join(format!("{hash}.json"))
}

fn image_data_path(hash: &str) -> PathBuf {
    cache_dir().join(format!("{hash}.img"))
}

fn index_path() -> PathBuf {
    cache_dir().join("index.json")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn read_index() -> Vec<String> {
    let path = index_path();
    if !path.exists() {
        return Vec::new();
    }
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_index(hashes: &[String]) -> Result<()> {
    let dir = cache_dir();
    fs::create_dir_all(&dir).context("create clip cache dir")?;
    let body = serde_json::to_string(hashes)?;
    fs::write(index_path(), body).context("write clip cache index")?;
    Ok(())
}

fn prune_index(mut hashes: Vec<String>) {
    while hashes.len() > MAX_ENTRIES {
        if let Some(old) = hashes.pop() {
            let _ = fs::remove_file(entry_path(&old));
            let _ = fs::remove_file(image_data_path(&old));
        }
    }
    let _ = write_index(&hashes);
}

/// Enregistre un collage (local ou reçu d'un voisin).
pub fn store_payload(payload: &ClipboardPayload, preview: &str, source_node: &str) {
    let item = CachedClip {
        hash: payload.hash.clone(),
        mime: payload.mime.clone(),
        data: payload.wire_data.clone(),
        preview: preview.to_string(),
        source_node: source_node.to_string(),
        at: now_secs(),
        is_image: payload.mime.starts_with("image/"),
    };
    if let Err(err) = store_cached(&item) {
        tracing::debug!("clip cache store: {err:#}");
    }
}

pub fn store_received(hash: &str, mime: &str, data: &str, preview: &str, source_node: &str) {
    let item = CachedClip {
        hash: hash.to_string(),
        mime: mime.to_string(),
        data: data.to_string(),
        preview: preview.to_string(),
        source_node: source_node.to_string(),
        at: now_secs(),
        is_image: mime.starts_with("image/"),
    };
    if let Err(err) = store_cached(&item) {
        tracing::debug!("clip cache store received: {err:#}");
    }
}

fn store_cached(item: &CachedClip) -> Result<()> {
    fs::create_dir_all(cache_dir()).context("create clip cache dir")?;
    let mut stored = item.clone();
    if item.is_image {
        use base64::{engine::general_purpose::STANDARD as B64, Engine};
        let bytes = B64
            .decode(&item.data)
            .context("decode image for local cache")?;
        fs::write(image_data_path(&item.hash), &bytes).context("write image cache")?;
        stored.data = String::new();
    }
    let body = serde_json::to_string(&stored)?;
    fs::write(entry_path(&item.hash), &body).context("write clip cache entry")?;

    let mut hashes = read_index();
    hashes.retain(|h| h != &item.hash);
    hashes.insert(0, item.hash.clone());
    prune_index(hashes);
    Ok(())
}

pub fn get(hash: &str) -> Option<(String, String)> {
    let path = entry_path(hash);
    if !path.exists() {
        return None;
    }
    let raw = fs::read_to_string(&path).ok()?;
    let item: CachedClip = serde_json::from_str(&raw).ok()?;
    if item.is_image {
        let img_path = image_data_path(hash);
        if img_path.exists() {
            let bytes = fs::read(&img_path).ok()?;
            use base64::{engine::general_purpose::STANDARD as B64, Engine};
            return Some((item.mime, B64.encode(bytes)));
        }
    }
    if item.data.is_empty() {
        return None;
    }
    Some((item.mime, item.data))
}

pub fn list_recent(limit: usize) -> Vec<HistoryItem> {
    let limit = limit.clamp(1, MAX_ENTRIES);
    let hashes = read_index();
    hashes
        .into_iter()
        .take(limit)
        .filter_map(|hash| load_history_item(&hash))
        .collect()
}

fn load_history_item(hash: &str) -> Option<HistoryItem> {
    let path = entry_path(hash);
    if !path.exists() {
        return None;
    }
    let raw = fs::read_to_string(&path).ok()?;
    let item: CachedClip = serde_json::from_str(&raw).ok()?;
    Some(HistoryItem {
        hash: item.hash,
        mime: item.mime,
        preview: item.preview,
        source_node: item.source_node,
        at: item.at,
        is_image: item.is_image,
        thumb_b64: None,
    })
}

pub fn clear_all() {
    let dir = cache_dir();
    if dir.is_dir() {
        let _ = fs::remove_dir_all(&dir);
    }
}

pub fn remove_hashes(hashes: &[String]) {
    if hashes.is_empty() {
        return;
    }
    let mut index = read_index();
    for h in hashes {
        let _ = fs::remove_file(entry_path(h));
        let _ = fs::remove_file(image_data_path(h));
        index.retain(|x| x != h);
    }
    let _ = write_index(&index);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn with_temp_cache<F: FnOnce()>(f: F) {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let tmp = std::env::temp_dir().join(format!("poolsync-clip-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        std::env::set_var("HOME", tmp.to_string_lossy().as_ref());
        f();
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn store_and_get_round_trip() {
        with_temp_cache(|| {
            let payload = ClipboardPayload {
                mime: "text/plain".into(),
                wire_data: "hello cache".into(),
                hash: "abc123".into(),
            };
            store_payload(&payload, "hello cache", "desk-a");
            let (mime, data) = get("abc123").expect("cached");
            assert_eq!(mime, "text/plain");
            assert_eq!(data, "hello cache");
        });
    }

    #[test]
    fn store_image_uses_binary_file() {
        with_temp_cache(|| {
            use base64::{engine::general_purpose::STANDARD as B64, Engine};
            let png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
            let payload = ClipboardPayload {
                mime: "image/png".into(),
                wire_data: B64.encode(&png),
                hash: "imghash1".into(),
            };
            store_payload(&payload, "[image]", "desk-a");
            let path = cache_dir().join("imghash1.img");
            assert!(path.is_file());
            let (mime, data) = get("imghash1").expect("image cached");
            assert_eq!(mime, "image/png");
            assert_eq!(B64.decode(data).unwrap(), png);
        });
    }

    #[test]
    fn clear_all_removes_cache() {
        with_temp_cache(|| {
            let payload = ClipboardPayload {
                mime: "text/plain".into(),
                wire_data: "x".into(),
                hash: "h1".into(),
            };
            store_payload(&payload, "x", "desk-a");
            assert!(cache_dir().is_dir());
            clear_all();
            assert!(!cache_dir().exists());
        });
    }
}
