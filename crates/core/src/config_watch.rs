//! Shared config-file polling.
//!
//! The indicator (Windows and macOS) and the history panel all watch the
//! same TOML file, but each used to carry its own mtime cache, parse-error
//! throttle and "keep the last good config" policy. This type owns that
//! contract once:
//!
//! - a successful parse advances the mtime cache and clears the error slot;
//! - a failed parse keeps the last good config and logs once per broken
//!   mtime when retries are enabled;
//! - the history popup opts out of retries so a broken file is retried only
//!   when its mtime changes (its existing behavior).

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::config::{config_path, Config, ConfigSource, SanitizeIssue};

/// One poll result. `Loaded` carries the same pieces `Config::load_with_source`
/// returns so the caller can log repairs and apply platform side effects.
pub enum ConfigPoll {
    /// The mtime cache still matches, or no config path is available.
    Unchanged,
    /// A new, successfully parsed config.
    Loaded {
        config: Config,
        issues: Vec<SanitizeIssue>,
        source: ConfigSource,
    },
    /// A new mtime that failed to parse. `logged` is true only for the first
    /// failure at this mtime, so callers can emit one warning per broken
    /// version instead of one per poll tick.
    Failed { error: String, logged: bool },
}

/// Watches one config path for mtime changes.
pub struct ConfigWatcher {
    path: Option<PathBuf>,
    last_mtime: u64,
    error_mtime: u64,
    retry_failed: bool,
}

impl Default for ConfigWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigWatcher {
    /// Watch the user's config path. The path is resolved once; a host where
    /// the data directory is unavailable simply reports `Unchanged`.
    pub fn new() -> Self {
        Self {
            path: config_path().ok(),
            last_mtime: 0,
            error_mtime: 0,
            retry_failed: true,
        }
    }

    /// Stop retrying a failed parse until the file's mtime changes again.
    /// Used by the history popup, whose panel keeps the last good language
    /// and CSV scope instead of parsing a broken file every refresh tick.
    pub fn without_retry(mut self) -> Self {
        self.retry_failed = false;
        self
    }

    /// Seed the cache with the file's current mtime after the caller loaded
    /// the initial config, so the first poll does not reload it.
    pub fn seed_now(&mut self) {
        self.last_mtime = self.path.as_deref().map(config_mtime_nanos).unwrap_or(0);
    }

    /// Poll the file once.
    pub fn poll(&mut self) -> ConfigPoll {
        let Some(path) = self.path.as_deref() else {
            return ConfigPoll::Unchanged;
        };
        let mtime = config_mtime_nanos(path);
        self.poll_mtime(mtime)
    }

    fn poll_mtime(&mut self, mtime: u64) -> ConfigPoll {
        if mtime == self.last_mtime {
            return ConfigPoll::Unchanged;
        }
        let Some(path) = self.path.as_ref() else {
            return ConfigPoll::Unchanged;
        };
        match Config::load_with_source(path) {
            Ok((config, issues, source)) => {
                self.last_mtime = mtime;
                self.error_mtime = 0;
                ConfigPoll::Loaded {
                    config,
                    issues,
                    source,
                }
            }
            Err(error) => {
                let logged = self.error_mtime != mtime;
                self.error_mtime = mtime;
                if !self.retry_failed {
                    self.last_mtime = mtime;
                }
                ConfigPoll::Failed {
                    error: error.to_string(),
                    logged,
                }
            }
        }
    }
}

/// Last-modified time of `path` in nanoseconds since the Unix epoch, or 0
/// when the file does not exist or cannot be read. Nanosecond precision is
/// required: the settings window saves on every click, and two saves inside
/// one second would otherwise be invisible to the poller.
fn config_mtime_nanos(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "sysvitals-config-watch-{}-{nanos}-{label}.toml",
            std::process::id()
        ))
    }

    fn watcher(path: &Path) -> ConfigWatcher {
        ConfigWatcher {
            path: Some(path.to_path_buf()),
            last_mtime: 0,
            error_mtime: 0,
            retry_failed: true,
        }
    }

    fn valid_toml() -> String {
        Config::default().to_toml_string().unwrap()
    }

    #[test]
    fn loaded_then_unchanged() {
        let path = temp_path("loaded");
        fs::write(&path, valid_toml()).unwrap();
        let mut watcher = watcher(&path);
        assert!(matches!(
            watcher.poll_mtime(10),
            ConfigPoll::Loaded {
                source: ConfigSource::File,
                ..
            }
        ));
        assert!(matches!(watcher.poll_mtime(10), ConfigPoll::Unchanged));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn failed_parse_retries_and_logs_once_per_mtime() {
        let path = temp_path("failed");
        fs::write(&path, valid_toml()).unwrap();
        let mut watcher = watcher(&path);
        assert!(matches!(watcher.poll_mtime(10), ConfigPoll::Loaded { .. }));

        fs::write(&path, "not valid toml").unwrap();
        match watcher.poll_mtime(20) {
            ConfigPoll::Failed { logged, .. } => assert!(logged),
            _ => panic!("expected a failed poll"),
        }
        match watcher.poll_mtime(20) {
            ConfigPoll::Failed { logged, .. } => assert!(!logged),
            _ => panic!("expected a retried failed poll"),
        }

        fs::write(&path, valid_toml()).unwrap();
        assert!(matches!(watcher.poll_mtime(30), ConfigPoll::Loaded { .. }));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn without_retry_advances_the_cache_on_failure() {
        let path = temp_path("no-retry");
        fs::write(&path, "not valid toml").unwrap();
        let mut watcher = watcher(&path).without_retry();
        match watcher.poll_mtime(20) {
            ConfigPoll::Failed { logged, .. } => assert!(logged),
            _ => panic!("expected a failed poll"),
        }
        assert!(matches!(watcher.poll_mtime(20), ConfigPoll::Unchanged));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn missing_path_reports_unchanged() {
        let mut watcher = ConfigWatcher {
            path: None,
            last_mtime: 0,
            error_mtime: 0,
            retry_failed: true,
        };
        assert!(matches!(watcher.poll_mtime(10), ConfigPoll::Unchanged));
    }

    #[test]
    fn seed_now_absorbs_the_current_mtime() {
        let path = temp_path("seed");
        fs::write(&path, valid_toml()).unwrap();
        let mut watcher = watcher(&path);
        watcher.seed_now();
        assert!(matches!(watcher.poll(), ConfigPoll::Unchanged));
        let _ = fs::remove_file(&path);
    }
}
