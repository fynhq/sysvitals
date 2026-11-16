//! Child process management for settings, history, and panels.

use super::*;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProcessState {
    Alive,
    Exited,
    Failed,
}

unsafe fn process_state(handle: HANDLE) -> ProcessState {
    match WaitForSingleObject(handle, 0) {
        WAIT_TIMEOUT => ProcessState::Alive,
        WAIT_OBJECT_0 => ProcessState::Exited,
        _ => ProcessState::Failed,
    }
}

pub(crate) fn settings_child_alive() -> bool {
    let mut slot = SETTINGS_PROCESS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(raw) = *slot else {
        return false;
    };
    let handle = HANDLE(raw as *mut std::ffi::c_void);
    if handle.is_invalid() {
        *slot = None;
        return false;
    }
    match unsafe { process_state(handle) } {
        ProcessState::Alive => true,
        ProcessState::Exited => {
            let _ = unsafe { CloseHandle(handle) };
            *slot = None;
            false
        }
        ProcessState::Failed => {
            let _ = unsafe { CloseHandle(handle) };
            *slot = None;
            log_event("WARN", "settings process wait failed; handle dropped");
            false
        }
    }
}

pub(crate) fn terminate_settings(wait_for_exit: bool) -> bool {
    let raw = {
        let mut slot = SETTINGS_PROCESS.lock().unwrap_or_else(|e| e.into_inner());
        slot.take()
    };
    let Some(raw) = raw else {
        return false;
    };
    let handle = HANDLE(raw as *mut std::ffi::c_void);
    if handle.is_invalid() {
        return false;
    }
    match unsafe { process_state(handle) } {
        ProcessState::Exited => {
            let _ = unsafe { CloseHandle(handle) };
            return false;
        }
        ProcessState::Alive => {}
        ProcessState::Failed => {
            let _ = unsafe { CloseHandle(handle) };
            log_event("WARN", "settings process wait failed; handle dropped");
            return false;
        }
    }
    let _ = unsafe { TerminateProcess(handle, 0) };
    if wait_for_exit {
        let wait = unsafe { WaitForSingleObject(handle, 2_000) };
        if wait != WAIT_OBJECT_0 {
            log_event("WARN", "settings window did not exit within 2s");
        }
    }
    let _ = unsafe { CloseHandle(handle) };
    log_event("INFO", "settings window process closed by indicator");
    true
}

pub(crate) fn history_terminate() -> bool {
    let mut slot = HISTORY_PROCESS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(raw) = slot.take() else {
        return false;
    };
    let handle = HANDLE(raw as *mut std::ffi::c_void);
    if handle.is_invalid() {
        return false;
    }
    match unsafe { process_state(handle) } {
        ProcessState::Alive => {}
        ProcessState::Exited => {
            let _ = unsafe { CloseHandle(handle) };
            return false;
        }
        ProcessState::Failed => {
            log_event("WARN", "history process wait failed; handle dropped");
            let _ = unsafe { CloseHandle(handle) };
            return false;
        }
    }
    let _ = unsafe { TerminateProcess(handle, 0) };
    let _ = unsafe { CloseHandle(handle) };
    log_event("INFO", "history process closed by indicator");
    true
}

pub(crate) fn close_children() {
    let _ = history_terminate();
    let _ = terminate_settings(false);
}

pub(crate) unsafe fn init_child_job() {
    if CHILD_JOB.load(Ordering::Relaxed) != 0 {
        return;
    }
    let job = match CreateJobObjectW(None, windows::core::PCWSTR::null()) {
        Ok(job) => job,
        Err(e) => {
            log_event("WARN", &format!("child job creation failed: {e}"));
            return;
        }
    };
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let configured = SetInformationJobObject(
        job,
        JobObjectExtendedLimitInformation,
        &info as *const _ as *const std::ffi::c_void,
        std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
    );
    if let Err(e) = configured {
        log_event("WARN", &format!("child job configuration failed: {e}"));
        let _ = CloseHandle(job);
        return;
    }
    CHILD_JOB.store(job.0 as isize, Ordering::Relaxed);
}

pub(crate) unsafe fn assign_to_child_job(process: HANDLE) {
    let raw = CHILD_JOB.load(Ordering::Relaxed);
    if raw == 0 {
        return;
    }
    let job = HANDLE(raw as *mut std::ffi::c_void);
    if let Err(e) = AssignProcessToJobObject(job, process) {
        log_event("WARN", &format!("child job assignment failed: {e}"));
    }
}

pub(crate) fn spawn_process(
    exe_path: &str,
    dir: &str,
    args: Option<&str>,
) -> Result<HANDLE, String> {
    let exe_w = utf16(exe_path);
    let dir_w = utf16(dir);
    let mut cmdline = utf16(&match args {
        Some(a) => format!("\"{exe_path}\" {a}"),
        None => format!("\"{exe_path}\""),
    });
    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_FORCEOFFFEEDBACK,
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();
    let result = unsafe {
        CreateProcessW(
            windows::core::PCWSTR(exe_w.as_ptr()),
            Some(windows::core::PWSTR(cmdline.as_mut_ptr())),
            None,
            None,
            false,
            Default::default(),
            None,
            windows::core::PCWSTR(dir_w.as_ptr()),
            &si,
            &mut pi,
        )
    };
    match result {
        Ok(()) => {
            let _ = unsafe { CloseHandle(pi.hThread) };
            Ok(pi.hProcess)
        }
        Err(e) => Err(e.to_string()),
    }
}

pub(crate) fn launch_settings(hwnd: HWND) {
    let mut args = format!("--parent-pid {}", unsafe { GetCurrentProcessId() });
    if let Some(anchor) = window_anchor_logical(hwnd) {
        args.push_str(&format!(
            " --anchor {} {} {} {}",
            anchor[0], anchor[1], anchor[2], anchor[3]
        ));
        let monitor = window_monitor_id(hwnd);
        if monitor != 0 {
            args.push_str(&format!(" --anchor-monitor {monitor}"));
        }
    }
    let monitor = window_monitor_id(hwnd) as isize;
    let alive = settings_child_alive();
    if alive && SETTINGS_MONITOR.load(Ordering::Relaxed) == monitor {
        log_event("INFO", "settings launch skipped: already on this monitor");
        return;
    }
    if SETTINGS_LAUNCHING.swap(true, Ordering::SeqCst) {
        log_event(
            "INFO",
            "settings launch skipped: another launch is in flight",
        );
        return;
    }
    if alive {
        log_event("INFO", "settings: moving window to the clicked monitor");
    }
    let Some(dir) = current_exe_dir() else {
        SETTINGS_LAUNCHING.store(false, Ordering::SeqCst);
        log_event("WARN", "launch_settings: cannot determine exe directory");
        return;
    };
    let exe_path = format!("{dir}\\SysVitalsSettings.exe");
    if !std::path::Path::new(&exe_path).exists() {
        SETTINGS_LAUNCHING.store(false, Ordering::SeqCst);
        log_event("WARN", &format!("launch_settings: not found: {exe_path}"));
        return;
    }
    log_event(
        "INFO",
        &format!("launching SysVitalsSettings.exe (replace={alive}) {args}"),
    );
    settings_wait_cursor_begin();
    let sentinel_raw = sentinel_hwnd().0 as isize;
    let replace = alive;
    let spawn = std::thread::Builder::new()
        .name("sysvitals-settings".to_string())
        .spawn(move || {
            if replace {
                let _ = terminate_settings(true);
            }
            let started = Instant::now();
            match spawn_process(&exe_path, &dir, Some(&args)) {
                Ok(handle) => {
                    unsafe { assign_to_child_job(handle) };
                    if QUITTING.load(Ordering::Relaxed) {
                        let _ = unsafe { TerminateProcess(handle, 0) };
                        let _ = unsafe { CloseHandle(handle) };
                        log_event("INFO", "settings launch cancelled by quit");
                    } else {
                        set_settings_pid(handle);
                        *SETTINGS_PROCESS.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(handle.0 as isize);
                        SETTINGS_MONITOR.store(monitor, Ordering::Relaxed);
                        log_event(
                            "INFO",
                            &format!(
                                "launched SysVitalsSettings.exe in {}ms",
                                started.elapsed().as_millis()
                            ),
                        );
                    }
                }
                Err(e) => {
                    log_event(
                        "WARN",
                        &format!("launch_settings: CreateProcessW failed: {e}"),
                    );
                }
            }
            SETTINGS_LAUNCHING.store(false, Ordering::SeqCst);
            if sentinel_raw != 0 {
                let _ = unsafe {
                    PostMessageW(
                        Some(HWND(sentinel_raw as *mut std::ffi::c_void)),
                        WM_APP_SETTINGS_READY,
                        WPARAM(0),
                        LPARAM(0),
                    )
                };
            }
        });
    if spawn.is_err() {
        SETTINGS_LAUNCHING.store(false, Ordering::SeqCst);
        wait_cursor_clear();
        log_event("WARN", "launch_settings: worker thread spawn failed");
    }
}

pub(crate) fn history_busy() -> bool {
    history_child_alive() || HISTORY_LAUNCHING.load(Ordering::SeqCst)
}

static HISTORY_LAST_EXIT_CODE: AtomicU32 = AtomicU32::new(u32::MAX);
static HISTORY_RELAUNCHED_GENERATION: AtomicU32 = AtomicU32::new(0);

pub(crate) fn history_child_alive() -> bool {
    let mut slot = HISTORY_PROCESS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(raw) = *slot else {
        return false;
    };
    let handle = HANDLE(raw as *mut std::ffi::c_void);
    if handle.is_invalid() {
        *slot = None;
        return false;
    }
    match unsafe { process_state(handle) } {
        ProcessState::Alive => true,
        ProcessState::Exited => {
            let mut code = u32::MAX;
            let _ = unsafe { GetExitCodeProcess(handle, &mut code) };
            HISTORY_LAST_EXIT_CODE.store(code, Ordering::Relaxed);
            if !QUITTING.load(Ordering::Relaxed) {
                if code == 0 {
                    log_event(
                        "INFO",
                        "history resident exited after idling; it will be relaunched on demand",
                    );
                } else {
                    log_event(
                        "WARN",
                        &format!("history resident exited unexpectedly (exit code {code})"),
                    );
                }
            }
            let _ = unsafe { CloseHandle(handle) };
            *slot = None;
            false
        }
        ProcessState::Failed => {
            log_event("WARN", "history process wait failed; handle dropped");
            let _ = unsafe { CloseHandle(handle) };
            *slot = None;
            false
        }
    }
}

static HISTORY_STATE_WARNED: AtomicBool = AtomicBool::new(false);

fn warn_history_state(message: &str) {
    if !HISTORY_STATE_WARNED.swap(true, Ordering::Relaxed) {
        log_event("WARN", message);
    }
}

fn history_state_security() -> Option<(
    windows::Win32::Security::SECURITY_ATTRIBUTES,
    windows::Win32::Security::PSECURITY_DESCRIPTOR,
)> {
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    let sddl = utf16("D:(A;;GA;;;OW)");
    let mut descriptor = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            windows::core::PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
    };
    if converted.is_err() {
        warn_history_state("history state security descriptor failed; using the default DACL");
        return None;
    }
    Some((
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: windows::core::BOOL(0),
        },
        descriptor,
    ))
}

pub(crate) fn history_state_ptr() -> Option<*mut sysvitals_core::HistoryState> {
    HISTORY_STATE.with(|slot| {
        if slot.borrow().is_none() {
            let name = utf16(sysvitals_core::HISTORY_STATE_MAP);
            let size = std::mem::size_of::<sysvitals_core::HistoryState>();
            let security = history_state_security();
            let mapping_result = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    security.as_ref().map(|(sa, _)| sa as *const _),
                    PAGE_READWRITE,
                    0,
                    size as u32,
                    windows::core::PCWSTR(name.as_ptr()),
                )
            };
            let already_exists = unsafe { windows::Win32::Foundation::GetLastError() }
                == windows::Win32::Foundation::ERROR_ALREADY_EXISTS;
            if let Some((_, descriptor)) = security {
                unsafe {
                    let _ = windows::Win32::Foundation::LocalFree(Some(
                        windows::Win32::Foundation::HLOCAL(descriptor.0),
                    ));
                }
            }
            let mapping = match mapping_result {
                Ok(mapping) => mapping,
                Err(e) => {
                    warn_history_state(&format!("history state mapping failed: {e}"));
                    return None;
                }
            };
            let view = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, size) };
            if view.Value.is_null() {
                let _ = unsafe { CloseHandle(mapping) };
                warn_history_state("history state map view failed");
                return None;
            }
            let ptr = view.Value as *mut sysvitals_core::HistoryState;
            let usable = already_exists
                && unsafe {
                    let header = std::ptr::read_volatile(ptr);
                    header.magic == sysvitals_core::HISTORY_STATE_MAGIC
                        && header.version == sysvitals_core::HISTORY_STATE_VERSION
                };
            if !usable {
                unsafe { std::ptr::write_volatile(ptr, sysvitals_core::HistoryState::new()) };
            }
            HISTORY_STATE_WARNED.store(false, Ordering::Relaxed);
            *slot.borrow_mut() = Some((ptr, mapping));
        }
        slot.borrow().as_ref().map(|(ptr, _)| *ptr)
    })
}

pub(crate) fn history_snapshot() -> Option<sysvitals_core::HistoryState> {
    let ptr = history_state_ptr()?;
    Some(unsafe { sysvitals_core::HistoryState::read_consistent(ptr) })
}

pub(crate) fn history_close_request() {
    let Some(ptr) = history_state_ptr() else {
        return;
    };
    unsafe {
        sysvitals_core::HistoryState::write_request(ptr, false, None, 0);
    }
}

pub(crate) fn history_mark_stopped() {
    let Some(ptr) = history_state_ptr() else {
        return;
    };
    unsafe {
        sysvitals_core::HistoryState::clear(ptr);
    }
}

const WAIT_NONE: u32 = 0;
const WAIT_HISTORY: u32 = 1;
const WAIT_SETTINGS: u32 = 2;
const WAIT_TASKMGR: u32 = 3;

static WAIT_TARGET: AtomicU32 = AtomicU32::new(WAIT_NONE);
static WAIT_SINCE_MS: AtomicU64 = AtomicU64::new(0);
static SETTINGS_PID: AtomicU32 = AtomicU32::new(0);
const WAIT_CURSOR_TIMEOUT_MS: u64 = 10_000;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn wait_cursor_begin(target: u32) {
    WAIT_SINCE_MS.store(now_ms(), Ordering::Relaxed);
    WAIT_TARGET.store(target, Ordering::Relaxed);
    if let Ok(cursor) = unsafe { LoadCursorW(None, IDC_WAIT) } {
        unsafe { SetCursor(Some(cursor)) };
    }
}

pub(crate) fn history_wait_cursor_begin() {
    wait_cursor_begin(WAIT_HISTORY);
}

pub(crate) fn settings_wait_cursor_begin() {
    SETTINGS_PID.store(0, Ordering::Relaxed);
    wait_cursor_begin(WAIT_SETTINGS);
}

pub(crate) fn taskmgr_wait_cursor_begin() {
    wait_cursor_begin(WAIT_TASKMGR);
}

pub(crate) fn wait_cursor_active() -> bool {
    WAIT_TARGET.load(Ordering::Relaxed) != WAIT_NONE
}

pub(crate) fn wait_cursor_clear() {
    if WAIT_TARGET.swap(WAIT_NONE, Ordering::Relaxed) == WAIT_NONE {
        return;
    }
    let mut point = POINT::default();
    if unsafe { GetCursorPos(&mut point) }.is_err() {
        return;
    }
    let under = unsafe { WindowFromPoint(point) };
    if overlay_by_hwnd(under).is_none() {
        return;
    }
    if let Ok(cursor) = unsafe { LoadCursorW(None, IDC_ARROW) } {
        unsafe { SetCursor(Some(cursor)) };
    }
}

pub(crate) fn wait_cursor_reconcile() {
    let target = WAIT_TARGET.load(Ordering::Relaxed);
    if target == WAIT_NONE {
        return;
    }
    if launch_window_visible(target) {
        wait_cursor_clear();
        return;
    }
    let waited_ms = now_ms().saturating_sub(WAIT_SINCE_MS.load(Ordering::Relaxed));
    if waited_ms >= WAIT_CURSOR_TIMEOUT_MS {
        let what = match target {
            WAIT_HISTORY => "history popup",
            WAIT_SETTINGS => "settings window",
            WAIT_TASKMGR => "task manager window",
            _ => "window",
        };
        log_event(
            "WARN",
            &format!("launch feedback: {what} never became visible; clearing the wait cursor"),
        );
        wait_cursor_clear();
    }
}

fn launch_window_visible(target: u32) -> bool {
    match target {
        WAIT_HISTORY => {
            let visible = history_snapshot().is_some_and(|state| state.visible != 0);
            let failed = !history_child_alive() && !HISTORY_LAUNCHING.load(Ordering::SeqCst);
            visible || failed
        }
        WAIT_SETTINGS => {
            if SETTINGS_LAUNCHING.load(Ordering::SeqCst) {
                return false;
            }
            if !settings_child_alive() {
                return true;
            }
            let pid = SETTINGS_PID.load(Ordering::Relaxed);
            pid != 0 && visible_window_of_pid(pid)
        }
        WAIT_TASKMGR => visible_task_manager(),
        _ => true,
    }
}

fn visible_window_of_pid(pid: u32) -> bool {
    struct Search {
        pid: u32,
        found: bool,
    }
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let search = &mut *(lparam.0 as *mut Search);
        let mut owner = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut owner));
        if owner == search.pid && IsWindowVisible(hwnd).as_bool() {
            search.found = true;
            return BOOL(0);
        }
        BOOL(1)
    }
    let mut search = Search { pid, found: false };
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize));
    }
    search.found
}

fn visible_task_manager() -> bool {
    let class = utf16("TaskManagerWindow");
    match unsafe { FindWindowW(PCWSTR(class.as_ptr()), None) } {
        Ok(hwnd) => unsafe { IsWindowVisible(hwnd) }.as_bool(),
        Err(_) => false,
    }
}

pub(crate) fn set_settings_pid(handle: HANDLE) {
    SETTINGS_PID.store(unsafe { GetProcessId(handle) }, Ordering::Relaxed);
}

pub(crate) fn window_monitor_id(hwnd: HWND) -> i64 {
    if let Some(ov) = overlay_by_hwnd(hwnd) {
        return ov.monitor as i64;
    }
    unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST).0 as i64 }
}

pub(crate) fn window_anchor_logical(hwnd: HWND) -> Option<[f32; 4]> {
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return None;
    }
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
    Some([
        rect.left as f32 / scale,
        rect.top as f32 / scale,
        rect.right as f32 / scale,
        rect.bottom as f32 / scale,
    ])
}

pub(crate) fn history_open_request(hwnd: HWND) -> bool {
    let Some(ptr) = history_state_ptr() else {
        return false;
    };
    let Some(anchor) = window_anchor_logical(hwnd) else {
        return false;
    };
    unsafe {
        sysvitals_core::HistoryState::write_request(
            ptr,
            true,
            Some(anchor),
            window_monitor_id(hwnd),
        );
    }
    true
}

pub(crate) fn ensure_history_resident() {
    start_history_resident(true);
}

fn start_history_resident(clear_request: bool) {
    if history_busy() {
        return;
    }
    if history_state_ptr().is_none() {
        log_event("WARN", "history resident unavailable: state mapping failed");
        return;
    }
    if clear_request {
        history_mark_stopped();
    }
    let Some(dir) = current_exe_dir() else {
        log_event("WARN", "history launch: cannot determine exe directory");
        return;
    };
    let exe_path = format!("{dir}\\SysVitalsHistory.exe");
    if !std::path::Path::new(&exe_path).exists() {
        log_event("WARN", &format!("history launch: not found: {exe_path}"));
        return;
    }
    let parent_pid = unsafe { GetCurrentProcessId() };
    let args = format!("--resident --parent-pid {parent_pid}");
    log_event("INFO", &format!("launching SysVitalsHistory.exe {args}"));
    HISTORY_LAUNCHING.store(true, Ordering::SeqCst);
    let sentinel_raw = sentinel_hwnd().0 as isize;
    let spawn = std::thread::Builder::new()
        .name("sysvitals-history-launch".to_string())
        .spawn(move || {
            let started = Instant::now();
            match spawn_process(&exe_path, &dir, Some(&args)) {
                Ok(handle) => {
                    unsafe { assign_to_child_job(handle) };
                    if QUITTING.load(Ordering::Relaxed) {
                        let _ = unsafe { TerminateProcess(handle, 0) };
                        let _ = unsafe { CloseHandle(handle) };
                        log_event("INFO", "history launch cancelled by quit");
                    } else {
                        *HISTORY_PROCESS.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(handle.0 as isize);
                        log_event(
                            "INFO",
                            &format!(
                                "launched SysVitalsHistory.exe in {}ms",
                                started.elapsed().as_millis()
                            ),
                        );
                    }
                }
                Err(e) => {
                    log_event("WARN", &format!("history launch failed: {e}"));
                }
            }
            HISTORY_LAUNCHING.store(false, Ordering::SeqCst);
            let _ = unsafe {
                PostMessageW(
                    Some(HWND(sentinel_raw as *mut std::ffi::c_void)),
                    WM_APP_HISTORY_READY,
                    WPARAM(0),
                    LPARAM(0),
                )
            };
        });
    if let Err(e) = spawn {
        HISTORY_LAUNCHING.store(false, Ordering::SeqCst);
        log_event("WARN", &format!("history launch thread failed: {e}"));
    }
}

// Reap a dead resident and reconcile its last request; a pending click must relaunch instead of being dropped.
pub(crate) fn sync_history_state() {
    wait_cursor_reconcile();
    let alive = history_child_alive();
    if alive || HISTORY_LAUNCHING.load(Ordering::SeqCst) {
        return;
    }
    let Some(state) = history_snapshot() else {
        return;
    };
    if state.desired_open != 0 {
        let raced_exit =
            state.click_raced_idle_exit(HISTORY_LAST_EXIT_CODE.load(Ordering::Relaxed) == 0);
        if raced_exit && HISTORY_RELAUNCHED_GENERATION.load(Ordering::Relaxed) != state.generation {
            HISTORY_RELAUNCHED_GENERATION.store(state.generation, Ordering::Relaxed);
            log_event(
                "INFO",
                "history: click raced the idle exit; relaunching with the pending request",
            );
            start_history_resident(false);
            return;
        }
        history_mark_stopped();
        return;
    }
    if state.visible != 0 {
        history_mark_stopped();
    }
}

fn current_exe_path() -> Option<String> {
    let mut buf = vec![0u16; 512];
    loop {
        let n = unsafe { GetModuleFileNameW(None, &mut buf) } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            return Some(String::from_utf16_lossy(&buf[..n]));
        }
        if buf.len() >= 32768 {
            return None;
        }
        buf.resize(buf.len() * 2, 0);
    }
}

pub(crate) fn current_exe_dir() -> Option<String> {
    let path = current_exe_path()?;
    path.rfind(['\\', '/']).map(|i| path[..i].to_string())
}

pub(crate) fn restart_self() {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let Some(exe) = current_exe_path() else {
        log_event("WARN", "restart_self: cannot determine exe path");
        return;
    };
    let Some(system) = sysvitals_support::system_directory() else {
        log_event(
            "WARN",
            "restart_self: cannot determine the Windows system directory",
        );
        return;
    };

    let cmd = system.join("cmd.exe");
    let ping = system.join("ping.exe");
    let script = format!(
        "\"{}\" -n 3 127.0.0.1 >nul & start \"\" \"%SYSVITALS_RESTART_EXE%\"",
        ping.display()
    );
    let script = format!("\"{script}\"");
    let spawned = Command::new(cmd)
        .arg("/c")
        .raw_arg(script)
        .env("SYSVITALS_RESTART_EXE", &exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();

    match spawned {
        Ok(_) => {
            log_event("INFO", "restart_self: scheduling relaunch and exiting");
            close_children();
            unsafe { shutdown_and_quit() };
        }
        Err(e) => log_event(
            "WARN",
            &format!("restart_self: relaunch failed ({e}), keeping the running instance"),
        ),
    }
}
