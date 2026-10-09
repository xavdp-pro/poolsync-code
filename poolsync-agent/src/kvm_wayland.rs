//! Backend KVM Wayland receive-only, fondé sur ydotool/uinput.
//!
//! Wayland interdit volontairement l'injection globale aux clients ordinaires.
//! ydotool fournit le chemin privilégié standard via /dev/uinput. La capture
//! globale reste désactivée : un portail RemoteDesktop doit être autorisé par
//! l'utilisateur et ne peut pas être silencieusement simulé.

use anyhow::{Context, Result};
use poolsync_core::InputKind;
use std::process::Command;

pub fn active() -> bool {
    std::env::var("XDG_SESSION_TYPE").is_ok_and(|value| value.eq_ignore_ascii_case("wayland"))
        && std::env::var_os("WAYLAND_DISPLAY").is_some()
}

fn run(args: &[String]) -> Result<()> {
    let status = Command::new("ydotool")
        .args(args)
        .status()
        .context("ydotool is required for Wayland KVM injection")?;
    if !status.success() {
        anyhow::bail!("ydotool failed with {status}");
    }
    Ok(())
}

pub fn warp_mouse(x: i32, y: i32) -> Result<()> {
    run(&[
        "mousemove".into(),
        "--absolute".into(),
        x.to_string(),
        y.to_string(),
    ])
}

pub fn inject(kind: &InputKind) -> Result<()> {
    match kind {
        InputKind::MouseMove { x, y } => warp_mouse(*x, *y),
        InputKind::MouseMoveRelative { dx, dy } => {
            run(&["mousemove".into(), dx.to_string(), dy.to_string()])
        }
        InputKind::MouseButton {
            button,
            pressed,
            x,
            y,
        } => {
            warp_mouse(*x, *y)?;
            let index = match button {
                1 => 0,
                2 => 2,
                3 => 1,
                other => u16::from(*other).saturating_sub(1),
            };
            let action = if *pressed { 0x40 } else { 0x80 } | index;
            run(&["click".into(), format!("0x{action:x}")])
        }
        InputKind::MouseWheel { delta, x, y } => {
            warp_mouse(*x, *y)?;
            let button = if *delta > 0 { "0xc4" } else { "0xc5" };
            run(&["click".into(), button.into()])
        }
        InputKind::Key { keycode, pressed } => {
            // X11 keycodes are evdev codes + 8 for the standard Linux keymap.
            let evdev = keycode.saturating_sub(8);
            run(&["key".into(), format!("{evdev}:{}", u8::from(*pressed))])
        }
    }
}
