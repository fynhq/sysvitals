//! Background history writer and database recovery.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::history_path;
use crate::store::{is_corruption_error, Store, RAW_RETENTION_SECS};

const WRITE_FAILURE_LIMIT: u32 = 3;

const OPEN_RETRY_COOLDOWN: Duration = Duration::from_secs(60);

const SPAWN_RETRY_COOLDOWN: Duration = Duration::from_secs(300);

pub type LogFn = Arc<dyn Fn(&str, &str) + Send + Sync + 'static>;

#[derive(Debug, Clone, Copy)]
struct HistorySample {
    ts: i64,
    cpu: f64,
    mem: f64,
    retention_days: u32,
}

pub struct HistoryWriter {
    tx: Sender<HistorySample>,
}

impl HistoryWriter {
    pub fn spawn(log: LogFn) -> std::io::Result<Self> {
        let path = history_path()
            .map_err(|e| std::io::Error::other(format!("history path unavailable: {e}")))?;
        Self::spawn_at(log, path)
    }

    pub fn spawn_at(log: LogFn, path: PathBuf) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("sysvitals-history".to_string())
            .spawn(move || run(rx, log, path))?;
        Ok(Self { tx })
    }

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

pub struct HistoryWriterSlot {
    writer: Option<HistoryWriter>,
    failed_at: Option<Instant>,
    log: Option<LogFn>,
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
            log: None,
        }
    }

    pub fn has_log(&self) -> bool {
        self.log.is_some()
    }

    pub fn set_log(&mut self, log: LogFn) {
        self.log = Some(log);
    }

    pub fn record(&mut self, ts: i64, cpu: f64, mem: f64, retention_days: u32) -> bool {
        let retry_due = self
            .failed_at
            .is_none_or(|at| at.elapsed() >= SPAWN_RETRY_COOLDOWN);
        if self.writer.is_none() && retry_due {
            let Some(log) = self.log.clone() else {
                return false;
            };
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
            self.writer = None;
            self.failed_at = Some(Instant::now());
            if let Some(log) = self.log.as_ref() {
                log(
                    "WARN",
                    &format!(
                        "history writer stopped; retrying in {}s",
                        SPAWN_RETRY_COOLDOWN.as_secs()
                    ),
                );
            }
        }
        send_ok
    }
}

// Corrupt database: move the main file aside first, then its -wal/-shm sidecars, rolling the main
// file back if a sidecar cannot follow so the database never loses uncommitted WAL frames.
fn quarantine_corrupt_db(path: &Path) -> std::io::Result<Option<PathBuf>> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let base = path.as_os_str();
    let mut backup_os = base.to_os_string();
    backup_os.push(format!(".corrupt-{stamp}-{}", std::process::id()));
    let backup = PathBuf::from(backup_os);

    if !path.exists() {
        return Ok(None);
    }
    fs::rename(path, &backup)?;

    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    for suffix in ["-wal", "-shm"] {
        let mut src_os = base.to_os_string();
        src_os.push(suffix);
        let src = PathBuf::from(src_os);
        if !src.exists() {
            continue;
        }
        let mut dest_os = backup.as_os_str().to_os_string();
        dest_os.push(suffix);
        let dest = PathBuf::from(dest_os);
        if let Err(error) = fs::rename(&src, &dest) {
            for (from, to) in moved.iter().rev() {
                let _ = fs::rename(from, to);
            }
            let _ = fs::rename(&backup, path);
            return Err(error);
        }
        moved.push((dest, src));
    }
    Ok(Some(backup))
}

fn quarantine_and_log(path: &Path, log: &LogFn, error_logged: &mut bool) -> bool {
    match quarantine_corrupt_db(path) {
        Ok(Some(backup)) => {
            log(
                "WARN",
                &format!(
                    "history store was corrupt; moved to {} and recreating",
                    backup.display()
                ),
            );
            true
        }
        Ok(None) => {
            log(
                "WARN",
                "history store was corrupt; no database file to move",
            );
            true
        }
        Err(e) => {
            if !*error_logged {
                *error_logged = true;
                log(
                    "WARN",
                    &format!("history store is corrupt but could not be moved aside: {e}"),
                );
            }
            false
        }
    }
}

fn log_store_opened(path: &Path, store: &Store, verb: &str, log: &LogFn) {
    let rollup = if store.rollup_ready() {
        "ready"
    } else {
        "unavailable, raw-only fallback"
    };
    log(
        "INFO",
        &format!("history store {verb}: {} (rollup {rollup})", path.display()),
    );
}

fn open_store(path: &Path, log: &LogFn, error_logged: &mut bool) -> Option<Store> {
    match Store::open(path) {
        Ok(opened) => {
            log_store_opened(path, &opened, "opened", log);
            *error_logged = false;
            Some(opened)
        }
        Err(e) if is_corruption_error(&e) => {
            if !quarantine_and_log(path, log, error_logged) {
                return None;
            }
            match Store::open(path) {
                Ok(opened) => {
                    log_store_opened(path, &opened, "recreated", log);
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

#[allow(clippy::too_many_arguments)]
fn note_write_failure(
    operation: &str,
    error: &anyhow::Error,
    write_failures: &mut u32,
    write_error_logged: &mut bool,
    drop_store: &mut bool,
    quarantine_next: &mut bool,
    log: &LogFn,
) {
    *write_failures = write_failures.saturating_add(1);
    if !*write_error_logged {
        *write_error_logged = true;
        log("WARN", &format!("history {operation} failed: {error}"));
    }
    if is_corruption_error(error) {
        *quarantine_next = true;
        *drop_store = true;
    } else if *write_failures >= WRITE_FAILURE_LIMIT {
        *drop_store = true;
        log(
            "WARN",
            &format!("history store dropped after repeated {operation} failures; reopening"),
        );
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
                can_open = quarantine_and_log(&path, &log, &mut open_error_logged);
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
            Ok(()) => {
                write_failures = 0;
                write_error_logged = false;
            }
            Err(e) => note_write_failure(
                "insert",
                &e,
                &mut write_failures,
                &mut write_error_logged,
                &mut drop_store,
                &mut quarantine_next,
                &log,
            ),
        }

        if tick.is_multiple_of(300) {
            match store_ref.prune(msg.ts, msg.retention_days) {
                Ok(report) => {
                    write_failures = 0;
                    write_error_logged = false;
                    if let Some((now, newest)) = report.clock_skew {
                        log(
                            "WARN",
                            &format!(
                                "history prune skipped: clock {now} is more than {RAW_RETENTION_SECS}s past the newest stored sample {newest}"
                            ),
                        );
                    }
                }
                Err(e) => note_write_failure(
                    "prune",
                    &e,
                    &mut write_failures,
                    &mut write_error_logged,
                    &mut drop_store,
                    &mut quarantine_next,
                    &log,
                ),
            }
        }

        if drop_store {
            store = None;
            write_failures = 0;
            retry_at = Some(Instant::now() + OPEN_RETRY_COOLDOWN);
            if quarantine_next {
                // Corruption recovers at once; the generic cooldown would drop every sample until it expires.
                quarantine_next = false;
                if quarantine_and_log(&path, &log, &mut open_error_logged) {
                    store = open_store(&path, &log, &mut open_error_logged);
                    if store.is_some() {
                        write_error_logged = false;
                        retry_at = None;
                    }
                }
            }
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

    fn capture_log() -> (LogFn, Arc<std::sync::Mutex<Vec<String>>>) {
        let logs: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = logs.clone();
        let log: LogFn = Arc::new(move |level: &str, message: &str| {
            sink.lock().unwrap().push(format!("{level} {message}"));
        });
        (log, logs)
    }

    #[test]
    fn a_blocked_sidecar_rolls_the_main_file_back() {
        let dir = temp_history_path("helper-rollback");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.db");
        let wal = PathBuf::from(format!("{}-wal", path.display()));
        fs::write(&path, b"not a database").unwrap();
        fs::write(&wal, b"wal").unwrap();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut blocked = Vec::new();
        for offset in 0..3 {
            let dest = dir.join(format!(
                "history.db.corrupt-{}-{}-wal",
                stamp + offset,
                std::process::id()
            ));
            fs::create_dir_all(&dest).unwrap();
            fs::write(dest.join("occupied"), b"x").unwrap();
            blocked.push(dest);
        }

        assert!(
            quarantine_corrupt_db(&path).is_err(),
            "an unmovable sidecar must fail the quarantine"
        );
        assert!(path.exists(), "the main file must be rolled back");
        assert!(wal.exists(), "the WAL must stay attached to its database");

        for dest in blocked {
            fs::remove_file(dest.join("occupied")).unwrap();
            fs::remove_dir(dest).unwrap();
        }
        fs::remove_file(&wal).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_slot_stores_its_log_once_and_refuses_to_record_without_one() {
        let mut slot = HistoryWriterSlot::new();
        assert!(!slot.has_log());
        assert!(
            !slot.record(1_700_000_000, 1.0, 2.0, 30),
            "without a stored log there is no writer thread to start"
        );
        let (log, _) = capture_log();
        slot.set_log(log);
        assert!(slot.has_log());
    }

    #[test]
    fn a_forward_clock_jump_is_logged_and_never_drops_the_store() {
        let dir = temp_history_path("skew");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.db");
        let (log_for_test, logs) = capture_log();
        let writer = HistoryWriter::spawn_at(log_for_test, path.clone()).unwrap();
        let base = 1_700_000_000i64;
        let jumped = base + 400 * 86_400;
        for tick in 1..=900u32 {
            // The jumped ticks carry no readable CPU, so they never reach the store and the
            // newest stored minute stays pre-jump for every prune round.
            let (ts, cpu) = if tick.is_multiple_of(300) {
                (jumped, f64::NAN)
            } else {
                (base, 10.0)
            };
            assert!(writer.record(ts, cpu, 20.0, 30));
        }

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let skipped = logs
                .lock()
                .unwrap()
                .iter()
                .filter(|line| line.contains("prune skipped"))
                .count();
            if skipped >= 3 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "only {skipped} skew warnings; logs={logs:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        let logged = logs.lock().unwrap();
        assert!(
            !logged
                .iter()
                .any(|line| line.contains("dropped after repeated")),
            "a skew skip must not feed the write-failure strikes: {logged:?}"
        );
        assert!(
            !logged.iter().any(|line| line.contains("prune failed")),
            "a skew skip is not a prune error: {logged:?}"
        );
        drop(logged);

        let reader = Store::open_read_only(&path).unwrap();
        let buckets = reader.buckets_offset(base - 60, base + 60, 60, 0).unwrap();
        assert_eq!(buckets.len(), 1, "the jumped clock must not delete history");
        assert!((buckets[0].cpu - 10.0).abs() < 1e-9);

        drop(writer);
        drop(reader);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn quarantine_helper_moves_the_database_then_reports_nothing_to_move() {
        let dir = temp_history_path("helper-move");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.db");
        fs::write(&path, b"not a database").unwrap();
        let (log, logs) = capture_log();
        let mut error_logged = false;

        assert!(quarantine_and_log(&path, &log, &mut error_logged));
        assert!(!path.exists());
        assert!(quarantine_and_log(&path, &log, &mut error_logged));

        let logged = logs.lock().unwrap();
        assert_eq!(logged.len(), 2);
        assert!(logged[0].contains("moved to"));
        assert!(logged[1].contains("no database file to move"));
        assert!(!error_logged);
        drop(logged);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_blocked_quarantine_logs_once_and_keeps_the_original() {
        let dir = temp_history_path("helper-blocked");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.db");
        fs::write(&path, b"not a database").unwrap();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        for offset in 0..3 {
            let blocked = dir.join(format!(
                "history.db.corrupt-{}-{}",
                stamp + offset,
                std::process::id()
            ));
            fs::create_dir_all(&blocked).unwrap();
            fs::write(blocked.join("occupied"), b"x").unwrap();
        }
        let (log, logs) = capture_log();
        let mut error_logged = false;

        assert!(!quarantine_and_log(&path, &log, &mut error_logged));
        assert!(!quarantine_and_log(&path, &log, &mut error_logged));
        assert!(path.exists(), "an unmovable database must stay in place");

        let logged = logs.lock().unwrap();
        assert_eq!(logged.len(), 1, "a blocked move is logged once");
        assert!(logged[0].contains("could not be moved aside"));
        assert!(error_logged);
        drop(logged);
        let _ = fs::remove_dir_all(&dir);
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
        let (log_for_test, logs) = capture_log();

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
        // The writer creates the file before SQLite writes the schema, so a
        // reader that arrives in between legitimately sees an empty database.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if Store::open_read_only(&path).is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "recreated database never became readable; logs={logs:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        drop(writer);
        let _ = fs::remove_dir_all(&dir);
    }
}
