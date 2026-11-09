//! platform: split out of the crate root (see `main.rs` for the module map).

use super::*;

// ---------------------------------------------------------------------------
// Platform gate for the layout section
// ---------------------------------------------------------------------------

/// Whether the indicator's layout section is offered on this machine: the
/// far-left anchor exists only on Windows 11 with a centered taskbar. Reads
/// the very same HKCU value as the overlay (`sysvitals_support::taskbar_al`)
/// so the two can never disagree.
#[cfg(windows)]
pub(crate) fn layout_available() -> bool {
    taskbar_al() == Some(1)
}

#[cfg(not(windows))]
pub(crate) fn layout_available() -> bool {
    false
}

/// Whether the taskbar is currently docked left or right. On such a bar the
/// margin value offsets the indicator from the bar's top edge, and that knob
/// must be editable even where the anchor section is not offered, so the
/// settings window probes the same app-bar position the overlay reads.
/// `false` anywhere the probe fails (macOS, hidden taskbar, pre-registered
/// shell) keeps the previous UI surface untouched.
#[cfg(windows)]
pub(crate) fn taskbar_is_vertical() -> bool {
    use windows::Win32::UI::Shell::{
        SHAppBarMessage, ABE_LEFT, ABE_RIGHT, ABM_GETTASKBARPOS, APPBARDATA,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    let mut abd = APPBARDATA {
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        ..Default::default()
    };
    if unsafe { SHAppBarMessage(ABM_GETTASKBARPOS, &mut abd) } == 0 {
        return false;
    }
    // Auto-hidden bars report a rect parked off-screen; then the orientation
    // does not matter for what is visible right now. Validate against the
    // virtual desktop: the main taskbar can sit on a secondary monitor with
    // negative or beyond-primary coordinates.
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if abd.rc.bottom <= vy || abd.rc.top >= vy + vh || abd.rc.right <= vx || abd.rc.left >= vx + vw
    {
        return false;
    }
    abd.uEdge == ABE_LEFT || abd.uEdge == ABE_RIGHT
}

#[cfg(not(windows))]
pub(crate) fn taskbar_is_vertical() -> bool {
    false
}

/// Whether another settings process already owns the instance lock. A
/// second window has its own startup snapshot and would overwrite the first
/// one's edits on its next save, so duplicate launches exit quietly.
#[cfg(windows)]
pub(crate) fn acquire_single_instance() -> bool {
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    let name = utf16("Local\\SysVitalsSettings_SingleInstance");
    match unsafe { CreateMutexW(None, false, windows::core::PCWSTR(name.as_ptr())) } {
        Ok(handle) => {
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                false
            } else {
                *SETTINGS_INSTANCE.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(handle.0 as isize);
                true
            }
        }
        // A mutex failure must not keep the panel from opening; the parent
        // launch slot remains the primary duplicate guard.
        Err(e) => {
            log_event(
                "WARN",
                &format!("settings: instance mutex unavailable: {e}; continuing"),
            );
            true
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn acquire_single_instance() -> bool {
    use std::os::fd::AsRawFd;
    let Ok(dir) = sysvitals_core::config::data_dir() else {
        log_event(
            "WARN",
            "settings: cannot resolve the data directory; instance guard disabled",
        );
        return true;
    };
    let _ = std::fs::create_dir_all(&dir);
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("settings.lock"))
    else {
        log_event(
            "WARN",
            &format!(
                "settings: cannot open {}; instance guard disabled",
                dir.join("settings.lock").display()
            ),
        );
        return true;
    };
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
            log_event(
                "WARN",
                &format!("settings: instance lock failed: {error}; exiting"),
            );
        }
        return false;
    }
    *SETTINGS_INSTANCE_FILE
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(file);
    true
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn acquire_single_instance() -> bool {
    true
}

#[cfg(windows)]
pub(crate) static SETTINGS_INSTANCE: std::sync::Mutex<Option<isize>> = std::sync::Mutex::new(None);
#[cfg(target_os = "macos")]
pub(crate) static SETTINGS_INSTANCE_FILE: std::sync::Mutex<Option<std::fs::File>> =
    std::sync::Mutex::new(None);

/// Parse `--parent-pid <pid>`; the pure parsing lives in `core::cli`
/// (unit-tested there).
pub(crate) fn parse_parent_pid() -> Option<u32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    sysvitals_core::cli::parse_parent_pid(&args)
}

/// Parse `--anchor L T R B`; the pure parsing lives in `core::cli`. Absent,
/// malformed, or degenerate rectangles yield `None`, which keeps the default
/// centered placement — a bad argument must never prevent the window from
/// opening.
pub(crate) fn parse_anchor() -> Option<AnchorRect> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    sysvitals_core::cli::parse_anchor(&args)
}

/// Parse `--anchor-monitor <handle>` from the process arguments: the native
/// display handle (HMONITOR on Windows, NSScreenNumber on macOS) that hosted
/// the indicator. `None` when absent, zero, or malformed. The pure parsing
/// lives in `core::cli` (unit-tested there).
pub(crate) fn parse_anchor_monitor() -> Option<DisplayId> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    sysvitals_core::cli::parse_anchor_monitor(&args).map(DisplayId::new)
}

const SETTINGS_MARGIN: f32 = 16.0;
const SETTINGS_MIN_SIZE: f32 = 200.0;

/// Fit the requested window size into a display's usable work area. GPUI's
/// `check_given_bounds()` only verifies the window center, so a fixed
/// 400x720 settings window can otherwise spill above/below a short work area
/// on a high-DPI laptop. The arithmetic lives in `core::layout` so it runs
/// in the Linux unit-test gate.
pub(crate) fn fit_window_size(
    requested: gpui_kit::Size<Pixels>,
    work: Bounds<Pixels>,
) -> gpui_kit::Size<Pixels> {
    let (width, height) = sysvitals_core::layout::fit_size(
        f32::from(requested.width),
        f32::from(requested.height),
        layout_rect(&work),
        SETTINGS_MARGIN,
        SETTINGS_MIN_SIZE,
    );
    size(px(width), px(height))
}

pub(crate) fn centered_on_work(
    work: Bounds<Pixels>,
    requested: gpui_kit::Size<Pixels>,
) -> WindowBounds {
    let fitted = fit_window_size(requested, work);
    let center = point(
        px(f32::from(work.origin.x) + f32::from(work.size.width) * 0.5),
        px(f32::from(work.origin.y) + f32::from(work.size.height) * 0.5),
    );
    WindowBounds::Windowed(Bounds::centered_at(center, fitted))
}

/// Center the settings window on the display that contains the anchor (the
/// indicator that opened it), so it follows the indicator across monitors.
/// Display selection is shared with the history popup
/// (`sysvitals_panel::anchor_visible_bounds`: native monitor handle first,
/// rectangle probe fallback); this function only centers on the result.
pub(crate) fn settings_window_bounds(
    anchor: Option<&AnchorRect>,
    monitor: Option<DisplayId>,
    // `Size` is ambiguous between `gpui::Size<Pixels>` (from `gpui_kit::*`)
    // and the `gpui_component::Size` enum (from `gpui_kit::component::*`);
    // the layout size is the generic geometry type.
    requested: gpui_kit::Size<Pixels>,
    cx: &App,
) -> WindowBounds {
    let Some(anchor) = anchor else {
        if let Some(display) = cx.primary_display() {
            return centered_on_work(display.visible_bounds(), requested);
        }
        return WindowBounds::centered(requested, cx);
    };
    let work = anchor_visible_bounds(cx, anchor, monitor)
        .or_else(|| cx.primary_display().map(|display| display.visible_bounds()));
    let Some(work) = work else {
        return WindowBounds::centered(requested, cx);
    };
    centered_on_work(work, requested)
}

/// Quit when the indicator that launched this panel disappears without
/// running its quit path (crash, Force Quit). The job object covers Windows
/// ordinary exits, but a monitored parent also catches a child launched
/// before the job was configured and mirrors the mac behavior.
#[cfg(windows)]
pub(crate) fn watch_parent(cx: &mut App) {
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };
    let Some(pid) = parse_parent_pid() else {
        return;
    };
    let raw = match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        Ok(handle) => handle.0 as isize,
        Err(e) => {
            log_event(
                "WARN",
                &format!("settings: parent process unavailable: {e}; exiting"),
            );
            cx.quit();
            return;
        }
    };
    log_event("INFO", &format!("settings: watching parent pid {pid}"));
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(1))
                .await;
            let exited = unsafe { WaitForSingleObject(HANDLE(raw as *mut std::ffi::c_void), 0) }
                != WAIT_TIMEOUT;
            if exited {
                log_event("INFO", "settings: parent process gone; exiting");
                cx.update(|app| app.quit());
                break;
            }
        }
        let _ = unsafe { CloseHandle(HANDLE(raw as *mut std::ffi::c_void)) };
    })
    .detach();
}

#[cfg(target_os = "macos")]
pub(crate) fn watch_parent(cx: &mut App) {
    // Shared one-second poll on the launcher pid (see `sysvitals_panel`).
    sysvitals_panel::watch_parent(cx, "settings");
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn watch_parent(_cx: &mut App) {}
