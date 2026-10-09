//! Native KVM grab contention proof; only run on a disposable container display.
#[allow(dead_code)]
#[path = "../src/kvm_input.rs"]
mod kvm_input;

use anyhow::{ensure, Result};
use kvm_input::InputGrab;
use std::{env, path::Path};
use x11rb::{
    connection::Connection,
    protocol::xproto::{ConnectionExt, EventMask, GrabMode, GrabStatus},
    CURRENT_TIME, NONE,
};

fn main() -> Result<()> {
    ensure!(
        Path::new("/run/.containerenv").exists(),
        "container required"
    );
    let display = env::var("DISPLAY")?;
    ensure!(
        env::var("POOLSYNC_ISOLATED_GRAB_DISPLAY").as_deref() == Ok(display.as_str())
            && display == ":196",
        "explicit disposable display :196 required"
    );
    let (holder, screen) = x11rb::connect(None)?;
    let root = holder.setup().roots[screen].root;
    let keyboard = holder
        .grab_keyboard(false, root, CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)?
        .reply()?;
    ensure!(
        keyboard.status == GrabStatus::SUCCESS,
        "fixture keyboard grab unavailable"
    );
    let attempt = InputGrab::begin(1024, 768);
    let rejected = attempt.is_err();
    // Dropping an accepted partial grab must release its pointer as well.
    drop(attempt);
    let (probe, _) = x11rb::connect(None)?;
    let pointer = probe
        .grab_pointer(
            false,
            root,
            EventMask::POINTER_MOTION,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            NONE,
            NONE,
            CURRENT_TIME,
        )?
        .reply()?;
    let pointer_free = pointer.status == GrabStatus::SUCCESS;
    probe.ungrab_pointer(CURRENT_TIME)?;
    let foreign_still_owns_keyboard = probe
        .grab_keyboard(false, root, CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)?
        .reply()?
        .status
        == GrabStatus::ALREADY_GRABBED;
    probe.ungrab_keyboard(CURRENT_TIME)?;
    probe.flush()?;
    holder.ungrab_keyboard(CURRENT_TIME)?;
    holder.flush()?;
    holder.get_input_focus()?.reply()?;
    let recovered = InputGrab::begin(1024, 768).is_ok();
    for (case, passed) in [
        (
            "reject_mouse_only_takeover_under_foreign_keyboard_grab",
            rejected,
        ),
        ("pointer_released_after_capture_attempt", pointer_free),
        (
            "foreign_keyboard_owner_preserved",
            foreign_still_owns_keyboard,
        ),
        ("complete_capture_recovers_after_contention", recovered),
    ] {
        println!("{{\"case\":\"{case}\",\"passed\":{passed}}}");
    }
    ensure!(
        rejected && pointer_free && foreign_still_owns_keyboard && recovered,
        "partial capture may split keyboard and pointer destinations"
    );
    Ok(())
}
