//! Settings config load/save and corrupt-file backup.

use super::*;

// Move a corrupt config aside before the settings window can overwrite it with defaults.
pub(crate) fn backup_corrupt_config(
    path: &std::path::Path,
    reason: &str,
) -> Option<std::path::PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let backup = path.with_file_name(format!(
        "config.toml.corrupt-{stamp}-{}",
        std::process::id()
    ));
    match std::fs::rename(path, &backup) {
        Ok(()) => {
            log_event(
                "WARN",
                &format!(
                    "settings: {reason}; backed up to {}; using defaults",
                    backup.display()
                ),
            );
            Some(backup)
        }
        Err(e) => {
            log_event(
                "WARN",
                &format!(
                    "settings: {reason}; could not back up to {} ({e}); using defaults",
                    backup.display()
                ),
            );
            None
        }
    }
}

pub(crate) fn load_config_with_backup() -> (Config, bool) {
    let Ok(path) = sysvitals_core::config::config_path() else {
        log_event(
            "WARN",
            "settings: cannot resolve the config path; using defaults",
        );
        return (Config::default(), false);
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log_event(
                "INFO",
                &format!(
                    "settings: config not found at {}; using defaults",
                    path.display()
                ),
            );
            return (Config::default(), true);
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::InvalidData {
                let backed_up =
                    backup_corrupt_config(&path, &format!("config is not valid UTF-8: {e}"))
                        .is_some();
                return (Config::default(), backed_up);
            }
            log_event(
                "WARN",
                &format!("settings: config read failed: {e}; using defaults and disabling saves"),
            );
            return (Config::default(), false);
        }
    };
    match Config::from_toml_str(&text) {
        Ok((cfg, issues)) => {
            sysvitals_panel::log_sanitize_issues("settings: config repair", &issues);
            log_event(
                "INFO",
                &format!("settings: config loaded from {}", path.display()),
            );
            (cfg, true)
        }
        Err(e) => {
            let backed_up =
                backup_corrupt_config(&path, &format!("config parse failed: {e}")).is_some();
            (Config::default(), backed_up)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::backup_corrupt_config;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "sysvitals-settings-test-{tag}-{}-{stamp}",
            std::process::id()
        ))
    }

    #[test]
    fn a_corrupt_config_is_moved_aside_not_deleted() {
        let dir = temp_dir("corrupt");
        std::fs::create_dir_all(&dir).expect("create the temp directory");
        let path = dir.join("config.toml");
        std::fs::write(&path, "not = [valid").expect("write the corrupt file");

        let backup = backup_corrupt_config(&path, "test").expect("the rename succeeds");
        assert!(!path.exists(), "the original name must be gone");
        assert_eq!(
            std::fs::read_to_string(&backup).expect("read the backup"),
            "not = [valid"
        );
        assert!(backup
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .starts_with("config.toml.corrupt-"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_has_nothing_to_back_up() {
        let dir = temp_dir("missing");
        std::fs::create_dir_all(&dir).expect("create the temp directory");
        let path = dir.join("config.toml");
        assert!(backup_corrupt_config(&path, "test").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
