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
    let _ = log_event_ok(level, message);
}

pub fn log_event_ok(level: &str, message: &str) -> bool {
    let Ok(dir) = sysvitals_core::config::data_dir() else {
        return false;
    };
    let (date, time) = local_now();
    sysvitals_core::logger::log_line(&dir, &date, &time, level, message)
}

pub fn log_sanitize_issues(prefix: &str, issues: &[sysvitals_core::config::SanitizeIssue]) {
    for issue in issues {
        log_event(
            "WARN",
            &format!("{prefix} {}: {}", issue.field, issue.message),
        );
    }
}

fn history_log_fn() -> sysvitals_core::history_writer::LogFn {
    static LOG: OnceLock<sysvitals_core::history_writer::LogFn> = OnceLock::new();
    LOG.get_or_init(|| {
        Arc::new(|level: &str, message: &str| {
            log_event(level, message);
        })
    })
    .clone()
}

pub fn record_history_sample(
    slot: &mut sysvitals_core::HistoryWriterSlot,
    cfg: &sysvitals_core::Config,
    now: i64,
    snapshot: &sysvitals_core::Snapshot,
) {
    if !slot.has_log() {
        slot.set_log(history_log_fn());
    }
    let _ = slot.record(
        now,
        snapshot.cpu_pct,
        snapshot.mem_pct,
        cfg.history.retention_days,
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
                        let text = match detail {
                            Some(detail) => format!("auto-start {state} ({detail})"),
                            None => format!("auto-start {state}"),
                        };
                        log_event("INFO", &text);
                    }
                    AutoStartOutcome::PendingApproval => {
                        log_event(
                            "INFO",
                            "auto-start requires approval in System Settings > Login Items",
                        );
                    }
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
pub const PANEL_REQUEST_POLL_MS: u64 = 50;
pub const PANEL_REQUEST_MAX_AGE_MS: u64 = 10_000;

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

fn panels_dir() -> Result<std::path::PathBuf, String> {
    sysvitals_core::config::data_dir()
        .map(|dir| dir.join("panels"))
        .map_err(|error| format!("data directory unavailable: {error}"))
}

pub fn write_panel_request(
    panel: &str,
    action: PanelAction,
    anchor: Option<sysvitals_core::cli::AnchorRect>,
    monitor: Option<u64>,
) -> Result<(), String> {
    let dir = panels_dir()?;
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    write_panel_request_in(
        &dir,
        panel,
        PanelRequest {
            action,
            anchor,
            monitor,
            requested_at_ms: unix_ms(),
        },
    )
}

fn write_panel_request_in(
    dir: &std::path::Path,
    panel: &str,
    request: PanelRequest,
) -> Result<(), String> {
    let path = dir.join(format!("{panel}.request"));
    let temp = dir.join(format!("{panel}.request.tmp-{}", std::process::id()));
    std::fs::write(&temp, request.encode())
        .map_err(|error| format!("cannot write {}: {error}", temp.display()))?;
    if let Err(first_error) = std::fs::rename(&temp, &path) {
        let _ = std::fs::remove_file(&path);
        if let Err(error) = std::fs::rename(&temp, &path) {
            let _ = std::fs::remove_file(&temp);
            return Err(format!(
                "cannot replace {} (first attempt: {first_error}): {error}",
                path.display()
            ));
        }
    }
    Ok(())
}

pub fn take_panel_request(panel: &str) -> Result<Option<PanelRequest>, String> {
    let dir = panels_dir()?;
    take_panel_request_in(&dir, panel)
}

// The rename claims the request before it is read, so a click written between the read and the
// delete can no longer be destroyed unread.
fn take_panel_request_in(
    dir: &std::path::Path,
    panel: &str,
) -> Result<Option<PanelRequest>, String> {
    let path = dir.join(format!("{panel}.request"));
    let claimed = dir.join(format!("{panel}.request.claimed-{}", std::process::id()));
    if std::fs::rename(&path, &claimed).is_err() {
        return Ok(None);
    }
    let text = match std::fs::read_to_string(&claimed) {
        Ok(text) => text,
        Err(error) => {
            let _ = std::fs::remove_file(&claimed);
            return Err(format!("cannot read {}: {error}", claimed.display()));
        }
    };
    let _ = std::fs::remove_file(&claimed);
    match PanelRequest::decode(&text) {
        Some(request) => Ok(Some(request)),
        None => Err(format!(
            "panel request {} is not decodable: {:?}",
            path.display(),
            text.trim_end()
        )),
    }
}

pub fn clear_panel_request(panel: &str) {
    let Ok(dir) = panels_dir() else {
        return;
    };
    let _ = std::fs::remove_file(dir.join(format!("{panel}.request")));
}

pub fn sweep_panel_temp_files() {
    let Ok(dir) = panels_dir() else {
        return;
    };
    sweep_panel_temp_files_in(&dir);
}

fn sweep_panel_temp_files_in(dir: &std::path::Path) -> usize {
    const ORPHAN_AGE_SECS: u64 = 300;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let cutoff =
        std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(ORPHAN_AGE_SECS));
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.contains(".request.tmp-") && !name.contains(".request.claimed-") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .zip(cutoff)
            .is_some_and(|(modified, cutoff)| modified <= cutoff);
        if stale && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelTarget {
    Focus,
    Close,
    Open,
    Move,
}

// A toggle with a window always closes it; rebuilding on another display belongs to Open only.
pub fn resolve_panel_target(
    action: PanelAction,
    same_monitor: bool,
    has_window: bool,
) -> PanelTarget {
    match (action, has_window) {
        (_, false) => PanelTarget::Open,
        (PanelAction::Toggle, true) => PanelTarget::Close,
        (PanelAction::Open, true) if same_monitor => PanelTarget::Focus,
        (PanelAction::Open, true) => PanelTarget::Move,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitCursorNext {
    Wait,
    Arrow,
    Timeout,
    Terminate,
}

pub fn resolve_wait_cursor(
    target_is_set: bool,
    target_visible: bool,
    elapsed_ms: u64,
    timeout_ms: u64,
    child_alive: bool,
) -> WaitCursorNext {
    if !target_is_set || target_visible {
        return WaitCursorNext::Arrow;
    }
    if elapsed_ms < timeout_ms {
        return WaitCursorNext::Wait;
    }
    if child_alive {
        WaitCursorNext::Terminate
    } else {
        WaitCursorNext::Timeout
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildNext {
    Spawn,
    Request,
    Ignore,
    Watch,
}

pub fn resolve_child_next(
    alive: bool,
    launching: bool,
    prewarm: bool,
    window_visible: bool,
    elapsed_ms: Option<u64>,
    grace_ms: u64,
) -> ChildNext {
    if launching {
        return if prewarm {
            ChildNext::Request
        } else {
            ChildNext::Watch
        };
    }
    if !alive {
        return ChildNext::Spawn;
    }
    let in_grace = !window_visible && !prewarm && elapsed_ms.is_some_and(|ms| ms < grace_ms);
    if in_grace {
        ChildNext::Ignore
    } else {
        ChildNext::Request
    }
}

pub const RESPAWN_DELAYS_MS: [u64; 3] = [5_000, 30_000, 120_000];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RespawnPolicy {
    attempts: u32,
}

impl RespawnPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    pub fn reset(&mut self) {
        self.attempts = 0;
    }

    pub fn next_delay_ms(&mut self) -> Option<u64> {
        let delay = RESPAWN_DELAYS_MS.get(self.attempts as usize).copied();
        if delay.is_some() {
            self.attempts += 1;
        }
        delay
    }
}

fn write_panic_fallback(dir: &std::path::Path, line: &str) -> bool {
    let path = dir.join(format!("sysvitals-panic-{}.log", std::process::id()));
    std::fs::write(&path, format!("FATAL {line}\n")).is_ok()
}

pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
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
        let line =
            format!("panic in thread {thread} at {location}: {message} | backtrace: {backtrace}");
        if !log_event_ok("FATAL", &line) {
            write_panic_fallback(&std::env::temp_dir(), &line);
        }
        previous(info);
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
    handle: windows::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl ParentProcess {
    pub fn open(pid: u32) -> Result<Self, String> {
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
        match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
            Ok(handle) => Ok(Self { handle }),
            Err(error) => Err(error.to_string()),
        }
    }

    pub fn exited(&self) -> bool {
        use windows::Win32::Foundation::WAIT_TIMEOUT;
        use windows::Win32::System::Threading::WaitForSingleObject;
        unsafe { WaitForSingleObject(self.handle, 0) != WAIT_TIMEOUT }
    }
}

#[cfg(windows)]
impl Drop for ParentProcess {
    fn drop(&mut self) {
        use windows::Win32::Foundation::CloseHandle;
        let _ = unsafe { CloseHandle(self.handle) };
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

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "sysvitals-support-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn sample_request() -> PanelRequest {
        PanelRequest {
            action: PanelAction::Toggle,
            anchor: Some(sysvitals_core::cli::AnchorRect {
                left: 1.0,
                top: 2.0,
                right: 101.0,
                bottom: 42.0,
            }),
            monitor: Some(7),
            requested_at_ms: unix_ms(),
        }
    }

    fn dir_names(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_written_request_is_taken_exactly_once() {
        let dir = temp_dir("roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let request = sample_request();
        write_panel_request_in(&dir, PANEL_HISTORY, request).unwrap();
        assert!(dir.join(format!("{PANEL_HISTORY}.request")).exists());
        assert_eq!(
            take_panel_request_in(&dir, PANEL_HISTORY).unwrap(),
            Some(request)
        );
        assert_eq!(
            take_panel_request_in(&dir, PANEL_HISTORY).unwrap(),
            None,
            "the claim is consumed"
        );
        assert!(
            dir_names(&dir).is_empty(),
            "no claimed or temp file may survive: {:?}",
            dir_names(&dir)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn take_without_a_request_reports_none() {
        let dir = temp_dir("absent");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(take_panel_request_in(&dir, PANEL_SETTINGS).unwrap(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_undecodable_request_is_reported_instead_of_swallowing_the_click() {
        let dir = temp_dir("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{PANEL_HISTORY}.request")), "toggle 1 2\n").unwrap();
        let error = take_panel_request_in(&dir, PANEL_HISTORY).unwrap_err();
        assert!(error.contains("not decodable"), "{error}");
        assert!(
            dir_names(&dir).is_empty(),
            "the claimed file must be removed: {:?}",
            dir_names(&dir)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_write_removes_its_temp_file() {
        let dir = temp_dir("writefail");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(
            dir.join(format!("{PANEL_HISTORY}.request"))
                .join("occupied"),
        )
        .unwrap();
        let error = write_panel_request_in(&dir, PANEL_HISTORY, sample_request()).unwrap_err();
        assert!(error.contains("cannot replace"), "{error}");
        assert_eq!(
            dir_names(&dir),
            vec![format!("{PANEL_HISTORY}.request")],
            "the temp file must not accumulate"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sweep_removes_only_stale_panel_temp_files() {
        let dir = temp_dir("sweep");
        std::fs::create_dir_all(&dir).unwrap();
        let stale_tmp = dir.join("history.request.tmp-1111");
        let stale_claimed = dir.join("history.request.claimed-2222");
        let fresh_tmp = dir.join("history.request.tmp-3333");
        let live = dir.join("history.request");
        let decoy = dir.join("notes.txt");
        for path in [&stale_tmp, &stale_claimed, &fresh_tmp, &live, &decoy] {
            std::fs::write(path, b"x").unwrap();
        }
        for path in [&stale_tmp, &stale_claimed] {
            let file = std::fs::File::options().write(true).open(path).unwrap();
            file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(600))
                .unwrap();
        }
        assert_eq!(sweep_panel_temp_files_in(&dir), 2);
        assert!(!stale_tmp.exists());
        assert!(!stale_claimed.exists());
        assert!(fresh_tmp.exists(), "a fresh temp belongs to a live writer");
        assert!(live.exists(), "a pending request is never swept");
        assert!(decoy.exists(), "an unrelated file is never swept");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_panic_fallback_file_carries_the_fatal_line() {
        let dir = temp_dir("panic");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(write_panic_fallback(
            &dir,
            "panic in thread main at x.rs:1:1: boom"
        ));
        let path = dir.join(format!("sysvitals-panic-{}.log", std::process::id()));
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "FATAL panic in thread main at x.rs:1:1: boom\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_toggle_with_a_window_always_closes() {
        for same_monitor in [true, false] {
            assert_eq!(
                resolve_panel_target(PanelAction::Toggle, same_monitor, true),
                PanelTarget::Close,
                "a toggle never rebuilds the window on another display"
            );
            assert_eq!(
                resolve_panel_target(PanelAction::Toggle, same_monitor, false),
                PanelTarget::Open
            );
            assert_eq!(
                resolve_panel_target(PanelAction::Open, same_monitor, false),
                PanelTarget::Open
            );
        }
        assert_eq!(
            resolve_panel_target(PanelAction::Open, true, true),
            PanelTarget::Focus
        );
        assert_eq!(
            resolve_panel_target(PanelAction::Open, false, true),
            PanelTarget::Move
        );
    }

    #[test]
    fn the_wait_cursor_escalates_to_terminating_a_deaf_host() {
        assert_eq!(
            resolve_wait_cursor(false, false, 0, 10_000, true),
            WaitCursorNext::Arrow
        );
        assert_eq!(
            resolve_wait_cursor(true, true, 99_999, 10_000, true),
            WaitCursorNext::Arrow
        );
        assert_eq!(
            resolve_wait_cursor(true, false, 9_999, 10_000, true),
            WaitCursorNext::Wait
        );
        assert_eq!(
            resolve_wait_cursor(true, false, 9_999, 10_000, false),
            WaitCursorNext::Wait
        );
        assert_eq!(
            resolve_wait_cursor(true, false, 10_000, 10_000, true),
            WaitCursorNext::Terminate,
            "a live but unresponsive host must be killed so the next click respawns it"
        );
        assert_eq!(
            resolve_wait_cursor(true, false, 60_000, 10_000, true),
            WaitCursorNext::Terminate
        );
        assert_eq!(
            resolve_wait_cursor(true, false, 10_000, 10_000, false),
            WaitCursorNext::Timeout
        );
    }

    #[test]
    fn the_history_click_decision_matches_the_launch_states() {
        assert_eq!(
            resolve_child_next(true, true, true, false, Some(100), 3_000),
            ChildNext::Request
        );
        assert_eq!(
            resolve_child_next(false, true, true, false, None, 3_000),
            ChildNext::Request
        );
        assert_eq!(
            resolve_child_next(false, true, false, false, Some(100), 3_000),
            ChildNext::Watch,
            "a one-shot launch is already on its way; the ready message will arrive"
        );
        assert_eq!(
            resolve_child_next(false, false, false, false, None, 3_000),
            ChildNext::Spawn
        );
        assert_eq!(
            resolve_child_next(false, false, true, false, Some(100), 3_000),
            ChildNext::Spawn
        );
        assert_eq!(
            resolve_child_next(true, false, false, false, Some(2_999), 3_000),
            ChildNext::Ignore,
            "inside the cold-start grace the window is not up yet"
        );
        assert_eq!(
            resolve_child_next(true, false, false, false, Some(3_000), 3_000),
            ChildNext::Request
        );
        assert_eq!(
            resolve_child_next(true, false, false, false, None, 3_000),
            ChildNext::Request
        );
        assert_eq!(
            resolve_child_next(true, false, false, true, Some(10), 3_000),
            ChildNext::Request,
            "a visible window ends the grace"
        );
        assert_eq!(
            resolve_child_next(true, false, true, false, Some(10), 3_000),
            ChildNext::Request,
            "a prewarmed host has no cold-start grace"
        );
    }

    #[test]
    fn respawn_backoff_caps_at_three_attempts() {
        let mut policy = RespawnPolicy::new();
        assert_eq!(policy.attempts(), 0);
        assert_eq!(policy.next_delay_ms(), Some(5_000));
        assert_eq!(policy.next_delay_ms(), Some(30_000));
        assert_eq!(policy.next_delay_ms(), Some(120_000));
        assert_eq!(policy.next_delay_ms(), None, "the cap must stop the loop");
        assert_eq!(policy.next_delay_ms(), None);
        assert_eq!(policy.attempts(), 3);
        policy.reset();
        assert_eq!(policy.attempts(), 0);
        assert_eq!(policy.next_delay_ms(), Some(5_000));
        assert_eq!(RESPAWN_DELAYS_MS, [5_000, 30_000, 120_000]);
    }
}
