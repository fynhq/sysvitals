//! GPUI-free shared helpers for every SysVitals process.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

pub const SV_BUILD_ID: &str = match option_env!("SV_BUILD_ID") {
    Some(id) => id,
    None => concat!("v", env!("CARGO_PKG_VERSION")),
};

pub fn command_args() -> Vec<String> {
    std::env::args().skip(1).collect()
}

pub fn prewarm_arg() -> bool {
    command_args().iter().any(|arg| arg == "--prewarm")
}

pub fn parent_pid_arg() -> Option<u32> {
    sysvitals_core::cli::parse_parent_pid(&command_args())
}

pub fn anchor_arg() -> Option<sysvitals_core::cli::AnchorRect> {
    sysvitals_core::cli::parse_anchor(&command_args())
}

pub fn anchor_monitor_arg() -> Option<u64> {
    sysvitals_core::cli::parse_anchor_monitor(&command_args())
}

pub fn ready_file_arg() -> Option<std::path::PathBuf> {
    sysvitals_core::cli::parse_ready_file(&command_args())
}

pub fn local_now() -> (String, String) {
    let now = chrono::Local::now();
    (
        now.format("%Y-%m-%d").to_string(),
        now.format("%H:%M:%S%.3f").to_string(),
    )
}

pub fn log_event(level: &str, message: &str) {
    let Ok(dir) = sysvitals_core::config::data_dir() else {
        return;
    };
    let (date, time) = local_now();
    sysvitals_core::logger::log_line(&dir, &date, &time, level, message);
}

pub fn log_sanitize_issues(prefix: &str, issues: &[sysvitals_core::config::SanitizeIssue]) {
    for issue in issues {
        log_event(
            "WARN",
            &format!("{prefix} {}: {}", issue.field, issue.message),
        );
    }
}

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

pub fn log_auto_start_failure(desired: bool, error: &str) {
    log_event(
        "WARN",
        &format!(
            "auto-start {} failed: {error}",
            if desired { "enable" } else { "disable" }
        ),
    );
}

const AUTO_START_RETRY_COOLDOWN: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoStartOutcome {
    Unchanged,
    Changed { detail: Option<String> },
    PendingApproval,
}

#[derive(Debug, Default)]
pub struct AutoStartController {
    applied: Option<bool>,
    retry_at: Option<Instant>,
}

impl AutoStartController {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.applied = None;
        self.retry_at = None;
    }

    pub fn apply<F>(&mut self, cfg: &sysvitals_core::Config, set: F)
    where
        F: FnOnce(bool) -> Result<AutoStartOutcome, String>,
    {
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

pub enum ConfigEvent {
    Unchanged,
    Failed,
    Loaded {
        config: sysvitals_core::Config,
        issues: Vec<sysvitals_core::config::SanitizeIssue>,
        source: sysvitals_core::config::ConfigSource,
    },
}

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

pub const PANEL_SETTINGS: &str = "settings";
pub const PANEL_HISTORY: &str = "history";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelAction {
    Open,
    Toggle,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelRequest {
    pub action: PanelAction,
    pub anchor: Option<sysvitals_core::cli::AnchorRect>,
    pub monitor: Option<u64>,
    pub requested_at_ms: u64,
}

impl PanelRequest {
    pub fn encode(self) -> String {
        let action = match self.action {
            PanelAction::Open => "open",
            PanelAction::Toggle => "toggle",
        };
        let anchor = self
            .anchor
            .map(|a| format!("{} {} {} {}", a.left, a.top, a.right, a.bottom))
            .unwrap_or_else(|| "- - - -".to_string());
        format!(
            "{action} {anchor} {} {}\n",
            self.monitor.unwrap_or(0),
            self.requested_at_ms
        )
    }

    pub fn decode(text: &str) -> Option<Self> {
        let mut parts = text.split_whitespace();
        let action = match parts.next()? {
            "open" => PanelAction::Open,
            "toggle" => PanelAction::Toggle,
            _ => return None,
        };
        let left = parts.next()?;
        let (anchor, monitor, requested_at_ms) = if left == "-" {
            for _ in 0..3 {
                if parts.next()? != "-" {
                    return None;
                }
            }
            let monitor = parse_monitor(parts.next()?)?;
            (None, monitor, parts.next()?.parse::<u64>().ok()?)
        } else {
            let left = parse_coord(left)?;
            let top = parse_coord(parts.next()?)?;
            let right = parse_coord(parts.next()?)?;
            let bottom = parse_coord(parts.next()?)?;
            if right <= left || bottom <= top {
                return None;
            }
            let monitor = parse_monitor(parts.next()?)?;
            (
                Some(sysvitals_core::cli::AnchorRect {
                    left,
                    top,
                    right,
                    bottom,
                }),
                monitor,
                parts.next()?.parse::<u64>().ok()?,
            )
        };
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            action,
            anchor,
            monitor,
            requested_at_ms,
        })
    }

    pub fn is_stale(self, max_age_ms: u64) -> bool {
        unix_ms().saturating_sub(self.requested_at_ms) > max_age_ms
    }
}

fn parse_coord(value: &str) -> Option<f32> {
    let value = value.parse::<f32>().ok()?;
    value.is_finite().then_some(value)
}

fn parse_monitor(value: &str) -> Option<Option<u64>> {
    let monitor = value.parse::<u64>().ok()?;
    Some((monitor != 0).then_some(monitor))
}

pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

pub fn write_panel_request(
    panel: &str,
    action: PanelAction,
    anchor: Option<sysvitals_core::cli::AnchorRect>,
    monitor: Option<u64>,
) -> Result<(), String> {
    let dir = sysvitals_core::config::data_dir()
        .map_err(|error| format!("data directory unavailable: {error}"))?
        .join("panels");
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let path = dir.join(format!("{panel}.request"));
    let temp = dir.join(format!("{panel}.request.tmp-{}", std::process::id()));
    let request = PanelRequest {
        action,
        anchor,
        monitor,
        requested_at_ms: unix_ms(),
    };
    std::fs::write(&temp, request.encode())
        .map_err(|error| format!("cannot write {}: {error}", temp.display()))?;
    if let Err(first_error) = std::fs::rename(&temp, &path) {
        let _ = std::fs::remove_file(&path);
        std::fs::rename(&temp, &path).map_err(|error| {
            format!(
                "cannot replace {} (first attempt: {first_error}): {error}",
                path.display()
            )
        })?;
    }
    Ok(())
}

pub fn take_panel_request(panel: &str) -> Option<PanelRequest> {
    let dir = sysvitals_core::config::data_dir().ok()?.join("panels");
    let path = dir.join(format!("{panel}.request"));
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    PanelRequest::decode(&text)
}

pub fn clear_panel_request(panel: &str) {
    let Ok(dir) = sysvitals_core::config::data_dir() else {
        return;
    };
    let _ = std::fs::remove_file(dir.join("panels").join(format!("{panel}.request")));
}

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

#[cfg(windows)]
pub fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

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

#[cfg(windows)]
pub fn taskbar_al() -> Option<u32> {
    hkcu_dword(
        "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced",
        "TaskbarAl",
    )
}

pub enum InstanceLock {
    Acquired(SingleInstanceGuard),
    Busy,
    Unavailable(String),
}

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

// Single-instance lock: named mutex on Windows, locked file on macOS; Busy means another process owns it.
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

#[cfg(windows)]
pub struct ParentProcess {
    handle: isize,
}

#[cfg(windows)]
impl ParentProcess {
    pub fn open(pid: u32) -> Result<Self, String> {
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
        match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
            Ok(handle) => Ok(Self {
                handle: handle.0 as isize,
            }),
            Err(error) => Err(error.to_string()),
        }
    }

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

    #[test]
    fn panel_request_round_trips_with_an_anchor() {
        let request = PanelRequest {
            action: PanelAction::Open,
            anchor: Some(sysvitals_core::cli::AnchorRect {
                left: 1.5,
                top: 2.0,
                right: 101.5,
                bottom: 42.0,
            }),
            monitor: Some(9_001),
            requested_at_ms: 123_456,
        };
        assert_eq!(PanelRequest::decode(&request.encode()), Some(request));
    }

    #[test]
    fn panel_request_round_trips_without_an_anchor() {
        let request = PanelRequest {
            action: PanelAction::Toggle,
            anchor: None,
            monitor: None,
            requested_at_ms: 7,
        };
        assert_eq!(PanelRequest::decode(&request.encode()), Some(request));
    }

    #[test]
    fn panel_request_rejects_malformed_lines() {
        assert_eq!(PanelRequest::decode(""), None);
        assert_eq!(PanelRequest::decode("open"), None);
        assert_eq!(PanelRequest::decode("open 1 2 3 4 0 1 extra"), None);
        assert_eq!(
            PanelRequest::decode("open 5 5 5 9 0 1"),
            None,
            "degenerate anchor must be rejected"
        );
        assert_eq!(
            PanelRequest::decode("open - - - x 0 1"),
            None,
            "incomplete no-anchor marker must be rejected"
        );
        assert_eq!(PanelRequest::decode("close - - - - 0 1"), None);
    }
}
