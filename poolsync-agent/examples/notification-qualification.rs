#[allow(dead_code)]
#[path = "../src/notify_util.rs"]
mod notify_util;
fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("denial-burst") => {
            for _ in 0..20 {
                notify_util::notify_master_claim("notification-dev", false);
            }
            println!("BURST_FINISHED");
            std::thread::sleep(std::time::Duration::from_secs(10));
        }
        Some("restart") => {
            notify_util::notify_master_claim("notification-dev", false);
            println!("RESTART_READY");
            std::thread::sleep(std::time::Duration::from_secs(5));
            notify_util::notify_master_claim("notification-dev", false);
            println!("RESTART_FINISHED");
            std::thread::sleep(std::time::Duration::from_secs(8));
        }
        Some("suspend") => {
            notify_util::notify_poolsync_toggle(false, "notification-dev");
            // Notifications are asynchronous; retain this standalone probe.
            std::thread::sleep(std::time::Duration::from_secs(10));
        }
        _ => panic!("unknown qualification operation"),
    }
}
