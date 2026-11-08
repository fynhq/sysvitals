//! Background history writer shared by the platform display layers.
//!
//! SQLite work is deliberately kept off the UI thread: opening an old
//! database may run a one-time rollup backfill, and `store::prune` may run a
//! `VACUUM`. Either can take seconds on a slow disk, which would freeze a
//! taskbar/menu-bar message loop. The platform layers only enqueue samples
//! through [`HistoryWriter::record`]; this worker owns the connection and
//! performs the open/insert/prune cycle.
//!
//! Failure policy:
//! - a failed open is retried on a short cooldown, and a corrupt database is
//!   moved aside before a fresh one is created;
//! - a few consecutive insert/prune failures drop the connection so the next
//!   open can recover from a database replaced or left read-only underneath
//!   the process;
//! - a non-finite sample is skipped by `Store::insert` and is not an error;
//! - failures are logged once per streak, and the flag is cleared after a
//!   later success so a new failure is visible again.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::history_path;
use crate::store::{is_corruption_error, Store};

/// How many consecutive insert/prune failures drop the connection. A single
/// transient busy/locked error should not throw away a healthy connection;
/// a persistent failure must not keep the writer stuck on it forever.
const WRITE_FAILURE_LIMIT: u32 = 3;

/// How long the worker waits before retrying an open after a failure. Opening
/// a corrupt database can itself be slow, so this is a time-based cooldown
/// rather than one retry per sample burst.
const OPEN_RETRY_COOLDOWN: Duration = Duration::from_secs(60);

/// How long a failed worker spawn waits before trying again. Thread creation
/// or path resolution can fail transiently (resource pressure, data directory
/// not ready); do not disable history for the rest of the process lifetime.
const SPAWN_RETRY_COOLDOWN: Duration = Duration::from_secs(300);

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

/// Per-process handle state shared by the two indicator layers. Keeping the
/// lazy spawn/retry policy here removes the copy that used to live in
/// `app-win` and `app-mac`.
pub struct HistoryWriterSlot {
    writer: Option<HistoryWriter>,
    failed_at: Option<Instant>,
}

impl Default for HistoryWriterSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl HistoryWriterSlot {
    pub const fn new() -> Self {
        Self {
            writer: None,
            failed_at: None,
        }
    }

    /// Enqueue one sample, starting the worker lazily. Returns `false` when
    /// the worker could not be started or its channel is closed; the caller
    /// has already been logged in that case.
    pub fn record(&mut self, ts: i64, cpu: f64, mem: f64, retention_days: u32, log: LogFn) -> bool {
        let retry_due = self
            .failed_at
            .is_none_or(|at| at.elapsed() >= SPAWN_RETRY_COOLDOWN);
        if self.writer.is_none() && retry_due {
            match HistoryWriter::spawn(log.clone()) {
                Ok(started) => {
                    self.writer = Some(started);
                    self.failed_at = None;
                    log("INFO", "history writer started");
                }
                Err(e) => {
                    self.failed_at = Some(Instant::now());
                    log(
                        "WARN",
                        &format!(
                            "history writer unavailable: {e}; retrying in {}s",
                            SPAWN_RETRY_COOLDOWN.as_secs()
                        ),
                    );
                    return false;
                }
            }
        }
        let Some(writer) = self.writer.as_ref() else {
            return false;
        };
        let send_ok = writer.record(ts, cpu, mem, retention_days);
        if !send_ok {
            // The worker thread stopped (for example after a panic). Drop the
            // handle and let the cooldown start a fresh one.
            self.writer = None;
            self.failed_at = Some(Instant::now());
            log(
                "WARN",
                &format!(
                    "history writer stopped; retrying in {}s",
                    SPAWN_RETRY_COOLDOWN.as_secs()
                ),
            );
        }
        send_ok
    }
}

/// Move a corrupt database and its sidecars aside. Sidecars are moved first
/// so a failed main-file rename leaves the original database intact and the
/// next retry can try again; the caller only opens a fresh database after the
/// main file has moved (or was already absent).
///
/// Returns the backup path when the main file was moved, `None` when no main
/// file existed. A permission/lock error is returned so the worker can retry
/// instead of silently creating a second database beside an open handle.
fn quarantine_corrupt_db(path: &Path) -> std::io::Result<Option<PathBuf>> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let base = path.as_os_str();
    let mut backup_os = base.to_os_string();
    backup_os.push(format!(".corrupt-{stamp}-{}", std::process::id()));
    let backup = PathBuf::from(backup_os);

    let mut main_moved = false;
    for suffix in ["-wal", "-shm", ""] {
        let mut src_os = base.to_os_string();
        src_os.push(suffix);
        let src = PathBuf::from(src_os);
        if !src.exists() {
            continue;
        }
        let mut dest_os = backup.as_os_str().to_os_string();
        dest_os.push(suffix);
        let dest = PathBuf::from(dest_os);
        fs::rename(&src, &dest)?;
        if suffix.is_empty() {
            main_moved = true;
        }
    }
    Ok(main_moved.then_some(backup))
}

/// Open the store, quarantining a corrupt database once before retrying.
/// `error_logged` throttles open failures so a permanently unavailable path
/// does not produce one WARN per cooldown window.
fn open_store(path: &Path, log: &LogFn, error_logged: &mut bool) -> Option<Store> {
    match Store::open(path) {
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
            *error_logged = false;
            Some(opened)
        }
        Err(e) if is_corruption_error(&e) => {
            match quarantine_corrupt_db(path) {
                Ok(Some(backup)) => log(
                    "WARN",
                    &format!(
                        "history store was corrupt; moved to {} and recreating",
                        backup.display()
                    ),
                ),
                Ok(None) => log(
                    "WARN",
                    "history store was corrupt; no database file to move",
                ),
                Err(qe) => {
                    if !*error_logged {
                        *error_logged = true;
                        log(
                            "WARN",
                            &format!("history store is corrupt but could not be moved aside: {qe}"),
                        );
                    }
                    return None;
                }
            }
            match Store::open(path) {
                Ok(opened) => {
                    let rollup = if opened.rollup_ready() {
                        "ready"
                    } else {
                        "unavailable, raw-only fallback"
                    };
                    log(
                        "INFO",
                        &format!(
                            "history store recreated: {} (rollup {rollup})",
                            path.display()
                        ),
                    );
                    *error_logged = false;
                    Some(opened)
                }
                Err(reopen_error) => {
                    if !*error_logged {
                        *error_logged = true;
                        log(
                            "WARN",
                            &format!(
                                "history store failed to open after quarantine: {reopen_error}"
                            ),
                        );
                    }
                    None
                }
            }
        }
        Err(e) => {
            if !*error_logged {
                *error_logged = true;
                log(
                    "WARN",
                    &format!("history store failed to open ({}): {e}", path.display()),
                );
            }
            None
        }
    }
}

fn run(rx: Receiver<HistorySample>, log: LogFn, path: PathBuf) {
    let mut store: Option<Store> = None;
    let mut tick: u32 = 0;
    let mut write_failures: u32 = 0;
    let mut write_error_logged = false;
    let mut open_error_logged = false;
    let mut retry_at: Option<Instant> = None;
    let mut quarantine_next = false;

    for msg in rx {
        tick = tick.wrapping_add(1);

        let now = Instant::now();
        if store.is_none() && retry_at.is_none_or(|at| now >= at) {
            let mut can_open = true;
            if quarantine_next {
                match quarantine_corrupt_db(&path) {
                    Ok(Some(backup)) => log(
                        "WARN",
                        &format!(
                            "history store was corrupt; moved to {} and recreating",
                            backup.display()
                        ),
                    ),
                    Ok(None) => log(
                        "WARN",
                        "history store was corrupt; no database file to move",
                    ),
                    Err(e) => {
                        if !open_error_logged {
                            open_error_logged = true;
                            log(
                                "WARN",
                                &format!(
                                    "history store is corrupt but could not be moved aside: {e}"
                                ),
                            );
                        }
                        can_open = false;
                    }
                }
                quarantine_next = false;
            }
            if can_open {
                store = open_store(&path, &log, &mut open_error_logged);
                if store.is_some() {
                    write_failures = 0;
                    write_error_logged = false;
                    retry_at = None;
                }
            }
            if store.is_none() {
                retry_at = Some(Instant::now() + OPEN_RETRY_COOLDOWN);
            }
        }

        let Some(store_ref) = store.as_ref() else {
            continue;
        };
        let mut drop_store = false;

        match store_ref.insert(msg.ts, msg.cpu, msg.mem) {
            Ok(_) => {
                write_failures = 0;
                write_error_logged = false;
            }
            Err(e) => {
                write_failures = write_failures.saturating_add(1);
                if !write_error_logged {
                    write_error_logged = true;
                    log("WARN", &format!("history insert failed: {e}"));
                }
                if is_corruption_error(&e) {
                    quarantine_next = true;
                    drop_store = true;
                } else if write_failures >= WRITE_FAILURE_LIMIT {
                    drop_store = true;
                    log(
                        "WARN",
                        "history store dropped after repeated insert failures; reopening",
                    );
                }
            }
        }

        if tick.is_multiple_of(300) {
            match store_ref.prune(msg.ts, msg.retention_days) {
                Ok(_) => {
                    write_failures = 0;
                    write_error_logged = false;
                }
                Err(e) => {
                    write_failures = write_failures.saturating_add(1);
                    if !write_error_logged {
                        write_error_logged = true;
                        log("WARN", &format!("history prune failed: {e}"));
                    }
                    if is_corruption_error(&e) {
                        quarantine_next = true;
                        drop_store = true;
                    } else if write_failures >= WRITE_FAILURE_LIMIT {
                        drop_store = true;
                        log(
                            "WARN",
                            "history store dropped after repeated prune failures; reopening",
                        );
                    }
                }
            }
        }

        if drop_store {
            store = None;
            write_failures = 0;
            retry_at = Some(Instant::now() + OPEN_RETRY_COOLDOWN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_history_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "sysvitals-history-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn corrupt_database_is_moved_with_its_sidecars() {
        let path = temp_history_path("quarantine");
        let wal = PathBuf::from(format!("{}-wal", path.display()));
        let shm = PathBuf::from(format!("{}-shm", path.display()));
        fs::write(&path, b"not a database").unwrap();
        fs::write(&wal, b"wal").unwrap();
        fs::write(&shm, b"shm").unwrap();

        let backup = quarantine_corrupt_db(&path).unwrap().expect("backup path");
        assert!(!path.exists());
        assert!(!wal.exists());
        assert!(!shm.exists());
        assert!(backup.exists());
        assert!(PathBuf::from(format!("{}-wal", backup.display())).exists());
        assert!(PathBuf::from(format!("{}-shm", backup.display())).exists());

        fs::remove_file(&backup).unwrap();
        fs::remove_file(format!("{}-wal", backup.display())).unwrap();
        fs::remove_file(format!("{}-shm", backup.display())).unwrap();
    }

    #[test]
    fn missing_database_reports_no_backup() {
        let path = temp_history_path("missing");
        assert!(quarantine_corrupt_db(&path).unwrap().is_none());
    }

    #[test]
    fn garbage_database_is_classified_as_corruption() {
        let path = temp_history_path("garbage");
        fs::write(&path, b"this is not a sqlite database").unwrap();
        let error = match Store::open(&path) {
            Ok(_) => panic!("garbage file unexpectedly opened as a database"),
            Err(error) => error,
        };
        assert!(
            is_corruption_error(&error),
            "expected a corruption error, got {error}"
        );
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn worker_quarantines_a_corrupt_database_and_recreates_it() {
        let dir = temp_history_path("worker-quarantine");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.db");
        fs::write(&path, b"not a database").unwrap();
        let logs: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log_for_test: LogFn = {
            let logs = logs.clone();
            Arc::new(move |level: &str, message: &str| {
                logs.lock().unwrap().push(format!("{level} {message}"));
            })
        };

        let writer = HistoryWriter::spawn_at(log_for_test, path.clone()).unwrap();
        assert!(writer.record(1_700_000_000, 10.0, 20.0, 30));

        let deadline = Instant::now() + Duration::from_secs(5);
        let backup_seen = loop {
            let backup = fs::read_dir(&dir).ok().and_then(|entries| {
                entries.flatten().map(|entry| entry.path()).find(|entry| {
                    entry
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().contains(".corrupt-"))
                })
            });
            if backup.is_some() && path.exists() {
                break backup;
            }
            assert!(
                Instant::now() < deadline,
                "worker did not quarantine and recreate the database; logs={logs:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(backup_seen.is_some());
        assert!(Store::open_read_only(&path).is_ok());

        drop(writer);
        let _ = fs::remove_dir_all(&dir);
    }
}
