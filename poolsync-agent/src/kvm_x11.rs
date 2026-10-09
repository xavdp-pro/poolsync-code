use anyhow::{Context, Result};
use poolsync_core::ScreenInfo;
use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
};
use tracing::warn;
use x11rb::connection::Connection;
use x11rb::protocol::randr;
use x11rb::protocol::xfixes;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt as XprotoExt, EventMask, PropMode,
};
use x11rb::protocol::xtest;
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;

thread_local! {
    static XDO: RefCell<Option<libxdo::XDo>> = const { RefCell::new(None) };
    static INJECTING: Cell<bool> = const { Cell::new(false) };
    static PASSIVE_EVENTS: Cell<bool> = const { Cell::new(false) };
}

/// Entrée physique locale (clavier / souris) — pas une injection KVM distante.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalInput {
    Key,
    Button,
    Motion,
}

pub fn set_injecting(active: bool) {
    INJECTING.with(|c| c.set(active));
}

/// Détecte une touche ou un clic physique (pour reprendre le rôle master sur un nœud esclave).
pub fn poll_physical_input() -> Option<PhysicalInput> {
    if let Some(physical) = crate::physical_input::poll() {
        return physical;
    }
    if INJECTING.with(|c| c.get()) {
        return None;
    }
    with_x11_conn(|conn, screen_num| {
        let root = conn.setup().roots[screen_num].root;
        if !PASSIVE_EVENTS.with(|c| c.get()) {
            conn.change_window_attributes(
                root,
                &ChangeWindowAttributesAux::new()
                    .event_mask(EventMask::KEY_PRESS | EventMask::BUTTON_PRESS),
            )?;
            conn.flush()?;
            PASSIVE_EVENTS.with(|c| c.set(true));
        }
        let mut physical = None;
        loop {
            match conn.poll_for_event() {
                Ok(Some(Event::KeyPress(ev))) if ev.detail != 0 => {
                    physical = Some(PhysicalInput::Key);
                }
                Ok(Some(Event::ButtonPress(_))) => physical = Some(PhysicalInput::Button),
                Ok(Some(_)) => continue,
                Ok(None) => return Ok(physical),
                Err(err) => return Err(err.into()),
            }
        }
    })
    .ok()
    .flatten()
}

/// Écran « pool » KVM : moniteur primaire RandR (pas le bureau X11 étendu).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KvmDisplay {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl KvmDisplay {
    pub fn screen_info(&self) -> ScreenInfo {
        ScreenInfo {
            width: self.width,
            height: self.height,
        }
    }

    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x
            && py >= self.y
            && px < self.x + self.width as i32
            && py < self.y + self.height as i32
    }

    pub fn to_local(self, px: i32, py: i32) -> (i32, i32) {
        (px - self.x, py - self.y)
    }

    pub fn to_root(self, lx: i32, ly: i32) -> (i32, i32) {
        (lx + self.x, ly + self.y)
    }

    /// Pixel at the geometric center of this monitor (root coordinates).
    pub fn center_root(&self) -> (i32, i32) {
        (
            self.x + self.width as i32 / 2,
            self.y + self.height as i32 / 2,
        )
    }
}

fn with_xdo<F>(f: F) -> Result<()>
where
    F: FnOnce(&libxdo::XDo) -> libxdo::OpResult,
{
    XDO.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot =
                Some(libxdo::XDo::new(None).map_err(|e| anyhow::anyhow!("libxdo init: {e:?}"))?);
        }
        f(slot.as_ref().expect("xdo")).map_err(|e| anyhow::anyhow!("{e:?}"))
    })
}

fn with_x11_conn<F, T>(f: F) -> Result<T>
where
    F: FnOnce(&x11rb::rust_connection::RustConnection, usize) -> Result<T>,
{
    thread_local! {
        static CONN: RefCell<Option<(x11rb::rust_connection::RustConnection, usize)>> =
            const { RefCell::new(None) };
    }
    CONN.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let (conn, screen) = x11rb::connect(None).context("connexion X11")?;
            *slot = Some((conn, screen));
        }
        let (conn, screen) = slot.as_mut().expect("x11");
        f(conn, *screen)
    })
}

pub fn mouse_location() -> Result<(i32, i32)> {
    with_x11_conn(|conn, screen_num| {
        let root = conn.setup().roots[screen_num].root;
        let reply = conn.query_pointer(root)?.reply()?;
        Ok((reply.root_x as i32, reply.root_y as i32))
    })
}

pub fn warp_mouse(x: i32, y: i32) -> Result<()> {
    with_xdo(|xdo| xdo.move_mouse(x, y, 0))
}

pub fn move_mouse_absolute(x: i32, y: i32) -> Result<()> {
    warp_mouse(x, y)
}
pub fn move_mouse_relative(dx: i32, dy: i32) -> Result<()> {
    if dx == 0 && dy == 0 {
        return Ok(());
    }
    with_xdo(|xdo| xdo.move_mouse_relative(dx, dy))
}

pub fn mouse_button(button: u8, pressed: bool) -> Result<()> {
    with_x11_conn(|conn, screen_num| {
        let root = conn.setup().roots[screen_num].root;
        xtest::fake_input(conn, if pressed { 4 } else { 5 }, button, 0, root, 0, 0, 0)?.check()?;
        Ok(())
    })
}

pub fn key_event(keycode: u32, pressed: bool) -> Result<()> {
    let code = keycode as u8;
    if code == 0 {
        return Ok(());
    }
    with_x11_conn(|conn, screen_num| {
        let root = conn.setup().roots[screen_num].root;
        let event_type = if pressed { 2u8 } else { 3u8 };
        xtest::fake_input(conn, event_type, code, 0, root, 0, 0, 0)?.check()?;
        Ok(())
    })
}

pub fn click_wheel_button(button: i32) -> Result<()> {
    mouse_button(button as u8, true)?;
    mouse_button(button as u8, false)
}

const HELD_MAGIC: u32 = 0x50534b31;
const HELD_MAX_WORDS: u32 = 4 + 248 + 255;

fn held_words(keys: &HashSet<u32>, buttons: &HashSet<u8>) -> Result<Vec<u32>> {
    anyhow::ensure!(
        keys.iter().all(|k| (8..=255).contains(k)) && !buttons.contains(&0),
        "invalid injected input state"
    );
    let mut keys: Vec<_> = keys.iter().copied().collect();
    let mut buttons: Vec<_> = buttons.iter().map(|b| u32::from(*b)).collect();
    keys.sort_unstable();
    buttons.sort_unstable();
    let mut words = vec![HELD_MAGIC, 1, keys.len() as u32];
    words.extend(keys);
    words.push(buttons.len() as u32);
    words.extend(buttons);
    Ok(words)
}

fn parse_held_words(words: &[u32]) -> Result<(HashSet<u32>, HashSet<u8>)> {
    anyhow::ensure!(
        words.len() >= 4
            && words.len() <= HELD_MAX_WORDS as usize
            && words[0] == HELD_MAGIC
            && words[1] == 1,
        "invalid injected input property header"
    );
    let count = words[2] as usize;
    anyhow::ensure!(
        count <= 248 && words.len() >= 4 + count,
        "invalid injected key count"
    );
    let buttons = words[3 + count] as usize;
    anyhow::ensure!(
        buttons <= 255 && words.len() == 4 + count + buttons,
        "invalid injected button count"
    );
    let key_words = &words[3..3 + count];
    let button_words = &words[4 + count..];
    anyhow::ensure!(
        key_words.iter().all(|k| (8..=255).contains(k))
            && button_words.iter().all(|b| (1..=255).contains(b)),
        "invalid injected input code"
    );
    let keys: HashSet<_> = key_words.iter().copied().collect();
    let buttons: HashSet<_> = button_words.iter().map(|b| *b as u8).collect();
    anyhow::ensure!(
        keys.len() == count && buttons.len() == button_words.len(),
        "duplicate injected input code"
    );
    Ok((keys, buttons))
}

/// Only currently held remote inputs live in X server memory. A root property
/// outlives the injecting process, but disappears with the graphical session.
/// It contains no text, completed input history or physical-device state.
pub fn store_injected_state(node: &str, keys: &HashSet<u32>, buttons: &HashSet<u8>) -> Result<()> {
    let words = held_words(keys, buttons)?;
    with_x11_conn(|conn, screen_num| {
        let root = conn.setup().roots[screen_num].root;
        let atom = conn
            .intern_atom(false, format!("_POOLSYNC_INJECTED_V1_{node}").as_bytes())?
            .reply()?
            .atom;
        if keys.is_empty() && buttons.is_empty() {
            conn.delete_property(root, atom)?.check()?;
        } else {
            conn.change_property32(PropMode::REPLACE, root, atom, AtomEnum::CARDINAL, &words)?
                .check()?;
        }
        Ok(())
    })
}

/// Recover this node's abandoned virtual inputs without moving the pointer or
/// clearing arbitrary physical/other-client input. Keep the property on error
/// so a subsequent restart can retry the idempotent releases.
pub fn recover_injected_state(node: &str) -> Result<()> {
    with_x11_conn(|conn, screen_num| {
        let root = conn.setup().roots[screen_num].root;
        let atom = conn
            .intern_atom(true, format!("_POOLSYNC_INJECTED_V1_{node}").as_bytes())?
            .reply()?
            .atom;
        if atom == 0 {
            return Ok(());
        }
        let reply = conn
            .get_property(false, root, atom, AtomEnum::ANY, 0, HELD_MAX_WORDS)?
            .reply()?;
        if reply.type_ == u32::from(AtomEnum::NONE) {
            return Ok(());
        }
        anyhow::ensure!(
            reply.type_ == u32::from(AtomEnum::CARDINAL)
                && reply.format == 32
                && reply.bytes_after == 0,
            "invalid injected input property format"
        );
        let words: Vec<_> = reply
            .value32()
            .context("missing injected input property values")?
            .collect();
        let (keys, buttons) = parse_held_words(&words)?;
        for key in keys {
            xtest::fake_input(conn, 3, key as u8, 0, root, 0, 0, 0)?.check()?;
        }
        for button in buttons {
            xtest::fake_input(conn, 5, button, 0, root, 0, 0, 0)?.check()?;
        }
        conn.delete_property(root, atom)?.check()?;
        Ok(())
    })
}

pub fn set_cursor_visible(visible: bool) -> Result<()> {
    with_x11_conn(|conn, screen_num| {
        let root = conn.setup().roots[screen_num].root;
        if visible {
            xfixes::show_cursor(conn, root)?;
        } else {
            xfixes::hide_cursor(conn, root)?;
        }
        conn.flush()?;
        Ok(())
    })
}

pub fn set_cursor_visible_best_effort(visible: bool) {
    if let Err(err) = set_cursor_visible(visible) {
        warn!("curseur X11 (visible={visible}): {err:#}");
    }
}
/// Moniteur primaire RandR : bords KVM + coordonnées pool (style Barrier).
pub fn kvm_display() -> Result<KvmDisplay> {
    with_x11_conn(|conn, screen_num| {
        let root = conn.setup().roots[screen_num].root;
        let primary_out = randr::get_output_primary(conn, root)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.output);

        let res = randr::get_screen_resources_current(conn, root)
            .context("randr get_screen_resources")?
            .reply()?;

        let mut fallback: Option<KvmDisplay> = None;

        for &crtc_id in &res.crtcs {
            let crtc = match randr::get_crtc_info(conn, crtc_id, res.config_timestamp) {
                Ok(cookie) => match cookie.reply() {
                    Ok(info) => info,
                    Err(_) => continue,
                },
                Err(_) => continue,
            };
            if crtc.width == 0 || crtc.height == 0 {
                continue;
            }
            let disp = KvmDisplay {
                x: crtc.x as i32,
                y: crtc.y as i32,
                width: crtc.width as u32,
                height: crtc.height as u32,
            };
            if let Some(primary) = primary_out {
                if crtc.outputs.contains(&primary) {
                    return Ok(disp);
                }
            }
            let area = disp.width.saturating_mul(disp.height);
            if fallback
                .map(|b| area > b.width.saturating_mul(b.height))
                .unwrap_or(true)
            {
                fallback = Some(disp);
            }
        }

        fallback.context("aucun moniteur actif (RandR)")
    })
}

/// All active RandR CRTCs (each physical monitor).
/// Tous les moniteurs actifs, avec leur nom RandR et le drapeau « primaire ».
///
/// `active_monitors` ne rend que des rectangles anonymes, suffisants pour le KVM
/// mais pas pour l'interface : on veut y afficher « eDP-1 » et « HDMI-1 », et
/// savoir lequel est primaire. Sans primaire déclaré, aucun n'est marqué —
/// l'appelant sait alors que le choix de l'écran de travail est arbitraire.
pub fn described_monitors() -> Result<Vec<poolsync_core::MonitorInfo>> {
    with_x11_conn(|conn, screen_num| {
        let root = conn.setup().roots[screen_num].root;
        let primary_out = randr::get_output_primary(conn, root)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.output);
        let res = randr::get_screen_resources_current(conn, root)
            .context("randr get_screen_resources")?
            .reply()?;
        let mut out = Vec::new();
        for &crtc_id in &res.crtcs {
            let crtc = match randr::get_crtc_info(conn, crtc_id, res.config_timestamp) {
                Ok(cookie) => match cookie.reply() {
                    Ok(info) => info,
                    Err(_) => continue,
                },
                Err(_) => continue,
            };
            if crtc.width == 0 || crtc.height == 0 {
                continue;
            }
            let mut name = String::new();
            let mut primary = false;
            for &output in &crtc.outputs {
                if Some(output) == primary_out {
                    primary = true;
                }
                if name.is_empty() {
                    if let Ok(cookie) = randr::get_output_info(conn, output, res.config_timestamp) {
                        if let Ok(info) = cookie.reply() {
                            name = String::from_utf8_lossy(&info.name).into_owned();
                        }
                    }
                }
            }
            out.push(poolsync_core::MonitorInfo {
                name,
                x: crtc.x as i32,
                y: crtc.y as i32,
                width: crtc.width as u32,
                height: crtc.height as u32,
                primary,
            });
        }
        out.sort_by_key(|m| (m.x, m.y));
        Ok(out)
    })
}

pub fn active_monitors() -> Result<Vec<KvmDisplay>> {
    with_x11_conn(|conn, screen_num| {
        let root = conn.setup().roots[screen_num].root;
        let res = randr::get_screen_resources_current(conn, root)
            .context("randr get_screen_resources")?
            .reply()?;
        let mut out = Vec::new();
        for &crtc_id in &res.crtcs {
            let crtc = match randr::get_crtc_info(conn, crtc_id, res.config_timestamp) {
                Ok(cookie) => match cookie.reply() {
                    Ok(info) => info,
                    Err(_) => continue,
                },
                Err(_) => continue,
            };
            if crtc.width == 0 || crtc.height == 0 {
                continue;
            }
            out.push(KvmDisplay {
                x: crtc.x as i32,
                y: crtc.y as i32,
                width: crtc.width as u32,
                height: crtc.height as u32,
            });
        }
        if out.is_empty() {
            let screen = &conn.setup().roots[screen_num];
            out.push(KvmDisplay {
                x: 0,
                y: 0,
                width: screen.width_in_pixels as u32,
                height: screen.height_in_pixels as u32,
            });
        }
        Ok(out)
    })
}

/// Warp the pointer to the center of the monitor that currently contains it.
/// Falls back to the KVM primary if the pointer is not on any CRTC.
pub fn center_pointer_on_current_monitor() -> Result<(i32, i32)> {
    let (px, py) = mouse_location()?;
    let monitors = active_monitors()?;
    let mon = monitors
        .iter()
        .find(|m| m.contains(px, py))
        .copied()
        .or_else(|| monitors.into_iter().next())
        .context("aucun moniteur")?;
    let (cx, cy) = mon.center_root();
    warp_mouse(cx, cy)?;
    Ok((cx, cy))
}

/// Rectangle englobant tous les moniteurs actifs (bureau X11 étendu).
pub fn kvm_desktop() -> Result<KvmDisplay> {
    with_x11_conn(|conn, screen_num| {
        let root = conn.setup().roots[screen_num].root;
        let res = randr::get_screen_resources_current(conn, root)
            .context("randr get_screen_resources")?
            .reply()?;

        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;
        let mut any = false;

        for &crtc_id in &res.crtcs {
            let crtc = match randr::get_crtc_info(conn, crtc_id, res.config_timestamp) {
                Ok(cookie) => match cookie.reply() {
                    Ok(info) => info,
                    Err(_) => continue,
                },
                Err(_) => continue,
            };
            if crtc.width == 0 || crtc.height == 0 {
                continue;
            }
            any = true;
            let cx = crtc.x as i32;
            let cy = crtc.y as i32;
            let cw = crtc.width as i32;
            let ch = crtc.height as i32;
            min_x = min_x.min(cx);
            min_y = min_y.min(cy);
            max_x = max_x.max(cx + cw);
            max_y = max_y.max(cy + ch);
        }

        if !any {
            return kvm_display();
        }

        Ok(KvmDisplay {
            x: min_x,
            y: min_y,
            width: (max_x - min_x).max(1) as u32,
            height: (max_y - min_y).max(1) as u32,
        })
    })
}

/// Repousse le curseur a l'interieur du moniteur pool apres un SwitchTo (evite rebond immediat).
pub fn nudge_kvm_enter(x: i32, y: i32, edge: i32, whole_desktop: bool) -> Result<(i32, i32)> {
    const ENTRY_ARM_PX: i32 = 24;
    let inset = edge + ENTRY_ARM_PX + 1;
    let pool = if whole_desktop {
        kvm_desktop()?
    } else {
        kvm_display()?
    };
    let (mut lx, mut ly) = pool.to_local(x, y);
    let w = pool.width as i32;
    let h = pool.height as i32;
    if lx <= edge {
        lx = inset;
    } else if lx >= w - edge {
        lx = (w - edge - ENTRY_ARM_PX - 1).max(inset);
    }
    if ly <= edge {
        ly = inset;
    } else if ly >= h - edge {
        ly = (h - edge - ENTRY_ARM_PX - 1).max(inset);
    }
    let (x, y) = pool.to_root(lx, ly);
    Ok(if whole_desktop {
        poolsync_core::clamp_pointer_to_monitors(&described_monitors()?, x, y)
    } else {
        (x, y)
    })
}

pub fn kvm_layout_snapshot() -> Result<poolsync_core::KvmDesktopInfo> {
    let primary = kvm_display()?;
    let desktop = kvm_desktop()?;
    Ok(poolsync_core::KvmDesktopInfo {
        monitor_x: primary.x,
        monitor_y: primary.y,
        desktop_x: desktop.x,
        desktop_y: desktop.y,
        desktop_width: desktop.width,
        desktop_height: desktop.height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_property_roundtrips_only_current_keys_and_buttons() {
        let keys = HashSet::from([50, 37, 255]);
        let buttons = HashSet::from([1, 5, 255]);
        let words = held_words(&keys, &buttons).unwrap();
        assert_eq!(parse_held_words(&words).unwrap(), (keys, buttons));
        assert_eq!(
            words,
            held_words(&HashSet::from([255, 37, 50]), &HashSet::from([255, 5, 1])).unwrap()
        );
        assert_eq!(
            parse_held_words(&held_words(&HashSet::new(), &HashSet::new()).unwrap()).unwrap(),
            (HashSet::new(), HashSet::new())
        );
    }

    #[test]
    fn held_property_rejects_invalid_lengths_versions_codes_and_duplicates() {
        for words in [
            vec![],
            vec![HELD_MAGIC, 2, 0, 0],
            vec![HELD_MAGIC, 1, u32::MAX, 0],
            vec![HELD_MAGIC, 1, 1, 50],
            vec![HELD_MAGIC, 1, 1, 256, 0],
            vec![HELD_MAGIC, 1, 1, 7, 0],
            vec![HELD_MAGIC, 1, 0, 1, 0],
            vec![HELD_MAGIC, 1, 0, 1, 256],
            vec![HELD_MAGIC, 1, 2, 50, 50, 0],
            vec![HELD_MAGIC, 1, 0, 2, 1, 1],
            vec![HELD_MAGIC, 1, 0, 0, 50],
        ] {
            assert!(parse_held_words(&words).is_err());
        }
        assert!(held_words(&HashSet::from([256]), &HashSet::new()).is_err());
        assert!(held_words(&HashSet::new(), &HashSet::from([0])).is_err());
    }

    #[test]
    fn held_property_accepts_the_bounded_maximum_state() {
        let keys = (8..=255).collect();
        let buttons = (1..=255).collect();
        let words = held_words(&keys, &buttons).unwrap();
        assert_eq!(words.len(), HELD_MAX_WORDS as usize);
        assert_eq!(parse_held_words(&words).unwrap(), (keys, buttons));
    }

    #[test]
    fn kvm_display_local_root_roundtrip() {
        let d = KvmDisplay {
            x: 1440,
            y: 145,
            width: 1344,
            height: 756,
        };
        assert_eq!(d.center_root(), (1440 + 672, 145 + 378));
        assert!(d.contains(1500, 400));
        assert!(!d.contains(100, 400));
        assert_eq!(d.to_local(1500, 400), (60, 255));
        assert_eq!(d.to_root(60, 255), (1500, 400));
    }
}
