//! GPUI-free shared helpers for every SysVitals process.
//!
//! The indicator processes (`app-win` / `app-mac`) and the GPUI panels all
//! log to the same daily file, install the same crash hook and — on Windows
//! — read the same HKCU probes. Keeping that plumbing here removes three
//! drifting copies; the GPUI theme layer (`sysvitals-panel`) re-exports
//! these so panel call sites stay unchanged.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Build stamp emitted by `build.rs`: `v{semver}` on a tagged commit,
/// `v{semver}_{short-hash} (dev)` otherwise. One definition keeps the
/// indicator startup logs and the settings footer on the same value.
pub const SV_BUILD_ID: &str = match option_env!("SV_BUILD_ID") {
    Some(id) => id,
    None => concat!("v", env!("CARGO_PKG_VERSION")),
};

/// Process arguments without the binary name. Panels share the CLI contract
/// in `core::cli`; this is the single place that reads `std::env::args`.
pub fn command_args() -> Vec<String> {
    std::env::args().skip(1).collect()
}

/// `--parent-pid <pid>` from the process arguments.
pub fn parent_pid_arg() -> Option<u32> {
    sysvitals_core::cli::parse_parent_pid(&command_args())
}

/// `--anchor L T R B` from the process arguments.
pub fn anchor_arg() -> Option<sysvitals_core::cli::AnchorRect> {
    sysvitals_core::cli::parse_anchor(&command_args())
}

/// `--anchor-monitor <handle>` from the process arguments.
pub fn anchor_monitor_arg() -> Option<u64> {
    sysvitals_core::cli::parse_anchor_monitor(&command_args())
}

/// Local date (`YYYY-MM-DD`) and time (`HH:MM:SS.mmm`) for log stamps, from
/// chrono's local clock. The display layers used to format these per
/// platform (`GetLocalTime` on Windows, `NSDate` on macOS); one clock keeps
/// every process's lines comparable.
pub fn local_now() -> (String, String) {
    let now = chrono::Local::now();
    (
        now.format("%Y-%m-%d").to_string(),
        now.format("%H:%M:%S%.3f").to_string(),
    )
}

/// Append one line to today's log file under the data dir. Logging must
/// never break an indicator or panel, so failures are silently ignored.
pub fn log_event(level: &str, message: &str) {
    let Ok(dir) = sysvitals_core::config::data_dir() else {
        return;
    };
    let (date, time) = local_now();
    sysvitals_core::logger::log_line(&dir, &date, &time, level, message);
}

/// Log sanitized-config repairs with one stable prefix shared by the
/// indicator, settings and history processes. `prefix` is the complete message
/// prefix (for example `"config repair"` or `"settings: config merge repair"`).
pub fn log_sanitize_issues(prefix: &str, issues: &[sysvitals_core::config::SanitizeIssue]) {
    for issue in issues {
        log_event(
            "WARN",
            &format!("{prefix} {}: {}", issue.field, issue.message),
        );
    }
}

/// Queue one sample for the background history writer. Both platform
/// indicators used to carry an identical copy of this closure/slot plumbing;
/// the log sink is cached here so each sample costs only an `Arc` clone.
pub fn record_history_sample(
    slot: &mut sysvitals_core::HistoryWriterSlot,
    cfg: &sysvitals_core::Config,
    now: i64,
    snapshot: &sysvitals_core::Snapshot,
) {
    static LOG: OnceLock<sysvitals_core::history_writer::LogFn> = OnceLock::new();
    let log = LOG
        .get_or_init(|| Arc::new(|level: &str, message: &str| log_event(level, message)))
        .clone();
    let _ = slot.record(
        now,
        snapshot.cpu_pct,
        snapshot.mem_pct,
        cfg.history.retention_days,
        log,
    );
}

/// Shared failure line for a platform login-startup apply. The platform owns
/// the retry cooldown and the side effect; this only keeps the message and
/// wording identical on Windows and macOS.
pub fn log_auto_start_failure(desired: bool, error: &str) {
    log_event(
        "WARN",
        &format!(
            "auto-start {} failed: {error}",
            if desired { "enable" } else { "disable" }
        ),
    );
}

/// A failed auto-start apply is retried without waiting for the user to touch
/// the setting again. The cooldown keeps a permanently unavailable system
/// login item from producing one warning per config poll.
const AUTO_START_RETRY_COOLDOWN: Duration = Duration::from_secs(300);

/// Result of one platform auto-start apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoStartOutcome {
    /// The system state already matched; nothing changed.
    Unchanged,
    /// The system state changed. `detail` carries platform status text.
    Changed { detail: Option<String> },
    /// macOS `SMAppService` is waiting for the user's approval.
    PendingApproval,
}

/// Shared login-startup policy for both indicators.
///
/// The platform owns the side effect and the durable state (Startup shortcut
/// or `SMAppService`); this controller owns the rules that must not drift:
/// an inherited `false` is not a user choice, the value is applied once per
/// change, and a failure retries after [`AUTO_START_RETRY_COOLDOWN`].
#[derive(Debug, Default)]
pub struct AutoStartController {
    applied: Option<bool>,
    retry_at: Option<Instant>,
}

impl AutoStartController {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget the last applied value and any retry. Used when the config file
    /// is missing: a later re-created file may apply its value again even if
    /// it matches the last one this process saw.
    pub fn reset(&mut self) {
        self.applied = None;
        self.retry_at = None;
    }

    /// Apply the configured value once. `set` performs the platform side
    /// effect and reports whether anything changed.
    pub fn apply<F>(&mut self, cfg: &sysvitals_core::Config, set: F)
    where
        F: FnOnce(bool) -> Result<AutoStartOutcome, String>,
    {
        // A `false` that merely came from the default (or a config re-created
        // after a corrupt file was backed up) is not a user choice, so it must
        // not remove a login item. A legacy `true` is still safe to apply: it
        // can only repair/add the entry, never remove one.
        if !cfg.startup.auto_start_explicit && !cfg.startup.auto_start {
            self.reset();
            return;
        }
        let desired = cfg.startup.auto_start;
        if self.applied == Some(desired) {
            self.retry_at = None;
            return;
        }
        match set(desired) {
            Ok(outcome) => {
                self.applied = Some(desired);
                self.retry_at = None;
                match outcome {
                    AutoStartOutcome::Unchanged => {}
                    AutoStartOutcome::Changed { detail } => {
                        let state = if desired { "enabled" } else { "disabled" };
                        match detail {
                            Some(detail) => {
                                log_event("INFO", &format!("auto-start {state} ({detail})"))
                            }
                            None => log_event("INFO", &format!("auto-start {state}")),
                        }
                    }
                    AutoStartOutcome::PendingApproval => log_event(
                        "INFO",
                        "auto-start requires approval in System Settings > Login Items",
                    ),
                }
            }
            Err(error) => {
                self.retry_at = Some(Instant::now() + AUTO_START_RETRY_COOLDOWN);
                log_auto_start_failure(desired, &error);
            }
        }
    }

    /// Retry a failed apply on its cooldown. Only a file-backed, successfully
    /// parsed config may drive this side effect.
    pub fn retry_if_due<F>(&mut self, file_backed: bool, cfg: &sysvitals_core::Config, set: F)
    where
        F: FnOnce(bool) -> Result<AutoStartOutcome, String>,
    {
        if !file_backed {
            self.retry_at = None;
            return;
        }
        if !self.retry_at.is_some_and(|at| Instant::now() >= at) {
            return;
        }
        self.retry_at = None;
        self.apply(cfg, set);
    }
}

/// Result of one shared config-file poll.
pub enum ConfigEvent {
    Unchanged,
    Failed,
    Loaded {
        config: sysvitals_core::Config,
        issues: Vec<sysvitals_core::config::SanitizeIssue>,
        source: sysvitals_core::config::ConfigSource,
    },
}

/// Poll the shared config file and log a parse failure once per broken mtime.
/// The retry/backoff policy lives in [`sysvitals_core::ConfigWatcher`]; this
/// only keeps the three panel/indicator call sites on the same message shape.
pub fn poll_config(watcher: &mut sysvitals_core::ConfigWatcher, error_prefix: &str) -> ConfigEvent {
    use sysvitals_core::config_watch::ConfigPoll;
    match watcher.poll() {
        ConfigPoll::Unchanged => ConfigEvent::Unchanged,
        ConfigPoll::Failed { error, logged } => {
            if logged {
                log_event("WARN", &format!("{error_prefix} failed: {error}"));
            }
            ConfigEvent::Failed
        }
        ConfigPoll::Loaded {
            config,
            issues,
            source,
        } => ConfigEvent::Loaded {
            config,
            issues,
            source,
        },
    }
}

/// Rust panic hook writing the panic (with location and backtrace) to the
/// log before the process dies. Shared by every binary; the Windows overlay
/// additionally installs a Win32 unhandled-exception filter on top.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let message = if let Some(text) = info.payload().downcast_ref::<&str>() {
            (*text).to_string()
        } else if let Some(text) = info.payload().downcast_ref::<String>() {
            text.clone()
        } else {
            "unknown panic".to_string()
        };
        let location = info
            .location()
            .map(|loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()))
            .unwrap_or_else(|| "unknown location".to_string());
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_string();
        let backtrace = std::backtrace::Backtrace::force_capture();
        log_event(
            "FATAL",
            &format!("panic in thread {thread} at {location}: {message} | backtrace: {backtrace}"),
        );
    }));
}

/// Encode a Rust string as a NUL-terminated UTF-16 vector for `PCWSTR`.
#[cfg(windows)]
pub fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Read a DWORD from HKCU; `None` when the key or value is absent.
#[cfg(windows)]
pub fn hkcu_dword(key: &str, value: &str) -> Option<u32> {
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, REG_DWORD,
        REG_VALUE_TYPE,
    };
    let key_w = utf16(key);
    let value_w = utf16(value);
    let mut hkey = HKEY::default();
    if unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(key_w.as_ptr()),
            None,
            KEY_READ,
            &mut hkey,
        )
    }
    .is_err()
    {
        return None;
    }
    let mut data = [0u8; 4];
    let mut size = 4u32;
    let mut kind = REG_VALUE_TYPE::default();
    let queried = unsafe {
        RegQueryValueExW(
            hkey,
            windows::core::PCWSTR(value_w.as_ptr()),
            None,
            Some(&mut kind),
            Some(data.as_mut_ptr()),
            Some(&mut size),
        )
    };
    let _ = unsafe { RegCloseKey(hkey) };
    if queried.is_ok() && size >= 4 && kind == REG_DWORD {
        Some(u32::from_le_bytes(data))
    } else {
        None
    }
}

/// The Windows system directory (`%SystemRoot%\System32`), or `None` when
/// the probe fails. System tools must be launched by absolute path: a bare
/// `cmd.exe`/`timeout.exe`/`taskmgr.exe` is resolved through the current
/// directory and `PATH`, so a file with the same name next to a portable
/// launch or on a developer `PATH` can win.
#[cfg(windows)]
pub fn system_directory() -> Option<std::path::PathBuf> {
    use windows::Win32::System::SystemInformation::GetSystemDirectoryW;

    let mut buf = vec![0u16; 260];
    let mut len = unsafe { GetSystemDirectoryW(Some(&mut buf[..])) } as usize;
    if len == 0 {
        return None;
    }
    if len >= buf.len() {
        buf.resize(len + 1, 0);
        len = unsafe { GetSystemDirectoryW(Some(&mut buf[..])) } as usize;
        if len == 0 || len >= buf.len() {
            return None;
        }
    }
    Some(std::path::PathBuf::from(String::from_utf16_lossy(
        &buf[..len],
    )))
}

/// The Windows OS build number, or `None` if the version probe fails.
///
/// `RtlGetVersion` is the documented way to read the real build (the classic
/// `GetVersionEx` is version-helpered and lies on 8.1+).
#[cfg(windows)]
pub fn os_build_number() -> Option<u32> {
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    if unsafe { RtlGetVersion(&mut info) }.is_err() {
        return None;
    }
    Some(info.dwBuildNumber)
}

/// HKCU Explorer taskbar-alignment flag (`TaskbarAl`): `1` when the Windows
/// 11 taskbar centers its icons. Read by both the overlay (placement) and
/// the settings panel (anchor choices), which must agree on the very same
/// value.
#[cfg(windows)]
pub fn taskbar_al() -> Option<u32> {
    hkcu_dword(
        "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced",
        "TaskbarAl",
    )
}

/// Outcome of acquiring the per-user single-instance lock.
pub enum InstanceLock {
    /// This process owns the lock; keep the guard alive for the process
    /// lifetime.
    Acquired(SingleInstanceGuard),
    /// Another process already owns it.
    Busy,
    /// The OS refused to create the lock; the caller decides whether that is
    /// fatal for its process.
    Unavailable(String),
}

/// Platform single-instance guard. Windows holds a named mutex handle;
/// macOS holds an exclusively flocked file; other hosts are a no-op.
#[cfg(windows)]
pub struct SingleInstanceGuard {
    handle: windows::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.handle) };
    }
}

#[cfg(target_os = "macos")]
pub struct SingleInstanceGuard {
    _file: std::fs::File,
}

#[cfg(not(any(windows, target_os = "macos")))]
pub struct SingleInstanceGuard;

/// Acquire the process-wide instance lock. `name` is a named-mutex name on
/// Windows and a file name under the data directory on macOS. Keeping the
/// value alive holds the lock; process exit releases it.
#[cfg(windows)]
pub fn acquire_instance_lock(name: &str) -> InstanceLock {
    use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    let name = utf16(name);
    match unsafe { CreateMutexW(None, false, windows::core::PCWSTR(name.as_ptr())) } {
        Ok(handle) => {
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                let _ = unsafe { CloseHandle(handle) };
                InstanceLock::Busy
            } else {
                InstanceLock::Acquired(SingleInstanceGuard { handle })
            }
        }
        Err(error) => InstanceLock::Unavailable(error.to_string()),
    }
}

#[cfg(target_os = "macos")]
pub fn acquire_instance_lock(name: &str) -> InstanceLock {
    use std::os::fd::AsRawFd;
    let Ok(dir) = sysvitals_core::config::data_dir() else {
        return InstanceLock::Unavailable("cannot resolve the data directory".to_string());
    };
    if let Err(error) = std::fs::create_dir_all(&dir) {
        return InstanceLock::Unavailable(format!("cannot create {}: {error}", dir.display()));
    }
    let path = dir.join(name);
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) => {
            return InstanceLock::Unavailable(format!("cannot open {}: {error}", path.display()))
        }
    };
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            InstanceLock::Busy
        } else {
            InstanceLock::Unavailable(format!("instance lock failed: {error}"))
        };
    }
    InstanceLock::Acquired(SingleInstanceGuard { _file: file })
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn acquire_instance_lock(_name: &str) -> InstanceLock {
    InstanceLock::Acquired(SingleInstanceGuard)
}

/// A Windows parent process handle watched by a child panel. The handle is
/// stored as `isize` so the struct can move into a GPUI foreground task;
/// dropping it closes the handle.
#[cfg(windows)]
pub struct ParentProcess {
    handle: isize,
}

#[cfg(windows)]
impl ParentProcess {
    /// Open `pid` with `PROCESS_SYNCHRONIZE`. The error string is the Windows
    /// error text so callers can keep their existing diagnostic line.
    pub fn open(pid: u32) -> Result<Self, String> {
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
        match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
            Ok(handle) => Ok(Self {
                handle: handle.0 as isize,
            }),
            Err(error) => Err(error.to_string()),
        }
    }

    /// Whether the parent has exited. A failed wait is treated as exited, the
    /// same conservative rule the panels used before this helper existed.
    pub fn exited(&self) -> bool {
        use windows::Win32::Foundation::{HANDLE, WAIT_TIMEOUT};
        use windows::Win32::System::Threading::WaitForSingleObject;
        unsafe {
            WaitForSingleObject(HANDLE(self.handle as *mut std::ffi::c_void), 0) != WAIT_TIMEOUT
        }
    }
}

#[cfg(windows)]
impl Drop for ParentProcess {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        let _ = unsafe { CloseHandle(HANDLE(self.handle as *mut std::ffi::c_void)) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn local_now_shapes_match_the_logger_contract() {
        let (date, time) = local_now();
        assert_eq!(date.len(), 10, "YYYY-MM-DD");
        assert_eq!(date.as_bytes()[4], b'-');
        assert_eq!(date.as_bytes()[7], b'-');
        assert!(time.len() >= 8, "HH:MM:SS at minimum");
        assert_eq!(time.as_bytes()[2], b':');
        assert_eq!(time.as_bytes()[5], b':');
    }

    #[test]
    fn inherited_auto_start_false_is_not_a_user_choice() {
        let mut controller = AutoStartController::new();
        let called = Cell::new(false);
        controller.apply(&sysvitals_core::Config::default(), |_| {
            called.set(true);
            Ok(AutoStartOutcome::Unchanged)
        });
        assert!(!called.get());
    }

    #[test]
    fn auto_start_applies_once_per_value() {
        let mut cfg = sysvitals_core::Config::default();
        cfg.startup.auto_start = true;
        cfg.startup.auto_start_explicit = true;
        let mut controller = AutoStartController::new();
        let calls = Cell::new(0);
        controller.apply(&cfg, |desired| {
            calls.set(calls.get() + 1);
            assert!(desired);
            Ok(AutoStartOutcome::Changed { detail: None })
        });
        controller.apply(&cfg, |_| {
            calls.set(calls.get() + 1);
            Ok(AutoStartOutcome::Changed { detail: None })
        });
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn retry_is_ignored_for_defaults() {
        let mut controller = AutoStartController::new();
        let mut cfg = sysvitals_core::Config::default();
        cfg.startup.auto_start = true;
        cfg.startup.auto_start_explicit = true;
        let called = Cell::new(false);
        controller.retry_if_due(false, &cfg, |_| {
            called.set(true);
            Ok(AutoStartOutcome::Unchanged)
        });
        assert!(!called.get());
    }
}
