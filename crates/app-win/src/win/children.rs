//! Child process management: a resident history host and one-shot settings.

use super::*;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};

use sysvitals_core::cli::AnchorRect;
use sysvitals_support::{
    clear_panel_request, write_panel_request, PanelAction, PANEL_HISTORY, PANEL_SETTINGS,
};

const PANEL_EXE: &str = "SysVitalsPanel.exe";

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProcessState {
    Alive,
    Exited,
    Failed,
}

struct Child {
    handle: Option<isize>,
    pid: u32,
    launching: bool,
    prewarm: bool,
    shown: bool,
    started_at: Option<Instant>,
}

impl Child {
    const fn new() -> Self {
        Self {
            handle: None,
            pid: 0,
            launching: false,
            prewarm: false,
            shown: false,
            started_at: None,
        }
    }

    fn take(&mut self) -> Option<isize> {
        let handle = self.handle.take();
        self.pid = 0;
        self.launching = false;
        self.prewarm = false;
        self.shown = false;
        self.started_at = None;
        handle
    }
}

static SETTINGS: Mutex<Child> = Mutex::new(Child::new());
static HISTORY: Mutex<Child> = Mutex::new(Child::new());

fn lock(slot: &'static Mutex<Child>) -> MutexGuard<'static, Child> {
    slot.lock().unwrap_or_else(|error| error.into_inner())
}

unsafe fn process_state(handle: HANDLE) -> ProcessState {
    match WaitForSingleObject(handle, 0) {
        WAIT_TIMEOUT => ProcessState::Alive,
        WAIT_OBJECT_0 => ProcessState::Exited,
        _ => ProcessState::Failed,
    }
}

fn child_alive(child: &mut Child, label: &str) -> bool {
    let Some(raw) = child.handle else {
        return false;
    };
    let handle = HANDLE(raw as *mut std::ffi::c_void);
    if handle.is_invalid() {
        child.take();
        return false;
    }
    match unsafe { process_state(handle) } {
        ProcessState::Alive => true,
        ProcessState::Exited => {
            let mut code = 0u32;
            let read = unsafe { GetExitCodeProcess(handle, &mut code) }.is_ok();
            let _ = unsafe { CloseHandle(handle) };
            child.take();
            if label == "settings" && read && code == 0 {
                log_event("INFO", "settings exited with code 0x0");
            } else {
                log_event(
                    "WARN",
                    &if read {
                        format!("{label} exited unexpectedly with code {code:#x}")
                    } else {
                        format!("{label} exited unexpectedly; exit code unavailable")
                    },
                );
            }
            false
        }
        ProcessState::Failed => {
            let _ = unsafe { CloseHandle(handle) };
            child.take();
            log_event(
                "WARN",
                &format!("{label} process wait failed; handle dropped"),
            );
            false
        }
    }
}

fn terminate(slot: &'static Mutex<Child>, wait_for_exit: bool, label: &str) -> bool {
    let raw = lock(slot).take();
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
            log_event(
                "WARN",
                &format!("{label} process wait failed; handle dropped"),
            );
            return false;
        }
    }
    let _ = unsafe { TerminateProcess(handle, 0) };
    if wait_for_exit {
        let wait = unsafe { WaitForSingleObject(handle, 2_000) };
        if wait != WAIT_OBJECT_0 {
            log_event("WARN", &format!("{label} window did not exit within 2s"));
        }
    }
    let _ = unsafe { CloseHandle(handle) };
    log_event("INFO", &format!("{label} process closed by indicator"));
    true
}

pub(crate) fn close_children() {
    let _ = terminate(&HISTORY, false, "history");
    let _ = terminate(&SETTINGS, false, "settings");
}

static PREWARM_POLICY: Mutex<Option<RespawnPolicy>> = Mutex::new(None);
static PREWARM_DUE_MS: AtomicU64 = AtomicU64::new(0);

fn schedule_prewarm_retry() {
    let mut guard = PREWARM_POLICY
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let policy = guard.get_or_insert_with(RespawnPolicy::new);
    match policy.next_delay_ms() {
        Some(delay) => {
            PREWARM_DUE_MS.store(now_ms().saturating_add(delay).max(1), Ordering::Relaxed);
            log_event(
                "WARN",
                &format!(
                    "history host exited; re-prewarming in {delay}ms (attempt {})",
                    policy.attempts()
                ),
            );
        }
        None => log_event(
            "WARN",
            "history host keeps exiting; automatic restarts stopped until the app restarts",
        ),
    }
}

pub(crate) fn prewarm_tick() {
    let due = PREWARM_DUE_MS.load(Ordering::Relaxed);
    if due == 0 || now_ms() < due {
        return;
    }
    PREWARM_DUE_MS.store(0, Ordering::Relaxed);
    prewarm_history();
}

pub(crate) fn reap_children() {
    let (was_alive, was_prewarm) = {
        let child = lock(&HISTORY);
        (child.handle.is_some(), child.prewarm)
    };
    let _ = {
        let mut child = lock(&SETTINGS);
        child_alive(&mut child, "settings")
    };
    let alive = {
        let mut child = lock(&HISTORY);
        child_alive(&mut child, "history")
    };
    if was_alive && was_prewarm && !alive {
        schedule_prewarm_retry();
    }
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

fn anchor_args(hwnd: HWND, with_monitor: bool) -> String {
    let mut args = format!("--parent-pid {}", unsafe { GetCurrentProcessId() });
    if let Some(anchor) = window_anchor_logical(hwnd) {
        args.push_str(&format!(
            " --anchor {} {} {} {}",
            anchor[0], anchor[1], anchor[2], anchor[3]
        ));
        if with_monitor {
            let monitor = window_monitor_id(hwnd);
            if monitor != 0 {
                args.push_str(&format!(" --anchor-monitor {monitor}"));
            }
        }
    }
    args
}

fn spawn_child(
    slot: &'static Mutex<Child>,
    label: &'static str,
    exe_name: &'static str,
    args: String,
    ready_msg: u32,
    prewarm: bool,
) -> bool {
    {
        let mut child = lock(slot);
        if child.launching {
            log_event(
                "INFO",
                &format!("{label} launch skipped: another launch is in flight"),
            );
            return false;
        }
        child.launching = true;
        child.prewarm = prewarm;
    }
    let Some(dir) = current_exe_dir() else {
        lock(slot).launching = false;
        log_event(
            "WARN",
            &format!("{label} launch: cannot determine exe directory"),
        );
        return false;
    };
    let exe_path = format!("{dir}\\{exe_name}");
    if !std::path::Path::new(&exe_path).exists() {
        lock(slot).launching = false;
        log_event("WARN", &format!("{label} launch: not found: {exe_path}"));
        return false;
    }
    log_event("INFO", &format!("launching {exe_name} {args}"));
    let sentinel_raw = sentinel_hwnd().0 as isize;
    let spawned = std::thread::Builder::new()
        .name(format!("sysvitals-{label}"))
        .spawn(move || {
            let started = Instant::now();
            match spawn_process(&exe_path, &dir, Some(&args)) {
                Ok(handle) => {
                    unsafe { assign_to_child_job(handle) };
                    if QUITTING.load(Ordering::Relaxed) {
                        let _ = unsafe { TerminateProcess(handle, 0) };
                        let _ = unsafe { CloseHandle(handle) };
                        log_event("INFO", &format!("{label} launch cancelled by quit"));
                    } else {
                        let pid = unsafe { GetProcessId(handle) };
                        let mut child = lock(slot);
                        child.handle = Some(handle.0 as isize);
                        child.pid = pid;
                        child.prewarm = prewarm;
                        child.shown = false;
                        child.started_at = Some(Instant::now());
                        child.launching = false;
                        drop(child);
                        log_event(
                            "INFO",
                            &format!("launched {exe_name} in {}ms", started.elapsed().as_millis()),
                        );
                    }
                }
                Err(e) => {
                    log_event("WARN", &format!("{label} launch failed: {e}"));
                }
            }
            lock(slot).launching = false;
            if sentinel_raw != 0 {
                let _ = unsafe {
                    PostMessageW(
                        Some(HWND(sentinel_raw as *mut std::ffi::c_void)),
                        ready_msg,
                        WPARAM(0),
                        LPARAM(0),
                    )
                };
            }
        });
    if spawned.is_err() {
        lock(slot).launching = false;
        log_event("WARN", &format!("{label} launch thread failed"));
        return false;
    }
    true
}

fn panel_request(hwnd: HWND, panel: &str, action: PanelAction) -> Result<(), String> {
    let anchor = window_anchor_logical(hwnd).map(|[left, top, right, bottom]| AnchorRect {
        left,
        top,
        right,
        bottom,
    });
    let monitor = window_monitor_id(hwnd);
    write_panel_request(
        panel,
        action,
        anchor,
        (monitor != 0).then_some(monitor as u64),
    )
}

pub(crate) fn prewarm_history() {
    clear_panel_request(PANEL_SETTINGS);
    clear_panel_request(PANEL_HISTORY);
    let args = format!("--history --parent-pid {} --prewarm", unsafe {
        GetCurrentProcessId()
    });
    let _ = spawn_child(
        &HISTORY,
        "history",
        PANEL_EXE,
        args,
        WM_APP_HISTORY_READY,
        true,
    );
}

pub(crate) fn launch_settings(hwnd: HWND) {
    let (alive, launching) = {
        let mut child = lock(&SETTINGS);
        let alive = child_alive(&mut child, "settings");
        (alive, child.launching)
    };
    if alive {
        settings_wait_cursor_begin();
        if let Err(error) = panel_request(hwnd, PANEL_SETTINGS, PanelAction::Open) {
            log_event("WARN", &format!("settings: request failed: {error}"));
            wait_cursor_clear();
        }
        return;
    }
    if launching {
        return;
    }
    clear_panel_request(PANEL_SETTINGS);
    settings_wait_cursor_begin();
    if !spawn_child(
        &SETTINGS,
        "settings",
        PANEL_EXE,
        format!("--settings {}", anchor_args(hwnd, true)),
        WM_APP_SETTINGS_READY,
        false,
    ) {
        wait_cursor_clear();
    }
}

fn launch_history(hwnd: HWND) {
    clear_panel_request(PANEL_HISTORY);
    history_wait_cursor_begin();
    if !spawn_child(
        &HISTORY,
        "history",
        PANEL_EXE,
        format!("--history {}", anchor_args(hwnd, true)),
        WM_APP_HISTORY_READY,
        false,
    ) {
        wait_cursor_clear();
    }
}

pub(crate) fn toggle_history(hwnd: HWND) {
    let (pid, launching, prewarm, shown, started_at) = {
        let mut child = lock(&HISTORY);
        let alive = child_alive(&mut child, "history");
        (
            if alive { child.pid } else { 0 },
            child.launching,
            child.prewarm,
            child.shown,
            child.started_at,
        )
    };
    if launching {
        if prewarm {
            if let Err(error) = panel_request(hwnd, PANEL_HISTORY, PanelAction::Toggle) {
                log_event("WARN", &format!("history: request failed: {error}"));
            }
        } else {
            log_event(
                "INFO",
                "history launch skipped: the popup is still starting",
            );
        }
        return;
    }
    if pid == 0 {
        launch_history(hwnd);
        return;
    }
    if !shown && !prewarm && started_at.is_some_and(|at| at.elapsed() < Duration::from_secs(3)) {
        log_event(
            "INFO",
            "history: popup is still starting; ignoring the click",
        );
        return;
    }
    if let Err(error) = panel_request(hwnd, PANEL_HISTORY, PanelAction::Toggle) {
        log_event("WARN", &format!("history: request failed: {error}"));
    }
}

const WAIT_NONE: u32 = 0;
const WAIT_HISTORY: u32 = 1;
const WAIT_SETTINGS: u32 = 2;
const WAIT_TASKMGR: u32 = 3;

static WAIT_TARGET: AtomicU32 = AtomicU32::new(WAIT_NONE);
static WAIT_SINCE_MS: AtomicU64 = AtomicU64::new(0);
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
    let waited_ms = now_ms().saturating_sub(WAIT_SINCE_MS.load(Ordering::Relaxed));
    let alive = match target {
        WAIT_HISTORY => child_alive(&mut lock(&HISTORY), "history"),
        WAIT_SETTINGS => child_alive(&mut lock(&SETTINGS), "settings"),
        _ => false,
    };
    match resolve_wait_cursor(
        target != WAIT_NONE,
        launch_window_visible(target),
        waited_ms,
        WAIT_CURSOR_TIMEOUT_MS,
        alive,
    ) {
        WaitCursorNext::Wait => {}
        WaitCursorNext::Arrow => wait_cursor_clear(),
        WaitCursorNext::Timeout => {
            log_event(
                "WARN",
                &format!(
                    "launch feedback: {} never became visible; clearing the wait cursor",
                    target_label(target)
                ),
            );
            wait_cursor_clear();
        }
        WaitCursorNext::Terminate => {
            log_event(
                "WARN",
                &format!(
                    "launch feedback: {} is still running but never showed a window; \
                     terminating it so the next click relaunches",
                    target_label(target)
                ),
            );
            match target {
                WAIT_HISTORY => {
                    let _ = terminate(&HISTORY, false, "history");
                }
                WAIT_SETTINGS => {
                    let _ = terminate(&SETTINGS, false, "settings");
                }
                _ => {}
            }
            wait_cursor_clear();
        }
    }
}

fn target_label(target: u32) -> &'static str {
    match target {
        WAIT_HISTORY => "history popup",
        WAIT_SETTINGS => "settings window",
        WAIT_TASKMGR => "task manager window",
        _ => "window",
    }
}

fn launch_window_visible(target: u32) -> bool {
    match target {
        WAIT_HISTORY => child_window_visible(&HISTORY, "history"),
        WAIT_SETTINGS => child_window_visible(&SETTINGS, "settings"),
        WAIT_TASKMGR => visible_task_manager(),
        _ => true,
    }
}

fn child_window_visible(slot: &'static Mutex<Child>, label: &str) -> bool {
    let (pid, launching) = {
        let mut child = lock(slot);
        let alive = child_alive(&mut child, label);
        (if alive { child.pid } else { 0 }, child.launching)
    };
    if launching {
        return false;
    }
    if pid == 0 {
        return true;
    }
    let visible = visible_window_of_pid(pid);
    if visible {
        let mut child = lock(slot);
        if child.pid == pid {
            child.shown = true;
        }
    }
    visible
}

fn visible_window_of_pid(pid: u32) -> bool {
    struct Search {
        pid: u32,
        found: bool,
    }
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `&mut Search` handed to EnumWindows below; the
        // call is synchronous, so it outlives every invocation of this callback.
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
        Err(e) => {
            log_event(
                "WARN",
                &format!("restart_self: relaunch failed ({e}), keeping the running instance"),
            );
        }
    }
}
