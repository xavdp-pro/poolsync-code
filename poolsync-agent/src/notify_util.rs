use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;
use tracing::{info, warn};

// All routine PoolSync status changes share one expiring toast. Serialize the
// returned ID so simultaneous hotkeys replace it instead of stacking windows.
static STATUS_NOTIFICATION_ID: Mutex<(String, u32)> = Mutex::new((String::new(), 0));

type NotificationJob = Box<dyn FnOnce() + Send + 'static>;

#[derive(Default)]
struct PendingNotification {
    next: Option<NotificationJob>,
    closed: bool,
}

struct NotificationQueue {
    pending: Mutex<PendingNotification>,
    ready: Condvar,
}

impl NotificationQueue {
    fn new() -> std::io::Result<Arc<Self>> {
        let queue = Arc::new(Self {
            pending: Mutex::new(PendingNotification::default()),
            ready: Condvar::new(),
        });
        let worker = Arc::clone(&queue);
        std::thread::Builder::new()
            .name("poolsync-notify".into())
            .spawn(move || loop {
                let job = {
                    let mut pending = worker.pending.lock().unwrap_or_else(|p| p.into_inner());
                    while pending.next.is_none() && !pending.closed {
                        pending = worker
                            .ready
                            .wait(pending)
                            .unwrap_or_else(|p| p.into_inner());
                    }
                    if pending.closed {
                        return;
                    }
                    pending.next.take().expect("ready notification")
                };
                // Never hold the queue lock while a daemon or external command waits.
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err() {
                    warn!("notification worker recovered from a failed job");
                }
            })?;
        Ok(queue)
    }

    fn enqueue(&self, job: NotificationJob) {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        if !pending.closed {
            // Routine status toasts share one replacement ID. Keep only the
            // latest pending status, plus the job already executing.
            pending.next = Some(job);
            self.ready.notify_one();
        }
    }

    #[cfg(test)]
    fn close(&self) {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        pending.closed = true;
        pending.next = None;
        self.ready.notify_one();
    }
}

fn queue_status_notification(job: impl FnOnce() + Send + 'static) {
    static QUEUE: OnceLock<Option<Arc<NotificationQueue>>> = OnceLock::new();
    let queue = QUEUE.get_or_init(|| match NotificationQueue::new() {
        Ok(queue) => Some(queue),
        Err(error) => {
            warn!("notification worker unavailable: {error}");
            None
        }
    });
    if let Some(queue) = queue {
        queue.enqueue(Box::new(job));
    }
}

pub fn notify_icon_path() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    format!("{home}/.local/share/poolsync/poolsync-tray.png")
}

fn session_env(cmd: &mut Command) {
    for key in [
        "DISPLAY",
        "DBUS_SESSION_BUS_ADDRESS",
        "XAUTHORITY",
        "XDG_RUNTIME_DIR",
        "XDG_CURRENT_DESKTOP",
    ] {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }
}

/// True if a non-zombie xfce4-notifyd is running.
fn notifyd_process_alive() -> bool {
    let Ok(output) = Command::new("ps")
        .args(["-C", "xfce4-notifyd", "-o", "stat="])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    String::from_utf8_lossy(&output.stdout).lines().any(|stat| {
        let s = stat.trim();
        !s.is_empty() && !s.starts_with('Z')
    })
}

/// True if org.freedesktop.Notifications answers on the session bus.
fn notifications_dbus_ok() -> bool {
    let mut cmd = Command::new("timeout");
    session_env(&mut cmd);
    cmd.args([
        "1",
        "busctl",
        "--user",
        "call",
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
        "GetServerInformation",
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .map(|s| s.success())
    .unwrap_or(false)
}

/// Relance xfce4-notifyd si absent / zombie (sinon notify-send hang / exit 1).
///
/// Important: never spawn notifyd as a direct child of poolsync-agent — if it
/// dies unreaped it becomes a zombie, `pgrep` still matches, and we never restart.
pub fn ensure_notify_daemon() {
    if notifications_dbus_ok() {
        return;
    }
    if notifyd_process_alive() && notifications_dbus_ok() {
        return;
    }

    warn!("xfce4-notifyd absent ou mort — relance");

    // Prefer systemd user unit (reparents under user@.service, no zombie under agent).
    let mut cmd = Command::new("systemctl");
    session_env(&mut cmd);
    let _ = cmd
        .args(["--user", "reset-failed", "xfce4-notifyd.service"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let mut cmd = Command::new("systemctl");
    session_env(&mut cmd);
    let _ = cmd
        .args(["--user", "restart", "xfce4-notifyd.service"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    std::thread::sleep(Duration::from_millis(500));
    if notifications_dbus_ok() {
        info!("xfce4-notifyd relancé via systemd");
        return;
    }

    let candidates = [
        "/usr/lib/x86_64-linux-gnu/xfce4/notifyd/xfce4-notifyd",
        "/usr/lib/xfce4/notifyd/xfce4-notifyd",
    ];
    for path in candidates {
        if !std::path::Path::new(path).is_file() {
            continue;
        }
        // Detach via bash so notifyd is not our child (avoids zombie under agent).
        let mut cmd = Command::new("bash");
        session_env(&mut cmd);
        cmd.args(["-c", &format!("nohup {path} >/dev/null 2>&1 </dev/null &")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match cmd.status() {
            Ok(s) if s.success() => {
                std::thread::sleep(Duration::from_millis(500));
                if notifications_dbus_ok() {
                    info!("xfce4-notifyd relancé ({path})");
                    return;
                }
            }
            Ok(_) => warn!("démarrage xfce4-notifyd ({path}): exit non-zéro"),
            Err(err) => warn!("démarrage xfce4-notifyd ({path}): {err}"),
        }
    }
}

/// Notification desktop locale (notify-send sous Linux).
pub fn notify_local(title: &str, body: &str) {
    let (title, body) = (title.to_owned(), body.to_owned());
    queue_status_notification(move || {
        let _ = notify_send(&title, &body, "normal", 5000);
    });
}

/// Notification visible pour le raccourci Ctrl+Alt+Shift+P (suspend / resume).
pub fn notify_poolsync_toggle(active: bool, node: &str) {
    let node = node.to_owned();
    queue_status_notification(move || notify_poolsync_toggle_now(active, &node));
}

fn notify_poolsync_toggle_now(active: bool, node: &str) {
    const HOTKEY: &str = "Ctrl+Alt+Shift+P";
    let (title, body, urgency, timeout_ms) = if active {
        (
            "PoolSync — ACTIVÉ",
            format!(
                "PoolSync réactivé sur {node}\n\
                 KVM + presse-papiers synchronisés.\n\
                 {HOTKEY} pour suspendre."
            ),
            "normal",
            6000u32,
        )
    } else {
        (
            "PoolSync — DÉSACTIVÉ",
            format!(
                "PoolSync suspendu sur {node}\n\
                 KVM et presse-papiers réseau coupés sur cette machine.\n\
                 {HOTKEY} pour réactiver."
            ),
            "normal",
            10000u32,
        )
    };
    if notify_send(title, &body, urgency, timeout_ms) {
        return;
    }
    warn!("notify-send toggle échoué — repli zenity");
    notify_zenity_fallback(title, &body);
}

/// Notification pour Ctrl+Alt+Shift+M (réclamer le master KVM).
pub fn notify_master_claim(node: &str, kvm_ok: bool) {
    let node = node.to_owned();
    queue_status_notification(move || notify_master_claim_now(&node, kvm_ok));
}

fn notify_master_claim_now(node: &str, kvm_ok: bool) {
    const HOTKEY: &str = "Ctrl+Alt+Shift+M";
    let (title, body, urgency) = if kvm_ok {
        (
            "PoolSync — MASTER",
            format!(
                "Cette machine ({node}) reprend clavier et souris.\n\
                 {HOTKEY} pour réclamer le master ici."
            ),
            "normal",
        )
    } else {
        (
            "PoolSync — MASTER indisponible",
            format!(
                "KVM inactif sur {node} (presse-papiers seul).\n\
                 Impossible de réclamer le master."
            ),
            "normal",
        )
    };
    if notify_send(title, &body, urgency, 5000) {
        return;
    }
    warn!("notify-send master claim échoué — repli zenity");
    notify_zenity_fallback(title, &body);
}

/// Debug toast when KVM master changes (systray opt-in).
pub fn notify_master_changed(local_node: &str, master: &str) {
    let (local_node, master) = (local_node.to_owned(), master.to_owned());
    queue_status_notification(move || notify_master_changed_now(&local_node, &master));
}

fn notify_master_changed_now(local_node: &str, master: &str) {
    let title = "PoolSync — MASTER";
    let body = if master == local_node {
        format!("This computer ({master}) is now KVM master.\nEdge switching uses this keyboard and mouse.")
    } else {
        format!("KVM master is now {master} (this node is {local_node}).")
    };
    if notify_send(title, &body, "normal", 3500) {
        return;
    }
    warn!("notify-send master changed échoué");
}

/// Notification for Ctrl+Alt+Shift+L (find the pointer).
pub fn notify_cursor_locate(node: &str, monitor: &str) {
    let (node, monitor) = (node.to_owned(), monitor.to_owned());
    queue_status_notification(move || notify_cursor_locate_now(&node, &monitor));
}

fn notify_cursor_locate_now(node: &str, monitor: &str) {
    let title = format!("PoolSync — {node}");
    let body = format!(
        "The mouse cursor is on this computer: {node}\n\
         Monitor {monitor}\n\
         Ctrl+Alt+Shift+L to locate again."
    );
    if notify_send(&title, &body, "normal", 4000) {
        return;
    }
    warn!("notify-send locate échoué — repli zenity");
    notify_zenity_fallback(&title, &body);
}

fn notify_send(title: &str, body: &str, urgency: &str, timeout_ms: u32) -> bool {
    ensure_notify_daemon();
    let mut previous_id = STATUS_NOTIFICATION_ID
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // IDs belong to a notification daemon instance. Never reuse a retained ID
    // after it restarts: that number could now belong to another application.
    let mut owner_command = Command::new("timeout");
    session_env(&mut owner_command);
    let owner = owner_command
        .args([
            "1",
            "busctl",
            "--user",
            "call",
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetNameOwner",
            "s",
            "org.freedesktop.Notifications",
        ])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();
    if owner.is_empty() || previous_id.0 != owner {
        *previous_id = (owner, 0);
    }
    let icon = notify_icon_path();
    let icon_arg = if std::path::Path::new(&icon).is_file() {
        icon
    } else {
        "dialog-information".into()
    };
    let timeout_secs = timeout_ms.div_ceil(1000).max(1).to_string();
    let mut cmd = Command::new("timeout");
    session_env(&mut cmd);
    cmd.args([
        &timeout_secs,
        "notify-send",
        "--print-id",
        "--replace-id",
        &previous_id.1.to_string(),
        "-a",
        "com.xavdp.poolsync",
        "-i",
        &icon_arg,
        "-t",
        &timeout_ms.to_string(),
        "-u",
        urgency,
    ])
    .arg(title)
    .arg(body)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    match cmd.output() {
        Ok(out) if out.status.success() => {
            if let Ok(id) = String::from_utf8_lossy(&out.stdout).trim().parse::<u32>() {
                previous_id.1 = id;
            }
            true
        }
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            warn!("notify-send exit {:?} — {}", out.status.code(), err.trim());
            false
        }
        Err(err) => {
            warn!("notify-send: {err}");
            false
        }
    }
}

fn notify_zenity_fallback(title: &str, body: &str) {
    let mut cmd = Command::new("zenity");
    session_env(&mut cmd);
    cmd.args([
        "--info",
        "--title",
        title,
        "--text",
        body,
        "--width",
        "420",
        "--timeout",
        "8",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    if cmd.spawn().is_err() {
        warn!("zenity indisponible — notification toggle non affichée");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn blocked_notification_does_not_block_enqueue_and_pending_status_is_latest() {
        let queue = NotificationQueue::new().expect("notification worker");
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        queue.enqueue(Box::new(move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }));
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let obsolete = done_tx.clone();
        queue.enqueue(Box::new(move || obsolete.send("obsolete").unwrap()));
        queue.enqueue(Box::new(move || done_tx.send("latest").unwrap()));
        // If enqueue waited on the command, this release could never run.
        release_tx.send(()).unwrap();
        assert_eq!(
            done_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "latest"
        );
        assert!(done_rx.try_recv().is_err());
        queue.close();
    }

    #[test]
    fn notification_worker_survives_a_job_panic() {
        let queue = NotificationQueue::new().expect("notification worker");
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        queue.enqueue(Box::new(move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            panic!("injected notification failure");
        }));
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        queue.enqueue(Box::new(move || done_tx.send(()).unwrap()));
        release_tx.send(()).unwrap();
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        queue.close();
    }
}
