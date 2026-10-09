//! Capture souris/clavier style Barrier (XWindowsScreen::leave / grabMouseAndKeyboard).
//! Fenêtre InputOnly plein écran + curseur pixmap vide → le curseur disparaît sur le master.

use anyhow::{Context, Result};
use std::thread;
use std::time::{Duration, Instant};
use tracing::warn;
use x11rb::connection::Connection;
use x11rb::protocol::xfixes;
use x11rb::protocol::xinput;
use x11rb::protocol::xproto::{
    Bool32, ConfigureWindowAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, Cursor,
    EventMask, GrabMode, GrabStatus, ImageFormat, InputFocus, Pixmap, StackMode, Window,
    WindowClass,
};
use x11rb::protocol::Event;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, CURRENT_TIME, NONE};

const GRAB_TIMEOUT: Duration = Duration::from_secs(1);
const GRAB_RETRY: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub enum GrabEvent {
    Motion { dx: i32, dy: i32 },
    Button { button: u8, pressed: bool },
    Key { keycode: u8, pressed: bool },
    LocalReturn,
}

pub struct InputGrab {
    conn: x11rb::rust_connection::RustConnection,
    screen_num: usize,
    grab_window: Window,
    blank_cursor: Cursor,
    cursor_pixmap: Pixmap,
    screen_w: u16,
    screen_h: u16,
    last_x: i16,
    last_y: i16,
    active: bool,
    return_keys: std::collections::HashSet<u8>,
    raw_motion: bool,
    motion_fraction: (f64, f64),
}

impl InputGrab {
    pub fn begin(_screen_w: u32, _screen_h: u32) -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None).context("X11 grab")?;
        let root = conn.setup().roots[screen_num].root;
        let setup = &conn.setup().roots[screen_num];
        let min = conn.setup().min_keycode;
        let count = conn.setup().max_keycode - min + 1;
        let mapping = conn.get_keyboard_mapping(min, count)?.reply()?;
        let return_keys = mapping
            .keysyms
            .chunks(mapping.keysyms_per_keycode as usize)
            .enumerate()
            .filter(|(_, syms)| syms.contains(&0x6d) || syms.contains(&0x4d))
            .map(|(i, _)| min + i as u8)
            .collect();
        let w = setup.width_in_pixels.max(1);
        let h = setup.height_in_pixels.max(1);
        let cx = (w / 2) as i16;
        let cy = (h / 2) as i16;

        let (blank_cursor, cursor_pixmap) = create_blank_cursor(&conn, root)?;
        let grab_window = create_grab_window(&conn, root, w, h, blank_cursor)?;

        release_stale_grabs(&conn);

        conn.map_window(grab_window)?;
        conn.configure_window(
            grab_window,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )?;
        let _ = xfixes::hide_cursor(&conn, root);
        if let Err(err) = grab_mouse_and_keyboard(&conn, grab_window) {
            let _ = conn.ungrab_pointer(CURRENT_TIME);
            let _ = conn.ungrab_keyboard(CURRENT_TIME);
            let _ = xfixes::show_cursor(&conn, root);
            let _ = conn.unmap_window(grab_window);
            let _ = conn.destroy_window(grab_window);
            let _ = conn.flush();
            return Err(err);
        }
        let _ = conn.set_input_focus(InputFocus::POINTER_ROOT, grab_window, CURRENT_TIME);
        conn.warp_pointer(NONE, root, 0, 0, 0, 0, cx, cy)?;
        conn.flush()?;
        // Pointer warps from recentering, peer entry or an RDP client are not
        // user motion. XI2 reports the relative device deltas independently of
        // those root-coordinate changes; core coordinates only locate the
        // source pointer for confinement/recentering.
        let raw_motion = match enable_raw_motion(&conn, root) {
            Ok(enabled) => enabled,
            Err(err) => {
                warn!("XI2 grab motion unavailable; core motion fallback: {err:#}");
                false
            }
        };

        Ok(Self {
            conn,
            screen_num,
            grab_window,
            blank_cursor,
            cursor_pixmap,
            screen_w: w,
            screen_h: h,
            last_x: cx,
            last_y: cy,
            active: true,
            return_keys,
            raw_motion,
            motion_fraction: (0.0, 0.0),
        })
    }

    pub fn end(&mut self) {
        if !self.active {
            return;
        }
        let root = self.conn.setup().roots[self.screen_num].root;
        let _ = self.conn.ungrab_pointer(CURRENT_TIME);
        let _ = self.conn.ungrab_keyboard(CURRENT_TIME);
        let _ = self.conn.unmap_window(self.grab_window);
        let _ = xfixes::show_cursor(&self.conn, root);
        let _ = self.conn.destroy_window(self.grab_window);
        let _ = self.conn.free_cursor(self.blank_cursor);
        let _ = self.conn.free_pixmap(self.cursor_pixmap);
        let _ = self.conn.flush();
        self.active = false;
    }

    pub fn recenter(&mut self, _screen_w: u32, _screen_h: u32) {
        if !self.active {
            return;
        }
        let root = self.conn.setup().roots[self.screen_num].root;
        let cx = (self.screen_w / 2) as i16;
        let cy = (self.screen_h / 2) as i16;
        let _ = self.conn.warp_pointer(NONE, root, 0, 0, 0, 0, cx, cy);
        let _ = self.conn.flush();
        self.last_x = cx;
        self.last_y = cy;
    }

    /// Recentre la souris physique si elle s'éloigne du centre (Barrier s_size = 32).
    pub fn needs_recenter(&self, threshold: i32) -> bool {
        if !self.active {
            return false;
        }
        let cx = (self.screen_w / 2) as i16;
        let cy = (self.screen_h / 2) as i16;
        i32::from(self.last_x - cx).abs() > threshold
            || i32::from(self.last_y - cy).abs() > threshold
    }

    pub fn poll(&mut self) -> Result<Vec<GrabEvent>> {
        let mut out = Vec::new();
        while let Some(event) = self.conn.poll_for_event()? {
            match event {
                Event::MotionNotify(e) => {
                    let dx = i32::from(e.event_x) - i32::from(self.last_x);
                    let dy = i32::from(e.event_y) - i32::from(self.last_y);
                    self.last_x = e.event_x;
                    self.last_y = e.event_y;
                    if !self.raw_motion && (dx != 0 || dy != 0) {
                        out.push(GrabEvent::Motion { dx, dy });
                    }
                }
                Event::XinputRawMotion(e) if self.raw_motion => {
                    let (dx, dy) = raw_relative_xy(&e);
                    let x = self.motion_fraction.0 + dx;
                    let y = self.motion_fraction.1 + dy;
                    let (dx, dy) = (x.trunc() as i32, y.trunc() as i32);
                    self.motion_fraction = (x - f64::from(dx), y - f64::from(dy));
                    if dx != 0 || dy != 0 {
                        out.push(GrabEvent::Motion { dx, dy });
                    }
                }
                Event::XinputHierarchy(_) | Event::XinputDeviceChanged(_) => {
                    self.raw_motion = relative_pointer_devices(&self.conn).unwrap_or(false);
                    self.motion_fraction = (0.0, 0.0);
                }
                Event::ButtonPress(e) => {
                    out.push(GrabEvent::Button {
                        button: e.detail,
                        pressed: true,
                    });
                }
                Event::ButtonRelease(e) => {
                    out.push(GrabEvent::Button {
                        button: e.detail,
                        pressed: false,
                    });
                }
                Event::KeyPress(e) => {
                    let modifiers = x11rb::protocol::xproto::KeyButMask::SHIFT
                        | x11rb::protocol::xproto::KeyButMask::CONTROL
                        | x11rb::protocol::xproto::KeyButMask::MOD1;
                    if e.state.contains(modifiers) && self.return_keys.contains(&e.detail) {
                        out.push(GrabEvent::LocalReturn);
                    } else {
                        out.push(GrabEvent::Key {
                            keycode: e.detail,
                            pressed: true,
                        });
                    }
                }
                Event::KeyRelease(e) => {
                    out.push(GrabEvent::Key {
                        keycode: e.detail,
                        pressed: false,
                    });
                }
                _ => {}
            }
        }
        Ok(out)
    }
}

fn enable_raw_motion(conn: &x11rb::rust_connection::RustConnection, root: Window) -> Result<bool> {
    let version = xinput::xi_query_version(conn, 2, 1)?.reply()?;
    anyhow::ensure!(
        (version.major_version, version.minor_version) >= (2, 1),
        "XI2.1 required"
    );
    xinput::xi_select_events(
        conn,
        root,
        &[
            xinput::EventMask {
                deviceid: 1,
                mask: vec![xinput::XIEventMask::RAW_MOTION | xinput::XIEventMask::DEVICE_CHANGED],
            },
            xinput::EventMask {
                deviceid: 0,
                mask: vec![xinput::XIEventMask::HIERARCHY],
            },
        ],
    )?
    .check()?;
    relative_pointer_devices(conn)
}

fn relative_pointer_devices(conn: &x11rb::rust_connection::RustConnection) -> Result<bool> {
    let devices = xinput::xi_query_device(conn, 0u16)?.reply()?;
    // Keep the existing core behavior for absolute tablets/touchscreens rather
    // than treating their device coordinates as mouse movement deltas.
    Ok(!devices
        .infos
        .iter()
        .flat_map(|device| &device.classes)
        .any(|class| {
            matches!(&class.data, xinput::DeviceClassData::Valuator(axis)
            if axis.number < 2 && axis.mode == xinput::ValuatorMode::ABSOLUTE)
        }))
}

fn raw_relative_xy(event: &xinput::RawMotionEvent) -> (f64, f64) {
    let mut values = event.axisvalues.iter();
    let mut xy = (0.0, 0.0);
    for (word, mask) in event.valuator_mask.iter().enumerate() {
        for bit in 0..32 {
            if mask & (1 << bit) == 0 {
                continue;
            }
            let Some(value) = values.next() else {
                return xy;
            };
            let value = f64::from(value.integral) + f64::from(value.frac) / 4_294_967_296.0;
            match word * 32 + bit {
                0 => xy.0 = value,
                1 => xy.1 = value,
                _ => {}
            }
        }
    }
    xy
}

impl Drop for InputGrab {
    fn drop(&mut self) {
        self.end();
    }
}

/// Libère un grab clavier/souris orphelin (évite ALREADY_GRABBED après échec précédent).
fn release_stale_grabs(conn: &x11rb::rust_connection::RustConnection) {
    let _ = conn.ungrab_keyboard(CURRENT_TIME);
    let _ = conn.ungrab_pointer(CURRENT_TIME);
    let _ = conn.flush();
}

/// Masque rapide avant que le grab Barrier soit prêt (warp hors écran).
/// Préférer InputGrab::begin seul — n'appeler qu'après grab réussi si besoin.
fn create_grab_window(
    conn: &x11rb::rust_connection::RustConnection,
    root: Window,
    width: u16,
    height: u16,
    cursor: Cursor,
) -> Result<Window> {
    let win = conn.generate_id()?;
    let event_mask = EventMask::POINTER_MOTION
        | EventMask::BUTTON_MOTION
        | EventMask::BUTTON_PRESS
        | EventMask::BUTTON_RELEASE
        | EventMask::KEY_PRESS
        | EventMask::KEY_RELEASE;
    conn.create_window(
        COPY_DEPTH_FROM_PARENT,
        win,
        root,
        0,
        0,
        width,
        height,
        0,
        WindowClass::INPUT_ONLY,
        COPY_FROM_PARENT,
        &CreateWindowAux::new()
            .override_redirect(Bool32::from(true))
            .event_mask(event_mask)
            .cursor(cursor),
    )?;
    Ok(win)
}

/// Curseur pixmap 1×1 vide — même technique que Barrier createBlankCursor().
fn create_blank_cursor(
    conn: &x11rb::rust_connection::RustConnection,
    root: Window,
) -> Result<(Cursor, Pixmap)> {
    let pixmap = conn.generate_id()?;
    conn.create_pixmap(1, pixmap, root, 1, 1)?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, pixmap, &CreateGCAux::new())?;
    conn.put_image(ImageFormat::XY_BITMAP, pixmap, gc, 1, 1, 0, 0, 0, 1, &[0u8])?;
    conn.free_gc(gc)?;
    let cursor = conn.generate_id()?;
    conn.create_cursor(cursor, pixmap, pixmap, 0, 0, 0, 0, 0, 0, 0, 0)?;
    Ok((cursor, pixmap))
}

/// Capture keyboard and pointer together or leave both on the local desktop.
/// A foreign keyboard grab must never silently create a mouse-only takeover.
fn grab_mouse_and_keyboard(
    conn: &x11rb::rust_connection::RustConnection,
    window: Window,
) -> Result<()> {
    let event_mask = EventMask::BUTTON_PRESS
        | EventMask::BUTTON_RELEASE
        | EventMask::ENTER_WINDOW
        | EventMask::LEAVE_WINDOW
        | EventMask::POINTER_MOTION;
    let deadline = Instant::now() + GRAB_TIMEOUT;
    loop {
        let ptr = conn
            .grab_pointer(
                false,
                window,
                event_mask,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                window,
                NONE,
                CURRENT_TIME,
            )?
            .reply()?;
        if ptr.status != GrabStatus::SUCCESS {
            if Instant::now() >= deadline {
                anyhow::bail!("grab pointeur timeout ({:?})", ptr.status);
            }
            thread::sleep(GRAB_RETRY);
            continue;
        }

        let kb = conn
            .grab_keyboard(
                false,
                window,
                CURRENT_TIME,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )?
            .reply()?;
        if kb.status != GrabStatus::SUCCESS {
            // Release our pointer before retrying so a foreign keyboard owner
            // cannot split the user's keyboard and mouse between desktops.
            conn.ungrab_pointer(CURRENT_TIME)?;
            conn.flush()?;
            if Instant::now() >= deadline {
                anyhow::bail!("keyboard grab timeout ({:?})", kb.status);
            }
            thread::sleep(GRAB_RETRY);
            continue;
        }
        return Ok(());
    }
}
