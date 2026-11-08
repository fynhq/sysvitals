//! children: split out of `win` (see `win` for the module map).

use super::*;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};

// ------------------------------------------------------- child process launch

/// Whether the Settings child is still running. Reaps the handle when the
/// process has exited. Never blocks the message thread.
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
    match unsafe { WaitForSingleObject(handle, 0) } {
        WAIT_TIMEOUT => true,
        WAIT_OBJECT_0 => {
            let _ = unsafe { CloseHandle(handle) };
            *slot = None;
            false
        }
        _ => {
            // WAIT_FAILED or an unexpected state: the handle can no
            // longer be trusted. Close it and let the next click relaunch.
            let _ = unsafe { CloseHandle(handle) };
            *slot = None;
            log_event("WARN", "settings process wait failed; handle dropped");
            false
        }
    }
}

/// Terminate the Settings child if it is still running. The handle is
/// closed and the slot cleared either way. `wait_for_exit` is only used
/// from the background launch worker (a replacement must wait for the old
/// instance's single-instance mutex to be released); the UI paths pass
/// `false` so closing never blocks the message loop.
pub(crate) fn terminate_settings(wait_for_exit: bool) -> bool {
    // Take the handle out of the shared slot before any blocking wait:
    // the UI thread must be able to call `settings_child_alive()` while a
    // replacement worker waits up to 2s for the old process object.
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
    // Already exited: reap quietly, treat as "nothing to close".
    match unsafe { WaitForSingleObject(handle, 0) } {
        WAIT_OBJECT_0 => {
            let _ = unsafe { CloseHandle(handle) };
            return false;
        }
        WAIT_TIMEOUT => {}
        _ => {
            let _ = unsafe { CloseHandle(handle) };
            log_event("WARN", "settings process wait failed; handle dropped");
            return false;
        }
    }
    let _ = unsafe { TerminateProcess(handle, 0) };
    if wait_for_exit {
        // The process object is signaled only after the child's handles
        // are closed — including its single-instance mutex. Without this,
        // a replacement launched immediately could see the dying instance
        // and exit. This runs on a worker thread, never the UI thread,
        // and the slot is already empty so the UI never waits on us.
        let wait = unsafe { WaitForSingleObject(handle, 2_000) };
        if wait != WAIT_OBJECT_0 {
            log_event("WARN", "settings window did not exit within 2s");
        }
    }
    let _ = unsafe { CloseHandle(handle) };
    log_event("INFO", "settings window process closed by indicator");
    true
}

/// Terminate the resident history process. Called only on restart/quit:
/// a normal popup close is a shared-state request that only removes the
/// window, leaving the framework warm.
pub(crate) fn history_terminate() -> bool {
    let mut slot = HISTORY_PROCESS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(raw) = slot.take() else {
        return false;
    };
    let handle = HANDLE(raw as *mut std::ffi::c_void);
    if handle.is_invalid() {
        return false;
    }
    match unsafe { WaitForSingleObject(handle, 0) } {
        WAIT_TIMEOUT => {}
        WAIT_OBJECT_0 => {
            let _ = unsafe { CloseHandle(handle) };
            return false;
        }
        _ => {
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

/// Close both child panels on restart/quit. Leaving them alive makes a
/// restarted instance open a second History window (its slot starts
/// empty) and leaves Settings orphaned after Quit.
pub(crate) fn close_children() {
    let _ = history_terminate();
    // Quit/restart: the relaunch is delayed by the caller, so there is no
    // single-instance race to wait out here; never block the UI thread.
    let _ = terminate_settings(false);
}

/// Create the kill-on-close job that owns every panel child. Keeping the
/// job handle for the process lifetime means an indicator crash, a quit
/// during a slow `CreateProcessW`, or an explorer restart can never leave
/// an orphan Settings/History process behind.
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

/// Put a freshly spawned panel into the kill-on-close job. Failure is
/// non-fatal: the explicit slot termination still covers the normal
/// quit/restart paths, and the child's `--parent-pid` covers mac-style
/// crash cleanup where it applies.
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

/// Spawn `exe_path` with the given argv tail. Returns the process handle
/// (the thread handle is closed here). Used by the background Settings
/// and resident-history launchers; never call this from the UI thread.
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
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();
    // CreateProcessW launches the sibling directly: the Shell execute path
    // used before can pump messages and adds shell lookup overhead. The
    // child is a GUI-subsystem binary, so no console/STARTUPINFO window
    // fields are needed.
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

/// Launch the external settings process (`SysVitalsSettings.exe`) on a
/// worker thread. `CreateProcessW` has been measured at hundreds of
/// milliseconds to several seconds on loaded hosts, and replacing a
/// window on another monitor additionally waits for the old instance's
/// single-instance mutex. Neither may block the indicator message loop.
pub(crate) fn launch_settings(hwnd: HWND) {
    let mut args = format!("--parent-pid {}", unsafe { GetCurrentProcessId() });
    if let Some(anchor) = window_anchor_logical(hwnd) {
        args.push_str(&format!(
            " --anchor {} {} {} {}",
            anchor[0], anchor[1], anchor[2], anchor[3]
        ));
        // The rectangle alone is ambiguous on mixed-DPI setups: GPUI's
        // logical space is scaled per display, so the panel also gets the
        // native display handle to select the target monitor explicitly.
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
        // GPUI cannot move an existing window, so a Settings click on
        // another screen closes the window and reopens it there. Every
        // setting is persisted on change, so nothing is lost except
        // transient scroll/focus state.
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
    let sentinel_raw = sentinel_hwnd().0 as isize;
    let replace = alive;
    let spawn = std::thread::Builder::new()
        .name("sysvitals-settings".to_string())
        .spawn(move || {
            if replace {
                // Must run on the worker: waits up to 2s for the old
                // instance's named mutex to be released.
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
                        *SETTINGS_PROCESS.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(handle.0 as isize);
                        // Record the monitor only after the process is
                        // actually alive; a failed replacement must not
                        // make a later click on the new monitor a no-op.
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
        log_event("WARN", "launch_settings: worker thread spawn failed");
    }
}

/// Whether the resident history framework is running or being launched.
pub(crate) fn history_busy() -> bool {
    history_child_alive() || HISTORY_LAUNCHING.load(Ordering::SeqCst)
}

/// Reap the resident history process if it exited (crash, manual kill).
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
    match unsafe { WaitForSingleObject(handle, 0) } {
        WAIT_TIMEOUT => true,
        WAIT_OBJECT_0 => {
            if !QUITTING.load(Ordering::Relaxed) {
                let mut code = 0u32;
                let _ = unsafe { GetExitCodeProcess(handle, &mut code) };
                log_event(
                    "WARN",
                    &format!("history resident exited unexpectedly (exit code {code})"),
                );
            }
            let _ = unsafe { CloseHandle(handle) };
            *slot = None;
            false
        }
        _ => {
            log_event("WARN", "history process wait failed; handle dropped");
            let _ = unsafe { CloseHandle(handle) };
            *slot = None;
            false
        }
    }
}

/// One-shot latch for shared-memory failures: the history sync timer calls
/// this path four times a second, so a persistent mapping failure must not
/// flood the daily log.
static HISTORY_STATE_WARNED: AtomicBool = AtomicBool::new(false);

fn warn_history_state(message: &str) {
    if !HISTORY_STATE_WARNED.swap(true, Ordering::Relaxed) {
        log_event("WARN", message);
    }
}

/// Create a security descriptor for the shared history block that grants the
/// creating user full access. `CreateFileMappingW` would otherwise apply the
/// process token's default DACL; making the owner-rights grant explicit keeps
/// the block usable after a token/integrity change and documents the
/// single-user boundary. Returns `None` (default DACL) if SDDL conversion
/// fails, which is no worse than the previous behaviour.
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

/// Create (or reopen) the shared history state block. Kept alive for the
/// process lifetime; the resident child maps it by name.
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
            // Capture before any other Win32 call can overwrite it.
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
            // An existing mapping belongs to a still-live resident child or
            // to a dead process's leftovers. Only clear it when the header is
            // not ours; a live block keeps its request/visibility flags.
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

/// Ask the resident child to close. The generation moves with the
/// request, so the resident loop can tell this close apart from the
/// generation that opened the window and will not reopen it while a
/// newer click is pending.
pub(crate) fn history_close_request() {
    let Some(ptr) = history_state_ptr() else {
        return;
    };
    unsafe {
        sysvitals_core::HistoryState::write_request(ptr, false, None, 0);
    }
}

/// Clear both flags after a resident-child death (or before starting a
/// fresh frame) so a stale request/visibility cannot make the next click
/// a no-op toggle-close or auto-open a window nobody asked for.
pub(crate) fn history_mark_stopped() {
    let Some(ptr) = history_state_ptr() else {
        return;
    };
    unsafe {
        sysvitals_core::HistoryState::clear(ptr);
    }
}

/// Native display handle (HMONITOR) hosting a window, carried as `i64`
/// through the shared history state and the settings command line.
/// The overlay registry is refreshed every move tick, so it is the
/// preferred source; a live query covers windows not in the registry.
pub(crate) fn window_monitor_id(hwnd: HWND) -> i64 {
    if let Some(ov) = overlay_by_hwnd(hwnd) {
        return ov.monitor as i64;
    }
    unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST).0 as i64 }
}

/// A window's rect converted to GPUI global logical pixels (physical
/// pixels divided by the window's own monitor DPI). Used as the history
/// popup anchor and as the settings window's monitor hint.
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

/// Publish an open request: the indicator's rect plus `desired_open=1`
/// under one generation write. `GetWindowRect` reports physical pixels;
/// the child lays its window out in GPUI's global logical pixel space
/// (top-left origin, y down), so scale by the window's own monitor DPI.
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

/// Start the resident history framework on a worker thread. `CreateProcessW`
/// has been measured at 190 ms – 5.4 s on loaded hosts; doing it here keeps
/// the indicator's message loop responsive. The child has no window until
/// the user opens the chart, so this only pays the GPUI/DirectX warm-up.
pub(crate) fn ensure_history_resident() {
    if history_busy() {
        return;
    }
    if history_state_ptr().is_none() {
        log_event("WARN", "history resident unavailable: state mapping failed");
        return;
    }
    // A fresh resident starts with no window. Clear any request left by a
    // previous child (for example a self-dismissed popup whose
    // `desired_open` stays 1 by design) so merely starting the framework
    // never opens a chart no click asked for. A click immediately
    // publishes a new request after this call.
    history_mark_stopped();
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
                        // The indicator began quitting while CreateProcessW
                        // was in flight. The job object would reap it on
                        // process exit, but terminating here also avoids
                        // briefly showing a panel that no click asked for.
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
            // Wake the UI so the launch handle converges without waiting for
            // the 250 ms sync timer. A destroyed sentinel makes this a no-op.
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

/// Reap a dead resident process and clear a request it left behind, so the
/// next click is not mistaken for a toggle-close of a popup that is gone.
pub(crate) fn sync_history_state() {
    let alive = history_child_alive();
    if alive || HISTORY_LAUNCHING.load(Ordering::SeqCst) {
        return;
    }
    // Do not bump the generation four times a second while idle; only
    // clear a state that a dead child actually left behind.
    let stale =
        history_snapshot().is_some_and(|state| state.desired_open != 0 || state.visible != 0);
    if stale {
        history_mark_stopped();
    }
}

/// Full path of the running executable. The buffer grows until
/// `GetModuleFileNameW` reports it fit, so a deep install path is not
/// silently truncated at a fixed 1024 UTF-16 units.
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

/// Best-effort: return the directory containing the current executable.
pub(crate) fn current_exe_dir() -> Option<String> {
    let path = current_exe_path()?;
    path.rfind(['\\', '/']).map(|i| path[..i].to_string())
}

/// Restart the overlay: schedule a delayed relaunch of the current exe
/// (after the single-instance mutex is released on exit), then exit.
pub(crate) fn restart_self() {
    /// `CREATE_NO_WINDOW`; passed as a raw flag because std's
    /// `CommandExt::creation_flags` takes a `u32`.
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

    // Launch through cmd only for the `start` builtin, but resolve both
    // cmd.exe and ping.exe by absolute path. `timeout.exe` is deliberately
    // not used: it fails immediately without console stdin (and a bare name
    // can resolve to Git's MSYS timeout first), which would race the
    // single-instance mutex. `ping` provides the delay without a console.
    let cmd = system.join("cmd.exe");
    let ping = system.join("ping.exe");
    let script = format!(
        "\"{}\" -n 3 127.0.0.1 >nul & start \"\" \"%SYSVITALS_RESTART_EXE%\"",
        ping.display()
    );
    let spawned = Command::new(cmd)
        .arg("/c")
        // cmd.exe does not use C-runtime quoting; append the script verbatim so
        // the inner quotes reach `start` intact.
        .raw_arg(script)
        // Pass the path in the environment, not interpolated into the command
        // line: cmd expands `%VAR%` once, so a `%` in a legal install path is
        // not reinterpreted.
        .env("SYSVITALS_RESTART_EXE", &exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();

    match spawned {
        Ok(_) => {
            log_event("INFO", "restart_self: scheduling relaunch and exiting");
            // The new instance starts with empty child slots; close ours so it
            // cannot open a second History window next to the old one.
            close_children();
            unsafe { shutdown_and_quit() };
        }
        Err(e) => log_event(
            "WARN",
            &format!("restart_self: relaunch failed ({e}), keeping the running instance"),
        ),
    }
}
