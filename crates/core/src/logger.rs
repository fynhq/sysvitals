//! Daily file logger with pruning.

use std::borrow::Cow;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const LOG_RETENTION_DAYS: u32 = 7;

// A failing open must not be retried on every line; the 2 s sampler would storm syscalls.
const OPEN_RETRY_COOLDOWN: Duration = Duration::from_secs(60);

static LOG_FILE: Mutex<Option<File>> = Mutex::new(None);
static LOG_DATE: Mutex<String> = Mutex::new(String::new());
static LOG_RETRY_AT: Mutex<Option<Instant>> = Mutex::new(None);

pub fn log_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("logs")
}

fn process_tag() -> &'static str {
    static TAG: OnceLock<String> = OnceLock::new();
    TAG.get_or_init(|| {
        let mut name = std::env::current_exe()
            .ok()
            .and_then(|path| {
                path.file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
            })
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "sysvitals".to_string());
        if name.eq_ignore_ascii_case("sysvitals") {
            name = "SysVitals".to_string();
        }
        format!("{name}[{}]", std::process::id())
    })
}

fn one_line(message: &str) -> Cow<'_, str> {
    if !message.contains('\n') && !message.contains('\r') {
        return Cow::Borrowed(message);
    }
    let mut out = String::with_capacity(message.len());
    let mut chars = message.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(' ');
            }
            '\n' => out.push(' '),
            _ => out.push(ch),
        }
    }
    Cow::Owned(out)
}

fn open_for_date(dir: &Path, date: &str) -> Option<File> {
    let _ = fs::create_dir_all(dir);
    let path = dir.join(format!("sysvitals-{date}.log"));
    OpenOptions::new().create(true).append(true).open(path).ok()
}

pub fn log_line(data_dir: &Path, date: &str, time: &str, level: &str, message: &str) -> bool {
    log_line_at(data_dir, date, time, level, message, Instant::now())
}

fn log_line_at(
    data_dir: &Path,
    date: &str,
    time: &str,
    level: &str,
    message: &str,
    now: Instant,
) -> bool {
    let mut file_guard = LOG_FILE.lock().unwrap_or_else(|e| e.into_inner());
    let mut date_guard = LOG_DATE.lock().unwrap_or_else(|e| e.into_inner());
    let mut retry_guard = LOG_RETRY_AT.lock().unwrap_or_else(|e| e.into_inner());

    let rolled = *date_guard != date;
    if rolled {
        *file_guard = None;
        *retry_guard = None;
        *date_guard = date.to_string();
        prune_logs(data_dir, date, LOG_RETENTION_DAYS);
    }
    if file_guard.is_none() && !retry_guard.is_some_and(|at| now < at) {
        match open_for_date(&log_dir(data_dir), date) {
            Some(file) => {
                *file_guard = Some(file);
                *retry_guard = None;
            }
            None => *retry_guard = Some(now + OPEN_RETRY_COOLDOWN),
        }
    }
    let Some(file) = file_guard.as_mut() else {
        return false;
    };
    let message = one_line(message);
    let line = format!("{date} {time} [{level}] {} {message}\n", process_tag());
    file.write_all(line.as_bytes()).is_ok()
}

pub fn prune_logs(data_dir: &Path, today: &str, retention_days: u32) {
    let dir = log_dir(data_dir);
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let Some(today_days) = date_to_days(today) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(file_date) = parse_log_name(&name) else {
            continue;
        };
        let Some(file_days) = date_to_days(&file_date) else {
            continue;
        };
        if today_days.saturating_sub(file_days) > retention_days as i64 {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn parse_log_name(name: &str) -> Option<String> {
    let rest = name.strip_prefix("sysvitals-")?.strip_suffix(".log")?;
    if rest.len() != 10 || !rest.as_bytes()[4].eq(&b'-') || !rest.as_bytes()[7].eq(&b'-') {
        return None;
    }
    if !rest.bytes().enumerate().all(|(i, b)| {
        if i == 4 || i == 7 {
            b == b'-'
        } else {
            b.is_ascii_digit()
        }
    }) {
        return None;
    }
    Some(rest.to_string())
}

fn date_to_days(date: &str) -> Option<i64> {
    let year: i32 = date.get(0..4)?.parse().ok()?;
    let month: u32 = date.get(5..7)?.parse().ok()?;
    let day: u32 = date.get(8..10)?.parse().ok()?;
    let parsed = chrono::NaiveDate::from_ymd_opt(year, month, day)?;
    let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1)?;
    Some((parsed - epoch).num_days())
}

#[cfg(test)]
mod tests {
    use super::*;

    static LOG_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn reset_log_state() -> std::sync::MutexGuard<'static, ()> {
        let guard = LOG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        *LOG_FILE.lock().unwrap() = None;
        *LOG_DATE.lock().unwrap() = String::new();
        *LOG_RETRY_AT.lock().unwrap() = None;
        guard
    }

    fn temp_log_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("sysvitals-log{tag}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn date_to_days_uses_the_proleptic_gregorian_calendar() {
        assert_eq!(date_to_days("1970-01-01"), Some(0));
        assert_eq!(date_to_days("1970-01-02"), Some(1));
        assert_eq!(date_to_days("2000-03-01"), Some(11_017));
        assert_eq!(
            date_to_days("2024-03-01").unwrap() - date_to_days("2024-02-28").unwrap(),
            2,
            "2024 is a leap year"
        );
        assert_eq!(date_to_days("2026-02-30"), None);
        assert_eq!(date_to_days("2026-13-01"), None);
        assert_eq!(date_to_days("2026-10-1"), None);
        assert_eq!(date_to_days("nope"), None);
    }

    #[test]
    fn parse_log_name_accepts_only_pattern() {
        assert_eq!(
            parse_log_name("sysvitals-2026-10-19.log"),
            Some("2026-10-19".into())
        );
        assert_eq!(parse_log_name("sysvitals-2026-10-1.log"), None);
        assert_eq!(parse_log_name("other-2026-10-19.log"), None);
        assert_eq!(parse_log_name("sysvitals-2026-10-19.txt"), None);
    }

    #[test]
    fn one_line_flattens_crlf_and_lf() {
        assert_eq!(&*one_line("plain"), "plain");
        assert_eq!(&*one_line("first\nsecond"), "first second");
        assert_eq!(&*one_line("first\r\nsecond\rthird"), "first second third");
    }

    #[test]
    fn process_tag_has_component_and_pid() {
        let tag = process_tag();
        assert!(tag.ends_with(&format!("[{}]", std::process::id())));
        assert!(!tag.starts_with('['));
        assert!(tag.len() > 2);
    }

    #[test]
    fn prune_removes_only_old_matching_files() {
        let dir = temp_log_dir("prune");
        let _ = fs::remove_dir_all(&dir);
        let logs = log_dir(&dir);
        fs::create_dir_all(&logs).unwrap();
        for name in [
            "sysvitals-2026-10-09.log",
            "sysvitals-2026-10-17.log",
            "sysvitals-2026-10-19.log",
            "notes.txt",
        ] {
            fs::write(logs.join(name), b"x").unwrap();
        }
        prune_logs(&dir, "2026-10-19", LOG_RETENTION_DAYS);
        assert!(
            !logs.join("sysvitals-2026-10-09.log").exists(),
            "old removed"
        );
        assert!(
            logs.join("sysvitals-2026-10-17.log").exists(),
            "recent kept"
        );
        assert!(logs.join("sysvitals-2026-10-19.log").exists(), "today kept");
        assert!(logs.join("notes.txt").exists(), "decoy untouched");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_line_writes_and_rolls() {
        let _guard = reset_log_state();
        let dir = temp_log_dir("write");
        let _ = fs::remove_dir_all(&dir);
        let logs = log_dir(&dir);
        fs::create_dir_all(&logs).unwrap();
        for name in ["sysvitals-2026-09-01.log", "sysvitals-2026-10-12.log"] {
            fs::write(logs.join(name), b"x").unwrap();
        }
        assert!(log_line_at(
            &dir,
            "2026-10-19",
            "13:00:00",
            "INFO",
            "hello",
            Instant::now()
        ));
        assert!(
            !logs.join("sysvitals-2026-09-01.log").exists(),
            "stale file pruned on first line"
        );
        assert!(
            logs.join("sysvitals-2026-10-12.log").exists(),
            "file inside the window kept"
        );
        assert!(log_line_at(
            &dir,
            "2026-10-19",
            "13:01:00",
            "WARN",
            "world",
            Instant::now()
        ));
        let f1 = log_dir(&dir).join("sysvitals-2026-10-19.log");
        let content = fs::read_to_string(&f1).unwrap();
        assert!(content.contains("2026-10-19 13:00:00 [INFO]"));
        assert!(content.contains("hello"));
        assert!(content.contains("2026-10-19 13:01:00 [WARN]"));
        assert!(content.contains("world"));
        assert!(
            content.contains(&format!("[{}]", std::process::id())),
            "every line carries the process tag: {content}"
        );
        assert!(log_line_at(
            &dir,
            "2026-10-20",
            "09:00:00",
            "INFO",
            "next day",
            Instant::now()
        ));
        assert!(log_dir(&dir).join("sysvitals-2026-10-20.log").exists());
        assert!(
            !logs.join("sysvitals-2026-10-12.log").exists(),
            "file pruned on date roll"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_open_reports_false_and_backs_off_until_the_cooldown_expires() {
        let _guard = reset_log_state();
        let dir = temp_log_dir("blocked");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(log_dir(&dir), b"blocker").unwrap();

        let start = Instant::now();
        assert!(
            !log_line_at(&dir, "2026-10-19", "13:00:00", "INFO", "blocked", start),
            "an unopenable log directory must be reported"
        );

        fs::remove_file(log_dir(&dir)).unwrap();
        fs::create_dir_all(log_dir(&dir)).unwrap();
        assert!(
            !log_line_at(
                &dir,
                "2026-10-19",
                "13:00:01",
                "INFO",
                "suppressed",
                start + Duration::from_secs(1)
            ),
            "the cooldown must suppress the retry"
        );
        assert!(
            log_line_at(
                &dir,
                "2026-10-19",
                "13:01:02",
                "INFO",
                "recovered",
                start + OPEN_RETRY_COOLDOWN + Duration::from_secs(1)
            ),
            "the cooldown must expire"
        );

        let content = fs::read_to_string(log_dir(&dir).join("sysvitals-2026-10-19.log")).unwrap();
        assert!(content.contains("recovered"));
        assert!(
            !content.contains("suppressed"),
            "a suppressed line is dropped, not queued"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_date_change_forces_a_retry_inside_the_cooldown() {
        let _guard = reset_log_state();
        let dir = temp_log_dir("rollblocked");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(log_dir(&dir), b"blocker").unwrap();

        let start = Instant::now();
        assert!(!log_line_at(
            &dir,
            "2026-10-19",
            "13:00:00",
            "INFO",
            "blocked",
            start
        ));

        fs::remove_file(log_dir(&dir)).unwrap();
        assert!(
            log_line_at(
                &dir,
                "2026-10-20",
                "09:00:00",
                "INFO",
                "next day",
                start + Duration::from_secs(1)
            ),
            "a new date must retry at once instead of waiting out the cooldown"
        );
        assert!(log_dir(&dir).join("sysvitals-2026-10-20.log").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_runs_even_when_the_log_file_cannot_be_opened() {
        let _guard = reset_log_state();
        let dir = temp_log_dir("pruneblocked");
        let _ = fs::remove_dir_all(&dir);
        let logs = log_dir(&dir);
        fs::create_dir_all(&logs).unwrap();
        fs::write(logs.join("sysvitals-2026-10-09.log"), b"x").unwrap();
        assert!(log_line_at(
            &dir,
            "2026-10-19",
            "13:00:00",
            "INFO",
            "first",
            Instant::now()
        ));
        assert!(
            !logs.join("sysvitals-2026-10-09.log").exists(),
            "stale file pruned"
        );

        fs::create_dir_all(logs.join("sysvitals-2026-10-20.log")).unwrap();
        fs::write(logs.join("sysvitals-2026-10-11.log"), b"x").unwrap();
        assert!(
            !log_line_at(
                &dir,
                "2026-10-20",
                "09:00:00",
                "INFO",
                "blocked",
                Instant::now()
            ),
            "today's log path is a directory, so it cannot be opened"
        );
        assert!(
            !logs.join("sysvitals-2026-10-11.log").exists(),
            "pruning must not depend on a successful open"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
