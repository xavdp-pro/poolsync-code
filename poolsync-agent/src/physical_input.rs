//! XI2 raw events reach us even while another application owns keyboard focus.
//! The XTEST source devices used for remote injection never claim ownership.

use crate::kvm_x11::PhysicalInput;
use anyhow::Result;
use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
};
use x11rb::{
    connection::Connection,
    protocol::{xinput, Event},
};

struct Tracker {
    conn: x11rb::rust_connection::RustConnection,
    synthetic: HashSet<u16>,
}

fn synthetic_name(name: &[u8]) -> bool {
    String::from_utf8_lossy(name)
        .to_ascii_lowercase()
        .contains("xtest")
}

impl Tracker {
    fn new() -> Result<Self> {
        let (conn, screen) = x11rb::connect(None)?;
        let version = xinput::xi_query_version(&conn, 2, 1)?.reply()?;
        anyhow::ensure!(
            (version.major_version, version.minor_version) >= (2, 1),
            "XI2.1 source device tracking unavailable"
        );
        let root = conn.setup().roots[screen].root;
        xinput::xi_select_events(
            &conn,
            root,
            &[
                xinput::EventMask {
                    deviceid: 1,
                    mask: vec![
                        xinput::XIEventMask::RAW_KEY_PRESS
                            | xinput::XIEventMask::RAW_BUTTON_PRESS
                            | xinput::XIEventMask::RAW_MOTION,
                    ],
                },
                xinput::EventMask {
                    deviceid: 0,
                    mask: vec![xinput::XIEventMask::HIERARCHY],
                },
            ],
        )?
        .check()?;
        let mut tracker = Self {
            conn,
            synthetic: HashSet::new(),
        };
        tracker.refresh_devices()?;
        Ok(tracker)
    }

    fn refresh_devices(&mut self) -> Result<()> {
        self.synthetic = xinput::xi_query_device(&self.conn, 0u16)?
            .reply()?
            .infos
            .into_iter()
            .filter(|d| synthetic_name(&d.name))
            .map(|d| d.deviceid)
            .collect();
        Ok(())
    }

    fn poll(&mut self) -> Result<Option<PhysicalInput>> {
        let mut physical = None;
        while let Some(event) = self.conn.poll_for_event()? {
            match event {
                Event::XinputRawKeyPress(e)
                    if e.sourceid != 0 && !self.synthetic.contains(&e.sourceid) =>
                {
                    physical = Some(PhysicalInput::Key)
                }
                Event::XinputRawButtonPress(e)
                    if e.sourceid != 0 && !self.synthetic.contains(&e.sourceid) =>
                {
                    physical = Some(PhysicalInput::Button)
                }
                Event::XinputRawMotion(e)
                    if e.sourceid != 0 && !self.synthetic.contains(&e.sourceid) =>
                {
                    if physical.is_none() {
                        physical = Some(PhysicalInput::Motion);
                    }
                }
                Event::XinputHierarchy(_) => self.refresh_devices()?,
                _ => {}
            }
        }
        // Drain the entire queue even while this machine already owns input.
        // Historical physical events must not reclaim it after a later handoff.
        Ok(physical)
    }
}

thread_local! {
    static INITIALIZED: Cell<bool> = const { Cell::new(false) };
    static TRACKER: RefCell<Option<Tracker>> = const { RefCell::new(None) };
}

/// Outer None selects the compatibility core-event backend. Inner None means
/// XI2 is active but no physical key/button was pressed since the last poll.
pub fn poll() -> Option<Option<PhysicalInput>> {
    TRACKER.with(|tracker| {
        let mut tracker = tracker.borrow_mut();
        if !INITIALIZED.with(|i| i.replace(true)) {
            match Tracker::new() {
                Ok(t) => {
                    tracing::info!("XI2 physical keyboard/button tracking active; XTEST excluded");
                    *tracker = Some(t);
                }
                Err(err) => {
                    tracing::warn!("XI2 tracking unavailable; core input fallback: {err:#}")
                }
            }
        }
        match tracker.as_mut()?.poll() {
            Ok(input) => Some(input),
            Err(err) => {
                tracing::warn!("XI2 tracking connection failed; core input fallback: {err:#}");
                *tracker = None;
                None
            }
        }
    })
}

pub fn active() -> bool {
    TRACKER.with(|t| t.borrow().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn injection_devices_are_distinct_from_physical_and_rdp_keyboards() {
        assert!(synthetic_name(b"Virtual core XTEST keyboard"));
        assert!(synthetic_name(b"Virtual core XTEST pointer"));
        assert!(!synthetic_name(b"AT Translated Set 2 keyboard"));
        assert!(!synthetic_name(b"xrdpKeyboard"));
        assert!(!synthetic_name(b"Logitech USB Receiver"));
    }
}
