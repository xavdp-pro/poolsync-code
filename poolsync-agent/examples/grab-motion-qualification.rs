//! Native capture regression fixture; run only on an isolated Xorg display.
#[allow(dead_code)]
#[path = "../src/kvm_input.rs"]
mod kvm_input;

use anyhow::Result;
use kvm_input::{GrabEvent, InputGrab};
use std::{thread, time::Duration};
use x11rb::{connection::Connection, protocol::xproto::ConnectionExt, CURRENT_TIME, NONE};

fn main() -> Result<()> {
    let (conn, screen) = x11rb::connect(None)?;
    let setup = &conn.setup().roots[screen];
    let (root, width, height) = (setup.root, setup.width_in_pixels, setup.height_in_pixels);
    let mut grab = InputGrab::begin(width.into(), height.into())?;
    // Drain startup events, then move the source with a different X11 client.
    thread::sleep(Duration::from_millis(20));
    let _ = grab.poll()?;
    conn.warp_pointer(NONE, root, 0, 0, 0, 0, 30, (height / 2) as i16)?;
    conn.flush()?;
    conn.get_input_focus()?.reply()?;
    thread::sleep(Duration::from_millis(20));
    let warp_events = grab.poll()?;
    let warp_delta: i32 = warp_events
        .iter()
        .map(|event| match event {
            GrabEvent::Motion { dx, .. } => dx.abs(),
            _ => 0,
        })
        .sum();
    println!("{{\"case\":\"external_pointer_warp\",\"forwarded_motion_pixels\":{warp_delta},\"passed\":{}}}", warp_delta == 0);
    grab.recenter(width.into(), height.into());
    thread::sleep(Duration::from_millis(20));
    let _ = grab.poll()?;
    let mut dx_total = 0;
    let mut wrong_direction = false;
    for _ in 0..40 {
        x11rb::protocol::xtest::fake_input(&conn, 6, 1, CURRENT_TIME, root, -4, 0, 0)?;
        conn.flush()?;
        conn.get_input_focus()?.reply()?;
        thread::sleep(Duration::from_millis(3));
        for event in grab.poll()? {
            if let GrabEvent::Motion { dx, .. } = event {
                dx_total += dx;
                wrong_direction |= dx > 0;
            }
        }
        if grab.needs_recenter(32) {
            grab.recenter(width.into(), height.into());
        }
    }
    thread::sleep(Duration::from_millis(20));
    for event in grab.poll()? {
        if let GrabEvent::Motion { dx, .. } = event {
            dx_total += dx;
            wrong_direction |= dx > 0;
        }
    }
    let physical_pass = dx_total == -160 && !wrong_direction;
    println!("{{\"case\":\"relative_motion_with_recentering\",\"expected_dx\":-160,\"observed_dx\":{dx_total},\"wrong_direction\":{wrong_direction},\"passed\":{physical_pass}}}");
    drop(grab);
    anyhow::ensure!(
        warp_delta == 0 && physical_pass,
        "native motion capture includes programmatic warps or loses real deltas"
    );
    Ok(())
}
