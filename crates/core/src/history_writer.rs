//! Background history writer shared by the platform display layers.
//!
//! SQLite work is deliberately kept off the UI thread: opening an old
//! database may run a one-time rollup backfill, and `store::prune` may run a
//! `VACUUM`. Either can take seconds on a slow disk, which would freeze a
//! taskbar/menu-bar message loop. The platform layers only enqueue samples
//! through [`HistoryWriter::record`]; this worker owns the connection and
//! performs the open/insert/prune cycle.
//!
//! The worker uses the same policy as the original per-platform writers:
//! - the first sample (and every 300th enabled sample) retries opening the
//!   database after a failure;
//! - a non-finite sample is skipped by `Store::insert` and is not an error;
//! - insert/prune failures are logged once per failure streak, and the flag
//!   is cleared after a later success so a new failure is visible again.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use crate::config::history_path;
use crate::store::Store;

/// Log sink used by the worker. The platform layers pass their own
/// `log_event` wrapper; tests can pass a no-op.
pub type LogFn = Arc<dyn Fn(&str, &str) + Send + Sync + 'static>;

/// One queued sample. `ts` is the local wall-clock second stamped by the
/// sampler; `retention_days` is read from the config that produced the
/// sample so a config change takes effect on the next prune.
#[derive(Debug, Clone, Copy)]
struct HistorySample {
    ts: i64,
    cpu: f64,
    mem: f64,
    retention_days: u32,
}

/// Handle used by the UI thread to enqueue history samples. Dropping it
/// closes the channel and lets the worker exit.
pub struct HistoryWriter {
    tx: Sender<HistorySample>,
}

impl HistoryWriter {
    /// Spawn the worker for the standard history database. A path resolution
    /// failure or thread spawn failure is returned so the caller can log it
    /// and retry later rather than blocking the UI.
    pub fn spawn(log: LogFn) -> std::io::Result<Self> {
        let path = history_path().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("history path unavailable: {e}"),
            )
        })?;
        Self::spawn_at(log, path)
    }

    /// Spawn the worker against an explicit database path. Exposed so the
    /// open/insert/prune policy can be exercised against a temporary store.
    pub fn spawn_at(log: LogFn, path: PathBuf) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("sysvitals-history".to_string())
            .spawn(move || run(rx, log, path))?;
        Ok(Self { tx })
    }

    /// Queue one sample. Returns `false` when the worker has stopped (for
    /// example after a panic); the caller can then drop this handle and let
    /// the lazy spawn policy start a fresh worker.
    pub fn record(&self, ts: i64, cpu: f64, mem: f64, retention_days: u32) -> bool {
        self.tx
            .send(HistorySample {
                ts,
                cpu,
                mem,
                retention_days,
            })
            .is_ok()
    }
}

fn should_attempt_open(tick: u32) -> bool {
    tick == 1 || tick.is_multiple_of(300)
}

/// Keep the sample timestamp monotonic across wall-clock jumps. `ts` is the
/// primary key of the raw table and duplicate inserts are ignored, so an NTP
/// rollback would otherwise silently drop every sample until the clock catches
/// up again. The display timestamp and the stored sample share the result.
pub fn next_sample_ts(last: Option<i64>, wall: i64) -> i64 {
    match last {
        Some(previous) => wall.max(previous.saturating_add(1)),
        None => wall,
    }
}

fn run(rx: Receiver<HistorySample>, log: LogFn, path: PathBuf) {
    let mut store: Option<Store> = None;
    let mut tick: u32 = 0;
    let mut write_error_logged = false;

    for msg in rx {
        tick = tick.wrapping_add(1);

        if store.is_none() && should_attempt_open(tick) {
            match Store::open(&path) {
                Ok(opened) => {
                    let rollup = if opened.rollup_ready() {
                        "ready"
                    } else {
                        "unavailable, raw-only fallback"
                    };
                    log(
                        "INFO",
                        &format!("history store opened: {} (rollup {rollup})", path.display()),
                    );
                    store = Some(opened);
                    write_error_logged = false;
                }
                Err(e) => {
                    log(
                        "WARN",
                        &format!("history store failed to open ({}): {e}", path.display()),
                    );
                    store = None;
                }
            }
        }

        let Some(store) = store.as_ref() else {
            continue;
        };

        match store.insert(msg.ts, msg.cpu, msg.mem) {
            Ok(_) => write_error_logged = false,
            Err(e) => {
                if !write_error_logged {
                    write_error_logged = true;
                    log("WARN", &format!("history insert failed: {e}"));
                }
            }
        }

        if tick.is_multiple_of(300) {
            match store.prune(msg.ts, msg.retention_days) {
                Ok(_) => write_error_logged = false,
                Err(e) => {
                    if !write_error_logged {
                        write_error_logged = true;
                        log("WARN", &format!("history prune failed: {e}"));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{next_sample_ts, should_attempt_open};

    #[test]
    fn open_retry_cadence_matches_the_documented_300_tick_window() {
        assert!(should_attempt_open(1));
        assert!(!should_attempt_open(2));
        assert!(!should_attempt_open(299));
        assert!(should_attempt_open(300));
        assert!(should_attempt_open(600));
        assert!(!should_attempt_open(301));
        // Wrapping the counter must not panic or take the retry path.
        assert!(!should_attempt_open(u32::MAX));
    }

    #[test]
    fn sample_timestamps_never_go_backwards() {
        assert_eq!(next_sample_ts(None, 1_700_000_000), 1_700_000_000);
        assert_eq!(
            next_sample_ts(Some(1_700_000_000), 1_700_000_002),
            1_700_000_002
        );
        // Wall clock rolled back: keep increasing instead of colliding.
        assert_eq!(
            next_sample_ts(Some(1_700_000_000), 1_699_999_000),
            1_700_000_001
        );
        assert_eq!(next_sample_ts(Some(i64::MAX), i64::MAX), i64::MAX);
    }
}
