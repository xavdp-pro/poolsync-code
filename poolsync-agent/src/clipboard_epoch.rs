//! Selection events distinguish native copies even when X11 reuses an owner
//! window and the application does not implement the TIMESTAMP target.

use anyhow::{Context, Result};
use std::sync::{Mutex, OnceLock};
use x11rb::{
    connection::Connection,
    protocol::{
        xfixes,
        xproto::{ConnectionExt, CreateWindowAux, WindowClass},
        Event,
    },
    COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT,
};

struct Tracker {
    conn: x11rb::rust_connection::RustConnection,
    epoch: u64,
    clipboard: u32,
    clipboard_epoch: u64,
    clipboard_copy_epoch: u64,
    clipboard_timestamp: Option<u32>,
}

impl Tracker {
    fn new() -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).context("clipboard selection events")?;
        xfixes::query_version(&conn, 5, 0)?.reply()?;
        let window = conn.generate_id()?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            conn.setup().roots[screen].root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            COPY_FROM_PARENT,
            &CreateWindowAux::new(),
        )?
        .check()?;
        let mask = xfixes::SelectionEventMask::SET_SELECTION_OWNER
            | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
            | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE;
        let mut clipboard = 0;
        for name in [b"CLIPBOARD".as_slice(), b"PRIMARY".as_slice()] {
            let atom = conn.intern_atom(false, name)?.reply()?.atom;
            if name == b"CLIPBOARD" {
                clipboard = atom;
            }
            xfixes::select_selection_input(&conn, window, atom, mask)?.check()?;
        }
        conn.flush()?;
        Ok(Self {
            conn,
            epoch: 0,
            clipboard,
            clipboard_epoch: 0,
            clipboard_copy_epoch: 0,
            clipboard_timestamp: None,
        })
    }
}

static TRACKER: OnceLock<Mutex<Option<Tracker>>> = OnceLock::new();

fn epochs() -> Option<(u64, u64, u64, Option<u32>)> {
    let mut guard = TRACKER
        .get_or_init(|| Mutex::new(Tracker::new().ok()))
        .lock()
        .ok()?;
    let tracker = guard.as_mut()?;
    while let Some(event) = tracker.conn.poll_for_event().ok()? {
        if let Event::XfixesSelectionNotify(event) = event {
            tracker.epoch = tracker.epoch.wrapping_add(1);
            if event.selection == tracker.clipboard {
                tracker.clipboard_epoch = tracker.clipboard_epoch.wrapping_add(1);
                if event.subtype == xfixes::SelectionEvent::SET_SELECTION_OWNER {
                    tracker.clipboard_copy_epoch = tracker.clipboard_copy_epoch.wrapping_add(1);
                    tracker.clipboard_timestamp = Some(event.selection_timestamp);
                }
            }
        }
    }
    Some((
        tracker.epoch,
        tracker.clipboard_epoch,
        tracker.clipboard_copy_epoch,
        tracker.clipboard_timestamp,
    ))
}

pub fn current() -> Option<u64> {
    epochs().map(|e| e.0)
}
pub fn clipboard_current() -> Option<u64> {
    epochs().map(|e| e.1)
}

pub fn clipboard_copy_current() -> Option<u64> {
    epochs().map(|e| e.2)
}

/// Server timestamp from XFixes, without requesting data from the owner.
pub fn selection_timestamp() -> Option<u32> {
    epochs().and_then(|e| e.3)
}
