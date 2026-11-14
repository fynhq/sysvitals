//! Shared config-file polling.
//!
//! The indicator (Windows and macOS) and the history panel all watch the
//! same TOML file, but each used to carry its own mtime cache, parse-error
//! throttle and "keep the last good config" policy. This type owns that
//! contract once:
//!
//! - a successful parse advances the mtime cache and clears the error slot;
//! - a failed parse keeps the last good config and logs once per broken
//!   mtime when retries are enabled; retrying the same broken mtime backs off
//!   (1s, 2s, 4s, ... up to 30s) instead of reparsing every poll tick;
//! - the history popup opts out of retries so a broken file is retried only
//!   when its mtime changes (its existing behavior).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::config::{config_path, Config, ConfigSource, SanitizeIssue};

/// First retry delay after a failed parse; doubles on each subsequent failure
/// at the same mtime, capped at [`MAX_RETRY_DELAY`].
const RETRY_BASE: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

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
    /// Consecutive failed parses at `error_mtime`, used for the retry backoff.
    failure_streak: u32,
    /// Earliest time the same broken mtime may be reparsed.
    failure_retry_at: Option<Instant>,
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
            failure_streak: 0,
            failure_retry_at: None,
        }
    }

    /// Stop retrying a failed parse until the file's mtime changes again.
    /// Used by the history popup, whose panel keeps the last good config
    /// instead of parsing a broken file every refresh tick.
    pub fn without_retry(mut self) -> Self {
        self.retry_failed = false;
        self
    }

    /// Load the initial config and seed the mtime cache **only on success**.
    ///
    /// The mtime is captured before the read: a save that lands between the
    /// read and the seed must be noticed by the next poll, not swallowed.
    /// A failed load leaves the cache unseeded so the first poll retries
    /// instead of accepting defaults for the rest of the process lifetime.
    pub fn load_initial(&mut self) -> anyhow::Result<(Config, Vec<SanitizeIssue>, ConfigSource)> {
        let Some(path) = self.path.as_ref() else {
            return Err(anyhow::anyhow!("config path unavailable"));
        };
        let mtime = config_mtime_nanos(path);
        match Config::load_with_source(path) {
            Ok((config, issues, source)) => {
                self.last_mtime = mtime;
                self.error_mtime = 0;
                self.failure_streak = 0;
                self.failure_retry_at = None;
                Ok((config, issues, source))
            }
            Err(error) => {
                let context = format!("while loading {}", path.display());
                self.note_failure(mtime, Instant::now());
                Err(error.context(context))
            }
        }
    }

    /// Seed the cache with the file's current mtime after the caller loaded
    /// the initial config, so the first poll does not reload it.
    pub fn seed_now(&mut self) {
        self.last_mtime = self.path.as_deref().map(config_mtime_nanos).unwrap_or(0);
        self.error_mtime = 0;
        self.failure_streak = 0;
        self.failure_retry_at = None;
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
        self.poll_mtime_at(mtime, Instant::now())
    }

    /// Testable poll: `now` is injected so the retry backoff can be exercised
    /// without sleeping.
    fn poll_mtime_at(&mut self, mtime: u64, now: Instant) -> ConfigPoll {
        // `mtime == 0` means missing/unreadable metadata; a failed initial
        // read at mtime 0 must still be retried even though `last_mtime` also
        // starts at 0. An unresolved error makes the equality check below a
        // retry slot rather than "unchanged".
        let unresolved =
            self.retry_failed && self.error_mtime == mtime && self.failure_retry_at.is_some();
        if mtime == self.last_mtime && !unresolved {
            return ConfigPoll::Unchanged;
        }
        let Some(path) = self.path.as_ref() else {
            return ConfigPoll::Unchanged;
        };
        // A broken mtime is reparsed only after its backoff deadline; a
        // different mtime is always a new attempt.
        if unresolved {
            if let Some(retry_at) = self.failure_retry_at {
                if now < retry_at {
                    return ConfigPoll::Unchanged;
                }
            }
        }
        match Config::load_with_source(path) {
            Ok((config, issues, source)) => {
                self.last_mtime = mtime;
                self.error_mtime = 0;
                self.failure_streak = 0;
                self.failure_retry_at = None;
                ConfigPoll::Loaded {
                    config,
                    issues,
                    source,
                }
            }
            Err(error) => {
                let logged = self.error_mtime != mtime;
                self.note_failure(mtime, now);
                if !self.retry_failed {
                    self.last_mtime = mtime;
                    self.failure_retry_at = None;
                }
                ConfigPoll::Failed {
                    error: error.to_string(),
                    logged,
                }
            }
        }
    }

    fn note_failure(&mut self, mtime: u64, now: Instant) {
        if self.error_mtime != mtime {
            self.failure_streak = 0;
        }
        self.error_mtime = mtime;
        self.failure_streak = self.failure_streak.saturating_add(1);
        self.failure_retry_at = Some(now + retry_delay(self.failure_streak));
    }
}

/// Exponential backoff for repeated failures at the same mtime.
fn retry_delay(streak: u32) -> Duration {
    let shift = streak.saturating_sub(1).min(5);
    Duration::from_secs(RETRY_BASE.as_secs() << shift).min(MAX_RETRY_DELAY)
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
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

    fn make_watcher(path: &Path) -> ConfigWatcher {
        ConfigWatcher {
            path: Some(path.to_path_buf()),
            last_mtime: 0,
            error_mtime: 0,
            retry_failed: true,
            failure_streak: 0,
            failure_retry_at: None,
        }
    }

    fn valid_toml() -> String {
        Config::default().to_toml_string().unwrap()
    }

    #[test]
    fn loaded_then_unchanged() {
        let path = temp_path("loaded");
        fs::write(&path, valid_toml()).unwrap();
        let mut watcher = make_watcher(&path);
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
        let mut watcher = make_watcher(&path);
        assert!(matches!(watcher.poll_mtime(10), ConfigPoll::Loaded { .. }));

        fs::write(&path, "not valid toml").unwrap();
        let now = Instant::now();
        match watcher.poll_mtime_at(20, now) {
            ConfigPoll::Failed { logged, .. } => assert!(logged),
            _ => panic!("expected a failed poll"),
        }
        // The first retry is deferred by the backoff; the same broken mtime
        // is not reparsed on every poll tick.
        assert!(matches!(
            watcher.poll_mtime_at(20, now + Duration::from_millis(500)),
            ConfigPoll::Unchanged
        ));
        match watcher.poll_mtime_at(20, now + Duration::from_secs(2)) {
            ConfigPoll::Failed { logged, .. } => assert!(!logged),
            _ => panic!("expected a retried failed poll"),
        }

        fs::write(&path, valid_toml()).unwrap();
        assert!(matches!(
            watcher.poll_mtime_at(30, now + Duration::from_secs(3)),
            ConfigPoll::Loaded { .. }
        ));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn initial_load_seeds_only_on_success_and_failure_is_retried() {
        let path = temp_path("initial");
        fs::write(&path, valid_toml()).unwrap();
        let mut watcher = make_watcher(&path);
        let (_, _, source) = watcher.load_initial().unwrap();
        assert_eq!(source, ConfigSource::File);
        assert!(matches!(watcher.poll(), ConfigPoll::Unchanged));

        fs::write(&path, "not valid toml").unwrap();
        let mut broken = make_watcher(&path);
        assert!(broken.load_initial().is_err());
        // A later valid file is picked up by the next poll even though the
        // initial load failed; the watcher did not absorb the broken mtime.
        fs::write(&path, valid_toml()).unwrap();
        assert!(matches!(
            broken.poll_mtime_at(2, Instant::now()),
            ConfigPoll::Loaded { .. }
        ));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn without_retry_advances_the_cache_on_failure() {
        let path = temp_path("no-retry");
        fs::write(&path, "not valid toml").unwrap();
        let mut watcher = make_watcher(&path).without_retry();
        match watcher.poll_mtime(20) {
            ConfigPoll::Failed { logged, .. } => assert!(logged),
            _ => panic!("expected a failed poll"),
        }
        assert!(matches!(watcher.poll_mtime(20), ConfigPoll::Unchanged));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn retry_backoff_doubles_and_caps() {
        assert_eq!(retry_delay(1), Duration::from_secs(1));
        assert_eq!(retry_delay(2), Duration::from_secs(2));
        assert_eq!(retry_delay(3), Duration::from_secs(4));
        assert_eq!(retry_delay(10), MAX_RETRY_DELAY);
    }

    #[test]
    fn an_unresolved_failure_at_mtime_zero_is_retried() {
        let path = temp_path("zero-mtime");
        fs::write(&path, valid_toml()).unwrap();
        let mut watcher = make_watcher(&path);
        let now = Instant::now();
        // Simulate an initial read failure whose metadata also failed, so
        // both `last_mtime` and `error_mtime` are zero.
        watcher.last_mtime = 0;
        watcher.error_mtime = 0;
        watcher.failure_streak = 1;
        watcher.failure_retry_at = Some(now + Duration::from_secs(1));
        assert!(matches!(
            watcher.poll_mtime_at(0, now),
            ConfigPoll::Unchanged
        ));
        assert!(matches!(
            watcher.poll_mtime_at(0, now + Duration::from_secs(2)),
            ConfigPoll::Loaded { .. }
        ));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn missing_path_reports_unchanged() {
        let mut watcher = ConfigWatcher {
            path: None,
            last_mtime: 0,
            error_mtime: 0,
            retry_failed: true,
            failure_streak: 0,
            failure_retry_at: None,
        };
        assert!(matches!(watcher.poll_mtime(10), ConfigPoll::Unchanged));
    }

    #[test]
    fn seed_now_absorbs_the_current_mtime() {
        let path = temp_path("seed");
        fs::write(&path, valid_toml()).unwrap();
        let mut watcher = make_watcher(&path);
        watcher.seed_now();
        assert!(matches!(watcher.poll(), ConfigPoll::Unchanged));
        let _ = fs::remove_file(&path);
    }
}
