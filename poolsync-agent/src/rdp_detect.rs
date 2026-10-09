use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::process::Command;

static LAST_RDP_STATE: Mutex<Option<(Instant, bool)>> = Mutex::new(None);
const RDP_STATE_TTL: Duration = Duration::from_secs(1);

/// Detect native clipboard redirection in this user's local RDP client.
/// Cache process discovery so the 50 ms clipboard poll does not spawn pgrep.
pub async fn rdp_client_active() -> bool {
    if let Some(active) = LAST_RDP_STATE.lock().ok().and_then(|state| {
        state
            .as_ref()
            .filter(|(at, _)| at.elapsed() < RDP_STATE_TTL)
            .map(|(_, active)| *active)
    }) {
        return active;
    }
    let uid = unsafe { libc::getuid() }.to_string();
    let output = Command::new("pgrep")
        .args(["-u", &uid, "-af", "xfreerdp"])
        .output()
        .await;
    let active = output.ok().filter(|o| o.status.success()).is_some_and(|o| {
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .any(redirects_clipboard)
    });
    if let Ok(mut state) = LAST_RDP_STATE.lock() {
        if state.as_ref().map(|(_, was)| *was) != Some(active) {
            tracing::info!("native RDP clipboard redirection active={active}");
        }
        *state = Some((Instant::now(), active));
    }
    active
}

fn redirects_clipboard(line: &str) -> bool {
    let mut words = line.split_whitespace();
    let Some(pid) = words.next() else {
        return false;
    };
    if pid.parse::<u32>().is_err() {
        return false;
    }
    let Some(program) = words.next() else {
        return false;
    };
    let executable = program.rsplit('/').next().unwrap_or(program);
    if executable != "xfreerdp" && executable != "xfreerdp3" {
        return false;
    }
    let args: Vec<_> = words.collect();
    args.iter().any(|arg| arg.starts_with("/v:"))
        && !args
            .iter()
            .any(|arg| *arg == "-clipboard" || arg.starts_with("/clipboard:direction-to:off"))
}

#[cfg(test)]
mod tests {
    use super::redirects_clipboard;

    #[test]
    fn native_redirection_is_detected_for_the_active_client() {
        assert!(redirects_clipboard(
            "7025 /usr/bin/xfreerdp3 /v:192.0.2.35 +clipboard"
        ));
        assert!(redirects_clipboard("123 xfreerdp /v:server /clipboard"));
    }

    #[test]
    fn disabled_redirection_does_not_pause_poolsync() {
        assert!(!redirects_clipboard("7025 xfreerdp3 /v:server -clipboard"));
        assert!(!redirects_clipboard(
            "7025 xfreerdp3 /v:server /clipboard:direction-to:off"
        ));
    }

    #[test]
    fn process_discovery_does_not_match_shell_commands_or_help() {
        assert!(!redirects_clipboard(
            "42 sh -c xfreerdp /v:server +clipboard"
        ));
        assert!(!redirects_clipboard("42 xfreerdp3 /help"));
        assert!(!redirects_clipboard("pgrep -af xfreerdp"));
    }
}
