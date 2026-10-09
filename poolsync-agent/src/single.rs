use anyhow::{Context, Result};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

pub struct InstanceLock {
    path: PathBuf,
}

impl InstanceLock {
    pub fn acquire() -> Result<Self> {
        let path = lock_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        if path.exists() {
            let raw = fs::read_to_string(&path).unwrap_or_default();
            if let Ok(pid) = raw.trim().parse::<i32>() {
                if is_alive(pid) {
                    anyhow::bail!("poolsync-agent déjà actif (pid {pid})");
                }
            }
            fs::remove_file(&path).ok();
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("create lock {}", path.display()))?;
        writeln!(file, "{}", std::process::id())?;
        Ok(Self { path })
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        fs::remove_file(&self.path).ok();
    }
}

fn lock_path() -> Result<PathBuf> {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
    Ok(PathBuf::from(base).join("poolsync-agent.pid"))
}

fn is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe { libc::kill(pid, 0) == 0 }
}
