//! User-owned local IPC controls the running agent without a hub.

use crate::state::AgentState;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::os::unix::{fs::PermissionsExt, net::UnixDatagram};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn socket_path() -> PathBuf {
    PathBuf::from(
        std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() })),
    )
    .join("poolsync-agent.control")
}

#[derive(Deserialize, Serialize)]
struct Request {
    config: PathBuf,
    command: Command,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "action", content = "value", rename_all = "snake_case")]
enum Command {
    Window,
    Away(bool),
}

#[derive(Deserialize, Serialize)]
struct Reply {
    error: Option<String>,
}

struct ClientPath(PathBuf);

impl Drop for ClientPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn request(config: &Path, command: Command) -> Result<()> {
    let endpoint = socket_path();
    let client =
        ClientPath(endpoint.with_file_name(format!("pc-{}.sock", uuid::Uuid::new_v4().simple())));
    let socket = UnixDatagram::bind(&client.0)?;
    std::fs::set_permissions(&client.0, std::fs::Permissions::from_mode(0o600))?;
    socket.set_read_timeout(Some(Duration::from_secs(3)))?;
    socket
        .connect(endpoint)
        .context("running agent control unavailable")?;
    socket.send(&serde_json::to_vec(&Request {
        config: std::fs::canonicalize(config)?,
        command,
    })?)?;
    let mut bytes = [0_u8; 4096];
    let n = socket
        .recv(&mut bytes)
        .context("running agent did not confirm the command")?;
    let reply: Reply = serde_json::from_slice(&bytes[..n])?;
    if let Some(error) = reply.error {
        anyhow::bail!(error);
    }
    Ok(())
}

pub fn request_window(config: &Path) -> Result<()> {
    request(config, Command::Window)
}

/// Return only after the live participation/privacy gates have changed.
pub fn request_away(config: &Path, away: bool) -> Result<()> {
    request(config, Command::Away(away))
}

fn dispatch(state: &AgentState, expected: &Path, bytes: &[u8]) -> Result<()> {
    // Accept the older window-only request from a currently installed CLI.
    if bytes == expected.as_os_str().as_encoded_bytes() {
        state.request_config_window();
        return Ok(());
    }
    let request: Request = serde_json::from_slice(bytes)?;
    anyhow::ensure!(
        request.config == expected,
        "agent configuration does not match"
    );
    match request.command {
        Command::Window => state.request_config_window(),
        Command::Away(away) => state.set_pool_away(away)?,
    }
    Ok(())
}

/// Called only after acquiring the user runtime's instance lock.
pub fn spawn(state: Arc<AgentState>) -> Result<()> {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let socket = UnixDatagram::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
    let expected = std::fs::canonicalize(&state.config_path)?;
    std::thread::Builder::new()
        .name("local-agent-control".into())
        .spawn(move || {
            let mut bytes = [0_u8; 4096];
            loop {
                match socket.recv_from(&mut bytes) {
                    Ok((n, sender)) => {
                        let reply = Reply {
                            error: dispatch(&state, &expected, &bytes[..n])
                                .err()
                                .map(|e| e.to_string()),
                        };
                        if let Some(path) = sender.as_pathname() {
                            if let Ok(bytes) = serde_json::to_vec(&reply) {
                                let _ = socket.send_to(&bytes, path);
                            }
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(error) => {
                        tracing::warn!("local agent control stopped: {error}");
                        break;
                    }
                }
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn departure_and_return_commands_round_trip() {
        for away in [true, false] {
            let bytes = serde_json::to_vec(&Request {
                config: PathBuf::from("/test/agent.toml"),
                command: Command::Away(away),
            })
            .unwrap();
            let decoded: Request = serde_json::from_slice(&bytes).unwrap();
            assert!(matches!(decoded.command, Command::Away(v) if v == away));
        }
    }

    #[test]
    fn commands_apply_live_gates_and_reject_another_configuration() {
        let _image = crate::clipboard_gtk::IMAGE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("poolsync-control-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("agent.toml");
        std::fs::write(&path, "fixture").unwrap();
        let config: poolsync_core::AgentConfig = toml::from_str(
            "node='test'\nhub_url='ws://invalid/ws'\ntoken='fixture'\nmode='clipboard_only'\n[screen]\nwidth=100\nheight=100\n",
        ).unwrap();
        let state = AgentState::new(config, path.clone());
        let command = |config: PathBuf, away| {
            serde_json::to_vec(&Request {
                config,
                command: Command::Away(away),
            })
            .unwrap()
        };
        assert!(dispatch(&state, &path, &command(root.join("other.toml"), true)).is_err());
        assert!(!state.pool_away());
        dispatch(&state, &path, &command(path.clone(), true)).unwrap();
        assert!(state.pool_away() && !state.local_poolsync_active());
        let revision = state.tray_status_revision();
        dispatch(&state, &path, &command(path.clone(), true)).unwrap();
        assert_eq!(state.tray_status_revision(), revision);
        dispatch(&state, &path, &command(path.clone(), false)).unwrap();
        state.refresh_pool_away().unwrap();
        assert!(!state.pool_away() && state.local_poolsync_active());
        assert!(!crate::participation::is_away(&path));
        std::fs::remove_dir_all(root).unwrap();
    }
}
