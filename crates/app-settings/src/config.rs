//! config: split out of the crate root (see `main.rs` for the module map).

use super::*;

/// Move a corrupt config aside before the settings window can overwrite it
/// with defaults. Returns the backup path when the rename succeeded.
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

/// Load the config, backing up a malformed or non-UTF-8 file before falling
/// back to defaults. Without the backup the first instant-save would
/// overwrite the user's (hand-edited) file with defaults.
pub(crate) fn load_config_with_backup() -> Config {
    let Ok(path) = sysvitals_core::config::config_path() else {
        log_event(
            "WARN",
            "settings: cannot resolve the config path; using defaults",
        );
        return Config::default();
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
            return Config::default();
        }
        Err(e) => {
            // Invalid UTF-8 is malformed content, not a transient sharing
            // violation: preserve the bytes so a later save cannot destroy
            // the user's file. Other read errors stay transient and are not
            // moved aside.
            if e.kind() == std::io::ErrorKind::InvalidData {
                let _ = backup_corrupt_config(&path, &format!("config is not valid UTF-8: {e}"));
            } else {
                log_event(
                    "WARN",
                    &format!("settings: config read failed: {e}; using defaults"),
                );
            }
            return Config::default();
        }
    };
    match Config::from_toml_str(&text) {
        Ok((cfg, issues)) => {
            for issue in &issues {
                log_event(
                    "WARN",
                    &format!("settings: config repair {}: {}", issue.field, issue.message),
                );
            }
            log_event(
                "INFO",
                &format!("settings: config loaded from {}", path.display()),
            );
            cfg
        }
        Err(e) => {
            let _ = backup_corrupt_config(&path, &format!("config parse failed: {e}"));
            Config::default()
        }
    }
}
