use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::thumb::{thumb_png_bytes_from_wire, NOTIFY_THUMB_MAX_PX};

pub fn notify_thumbnail_path(mime: &str, wire_b64: &str) -> Result<String> {
    let png = thumb_png_bytes_from_wire(wire_b64, NOTIFY_THUMB_MAX_PX)
        .with_context(|| format!("thumbnail for notification ({mime})"))?;

    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let dir = PathBuf::from(home).join(".cache/poolsync");
    std::fs::create_dir_all(&dir).context("create notify thumb cache dir")?;
    let path = dir.join("notify-thumb.png");
    std::fs::write(&path, png).with_context(|| format!("write {}", path.display()))?;
    Ok(path.to_string_lossy().into_owned())
}
