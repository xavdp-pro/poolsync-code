//! Persistent, local-only absence. Leaving never revokes the device identity.
use std::path::Path;

pub fn is_away(config_path: &Path) -> bool {
    std::fs::read_to_string(config_path.with_extension("away"))
        .map(|value| value.trim() == "away")
        .unwrap_or(false)
}

pub fn set_away(config_path: &Path, away: bool) -> std::io::Result<()> {
    let path = config_path.with_extension("away");
    if away {
        // Atomic replacement prevents a crash from writing a partial state.
        let temp = path.with_extension(format!("away.{}.tmp", std::process::id()));
        std::fs::write(&temp, "away\n")?;
        std::fs::rename(temp, path)
    } else {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn absence_survives_restart_and_rejoin_preserves_configuration() {
        let root = std::env::temp_dir().join(format!("poolsync-away-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let config = root.join("agent.toml");
        std::fs::write(&config, "node = \"test\"\n").unwrap();
        assert!(!is_away(&config));
        set_away(&config, true).unwrap();
        assert!(is_away(&config));
        set_away(&config, false).unwrap();
        set_away(&config, false).unwrap();
        assert!(!is_away(&config));
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "node = \"test\"\n"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
