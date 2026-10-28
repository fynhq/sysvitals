//! Windows display layer: one topmost layered overlay per taskbar, so every
//! monitor that shows a taskbar also shows the indicator.
//!
//! Why an overlay and not a `SetParent` child of `Shell_TrayWnd`: on current
//! Windows 11 builds the taskbar's own content is composited by a XAML Islands
//! child window whose visual sits above every GDI child of the tray, so an
//! embedded child window can never render crisply (verified empirically with
//! calibration windows: an embedded opaque child is blurred and tinted by the
//! taskbar acrylic, while an identical topmost layered popup is pixel-crisp).
//! A topmost layered popup aligned to the taskbar geometry gives the same
//! visual result through a mechanism the compositor actually honors.
//!
//! Consequences handled here:
//! - One overlay exists per taskbar: `Shell_TrayWnd` (the main bar) plus every
//!   `Shell_SecondaryTrayWnd`. Each overlay is owned by its own taskbar so it
//!   follows that bar's z-band, and each measures/renders at its monitor's own
//!   DPI. A hidden sentinel window owns the shared timers (one sample per tick
//!   drives every overlay) and reconciles the overlay set whenever taskbars or
//!   monitors appear/disappear.
//! - The overlay does not share the taskbar's z-order or visibility, so each
//!   tick re-checks whether its taskbar is present (auto-hide). The indicator
//!   hides only when that taskbar leaves the screen; no foreground-window
//!   heuristics are involved.
//! - Nothing is embedded, so there is no explorer-side state to roll back on
//!   exit; destroying the window is the whole teardown.
//!
//! Settings architecture (post-refactor):
//! The settings UI lives in a separate process (`SysVitalsSettings.exe`,
//! built with GPUI). This overlay process does NOT host any settings dialog.
//! Instead it watches the shared config file for changes: a 500 ms poll timer
//! compares the file's last-modified timestamp and, on change, reloads the
//! config and re-renders. The context menu "Settings" item simply spawns the
//! settings executable from the same directory. This decoupling keeps the
//! overlay process minimal (no UI toolkit, no modal loops) and lets the
//! settings app evolve independently.
//!
//! Typography mirrors the taskbar clock: the system UI message font at the
//! system size, semibold weight, and pure white / pure black glyphs per theme
//! so the indicator reads as part of the taskbar rather than a foreign widget.
//! Rendering is GDI text; the layout strings come from the pure core crate so
//! the single-row / dual-row quadrant rules stay unit-tested there. Glyphs are
//! drawn at 2x supersampled resolution and box-filtered down for smooth edges
//! without ClearType (which corrupts per-pixel-alpha compositing).

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod win {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, AtomicU64, Ordering};
    use std::sync::{Mutex, OnceLock, RwLock};
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use sysvitals_core::format::FormattedRow;
    use sysvitals_core::{
        format_frame, format_frame_vertical, t, Config, Language, NetSelection, Sampler, Theme,
    };
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT};
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW,
        GetDC, GetMonitorInfoW, MonitorFromWindow, ReleaseDC, SelectObject, SetBkMode,
        SetTextColor, AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER,
        BI_RGB, BLENDFUNCTION, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS,
        DT_CALCRECT, DT_CENTER, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FF_SWISS,
        FW_REGULAR, FW_SEMIBOLD, HFONT, MONITORINFO, MONITOR_DEFAULTTONEAREST, OUT_DEFAULT_PRECIS,
        TRANSPARENT,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::System::Diagnostics::Debug::{
        SetUnhandledExceptionFilter, EXCEPTION_POINTERS,
    };
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::LibraryLoader::{
        GetModuleFileNameW, GetModuleHandleExW, GetModuleHandleW,
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };
    use windows::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
    };
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
        REG_VALUE_TYPE,
    };
    use windows::Win32::System::SystemInformation::{GetLocalTime, OSVERSIONINFOW};
    use windows::Win32::System::Threading::{
        CreateMutexW, CreateProcessW, GetCurrentProcessId, GetExitCodeProcess, TerminateProcess,
        WaitForSingleObject, PROCESS_INFORMATION, STARTUPINFOW,
    };
    use windows::Win32::UI::HiDpi::{
        GetDpiForSystem, GetDpiForWindow, SetProcessDpiAwarenessContext,
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows::Win32::UI::Shell::{IVirtualDesktopManager, ShellExecuteW, VirtualDesktopManager};
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
        DispatchMessageW, FindWindowExW, FindWindowW, GetClassNameW, GetCursorPos,
        GetForegroundWindow, GetMessageW, GetShellWindow, GetSystemMetrics, GetWindowLongPtrW,
        GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
        IsWindowVisible, KillTimer, MoveWindow, PostMessageW, PostQuitMessage, RegisterClassExW,
        RegisterWindowMessageW, SetForegroundWindow, SetTimer, SetWindowPos, ShowWindow,
        SystemParametersInfoW, TrackPopupMenu, TranslateMessage, UpdateLayeredWindow,
        WindowFromPoint, CS_HREDRAW, CS_VREDRAW, GWL_EXSTYLE, HWND_TOPMOST, MF_SEPARATOR,
        MF_STRING, MSG, NONCLIENTMETRICSW, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN,
        SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SPI_GETNONCLIENTMETRICS,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE, SW_SHOWNORMAL,
        TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, ULW_ALPHA, WM_DESTROY, WM_DISPLAYCHANGE,
        WM_DPICHANGED, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_SETTINGCHANGE, WM_TIMER,
        WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    };

    // ------------------------------------------------------------------ consts

    const TIMER_SAMPLE: usize = 0x515;
    const TIMER_MOVE: usize = 0x516;
    /// Timer id for the config-file change poller.
    const TIMER_CONFIG: usize = 0x517;
    const MOVE_MS: u32 = 1_000;
    /// How often (ms) we poll the config file's mtime for external changes.
    const CONFIG_POLL_MS: u32 = 500;
    /// Posted by the background history-launch thread once the resident
    /// process handle is stored (or the launch failed).
    const WM_APP_HISTORY_READY: u32 = 0x8000 + 3;
    /// Polls the resident history process so a crash is reaped and a stale
    /// request cleared without waiting for the next click.
    const TIMER_HISTORY: usize = 0x519;
    const HISTORY_SYNC_MS: u32 = 250;
    /// Retry cadence for re-finding `Shell_TrayWnd` after explorer restarts.
    const TIMER_REBIND: usize = 0x51A;
    const REBIND_RETRY_MS: u32 = 500;
    /// Sentinel watchdog: recreates the overlay if it ever vanishes without a
    /// `TaskbarCreated` broadcast (defensive; the broadcast covers the normal
    /// explorer-restart path).
    const TIMER_WATCH: usize = 0x51B;
    const WATCHDOG_MS: u32 = 5_000;
    /// Horizontal padding around the measured text.
    const PAD_PX: i32 = 6;
    /// Extra width slack added when sizing the overlay. Sizing measures text at
    /// 1x with the overlay font, but rendering draws at 2x and downsamples; the
    /// two rounded widths do not match exactly, so without slack the last
    /// characters get clipped.
    const WIDTH_SLACK_PX: i32 = 8;
    /// Font size for the vertical (left/right) taskbar layout, as a fraction
    /// of the system UI size expressed in hundredths. On a vertical taskbar
    /// the bar's *thickness* — not its length — is the width budget: a typical
    /// bar is ~48 logical px wide, so the system size would clip; the stacked
    /// label/value column plus this reduction keeps every line inside the bar.
    /// Horizontal taskbars are limited by height, not width, so they keep the
    /// system size untouched.
    ///
    /// 0.75× (was 0.7×): matches the macOS dual-row scale factor exactly so
    /// the two platforms shrink the same system font by the same amount.
    const VERTICAL_FONT_HUNDREDTHS: i32 = 75;
    /// Horizontal padding for the vertical layout: the thickness is the binding
    /// constraint, so the text keeps a tighter margin than on horizontal bars.
    const VERTICAL_PAD_PX: i32 = 3;
    /// Width slack for the vertical layout (same rounding compensation as
    /// `WIDTH_SLACK_PX`, trimmed because every pixel counts against the bar).
    const VERTICAL_WIDTH_SLACK_PX: i32 = 4;
    const THEME_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
    const THEME_VALUE: &str = "SystemUsesLightTheme";
    const TASKBAR_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced";
    const TASKBAR_VALUE: &str = "TaskbarAl";

    const IDM_SETTINGS: usize = 901;
    const IDM_QUIT: usize = 902;
    const IDM_RESTART: usize = 903;
    /// Right-click menu entry that launches Task Manager. This used to be the
    /// double-click action on the indicator; moving it here is what lets the
    /// left click fire the trend chart immediately (see `WM_LBUTTONUP`).
    const IDM_TASKMGR: usize = 904;

    // ----------------------------------------------------------------- statics

    /// Cached `SystemUsesLightTheme` (1 = light taskbar). Refreshed on
    /// `WM_SETTINGCHANGE` so a live theme switch repaints correctly.
    static LIGHT_THEME: AtomicU32 = AtomicU32::new(0);
    /// Glyph weight follows the taskbar theme, like the clock: semibold on
    /// the dark taskbar, regular on the light one.
    static TEXT_WEIGHT: AtomicI32 = AtomicI32::new(600);
    static CONFIG: OnceLock<RwLock<Config>> = OnceLock::new();
    /// Job object owning every panel child. Created once at startup with
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`; the handle is deliberately kept
    /// for the process lifetime, so a crash or a quit during a slow
    /// `CreateProcessW` cannot leave an orphan child behind.
    static CHILD_JOB: AtomicIsize = AtomicIsize::new(0);
    /// Resident history process handle, stored as a raw `isize` because the
    /// launch happens on a worker thread (`HANDLE` is not `Send`). The UI
    /// thread reaps and closes it.
    static HISTORY_PROCESS: Mutex<Option<isize>> = Mutex::new(None);
    /// True from the moment the background launch is requested until the
    /// worker finishes; a click during startup must not spawn a second child.
    static HISTORY_LAUNCHING: AtomicBool = AtomicBool::new(false);
    /// Monitor (HMONITOR as isize) the live settings window was opened on.
    /// A Settings click on a different indicator moves the window there by
    /// closing and relaunching it; a click on the same screen is a no-op
    /// (there is no IPC to raise an existing GPUI window).
    static SETTINGS_MONITOR: AtomicIsize = AtomicIsize::new(0);
    /// The process is shutting down intentionally: the overlay's `WM_DESTROY`
    /// must quit rather than wait for a shell-driven rebind.
    static QUITTING: AtomicBool = AtomicBool::new(false);
    /// Hidden sentinel window handle. It owns every periodic timer (sample,
    /// move, config poll, history sync, watchdog) and receives the
    /// `TaskbarCreated` broadcast, so the timers survive explorer restarts and
    /// drive every overlay at once instead of one window driving itself.
    static SENTINEL_HWND: AtomicIsize = AtomicIsize::new(0);
    /// Monitor (HMONITOR) of the overlay whose indicator currently anchors the
    /// open history popup. A click on the same monitor's indicator closes the
    /// popup (toggle); a click on a different monitor's indicator moves it
    /// there instead.
    static HISTORY_ANCHOR_MONITOR: AtomicIsize = AtomicIsize::new(0);
    /// `RegisterWindowMessageW("TaskbarCreated")`; 0 until registered.
    static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
    /// Last observed config mtime in nanoseconds since UNIX_EPOCH. Nanosecond
    /// precision is required: the settings window saves on every click, and
    /// two saves inside one second would otherwise be invisible to the poller.
    static CONFIG_MTIME: AtomicU64 = AtomicU64::new(0);
    /// mtime of the last config parse failure, so a broken file logs once
    /// instead of every CONFIG_POLL_MS tick.
    static CONFIG_ERROR_MTIME: AtomicU64 = AtomicU64::new(0);
    /// Whether a history write/prune failure has already been logged. Open
    /// failures are retried periodically; write failures are logged once so a
    /// full disk does not spam the log every tick.
    static STORE_WRITE_ERROR_LOGGED: AtomicBool = AtomicBool::new(false);

    /// Per-taskbar overlay state. One entry per visible taskbar: the main bar
    /// plus one per extra monitor. Everything that used to be a process-wide
    /// global (window size, orientation) lives here so mixed-DPI multi-monitor
    /// setups measure and render each indicator independently. All access is on
    /// the message thread.
    #[derive(Clone, Copy)]
    struct Overlay {
        hwnd: HWND,
        /// The taskbar window that owns this overlay (its z-band follows it).
        taskbar: HWND,
        /// Monitor hosting the taskbar (HMONITOR as isize). Identifies the
        /// indicator for the history popup's move-to-screen decision.
        monitor: isize,
        vertical: bool,
        left_edge: bool,
        /// Measured content size in physical pixels at this overlay's DPI.
        /// Width only grows; a config/orientation/DPI change resets it.
        win_w: i32,
        win_h: i32,
    }

    fn overlay_by_hwnd(hwnd: HWND) -> Option<Overlay> {
        OVERLAYS.with(|slot| slot.borrow().iter().find(|o| o.hwnd == hwnd).copied())
    }

    /// Reset one overlay's grow-only width so a fresh measurement takes effect
    /// (config change, DPI change, orientation flip).
    fn reset_width(hwnd: HWND) {
        OVERLAYS.with(|slot| {
            if let Some(ov) = slot.borrow_mut().iter_mut().find(|o| o.hwnd == hwnd) {
                ov.win_w = 0;
            }
        });
    }

    fn sentinel_hwnd() -> HWND {
        HWND(SENTINEL_HWND.load(Ordering::Relaxed) as *mut std::ffi::c_void)
    }

    thread_local! {
        static SAMPLER: RefCell<Option<Sampler>> = const { RefCell::new(None) };
        /// Raw sample shared by every overlay, so a config/DPI/theme change can
        /// rebuild each layout without taking another reading (which used to
        /// inject a millisecond-scale network spike on every save).
        static LAST_SNAPSHOT: RefCell<Option<sysvitals_core::Snapshot>> = const { RefCell::new(None) };
        /// One entry per visible taskbar; the registry the sentinel drives.
        static OVERLAYS: RefCell<Vec<Overlay>> = const { RefCell::new(Vec::new()) };
        // History store. Lazily opened on the first sample so a missing
        // data dir never blocks the indicator itself.
        static STORE: RefCell<Option<sysvitals_core::Store>> = const { RefCell::new(None) };
        static TICK_COUNT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        // Child process handles, kept so we can detect a still-running
        // instance and avoid stacking duplicate windows.
        static SETTINGS_PROCESS: RefCell<Option<HANDLE>> = const { RefCell::new(None) };
        /// Shared command/status block between this overlay and the resident
        /// history process. The mapping handle is kept alive for the process
        /// lifetime.
        static HISTORY_STATE: RefCell<Option<(*mut sysvitals_core::HistoryState, HANDLE)>> = const { RefCell::new(None) };
        // COM virtual-desktop manager, created on demand the first time the
        // indicator is found cloaked. All access is on the message thread.
        static VD_MANAGER: RefCell<Option<IVirtualDesktopManager>> = const { RefCell::new(None) };
    }

    // ------------------------------------------------------------ config access

    fn config() -> Config {
        CONFIG
            .get_or_init(|| {
                // A missing or broken config file must not stop the indicator:
                // fall back to defaults and keep running, but leave a trace so
                // a parse error is diagnosable.
                let loaded = match sysvitals_core::config::config_path() {
                    Ok(p) => match Config::load(&p) {
                        Ok((c, _)) => c,
                        Err(e) => {
                            log_event(
                                "WARN",
                                &format!(
                                    "config load failed ({}): {e}; using defaults",
                                    p.display()
                                ),
                            );
                            Config::default()
                        }
                    },
                    Err(e) => {
                        log_event("WARN", &format!("config path unavailable: {e}"));
                        Config::default()
                    }
                };
                RwLock::new(loaded)
            })
            .read()
            .expect("config lock")
            .clone()
    }

    fn store_config(new: Config) {
        if let Some(slot) = CONFIG.get() {
            *slot.write().expect("config lock") = new;
        }
    }

    // ----------------------------------------------------------------- logging

    /// Current local date/time strings from the OS clock.
    fn local_now() -> (String, String) {
        let st = unsafe { GetLocalTime() };
        (
            format!("{:04}-{:02}-{:02}", st.wYear, st.wMonth, st.wDay),
            format!(
                "{:02}:{:02}:{:02}.{:03}",
                st.wHour, st.wMinute, st.wSecond, st.wMilliseconds
            ),
        )
    }

    /// Append one line to today's log file under the data dir. Logging must
    /// never break the indicator, so failures are silently ignored.
    fn log_event(level: &str, message: &str) {
        let Ok(dir) = sysvitals_core::config::data_dir() else {
            return;
        };
        let (date, time) = local_now();
        sysvitals_core::logger::log_line(&dir, &date, &time, level, message);
    }

    /// Remove log files older than the retention window. Run once at startup.
    fn prune_old_logs() {
        let Ok(dir) = sysvitals_core::config::data_dir() else {
            return;
        };
        let (date, _) = local_now();
        sysvitals_core::logger::prune_logs(
            &dir,
            &date,
            sysvitals_core::logger::DEFAULT_RETENTION_DAYS,
        );
    }

    // ------------------------------------------------------------------ helpers

    fn utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Read a DWORD from HKCU; `None` when the key or value is absent.
    unsafe fn hkcu_dword(key: &str, value: &str) -> Option<u32> {
        let key = utf16(key);
        let value = utf16(value);
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(key.as_ptr()),
            None,
            KEY_READ,
            &mut hkey,
        )
        .is_err()
        {
            return None;
        }
        let mut data = [0u8; 4];
        let mut size = 4u32;
        let mut kind = REG_VALUE_TYPE::default();
        let queried = RegQueryValueExW(
            hkey,
            windows::core::PCWSTR(value.as_ptr()),
            None,
            Some(&mut kind),
            Some(data.as_mut_ptr()),
            Some(&mut size),
        );
        let _ = RegCloseKey(hkey);
        if queried.is_ok() && size >= 4 {
            Some(u32::from_le_bytes(data))
        } else {
            None
        }
    }

    unsafe fn refresh_theme() {
        LIGHT_THEME.store(
            hkcu_dword(THEME_KEY, THEME_VALUE).unwrap_or(0).min(1),
            Ordering::Relaxed,
        );
        let weight = if LIGHT_THEME.load(Ordering::Relaxed) == 1 {
            FW_REGULAR.0 as i32
        } else {
            FW_SEMIBOLD.0 as i32
        };
        TEXT_WEIGHT.store(weight, Ordering::Relaxed);
    }

    /// The real Windows build number (`GetVersionEx` is version-helpered).
    unsafe fn os_build_number() -> Option<u32> {
        let mut info = OSVERSIONINFOW::default();
        info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
        if RtlGetVersion(&mut info).is_err() {
            return None;
        }
        Some(info.dwBuildNumber)
    }

    /// Win11 with centered taskbar icons: the only case where the far-left
    /// anchor is offered.
    unsafe fn win11_centered_taskbar() -> bool {
        hkcu_dword(TASKBAR_KEY, TASKBAR_VALUE) == Some(1)
    }

    /// Whether the effective theme is light (config override wins over the
    /// system flag).
    fn theme_is_light() -> bool {
        match config().display.theme {
            Theme::Auto => LIGHT_THEME.load(Ordering::Relaxed) == 1,
            Theme::Light => true,
            Theme::Dark => false,
        }
    }

    // ------------------------------------------------------------------- fonts

    /// System UI face + pixel height, cached on the message thread. Read
    /// from `NONCLIENTMETRICS.lfMessageFont` so the indicator's font follows
    /// the system setting (face and size). `WM_SETTINGCHANGE` and
    /// `WM_DPICHANGED` invalidate this cache: text-size and per-monitor DPI
    /// changes must not require a restart.
    thread_local! {
        static UI_FONT: RefCell<Option<(i32, Vec<u16>)>> = const { RefCell::new(None) };
    }

    fn ui_font() -> (i32, Vec<u16>) {
        UI_FONT.with(|slot| {
            if let Some(font) = slot.borrow().as_ref() {
                return font.clone();
            }
            let font = unsafe {
                let fallback = || (-(12 * GetDpiForSystem() as i32 / 96), utf16("Segoe UI"));
                let mut nc: NONCLIENTMETRICSW = std::mem::zeroed();
                nc.cbSize = std::mem::size_of::<NONCLIENTMETRICSW>() as u32;
                let ok = SystemParametersInfoW(
                    SPI_GETNONCLIENTMETRICS,
                    std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
                    Some(std::ptr::from_mut(&mut nc).cast::<std::ffi::c_void>()),
                    windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                )
                .is_ok();
                if !ok {
                    return fallback();
                }
                let lf = &nc.lfMessageFont;
                // lfHeight is negative (character height) in practice, but be
                // explicit: normalize to a negative em height so CreateFontW
                // sizes glyphs exactly like the system does.
                let height = match lf.lfHeight {
                    h if h < 0 => h,
                    h if h > 0 => -h,
                    _ => fallback().0,
                };
                let mut face: Vec<u16> = lf
                    .lfFaceName
                    .iter()
                    .copied()
                    .take_while(|&c| c != 0)
                    .collect();
                if face.is_empty() {
                    face = utf16("Segoe UI");
                } else {
                    face.push(0);
                }
                (height, face)
            };
            *slot.borrow_mut() = Some(font.clone());
            font
        })
    }

    fn invalidate_ui_font() {
        UI_FONT.with(|slot| *slot.borrow_mut() = None);
    }

    /// DPI of the monitor hosting `hwnd`. `GetDpiForSystem` is only a fallback
    /// when the window handle is unavailable.
    fn dpi_for(hwnd: HWND) -> u32 {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        (if dpi == 0 {
            unsafe { GetDpiForSystem() }
        } else {
            dpi
        })
        .max(96)
    }

    /// Effective glyph height in physical pixels for one overlay's taskbar
    /// orientation. The cached system font height is expressed at the system
    /// DPI; scale it to the overlay's own monitor and then apply the vertical
    /// reduction, so mixed-DPI setups and live text-size changes both work.
    fn effective_font_height_for(hwnd: HWND, vertical: bool) -> i32 {
        let (height, _) = ui_font();
        let system_dpi = (unsafe { GetDpiForSystem() }).max(96) as i32;
        let window_dpi = dpi_for(hwnd) as i32;
        let height = height * window_dpi / system_dpi;
        if vertical {
            // `height` is a negative character height; clamp the reduction so a
            // tiny system font can never produce 0 (which CreateFontW would
            // silently interpret as "default font").
            (height * VERTICAL_FONT_HUNDREDTHS / 100).min(-1)
        } else {
            height
        }
    }

    /// Horizontal padding for the given orientation.
    fn pad_px(vertical: bool) -> i32 {
        if vertical {
            VERTICAL_PAD_PX
        } else {
            PAD_PX
        }
    }

    /// Width slack for the given orientation.
    fn width_slack_px(vertical: bool) -> i32 {
        if vertical {
            VERTICAL_WIDTH_SLACK_PX
        } else {
            WIDTH_SLACK_PX
        }
    }

    /// Overlay font: the system UI face at the system UI size, grayscale AA.
    /// Antialiased (not ClearType) because the glyphs are composited
    /// per-pixel-alpha over the taskbar.
    unsafe fn make_font_for(hwnd: HWND, vertical: bool) -> HFONT {
        let (_, face) = ui_font();
        CreateFontW(
            effective_font_height_for(hwnd, vertical),
            0,
            0,
            0,
            TEXT_WEIGHT.load(Ordering::Relaxed),
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            (DEFAULT_PITCH.0 | FF_SWISS.0) as u32,
            windows::core::PCWSTR(face.as_ptr()),
        )
    }

    /// Overlay font at an integer supersample multiplier. Used to render at a
    /// supersampled scale (2x) before downsampling.
    unsafe fn make_font_at_for(hwnd: HWND, scale: i32, vertical: bool) -> HFONT {
        let (_, face) = ui_font();
        CreateFontW(
            effective_font_height_for(hwnd, vertical) * scale.max(1),
            0,
            0,
            0,
            TEXT_WEIGHT.load(Ordering::Relaxed),
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            (DEFAULT_PITCH.0 | FF_SWISS.0) as u32,
            windows::core::PCWSTR(face.as_ptr()),
        )
    }

    // ------------------------------------------------------- taskbar geometry

    /// Taskbar geometry: rect in screen coordinates plus orientation.
    struct TaskbarInfo {
        rect: RECT,
        vertical: bool,
        /// True when a vertical bar is docked to the *left* screen edge. The
        /// placement code uses it to pick which edge the indicator must never
        /// run past when its column is wider than the bar's thickness.
        left_edge: bool,
        /// Monitor hosting this taskbar (HMONITOR as isize).
        monitor: isize,
    }

    /// Geometry for one specific taskbar window, or `None` when that bar is
    /// not currently on screen (auto-hidden, or shell not ready).
    ///
    /// The main bar's rect used to come from `ABM_GETTASKBARPOS`, but that call
    /// only reports the main taskbar; secondary bars (`Shell_SecondaryTrayWnd`)
    /// must be measured with `GetWindowRect`, so every bar uses the same path.
    /// Orientation is inferred from the rect: a taskbar is always far longer
    /// along its docked edge than it is thick.
    unsafe fn taskbar_info_for(bar: HWND) -> Option<TaskbarInfo> {
        let mut r = RECT::default();
        if GetWindowRect(bar, &mut r).is_err() {
            return None;
        }
        if r.right <= r.left || r.bottom <= r.top {
            return None;
        }
        // A taskbar window that is not visible has no indicator. This covers
        // auto-hide (the shell slides the bar away and hides it) and a
        // secondary bar that still exists as a window after the user turns
        // off "show taskbar on all displays". A visible bar parked off its
        // monitor is rejected by the intersection checks below.
        if !IsWindowVisible(bar).as_bool() {
            return None;
        }
        // The bar can live on any monitor, including negative virtual
        // coordinates. Validate against the virtual screen, never the primary.
        let virtual_rect = virtual_screen_rect();
        let on_screen = r.bottom > virtual_rect.top
            && r.top < virtual_rect.bottom
            && r.right > virtual_rect.left
            && r.left < virtual_rect.right;
        if !on_screen {
            return None;
        }
        let monitor = MonitorFromWindow(bar, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let mon_rect = if GetMonitorInfoW(monitor, &mut mi).as_bool() {
            mi.rcMonitor
        } else {
            virtual_rect
        };
        // A hidden bar is parked just off its own monitor edge; require the
        // rect to still intersect the monitor that hosts it.
        let intersects = r.bottom > mon_rect.top
            && r.top < mon_rect.bottom
            && r.right > mon_rect.left
            && r.left < mon_rect.right;
        if !intersects {
            return None;
        }
        let vertical = (r.bottom - r.top) > (r.right - r.left);
        let left_edge = vertical && r.left <= mon_rect.left;
        Some(TaskbarInfo {
            rect: r,
            vertical,
            left_edge,
            monitor: monitor.0 as isize,
        })
    }

    /// Bounding rectangle of the whole virtual desktop (all monitors), in
    /// physical pixels. Used for on-screen validation and for monitor-local
    /// clamping that must work with negative coordinates.
    unsafe fn virtual_screen_rect() -> RECT {
        let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        let (fallback_w, fallback_h) =
            (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        let width = if width > 0 { width } else { fallback_w };
        let height = if height > 0 { height } else { fallback_h };
        RECT {
            left,
            top,
            right: left + width.max(1),
            bottom: top + height.max(1),
        }
    }

    /// Monitor that hosts a given taskbar, or the virtual desktop when the
    /// monitor cannot be resolved. The indicator must be clamped to this
    /// monitor, not the primary one, for secondary/negative-coordinate bars.
    unsafe fn taskbar_monitor_rect_for(bar: HWND) -> RECT {
        let fallback = virtual_screen_rect();
        let monitor = MonitorFromWindow(bar, MONITOR_DEFAULTTONEAREST);
        if monitor.is_invalid() {
            return fallback;
        }
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            info.rcMonitor
        } else {
            fallback
        }
    }

    /// Left edge of one visible direct child of a taskbar, if present.
    unsafe fn visible_child_left(bar: HWND, class: &str) -> Option<i32> {
        let cls = utf16(class);
        let child =
            FindWindowExW(Some(bar), None, windows::core::PCWSTR(cls.as_ptr()), None).ok()?;
        if child.is_invalid() || !IsWindowVisible(child).as_bool() {
            return None;
        }
        let mut rect = RECT::default();
        if GetWindowRect(child, &mut rect).is_err() || rect.right <= rect.left {
            return None;
        }
        Some(rect.left)
    }

    /// Horizontal anchor for an indicator inside one taskbar: the left edge of
    /// its notification area when the bar shows tray icons, otherwise the left
    /// edge of the show-desktop button, otherwise the bar's right edge.
    ///
    /// Secondary bars have no `TrayNotifyWnd` until "show tray icons on all
    /// displays" is enabled. Anchoring such a bar to its raw right edge would
    /// put the indicator on top of the show-desktop button, so that button —
    /// when present — is the better boundary.
    unsafe fn taskbar_anchor_left(bar: HWND) -> i32 {
        if let Some(left) = visible_child_left(bar, "TrayNotifyWnd") {
            return left;
        }
        if let Some(left) = visible_child_left(bar, "TrayShowDesktopButtonWClass") {
            return left;
        }
        let mut rect = RECT::default();
        if GetWindowRect(bar, &mut rect).is_ok() {
            rect.right
        } else {
            0
        }
    }

    /// Every taskbar window: the main bar plus one per extra monitor. All
    /// secondary bars share the class name `Shell_SecondaryTrayWnd`, so they
    /// are enumerated with `FindWindowExW`.
    unsafe fn taskbar_windows() -> Vec<HWND> {
        let mut out = Vec::new();
        if let Some(main) = find_taskbar() {
            out.push(main);
        }
        let cls = utf16("Shell_SecondaryTrayWnd");
        let mut prev: Option<HWND> = None;
        loop {
            match FindWindowExW(None, prev, windows::core::PCWSTR(cls.as_ptr()), None) {
                Ok(h) if !h.is_invalid() => {
                    out.push(h);
                    prev = Some(h);
                }
                _ => break,
            }
        }
        out
    }

    /// How many times the topmost band was found lost between two ticks.
    /// Logged on the first loss and every 60th so a hostile z-order partner
    /// cannot spam the log.
    static TOPMOST_LOST_COUNT: AtomicU32 = AtomicU32::new(0);
    /// Set while a context menu is being tracked. `TrackPopupMenu` runs a
    /// modal loop that still dispatches our `WM_TIMER`, and the menu lives
    /// in the topmost band — re-asserting the indicator's band during tracking
    /// would lift the tiny overlay back above the open menu. Skip those ticks.
    static MENU_TRACKING: AtomicBool = AtomicBool::new(false);
    /// Last top-level window seen covering the indicator's center. Used to
    /// log an occlusion once per covering window instead of every tick.
    static LAST_OCCLUDER: AtomicIsize = AtomicIsize::new(0);
    /// Whether the last visibility probe found the overlay hidden/minimized.
    static VISIBILITY_ANOMALY: AtomicBool = AtomicBool::new(false);
    /// The first show of a fresh process is not an anomaly; it suppresses the
    /// startup "not visible; restoring" diagnostic.
    static OVERLAY_SHOWN_ONCE: AtomicBool = AtomicBool::new(false);
    /// De-duplicates the "cloaked on another virtual desktop" log line.
    static CLOAK_LOGGED: AtomicBool = AtomicBool::new(false);
    static CLOAK_MOVE_FAIL_LOGGED: AtomicBool = AtomicBool::new(false);
    static LAYERED_FAIL_LOGGED: AtomicBool = AtomicBool::new(false);
    /// De-duplicates the "overlay creation failed" warning: reconcile retries
    /// every second, so a persistent failure must not log on every attempt.
    static OVERLAY_CREATE_FAIL_LOGGED: AtomicBool = AtomicBool::new(false);

    /// The Windows virtual-desktop manager, created on demand. The overlay is
    /// a top-level window, so Windows cloaks it when the user switches to
    /// another virtual desktop; the taskbar itself appears on every desktop,
    /// so the indicator should follow the user too.
    unsafe fn vd_manager() -> Option<IVirtualDesktopManager> {
        VD_MANAGER.with(|slot| {
            if slot.borrow().is_none() {
                let created: windows::core::Result<IVirtualDesktopManager> = CoCreateInstance(
                    &VirtualDesktopManager,
                    None::<&windows::core::IUnknown>,
                    CLSCTX_INPROC_SERVER,
                );
                match created {
                    Ok(manager) => *slot.borrow_mut() = Some(manager),
                    Err(e) => {
                        log_event("WARN", &format!("virtual desktop manager unavailable: {e}"));
                        return None;
                    }
                }
            }
            slot.borrow().clone()
        })
    }

    /// Move the overlay to the desktop currently hosting the foreground
    /// window (or the shell window on an empty desktop). Used only when DWM
    /// reports the overlay as cloaked.
    unsafe fn move_to_current_desktop(hwnd: HWND) {
        let Some(manager) = vd_manager() else {
            return;
        };
        let mut target = GetForegroundWindow();
        if target.is_invalid() {
            target = GetShellWindow();
        }
        if target.is_invalid() {
            log_event("WARN", "cloaked overlay: no foreground window to follow");
            return;
        }
        let Ok(desktop) = manager.GetWindowDesktopId(target) else {
            if !CLOAK_MOVE_FAIL_LOGGED.swap(true, Ordering::Relaxed) {
                log_event("WARN", "cloaked overlay: cannot read the target desktop id");
            }
            return;
        };
        match manager.MoveWindowToDesktop(hwnd, &desktop) {
            Ok(()) => {
                CLOAK_MOVE_FAIL_LOGGED.store(false, Ordering::Relaxed);
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
            Err(e) => {
                if !CLOAK_MOVE_FAIL_LOGGED.swap(true, Ordering::Relaxed) {
                    log_event("WARN", &format!("overlay virtual-desktop move failed: {e}"));
                }
            }
        }
    }

    /// Restore a hidden or minimized overlay, and bring it back from another
    /// virtual desktop. `IsWindowVisible` alone cannot see either state: a
    /// minimized window still has `WS_VISIBLE`, and a cloaked window still
    /// reports itself visible, so both used to leave the indicator invisible
    /// with no log line until the process was restarted.
    unsafe fn maintain_visibility(hwnd: HWND) {
        let hidden = !IsWindowVisible(hwnd).as_bool();
        let minimized = IsIconic(hwnd).as_bool();
        let anomaly = hidden || minimized;
        let was_anomalous = VISIBILITY_ANOMALY.swap(anomaly, Ordering::Relaxed);
        if anomaly {
            let first_show = !OVERLAY_SHOWN_ONCE.swap(true, Ordering::Relaxed);
            if !was_anomalous && !first_show {
                log_event(
                    "INFO",
                    &format!(
                        "overlay not visible (hidden={hidden} minimized={minimized}); restoring"
                    ),
                );
            }
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        } else {
            OVERLAY_SHOWN_ONCE.store(true, Ordering::Relaxed);
        }

        let mut cloaked = 0u32;
        let is_cloaked = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut std::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0;
        if is_cloaked {
            if !CLOAK_LOGGED.swap(true, Ordering::Relaxed) {
                log_event(
                    "INFO",
                    "overlay cloaked on another virtual desktop; moving it here",
                );
            }
            move_to_current_desktop(hwnd);
        } else {
            CLOAK_LOGGED.store(false, Ordering::Relaxed);
            CLOAK_MOVE_FAIL_LOGGED.store(false, Ordering::Relaxed);
        }
    }

    /// Log once per new covering window when another topmost window sits on
    /// the indicator's center after the band assertion. This is the
    /// diagnostic that distinguishes "our window vanished" from "a shell
    /// flyout or third-party topmost window is parked over us".
    unsafe fn log_occlusion(hwnd: HWND) {
        if MENU_TRACKING.load(Ordering::Relaxed) {
            return;
        }
        let mut rc = RECT::default();
        if GetWindowRect(hwnd, &mut rc).is_err() {
            return;
        }
        let point = POINT {
            x: (rc.left + rc.right) / 2,
            y: (rc.top + rc.bottom) / 2,
        };
        let hit = WindowFromPoint(point);
        if hit.is_invalid() || hit == hwnd {
            LAST_OCCLUDER.store(0, Ordering::Relaxed);
            return;
        }
        let hit_raw = hit.0 as isize;
        if LAST_OCCLUDER.swap(hit_raw, Ordering::Relaxed) == hit_raw {
            return;
        }
        let mut class_buf = [0u16; 128];
        let class_len = GetClassNameW(hit, &mut class_buf).max(0) as usize;
        let mut title_buf = [0u16; 160];
        let title_len = GetWindowTextW(hit, &mut title_buf).max(0) as usize;
        let mut pid = 0u32;
        GetWindowThreadProcessId(hit, Some(&mut pid));
        log_event(
            "WARN",
            &format!(
                "overlay center covered by class='{}' title='{}' pid={pid}",
                String::from_utf16_lossy(&class_buf[..class_len]),
                String::from_utf16_lossy(&title_buf[..title_len]),
            ),
        );
    }

    /// Re-assert the topmost style only when Windows has actually dropped it.
    ///
    /// The overlay is owned by `Shell_TrayWnd`; owned windows stay above their
    /// owner and follow the taskbar's z-band, which is what keeps the
    /// indicator visible while Start / Task View push the taskbar into its
    /// higher band. Re-inserting the window at the top of the topmost band
    /// every tick cannot cross bands and only fights the shell's animations,
    /// so the unconditional assertion was removed. A lost style bit (seen on
    /// live installs after some z-order reshuffles) is still repaired once.
    ///
    /// Skip while the context menu is tracking: the menu lives in the
    /// topmost band — re-raising now would lift the indicator above the menu
    /// the user is reading. This branch runs only on the rare lost-bit path.
    unsafe fn maintain_topmost(hwnd: HWND) {
        if MENU_TRACKING.load(Ordering::Relaxed) {
            return;
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if (ex & WS_EX_TOPMOST.0 as isize) != 0 {
            return;
        }
        let n = TOPMOST_LOST_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if n == 1 || n % 60 == 0 {
            log_event(
                "INFO",
                &format!("topmost band lost; re-asserting (count={n})"),
            );
        }
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }

    /// Move-only placement for one overlay. Visibility follows that overlay's
    /// taskbar alone: as long as the taskbar is on screen the indicator stays
    /// visible. It hides only when the taskbar itself is gone (auto-hide or a
    /// true fullscreen app pushing it off screen). No foreground-window
    /// heuristics involved.
    unsafe fn reposition_overlay(hwnd: HWND) {
        let Some(ov) = overlay_by_hwnd(hwnd) else {
            return;
        };
        let Some(info) = taskbar_info_for(ov.taskbar) else {
            if IsWindowVisible(hwnd).as_bool() {
                log_event(
                    "INFO",
                    "overlay hidden: taskbar not on screen (auto-hide or fullscreen app)",
                );
            }
            // The hide is intentional; the next visible tick should restore
            // without reporting it as an unexpected disappearance.
            VISIBILITY_ANOMALY.store(true, Ordering::Relaxed);
            let _ = ShowWindow(hwnd, SW_HIDE);
            return;
        };
        maintain_visibility(hwnd);
        // Keep the cached monitor/edge fresh (a Windows 10 bar can be dragged
        // to another monitor or docked edge between ticks) and detect an
        // orientation flip, which resets the grow-only width and re-layouts.
        let orientation_changed = ov.vertical != info.vertical;
        // The taskbar can move to another monitor (Windows 10 drag, or a
        // display reconfiguration). Re-measure instead of keeping the width
        // grown at the old monitor's DPI; the hidden-overlay case does not
        // always deliver WM_DPICHANGED.
        let monitor_changed = ov.monitor != info.monitor;
        OVERLAYS.with(|slot| {
            if let Some(o) = slot.borrow_mut().iter_mut().find(|o| o.hwnd == hwnd) {
                o.monitor = info.monitor;
                o.left_edge = info.left_edge;
                if orientation_changed {
                    o.vertical = info.vertical;
                }
                if orientation_changed || monitor_changed {
                    o.win_w = 0;
                }
            }
        });
        if orientation_changed || monitor_changed {
            if orientation_changed {
                log_event(
                    "INFO",
                    &format!("taskbar orientation changed: vertical={}", info.vertical),
                );
            }
            if monitor_changed {
                log_event("INFO", "taskbar moved to another monitor; re-measuring");
            }
            relayout_overlay(hwnd);
            return;
        }
        apply_position_with_info(hwnd, info);
    }

    /// Place one overlay at its anchor using its measured size. Called both by
    /// the once-per-second geometry tick and immediately after a measured
    /// resize: growing the width while keeping the old left edge would push a
    /// right-anchored overlay into the tray until the next tick.
    unsafe fn apply_position(hwnd: HWND) {
        let Some(ov) = overlay_by_hwnd(hwnd) else {
            return;
        };
        let Some(info) = taskbar_info_for(ov.taskbar) else {
            if IsWindowVisible(hwnd).as_bool() {
                log_event(
                    "INFO",
                    "overlay hidden: taskbar not on screen (auto-hide or fullscreen app)",
                );
            }
            // The hide is intentional; the next visible tick should restore
            // without reporting it as an unexpected disappearance.
            VISIBILITY_ANOMALY.store(true, Ordering::Relaxed);
            let _ = ShowWindow(hwnd, SW_HIDE);
            return;
        };
        maintain_visibility(hwnd);
        apply_position_with_info(hwnd, info);
    }

    unsafe fn apply_position_with_info(hwnd: HWND, info: TaskbarInfo) {
        // Taskbar is on screen: keep the indicator in the topmost band (it can
        // be re-banded below shell flyouts between ticks; see maintain_topmost).
        maintain_topmost(hwnd);
        let Some(ov) = overlay_by_hwnd(hwnd) else {
            return;
        };
        let bar = info.rect;
        let cfg = config();
        let w = ov.win_w;
        let h = ov.win_h;
        if w <= 0 || h <= 0 {
            return; // not measured yet
        }

        let (x, y) = if info.vertical {
            // Vertical taskbar: horizontally centered within the bar thickness.
            // When the stacked column is wider than the bar's thickness,
            // centering would push the window past the screen edge it is
            // docked to and clip it. Clamp so the overflow goes into the
            // desktop instead: keep the left edge on-screen for a left-docked
            // bar, the right edge on-screen for a right-docked bar.
            let center_x = bar.left + (bar.right - bar.left - w) / 2;
            let monitor = taskbar_monitor_rect_for(ov.taskbar);
            let x = if info.left_edge {
                center_x.max(monitor.left)
            } else {
                center_x.min(monitor.right - w).max(monitor.left)
            };
            // On a vertical bar `left_margin_px` measures from the bar's TOP
            // edge. The far-left anchor concept does not exist here, so the
            // value is honored unconditionally.
            let y = bar.top + cfg.windows.left_margin_px as i32;
            (x, y)
        } else {
            // Horizontal taskbar (bottom or top).
            let left_anchor =
                cfg.windows.anchor == sysvitals_core::Anchor::Left && win11_centered_taskbar();
            let x = if left_anchor {
                bar.left + cfg.windows.left_margin_px as i32
            } else {
                // Anchor to this bar's own notification area (or its
                // show-desktop button when the bar has no tray icons).
                taskbar_anchor_left(ov.taskbar) - w
            };
            let y = bar.top + (bar.bottom - bar.top - h) / 2;
            (x, y)
        };

        if info.vertical {
            if y < bar.top || y + h > bar.bottom {
                return;
            }
        } else {
            // Negative virtual coordinates are legitimate on a monitor placed
            // left of/above the primary one; clamp to that monitor instead of
            // treating every negative number as off-screen.
            let monitor = taskbar_monitor_rect_for(ov.taskbar);
            if x < monitor.left
                || y < monitor.top
                || x + w > monitor.right
                || y + h > monitor.bottom
            {
                return; // would leave the monitor; keep last position
            }
        }
        let _ = MoveWindow(hwnd, x, y, w, h, true);
        // Only re-show when actually hidden (e.g. returning from auto-hide).
        if !IsWindowVisible(hwnd).as_bool() {
            log_event("INFO", "overlay re-shown after being hidden");
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        log_occlusion(hwnd);
    }

    // ------------------------------------------------------- measurement/render

    /// Geometry of one frame measured with the font currently selected into
    /// `hdc`.
    struct FrameMetrics {
        line_h: i32,
        want_w: i32,
        right_x: Option<i32>,
        multi_line: bool,
        item_widths: Option<Vec<i32>>,
        /// Vertical layout only: height of each line slot in pixels, in line
        /// order. Blank separator lines take `item_spacing` (the same gap the
        /// horizontal single-row layout puts between metrics) instead of a
        /// full text line, so both layouts use one consistent block gap and
        /// the column stays short. Empty for the other layouts, where every
        /// line slot has the uniform `line_h`.
        line_heights: Vec<i32>,
    }

    /// Whether a blank line is the vertical layout's inter-block separator
    /// rather than content. The vertical frame is the only multi-line,
    /// single-column frame whose lines can be empty (the stacked builder
    /// inserts one blank line between metric blocks).
    fn is_vertical_separator(line: &sysvitals_core::Line) -> bool {
        line.left.is_empty() && line.right.is_empty()
    }

    unsafe fn measure_frame(
        hdc: windows::Win32::Graphics::Gdi::HDC,
        row: &FormattedRow,
        spacing_px: i32,
        vertical: bool,
        line_pad: i32,
    ) -> FrameMetrics {
        let multi_line = row.lines.len() > 1;
        let has_right_col = row.lines.iter().any(|l| !l.right.is_empty());
        let mut line_h = 0i32;
        let mut text_w = |text: &str| -> i32 {
            let mut rc = RECT::default();
            let mut buf = utf16(text);
            DrawTextW(
                hdc,
                &mut buf,
                &mut rc,
                DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
            );
            line_h = line_h.max(rc.bottom - rc.top);
            rc.right - rc.left
        };

        // Single-row with individual items: lay out with a pixel gap.
        if !row.items.is_empty() && !multi_line {
            let mut item_widths = Vec::with_capacity(row.items.len());
            let mut total_w = 0i32;
            for (i, item) in row.items.iter().enumerate() {
                let w = text_w(item);
                item_widths.push(w);
                total_w += w;
                if i + 1 < row.items.len() {
                    total_w += spacing_px;
                }
            }
            return FrameMetrics {
                line_h,
                want_w: total_w,
                right_x: None,
                multi_line: false,
                item_widths: Some(item_widths),
                line_heights: Vec::new(),
            };
        }

        if !multi_line || !has_right_col {
            let mut want_w = 0i32;
            for line in &row.lines {
                want_w = want_w.max(text_w(&line.render()));
            }
            // Vertical layout: separator lines shrink to the configured block
            // gap so it matches the horizontal single-row metric gap; content
            // lines keep the measured text height plus a breathing margin.
            // `line_pad` is supplied by the caller so the 2x supersampled
            // measurement scales it along with the glyphs.
            let line_heights: Vec<i32> = if vertical {
                row.lines
                    .iter()
                    .map(|l| {
                        if is_vertical_separator(l) {
                            spacing_px.max(1)
                        } else {
                            line_h + line_pad
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            };
            return FrameMetrics {
                line_h,
                want_w,
                right_x: None,
                multi_line,
                item_widths: None,
                line_heights,
            };
        }
        let mut left_col_w = 0i32;
        let mut right_col_w = 0i32;
        for line in &row.lines {
            left_col_w = left_col_w.max(text_w(&line.left));
            if !line.right.is_empty() {
                right_col_w = right_col_w.max(text_w(&line.right));
            }
        }
        let right_x = left_col_w + spacing_px;
        FrameMetrics {
            line_h,
            want_w: right_x + right_col_w,
            right_x: Some(right_x),
            multi_line,
            item_widths: None,
            line_heights: Vec::new(),
        }
    }

    /// Measure the formatted rows and grow one overlay to fit. Width only
    /// ever grows; height follows the row count, and in the vertical layout
    /// the per-line slot heights from [`measure_frame`] (separator lines
    /// sized to `item_spacing`, not a full text line).
    unsafe fn resize_overlay(hwnd: HWND, row: &FormattedRow, vertical: bool) {
        let cfg = config();
        let spacing_px = cfg.display.item_spacing as i32;
        let hdc = GetDC(Some(hwnd));
        let font = make_font_for(hwnd, vertical);
        let old = SelectObject(hdc, font.into());
        let m = measure_frame(hdc, row, spacing_px, vertical, 2);
        SelectObject(hdc, old);
        let _ = DeleteObject(font.into());
        let _ = ReleaseDC(Some(hwnd), hdc);
        if m.line_h == 0 {
            return;
        }
        let want_w = m.want_w + pad_px(vertical) * 2 + width_slack_px(vertical);
        let want_h = if m.line_heights.is_empty() {
            (m.line_h + 2) * row.lines.len() as i32 + 2
        } else {
            m.line_heights.iter().sum::<i32>() + 2
        };
        OVERLAYS.with(|slot| {
            if let Some(ov) = slot.borrow_mut().iter_mut().find(|o| o.hwnd == hwnd) {
                ov.win_w = ov.win_w.max(want_w);
                ov.win_h = want_h;
            }
        });
    }

    /// One sampling tick: take a single reading, store it, write it to the
    /// history database and re-layout every overlay from it. All overlays
    /// share this one sample so network rates are computed over the full
    /// interval regardless of how many taskbars are showing.
    unsafe fn sample_tick() {
        let cfg = config();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let snapshot = SAMPLER.with(|s| {
            let mut guard = s.borrow_mut();
            let sampler = guard
                .get_or_insert_with(|| Sampler::new(NetSelection::Auto, cfg.sample.cpu_smoothing));
            sampler.sample(now)
        });
        LAST_SNAPSHOT.with(|s| *s.borrow_mut() = Some(snapshot));
        record_history(&cfg, now, &snapshot);
        relayout_all();
    }

    /// Re-layout and redraw every overlay from the last sample.
    fn relayout_all() {
        let hwnds: Vec<HWND> = OVERLAYS.with(|slot| slot.borrow().iter().map(|o| o.hwnd).collect());
        for hwnd in hwnds {
            unsafe { relayout_overlay(hwnd) };
        }
    }

    /// Rebuild one overlay's frame from the last sample with the current
    /// config, its own orientation and font. Config hot-reload, DPI/text-size
    /// changes and theme flips all use this: taking a fresh sample for an
    /// unrelated setting change used to compute network rates over a few
    /// milliseconds and paint a bogus spike. Only the periodic timer (and
    /// startup) samples.
    unsafe fn relayout_overlay(hwnd: HWND) {
        let Some(snapshot) = LAST_SNAPSHOT.with(|s| *s.borrow()) else {
            sample_tick();
            return;
        };
        let Some(ov) = overlay_by_hwnd(hwnd) else {
            return;
        };
        let cfg = config();
        let row = if ov.vertical {
            format_frame_vertical(&snapshot, &cfg)
        } else {
            format_frame(&snapshot, &cfg)
        };
        resize_overlay(hwnd, &row, ov.vertical);
        // Re-place with the new size before painting: a right-anchored window
        // must not grow into the tray until the next one-second move tick.
        apply_position(hwnd);
        render_layered(hwnd, &row, ov.vertical);
    }

    /// Insert the current CPU/MEM sample into the history database and prune
    /// old rows periodically. A failed open is retried every 300 ticks (ten
    /// minutes at the default interval); write failures are logged once.
    fn record_history(cfg: &Config, now: i64, snapshot: &sysvitals_core::Snapshot) {
        if !cfg.history.enabled {
            return;
        }
        let tick = TICK_COUNT.with(|c| {
            let v = c.get().wrapping_add(1);
            c.set(v);
            v
        });
        STORE.with(|s| {
            let mut guard = s.borrow_mut();
            if guard.is_none() && (tick == 1 || tick % 300 == 0) {
                if let Ok(path) = sysvitals_core::config::history_path() {
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    match sysvitals_core::Store::open(&path) {
                        Ok(store) => {
                            let rollup = if store.rollup_ready() {
                                "ready"
                            } else {
                                "unavailable, raw-only fallback"
                            };
                            log_event(
                                "INFO",
                                &format!(
                                    "history store opened: {} (rollup {rollup})",
                                    path.display()
                                ),
                            );
                            *guard = Some(store);
                        }
                        Err(e) => {
                            log_event(
                                "WARN",
                                &format!(
                                    "history store failed to open ({}): {}",
                                    path.display(),
                                    e
                                ),
                            );
                            *guard = None;
                        }
                    }
                }
            }
            if let Some(store) = guard.as_ref() {
                if let Err(e) = store.insert(now, snapshot.cpu_pct, snapshot.mem_pct) {
                    if !STORE_WRITE_ERROR_LOGGED.swap(true, Ordering::Relaxed) {
                        log_event("WARN", &format!("history insert failed: {e}"));
                    }
                }
                if tick % 300 == 0 {
                    if let Err(e) = store.prune(now, cfg.history.retention_days) {
                        if !STORE_WRITE_ERROR_LOGGED.swap(true, Ordering::Relaxed) {
                            log_event("WARN", &format!("history prune failed: {e}"));
                        }
                    }
                }
            }
        });
    }

    /// Render a frame into a 32bpp premultiplied-alpha DIB and blit it onto
    /// the layered window with `UpdateLayeredWindow`. Glyphs are rendered at
    /// 2x supersampled resolution and box-filtered down.
    unsafe fn render_layered(hwnd: HWND, row: &FormattedRow, vertical: bool) {
        let mut win_rc = RECT::default();
        if GetWindowRect(hwnd, &mut win_rc).is_err() {
            return;
        }
        let w = win_rc.right - win_rc.left;
        let h = win_rc.bottom - win_rc.top;
        if w <= 0 || h <= 0 {
            return;
        }

        let screen_dc = GetDC(None);
        let mem_dc = CreateCompatibleDC(Some(screen_dc));

        // --- Scratch surface at 2x supersampled scale ---------------------
        const SS: i32 = 2;
        let sw = w * SS;
        let sh = h * SS;
        let mut ss_bmi = BITMAPINFO::default();
        ss_bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        ss_bmi.bmiHeader.biWidth = sw;
        ss_bmi.bmiHeader.biHeight = -sh;
        ss_bmi.bmiHeader.biPlanes = 1;
        ss_bmi.bmiHeader.biBitCount = 32;
        ss_bmi.bmiHeader.biCompression = BI_RGB.0;
        let mut ss_bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let Ok(ss_bmp) =
            CreateDIBSection(Some(mem_dc), &ss_bmi, DIB_RGB_COLORS, &mut ss_bits, None, 0)
        else {
            let _ = DeleteDC(mem_dc);
            let _ = ReleaseDC(None, screen_dc);
            return;
        };
        let old_ss_bmp = SelectObject(mem_dc, ss_bmp.into());
        let ss_font = make_font_at_for(hwnd, SS, vertical);
        let old_ss_font = SelectObject(mem_dc, ss_font.into());
        SetBkMode(mem_dc, TRANSPARENT);
        SetTextColor(mem_dc, COLORREF(0x00FF_FFFF));

        // Clear the scratch surface to black (coverage 0).
        let ss_px = ss_bits as *mut u32;
        let ss_n = (sw * sh) as usize;
        for i in 0..ss_n {
            *ss_px.add(i) = 0;
        }

        let spacing_px = config().display.item_spacing as i32;
        // Padding follows the orientation so the vertical layout can use its
        // tighter margin; measurement and rendering must agree on it.
        let pad = pad_px(vertical) * SS;
        let fm = measure_frame(mem_dc, row, spacing_px * SS, vertical, 2 * SS);
        // Line slot tops in supersampled pixels. Uniform division works for
        // the horizontal layouts; the vertical layout has per-line heights
        // (separator lines sized to `item_spacing`, content lines sized to
        // the text height + breathing), so tops accumulate from that table.
        let ss_line_h = sh / row.lines.len().max(1) as i32;
        let mut tops: Vec<(i32, i32)> = Vec::with_capacity(row.lines.len());
        if fm.line_heights.is_empty() {
            for i in 0..row.lines.len() {
                tops.push((i as i32 * ss_line_h, (i as i32 + 1) * ss_line_h));
            }
        } else {
            let mut acc = 0i32;
            for h in &fm.line_heights {
                tops.push((acc, acc + h));
                acc += h;
            }
        }
        for (i, line) in row.lines.iter().enumerate() {
            let (top, bottom) = tops.get(i).copied().unwrap_or((0, sh));
            if let Some(ref item_widths) = fm.item_widths {
                // Single-row pixel-spacing layout.
                let gap = spacing_px * SS;
                let total_w: i32 = item_widths.iter().sum::<i32>()
                    + gap * (row.items.len().saturating_sub(1) as i32);
                let mut x = (sw - total_w) / 2;
                for (j, item) in row.items.iter().enumerate() {
                    let mut rc = RECT {
                        left: x,
                        top,
                        right: x + item_widths[j],
                        bottom,
                    };
                    let mut buf = utf16(item);
                    DrawTextW(
                        mem_dc,
                        &mut buf,
                        &mut rc,
                        DT_SINGLELINE | DT_LEFT | DT_VCENTER | DT_NOPREFIX,
                    );
                    x += item_widths[j] + gap;
                }
            } else if let Some(right_x) = fm.right_x {
                // Dual-row: left column left-aligned, right column at offset.
                let mut left_rc = RECT {
                    left: pad,
                    top,
                    right: pad + right_x,
                    bottom,
                };
                let mut lbuf = utf16(&line.left);
                DrawTextW(
                    mem_dc,
                    &mut lbuf,
                    &mut left_rc,
                    DT_SINGLELINE | DT_LEFT | DT_VCENTER | DT_NOPREFIX,
                );
                if !line.right.is_empty() {
                    let mut right_rc = RECT {
                        left: pad + right_x,
                        top,
                        right: sw - pad,
                        bottom,
                    };
                    let mut rbuf = utf16(&line.right);
                    DrawTextW(
                        mem_dc,
                        &mut rbuf,
                        &mut right_rc,
                        DT_SINGLELINE | DT_LEFT | DT_VCENTER | DT_NOPREFIX,
                    );
                }
            } else if fm.multi_line {
                // Multi-line single column: left-aligned.
                let text = line.render();
                let mut line_rc = RECT {
                    left: pad,
                    top,
                    right: sw - pad,
                    bottom,
                };
                let mut buf = utf16(&text);
                DrawTextW(
                    mem_dc,
                    &mut buf,
                    &mut line_rc,
                    DT_SINGLELINE | DT_LEFT | DT_VCENTER | DT_NOPREFIX,
                );
            } else {
                // Single row: centered.
                let text = line.render();
                let mut line_rc = RECT {
                    left: 0,
                    top,
                    right: sw,
                    bottom,
                };
                let mut buf = utf16(&text);
                DrawTextW(
                    mem_dc,
                    &mut buf,
                    &mut line_rc,
                    DT_SINGLELINE | DT_CENTER | DT_VCENTER | DT_NOPREFIX,
                );
            }
        }

        SelectObject(mem_dc, old_ss_font);
        let _ = DeleteObject(ss_font.into());

        // --- Final surface at window scale, downsampled 2x2 ---------------
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = w;
        bmi.bmiHeader.biHeight = -h;
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let Ok(bmp) = CreateDIBSection(Some(mem_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
        else {
            SelectObject(mem_dc, old_ss_bmp);
            let _ = DeleteObject(ss_bmp.into());
            let _ = DeleteDC(mem_dc);
            let _ = ReleaseDC(None, screen_dc);
            return;
        };

        let light_theme = theme_is_light();
        let (tr, tg, tb) = if light_theme {
            (0u32, 0u32, 0u32)
        } else {
            (255u32, 255u32, 255u32)
        };
        let px = bits as *mut u32;
        // Glyph-only composition. The indicator draws no hover/pressed/open
        // background at all: the whole rectangle keeps a minimal (but
        // non-zero) alpha so it stays hit-testable over the taskbar, and the
        // supersampled glyph coverage is written premultiplied for
        // `AC_SRC_ALPHA`.
        for y in 0..h {
            for x in 0..w {
                let mut acc = 0u32;
                for dy in 0..SS {
                    for dx in 0..SS {
                        let sx = x * SS + dx;
                        let sy = y * SS + dy;
                        acc += *ss_px.add((sy * sw + sx) as usize) & 0xFF;
                    }
                }
                let cov = acc / (SS * SS) as u32;
                if cov == 0 {
                    *px.add((y * w + x) as usize) = 0x0100_0000;
                } else {
                    let mix = |glyph: u32| glyph * cov / 255;
                    *px.add((y * w + x) as usize) =
                        (cov << 24) | (mix(tr) << 16) | (mix(tg) << 8) | mix(tb);
                }
            }
        }

        SelectObject(mem_dc, old_ss_bmp);
        let _ = DeleteObject(ss_bmp.into());

        let old_bmp = SelectObject(mem_dc, bmp.into());
        let src_pt = POINT { x: 0, y: 0 };
        let size = SIZE { cx: w, cy: h };
        let dst_pt = POINT {
            x: win_rc.left,
            y: win_rc.top,
        };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        match UpdateLayeredWindow(
            hwnd,
            None,
            Some(&dst_pt),
            Some(&size),
            Some(mem_dc),
            Some(&src_pt),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        ) {
            Ok(()) => LAYERED_FAIL_LOGGED.store(false, Ordering::Relaxed),
            Err(e) => {
                if !LAYERED_FAIL_LOGGED.swap(true, Ordering::Relaxed) {
                    log_event("WARN", &format!("UpdateLayeredWindow failed: {e}"));
                }
            }
        }

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(None, screen_dc);
    }

    // ------------------------------------------------- config file watching

    /// Read the config file's last-modified time as nanoseconds since UNIX
    /// epoch. Returns 0 when the file does not exist or cannot be read.
    fn config_mtime() -> u64 {
        let Ok(path) = sysvitals_core::config::config_path() else {
            return 0;
        };
        let Ok(meta) = std::fs::metadata(&path) else {
            return 0;
        };
        let Ok(modified) = meta.modified() else {
            return 0;
        };
        modified
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }

    /// Called on each TIMER_CONFIG tick. Compares the config file's mtime
    /// against the cached value; on change, reloads the config, re-seeds the
    /// sample timer (interval may have changed), and re-lays out every overlay.
    fn poll_config_change() {
        let current = config_mtime();
        let previous = CONFIG_MTIME.load(Ordering::Relaxed);
        if current == previous {
            return;
        }
        let loaded =
            sysvitals_core::config::config_path().and_then(|p| Config::load(&p).map(|(c, _)| c));
        let loaded = match loaded {
            Ok(c) => c,
            Err(e) => {
                // Keep the last good config. Do not advance CONFIG_MTIME, so a
                // fixed file is picked up on the next poll; throttle the log
                // to one line per broken mtime.
                if CONFIG_ERROR_MTIME.swap(current, Ordering::Relaxed) != current {
                    log_event("WARN", &format!("config reload failed: {e}"));
                }
                return;
            }
        };
        CONFIG_MTIME.store(current, Ordering::Relaxed);
        CONFIG_ERROR_MTIME.store(0, Ordering::Relaxed);
        store_config(loaded.clone());
        log_event(
            "INFO",
            &format!(
                "config file change detected: interval={}ms language={:?} theme={:?} \
                 dual_row={} show_resources={} show_network={} item_spacing={}px \
                 anchor={:?} left_margin={}px",
                loaded.sample.interval_ms,
                loaded.display.language,
                loaded.display.theme,
                loaded.display.dual_row,
                loaded.display.show_resources,
                loaded.display.show_network,
                loaded.display.item_spacing,
                loaded.windows.anchor,
                loaded.windows.left_margin_px,
            ),
        );
        // Re-seed the sample timer in case the interval changed.
        unsafe {
            let sentinel = sentinel_hwnd();
            let _ = KillTimer(Some(sentinel), TIMER_SAMPLE);
            SetTimer(
                Some(sentinel),
                TIMER_SAMPLE,
                loaded.sample.interval_ms.clamp(500, 5000) as u32,
                None,
            );
        }
        // Reset the grow-only widths so a narrower layout takes effect, then
        // re-lay out every overlay from the last sample (no re-sample spike).
        OVERLAYS.with(|slot| {
            for ov in slot.borrow_mut().iter_mut() {
                ov.win_w = 0;
            }
        });
        relayout_all();
    }

    // ------------------------------------------------------- child process launch

    /// Whether the child process behind `slot` is still running. Reaps the
    /// handle when the process has exited.
    fn child_alive(slot: &std::cell::RefCell<Option<HANDLE>>) -> bool {
        let mut borrow = slot.borrow_mut();
        let Some(handle) = borrow.as_ref() else {
            return false;
        };
        if handle.is_invalid() {
            *borrow = None;
            return false;
        }
        let wait = unsafe { WaitForSingleObject(*handle, 0) };
        if wait == WAIT_TIMEOUT {
            true
        } else {
            let _ = unsafe { CloseHandle(*handle) };
            *borrow = None;
            false
        }
    }

    /// Terminate the process behind `slot` if it is still running. The handle
    /// is closed and the slot cleared either way. Returns `true` only when a
    /// live process was terminated. `wait_for_exit` blocks briefly until the
    /// process object is signaled, which also guarantees that a child's named
    /// single-instance mutex has been released before a replacement starts.
    fn terminate_child(
        slot: &std::cell::RefCell<Option<HANDLE>>,
        label: &str,
        wait_for_exit: bool,
    ) -> bool {
        let mut borrow = slot.borrow_mut();
        let Some(handle) = borrow.as_ref() else {
            return false;
        };
        if handle.is_invalid() {
            *borrow = None;
            return false;
        }
        // Already exited: reap quietly, treat as "nothing to close".
        if unsafe { WaitForSingleObject(*handle, 0) } != WAIT_TIMEOUT {
            let _ = unsafe { CloseHandle(*handle) };
            *borrow = None;
            return false;
        }
        let _ = unsafe { TerminateProcess(*handle, 0) };
        if wait_for_exit {
            // Waits for the process object, which is signaled only after the
            // child's handles are closed — including its single-instance
            // mutex. Without this, a replacement launched immediately could
            // see the dying instance and exit.
            let wait = unsafe { WaitForSingleObject(*handle, 2_000) };
            if wait == WAIT_TIMEOUT {
                log_event("WARN", &format!("{label} did not exit within 2s"));
            }
        }
        let _ = unsafe { CloseHandle(*handle) };
        *borrow = None;
        log_event("INFO", &format!("{label} process closed by indicator"));
        true
    }

    /// Terminate the resident history process. Called only on restart/quit:
    /// a normal popup close is a shared-state request that only removes the
    /// window, leaving the framework warm.
    fn history_terminate() -> bool {
        let mut slot = HISTORY_PROCESS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(raw) = slot.take() else {
            return false;
        };
        let handle = HANDLE(raw as *mut std::ffi::c_void);
        if handle.is_invalid() {
            return false;
        }
        if unsafe { WaitForSingleObject(handle, 0) } != WAIT_TIMEOUT {
            let _ = unsafe { CloseHandle(handle) };
            return false;
        }
        let _ = unsafe { TerminateProcess(handle, 0) };
        let _ = unsafe { CloseHandle(handle) };
        log_event("INFO", "history process closed by indicator");
        true
    }

    /// Close both child panels on restart/quit. Leaving them alive makes a
    /// restarted instance open a second History window (its slot starts
    /// empty) and leaves Settings orphaned after Quit.
    fn close_children() {
        let _ = history_terminate();
        SETTINGS_PROCESS.with(|slot| {
            // Quit/restart: the relaunch is delayed by the caller, so there is
            // no single-instance race to wait out here.
            let _ = terminate_child(slot, "settings window", false);
        });
    }

    /// Create the kill-on-close job that owns every panel child. Keeping the
    /// job handle for the process lifetime means an indicator crash, a quit
    /// during a slow `CreateProcessW`, or an explorer restart can never leave
    /// an orphan Settings/History process behind.
    unsafe fn init_child_job() {
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
    unsafe fn assign_to_child_job(process: HANDLE) {
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
    /// (the thread handle is closed here). Used both by the synchronous
    /// Settings launch and by the background resident-history launcher.
    fn spawn_process(exe_path: &str, dir: &str, args: Option<&str>) -> Result<HANDLE, String> {
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

    fn launch_child_inner(
        exe_name: &str,
        args: Option<&str>,
        slot: &std::cell::RefCell<Option<HANDLE>>,
    ) -> bool {
        let Some(dir) = current_exe_dir() else {
            log_event(
                "WARN",
                &format!("launch_child: cannot determine exe directory: {exe_name}"),
            );
            return false;
        };
        let exe_path = format!("{dir}\\{exe_name}");
        if !std::path::Path::new(&exe_path).exists() {
            log_event("WARN", &format!("launch_child: not found: {exe_path}"));
            return false;
        }
        log_event(
            "INFO",
            &format!(
                "launching {exe_name}{}",
                args.map(|a| format!(" {a}")).unwrap_or_default()
            ),
        );
        let started = Instant::now();
        match spawn_process(&exe_path, &dir, args) {
            Ok(handle) => {
                unsafe { assign_to_child_job(handle) };
                *slot.borrow_mut() = Some(handle);
                log_event(
                    "INFO",
                    &format!("launched {exe_name} in {}ms", started.elapsed().as_millis()),
                );
                true
            }
            Err(e) => {
                log_event(
                    "WARN",
                    &format!("launch_child: CreateProcessW failed for {exe_name}: {e}"),
                );
                false
            }
        }
    }

    /// Launch the external settings process (`SysVitalsSettings.exe`). The
    /// launch is synchronous (`CreateProcessW` does not pump messages), so
    /// the slot liveness check alone is enough; there is no in-flight window
    /// to guard. Pass the parent pid so the panel can close itself if the
    /// indicator dies without running its quit path, and the clicked
    /// indicator's rect so the window opens on that indicator's monitor.
    fn launch_settings(hwnd: HWND) {
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
        SETTINGS_PROCESS.with(|slot| {
            if child_alive(slot) {
                if SETTINGS_MONITOR.load(Ordering::Relaxed) == monitor {
                    log_event("INFO", "settings launch skipped: already on this monitor");
                    return;
                }
                // GPUI cannot move an existing window, so a Settings click on
                // another screen closes the window and reopens it there.
                // Every setting is persisted on change, so nothing is lost
                // except transient scroll/focus state.
                log_event("INFO", "settings: moving window to the clicked monitor");
                let _ = terminate_child(slot, "settings window", true);
            }
            SETTINGS_MONITOR.store(monitor, Ordering::Relaxed);
            let _ = launch_child_inner("SysVitalsSettings.exe", Some(&args), slot);
        });
    }

    /// Whether the resident history framework is running or being launched.
    fn history_busy() -> bool {
        history_child_alive() || HISTORY_LAUNCHING.load(Ordering::SeqCst)
    }

    /// Reap the resident history process if it exited (crash, manual kill).
    fn history_child_alive() -> bool {
        let mut slot = HISTORY_PROCESS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(raw) = *slot else {
            return false;
        };
        let handle = HANDLE(raw as *mut std::ffi::c_void);
        if handle.is_invalid() {
            *slot = None;
            return false;
        }
        if unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT {
            true
        } else {
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
    }

    /// Create (or reopen) the shared history state block. Kept alive for the
    /// process lifetime; the resident child maps it by name.
    fn history_state_ptr() -> Option<*mut sysvitals_core::HistoryState> {
        HISTORY_STATE.with(|slot| {
            if slot.borrow().is_none() {
                let name = utf16(sysvitals_core::HISTORY_STATE_MAP);
                let size = std::mem::size_of::<sysvitals_core::HistoryState>();
                let mapping = match unsafe {
                    CreateFileMappingW(
                        INVALID_HANDLE_VALUE,
                        None,
                        PAGE_READWRITE,
                        0,
                        size as u32,
                        windows::core::PCWSTR(name.as_ptr()),
                    )
                } {
                    Ok(mapping) => mapping,
                    Err(e) => {
                        log_event("WARN", &format!("history state mapping failed: {e}"));
                        return None;
                    }
                };
                let view = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, size) };
                if view.Value.is_null() {
                    let _ = unsafe { CloseHandle(mapping) };
                    log_event("WARN", "history state map view failed");
                    return None;
                }
                let ptr = view.Value as *mut sysvitals_core::HistoryState;
                unsafe { std::ptr::write_volatile(ptr, sysvitals_core::HistoryState::new()) };
                *slot.borrow_mut() = Some((ptr, mapping));
            }
            slot.borrow().as_ref().map(|(ptr, _)| *ptr)
        })
    }

    fn history_snapshot() -> Option<sysvitals_core::HistoryState> {
        let ptr = history_state_ptr()?;
        Some(unsafe { sysvitals_core::HistoryState::read_consistent(ptr) })
    }

    /// Ask the resident child to close. The generation moves with the
    /// request, so the resident loop can tell this close apart from the
    /// generation that opened the window and will not reopen it while a
    /// newer click is pending.
    fn history_close_request() {
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
    fn history_mark_stopped() {
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
    fn window_monitor_id(hwnd: HWND) -> i64 {
        if let Some(ov) = overlay_by_hwnd(hwnd) {
            return ov.monitor as i64;
        }
        unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST).0 as i64 }
    }

    /// A window's rect converted to GPUI global logical pixels (physical
    /// pixels divided by the window's own monitor DPI). Used as the history
    /// popup anchor and as the settings window's monitor hint.
    fn window_anchor_logical(hwnd: HWND) -> Option<[f32; 4]> {
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
    fn history_open_request(hwnd: HWND) -> bool {
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
    fn ensure_history_resident() {
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
        std::thread::spawn(move || {
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
    }

    /// Reap a dead resident process and clear a request it left behind, so the
    /// next click is not mistaken for a toggle-close of a popup that is gone.
    fn sync_history_state() {
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

    /// Best-effort: return the directory containing the current executable.
    fn current_exe_dir() -> Option<String> {
        let mut buf = [0u16; 1024];
        let n = unsafe { GetModuleFileNameW(None, &mut buf) };
        if n == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..n as usize]);
        path.rfind(['\\', '/']).map(|i| path[..i].to_string())
    }

    /// Restart the overlay: schedule a delayed relaunch of the current exe
    /// (after the single-instance mutex is released on exit), then exit.
    fn restart_self() {
        let mut buf = [0u16; 1024];
        let n = unsafe { GetModuleFileNameW(None, &mut buf) };
        if n == 0 {
            log_event("WARN", "restart_self: cannot determine exe path");
            return;
        }
        let exe = String::from_utf16_lossy(&buf[..n as usize]);
        // Delay 2 seconds so this process exits and releases the mutex first.
        let cmd = format!("/c timeout /t 2 /nobreak >nul & start \"\" \"{}\"", exe);
        let cmd_w = utf16(&cmd);
        let verb = utf16("open");
        let target = utf16("cmd.exe");
        let params = windows::core::PCWSTR(cmd_w.as_ptr());
        // Close child panels first: the new instance starts with empty slots
        // and would otherwise open a second History window next to the old one.
        close_children();
        log_event("INFO", "restart_self: scheduling relaunch and exiting");
        unsafe {
            let _ = ShellExecuteW(
                None,
                windows::core::PCWSTR(verb.as_ptr()),
                windows::core::PCWSTR(target.as_ptr()),
                params,
                windows::core::PCWSTR::null(),
                SW_HIDE,
            );
            shutdown_and_quit();
        }
    }

    // -------------------------------------------------------------------- menu

    /// Launch the system resource viewer (Task Manager). This action used to
    /// live on a double-click of the indicator; putting it in the context
    /// menu is what lets the left click open the trend chart immediately,
    /// without waiting for a possible second press.
    unsafe fn launch_taskmgr() {
        let verb = utf16("open");
        let target = utf16("taskmgr.exe");
        let _ = ShellExecuteW(
            None,
            windows::core::PCWSTR(verb.as_ptr()),
            windows::core::PCWSTR(target.as_ptr()),
            windows::core::PCWSTR::null(),
            windows::core::PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }

    /// One row of the context menu. Separators use `id == 0` and an empty
    /// label.
    #[derive(Clone)]
    struct MenuEntry {
        id: usize,
        label: String,
        separator: bool,
    }

    fn menu_entries(lang: Language) -> Vec<MenuEntry> {
        // Order: system resource viewer first (most actionable when the
        // indicator is in the way), then Settings, then the lifecycle group.
        vec![
            MenuEntry {
                id: IDM_TASKMGR,
                label: t(lang, "menu.task_manager").to_string(),
                separator: false,
            },
            MenuEntry {
                id: IDM_SETTINGS,
                label: t(lang, "menu.settings").to_string(),
                separator: false,
            },
            MenuEntry {
                id: 0,
                label: String::new(),
                separator: true,
            },
            MenuEntry {
                id: IDM_RESTART,
                label: t(lang, "menu.restart").to_string(),
                separator: false,
            },
            MenuEntry {
                id: IDM_QUIT,
                label: t(lang, "menu.quit").to_string(),
                separator: false,
            },
        ]
    }

    /// The context menu is the system popup menu. It is the only menu
    /// surface the shell renders natively, so it keeps system theming,
    /// hit-testing and dismissal semantics without a second message loop.
    unsafe fn show_native_menu(owner: HWND, entries: &[MenuEntry]) -> Option<usize> {
        let menu = CreatePopupMenu().ok()?;
        for entry in entries {
            if entry.separator {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, windows::core::PCWSTR::null());
            } else {
                let label = utf16(&entry.label);
                let _ = AppendMenuW(
                    menu,
                    MF_STRING,
                    entry.id,
                    windows::core::PCWSTR(label.as_ptr()),
                );
            }
        }
        let mut point = POINT::default();
        let _ = GetCursorPos(&mut point);
        // The overlay is WS_EX_NOACTIVATE, so the classic menu needs the
        // documented tray pattern to dismiss on a click-away: foreground the
        // owner first, then post WM_NULL after it returns.
        let _ = SetForegroundWindow(owner);
        let choice = TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD,
            point.x,
            point.y,
            None,
            owner,
            None,
        )
        .0 as usize;
        let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        Some(choice)
    }

    unsafe fn perform_menu_choice(hwnd: HWND, choice: usize) {
        match choice {
            id if id == IDM_TASKMGR => launch_taskmgr(),
            id if id == IDM_SETTINGS => launch_settings(hwnd),
            id if id == IDM_RESTART => restart_self(),
            id if id == IDM_QUIT => {
                close_children();
                shutdown_and_quit();
            }
            _ => {}
        }
    }

    /// Destroy every overlay, stop the sentinel timers and exit the message
    /// loop. Shared by Quit and Restart (which schedules a relaunch first).
    /// `QUITTING` is set before the windows are torn down so their
    /// `WM_DESTROY` handlers treat it as an intentional exit.
    unsafe fn shutdown_and_quit() {
        QUITTING.store(true, Ordering::Relaxed);
        let hwnds: Vec<HWND> = OVERLAYS.with(|slot| slot.borrow().iter().map(|o| o.hwnd).collect());
        for hwnd in hwnds {
            let _ = DestroyWindow(hwnd);
        }
        OVERLAYS.with(|slot| slot.borrow_mut().clear());
        let sentinel = sentinel_hwnd();
        let _ = KillTimer(Some(sentinel), TIMER_SAMPLE);
        let _ = KillTimer(Some(sentinel), TIMER_MOVE);
        let _ = KillTimer(Some(sentinel), TIMER_CONFIG);
        let _ = KillTimer(Some(sentinel), TIMER_HISTORY);
        let _ = KillTimer(Some(sentinel), TIMER_WATCH);
        PostQuitMessage(0);
    }

    unsafe fn show_context_menu(hwnd: HWND) {
        let entries = menu_entries(config().display.language);
        // Keep maintain_topmost from lifting the overlay above the menu while
        // it is open (the modal loop still dispatches our WM_TIMER).
        MENU_TRACKING.store(true, Ordering::Relaxed);
        let choice = match show_native_menu(hwnd, &entries) {
            Some(choice) => choice,
            None => {
                log_event("WARN", "menu: the system menu could not be created");
                0
            }
        };
        MENU_TRACKING.store(false, Ordering::Relaxed);
        perform_menu_choice(hwnd, choice);
    }

    // ----------------------------------------------------------------- wndproc

    /// Left-click toggle. Opens the popup anchored at this indicator when it
    /// is closed. When it is open, a click on the same monitor's indicator
    /// closes it (toggle); a click on a different monitor's indicator moves it
    /// there. `visible` (owned by the child) is the actual open state;
    /// `desired_open` alone is not (it stays 1 after a blur/Esc dismissal).
    fn handle_history_click(hwnd: HWND) {
        if !history_child_alive() {
            history_mark_stopped();
            HISTORY_ANCHOR_MONITOR.store(0, Ordering::Relaxed);
        }
        let visible = history_snapshot().is_some_and(|state| state.visible != 0);
        let this_monitor = overlay_by_hwnd(hwnd).map(|o| o.monitor).unwrap_or(0);
        if !visible {
            ensure_history_resident();
            if history_open_request(hwnd) {
                HISTORY_ANCHOR_MONITOR.store(this_monitor, Ordering::Relaxed);
            }
        } else if HISTORY_ANCHOR_MONITOR.load(Ordering::Relaxed) == this_monitor {
            history_close_request();
            HISTORY_ANCHOR_MONITOR.store(0, Ordering::Relaxed);
        } else {
            // A different monitor's indicator was clicked while the popup is
            // open: republish the open request with this indicator's anchor.
            // The resident loop sees the generation change while a window is
            // open and reopens it there.
            log_event(
                "INFO",
                "history: moving popup to the clicked indicator's screen",
            );
            if history_open_request(hwnd) {
                HISTORY_ANCHOR_MONITOR.store(this_monitor, Ordering::Relaxed);
            }
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_SETTINGCHANGE => {
                // Theme, text-size and font changes arrive here. Re-read the
                // theme, drop the cached font metrics and re-measure: the
                // previous code repainted with the old width, clipping the
                // new weight/size until the next sample tick.
                let old_light = LIGHT_THEME.load(Ordering::Relaxed);
                refresh_theme();
                let new_light = LIGHT_THEME.load(Ordering::Relaxed);
                if old_light != new_light {
                    log_event("INFO", &format!("theme changed: light={}", new_light == 1));
                }
                invalidate_ui_font();
                reset_width(hwnd);
                relayout_overlay(hwnd);
                LRESULT(0)
            }
            WM_DPICHANGED => {
                log_event(
                    "INFO",
                    &format!(
                        "WM_DPICHANGED: dpi={} (system {})",
                        GetDpiForWindow(hwnd),
                        GetDpiForSystem()
                    ),
                );
                invalidate_ui_font();
                reset_width(hwnd);
                relayout_overlay(hwnd);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                // Toggle the trend chart. There is no deferral: the class
                // style dropped `CS_DBLCLKS`, so Windows does not synthesize a
                // `WM_LBUTTONDBLCLK`, and Task Manager lives in the
                // right-click menu. Any left click is a left click. A press
                // paints nothing: the indicator has no pressed/hover backdrop.
                handle_history_click(hwnd);
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                show_context_menu(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                OVERLAYS.with(|slot| slot.borrow_mut().retain(|o| o.hwnd != hwnd));
                if !QUITTING.load(Ordering::Relaxed) {
                    // The taskbar owns this window; explorer restarted and
                    // destroyed it. The sentinel's reconcile recreates it
                    // against the new taskbar. The resident History process is
                    // independent and keeps running.
                    log_event(
                        "INFO",
                        "overlay destroyed with its taskbar; reconcile will recreate",
                    );
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    // ------------------------------------------------------------ crash hooks

    /// Install crash diagnostics: a Rust panic hook and a Win32 unhandled
    /// exception filter. Both write the crash reason to the log file so a
    /// silent process death becomes diagnosable.
    fn install_crash_hooks() {
        std::panic::set_hook(Box::new(|info| {
            let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };
            let loc = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_else(|| "unknown location".to_string());
            let thread = std::thread::current()
                .name()
                .unwrap_or("unnamed")
                .to_string();
            let backtrace = std::backtrace::Backtrace::force_capture();
            log_event(
                "FATAL",
                &format!("panic in thread {thread} at {loc}: {msg} | backtrace: {backtrace}"),
            );
        }));
        unsafe {
            SetUnhandledExceptionFilter(Some(unhandled_exception_filter));
        }
    }

    unsafe extern "system" fn unhandled_exception_filter(info: *const EXCEPTION_POINTERS) -> i32 {
        let mut code = 0u32;
        let mut addr = 0usize;
        if !info.is_null() && !(*info).ExceptionRecord.is_null() {
            let rec = &*(*info).ExceptionRecord;
            code = rec.ExceptionCode.0 as u32;
            addr = rec.ExceptionAddress as usize;
        }
        let module = module_name_for_address(addr as *mut std::ffi::c_void);
        log_event(
            "FATAL",
            &format!(
                "unhandled exception: 0x{code:08X} at 0x{addr:016X} in {module} — process will terminate"
            ),
        );
        // EXCEPTION_EXECUTE_HANDLER: let the OS terminate after our logging.
        1
    }

    /// Best-effort: return the module file name that contains `addr`.
    unsafe fn module_name_for_address(addr: *mut std::ffi::c_void) -> String {
        let mut hmod = windows::Win32::Foundation::HMODULE::default();
        let ok = GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            windows::core::PCWSTR(addr as *const u16),
            &mut hmod,
        )
        .is_ok();
        if !ok {
            return "<unknown module>".to_string();
        }
        let mut buf = [0u16; 512];
        let n = GetModuleFileNameW(Some(hmod), &mut buf);
        if n == 0 {
            return "<unknown module>".to_string();
        }
        let path = String::from_utf16_lossy(&buf[..n as usize]);
        path.rsplit(['\\', '/'])
            .next()
            .unwrap_or("<unknown module>")
            .to_string()
    }

    /// Primary taskbar window. Re-queried after every explorer restart; the
    /// overlay uses it as its owner so the two share a z-band.
    unsafe fn find_taskbar() -> Option<HWND> {
        let cls = utf16("Shell_TrayWnd");
        FindWindowW(windows::core::PCWSTR(cls.as_ptr()), None).ok()
    }

    /// Create the layered overlay, optionally owned by the taskbar.
    ///
    /// Owner (not parent) is what makes Start menu / Task View safe: those
    /// shell surfaces promote `Shell_TrayWnd` from z-band 1 to band 6, and an
    /// owned window follows its owner's band automatically. `HWND_TOPMOST`
    /// alone cannot cross bands, which is why the previous per-tick assertion
    /// only produced a one-second flicker.
    unsafe fn create_overlay(owner: Option<HWND>) -> Option<HWND> {
        let hmodule = GetModuleHandleW(None).ok()?;
        let class_name = utf16("SysVitalsOverlay");
        let title = utf16("SysVitals");
        let hwnd = CreateWindowExW(
            // `WS_EX_NOACTIVATE` keeps a click from pulling foreground away
            // from the History popup; the popup activates itself on reveal.
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            windows::core::PCWSTR(class_name.as_ptr()),
            windows::core::PCWSTR(title.as_ptr()),
            WS_POPUP,
            -32_000,
            -32_000,
            200,
            24,
            owner,
            None,
            Some(hmodule.into()),
            None,
        )
        .ok()?;
        // Topmost so the overlay sits above the taskbar's own content.
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        Some(hwnd)
    }

    /// Bring the set of overlays in line with the set of taskbars: destroy
    /// overlays whose taskbar is gone and create overlays for taskbars that do
    /// not have one yet. Called at startup, on `TaskbarCreated`, on display
    /// changes, and by the move tick (which also catches secondary bars that
    /// appear a moment after the main one, and hot-plugged monitors).
    unsafe fn reconcile_overlays() {
        if QUITTING.load(Ordering::Relaxed) {
            return;
        }
        let bars = taskbar_windows();
        // Drop overlays whose taskbar no longer exists. Collect first and
        // destroy after releasing the registry borrow: `DestroyWindow` sends
        // `WM_DESTROY` synchronously, and that handler re-borrows `OVERLAYS`.
        let mut to_destroy = Vec::new();
        OVERLAYS.with(|slot| {
            let mut overlays = slot.borrow_mut();
            let mut i = 0;
            while i < overlays.len() {
                if IsWindow(Some(overlays[i].taskbar)).as_bool() {
                    i += 1;
                    continue;
                }
                let ov = overlays.remove(i);
                to_destroy.push(ov.hwnd);
                log_event("INFO", "overlay removed: its taskbar disappeared");
            }
        });
        for hwnd in to_destroy {
            if IsWindow(Some(hwnd)).as_bool() {
                let _ = DestroyWindow(hwnd);
            }
        }
        // Create overlays for taskbars that lack one.
        for &bar in &bars {
            let exists = OVERLAYS.with(|slot| slot.borrow().iter().any(|o| o.taskbar == bar));
            if exists {
                continue;
            }
            let Some(hwnd) = create_overlay(Some(bar)) else {
                if !OVERLAY_CREATE_FAIL_LOGGED.swap(true, Ordering::Relaxed) {
                    log_event("WARN", "overlay creation failed for a taskbar");
                }
                continue;
            };
            OVERLAY_CREATE_FAIL_LOGGED.store(false, Ordering::Relaxed);
            let info = taskbar_info_for(bar);
            let (vertical, left_edge, monitor) = match info {
                Some(i) => (i.vertical, i.left_edge, i.monitor),
                None => (
                    false,
                    false,
                    MonitorFromWindow(bar, MONITOR_DEFAULTTONEAREST).0 as isize,
                ),
            };
            OVERLAYS.with(|slot| {
                slot.borrow_mut().push(Overlay {
                    hwnd,
                    taskbar: bar,
                    monitor,
                    vertical,
                    left_edge,
                    win_w: 0,
                    win_h: 0,
                })
            });
            log_event(
                "INFO",
                &format!("overlay created for a taskbar (vertical={vertical})"),
            );
            // Show before the first layout so the placement pass does not log
            // a spurious "re-shown"; the window starts off-screen anyway.
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            relayout_overlay(hwnd);
        }
    }

    /// Re-place every overlay against its own taskbar. Per-overlay show/hide
    /// follows each taskbar's visibility (auto-hide, fullscreen on that
    /// monitor), so one hidden bar does not affect the others.
    fn reposition_all() {
        let hwnds: Vec<HWND> = OVERLAYS.with(|slot| slot.borrow().iter().map(|o| o.hwnd).collect());
        for hwnd in hwnds {
            unsafe { reposition_overlay(hwnd) };
        }
    }

    /// Hidden top-level window that survives explorer restarts. It owns every
    /// periodic timer and receives the `TaskbarCreated` broadcast, driving all
    /// overlays at once (the overlays themselves are owned by their taskbars
    /// and die with them, so they cannot be the ones holding the timers).
    unsafe extern "system" fn sentinel_wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if TASKBAR_CREATED.get().is_some_and(|created| *created == msg) {
            log_event("INFO", "TaskbarCreated received; reconciling overlays");
            let _ = KillTimer(Some(hwnd), TIMER_REBIND);
            if find_taskbar().is_some() {
                reconcile_overlays();
                reposition_all();
            } else {
                let _ = SetTimer(Some(hwnd), TIMER_REBIND, REBIND_RETRY_MS, None);
            }
            return LRESULT(0);
        }
        if msg == WM_APP_HISTORY_READY {
            sync_history_state();
            return LRESULT(0);
        }
        if msg == WM_DISPLAYCHANGE {
            log_event("INFO", "WM_DISPLAYCHANGE: display configuration changed");
            // Monitor handles are not stable across a display reconfiguration;
            // forget which screen anchors the popup so the next click opens
            // or moves it again instead of being mistaken for a same-screen
            // toggle.
            HISTORY_ANCHOR_MONITOR.store(0, Ordering::Relaxed);
            invalidate_ui_font();
            OVERLAYS.with(|slot| {
                for ov in slot.borrow_mut().iter_mut() {
                    ov.win_w = 0;
                }
            });
            reconcile_overlays();
            relayout_all();
            return LRESULT(0);
        }
        if msg == WM_TIMER {
            match wparam.0 {
                TIMER_SAMPLE => {
                    sample_tick();
                    return LRESULT(0);
                }
                TIMER_MOVE => {
                    reconcile_overlays();
                    reposition_all();
                    return LRESULT(0);
                }
                TIMER_CONFIG => {
                    poll_config_change();
                    return LRESULT(0);
                }
                TIMER_HISTORY => {
                    sync_history_state();
                    return LRESULT(0);
                }
                TIMER_REBIND => {
                    if find_taskbar().is_some() {
                        let _ = KillTimer(Some(hwnd), TIMER_REBIND);
                        reconcile_overlays();
                        reposition_all();
                    }
                    return LRESULT(0);
                }
                TIMER_WATCH => {
                    if !QUITTING.load(Ordering::Relaxed) {
                        reconcile_overlays();
                    }
                    return LRESULT(0);
                }
                _ => {}
            }
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    // --------------------------------------------------------------------- run

    pub fn run() {
        unsafe {
            // Crash diagnostics first.
            install_crash_hooks();

            // Single-instance guard.
            let mutex_name = utf16("Local\\SysVitals_SingleInstance");
            let _mutex = match CreateMutexW(None, false, windows::core::PCWSTR(mutex_name.as_ptr()))
            {
                Ok(h) => {
                    if windows::Win32::Foundation::GetLastError()
                        == windows::Win32::Foundation::ERROR_ALREADY_EXISTS
                    {
                        log_event("INFO", "another instance is already running; exiting");
                        return;
                    }
                    h
                }
                Err(_) => return,
            };

            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            // Every panel child is owned by this job; closing the handle on
            // process exit kills the children even after a crash.
            init_child_job();
            // Needed only when the indicator is found cloaked on another
            // virtual desktop; the apartment is never otherwise used.
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            refresh_theme();
            let cfg = config();

            // Seed the config mtime so the first poll does not false-trigger.
            CONFIG_MTIME.store(config_mtime(), Ordering::Relaxed);

            // Daily-file logging: prune stale logs once at startup.
            prune_old_logs();
            log_event(
                "INFO",
                &format!("SysVitals starting (version={})", env!("CARGO_PKG_VERSION")),
            );
            log_event(
                "INFO",
                &format!(
                    "config loaded: interval={}ms language={:?} theme={:?} dual_row={} \
                     show_resources={} show_network={} item_spacing={}px anchor={:?} \
                     left_margin={}px",
                    cfg.sample.interval_ms,
                    cfg.display.language,
                    cfg.display.theme,
                    cfg.display.dual_row,
                    cfg.display.show_resources,
                    cfg.display.show_network,
                    cfg.display.item_spacing,
                    cfg.windows.anchor,
                    cfg.windows.left_margin_px,
                ),
            );
            let centered = win11_centered_taskbar();
            log_event(
                "INFO",
                &format!(
                    "environment: win11_centered_taskbar={} dpi={} build={:?}",
                    centered,
                    GetDpiForSystem(),
                    os_build_number()
                ),
            );

            let hmodule = GetModuleHandleW(None).expect("module handle");
            let class_name = utf16("SysVitalsOverlay");
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                // No `CS_DBLCLKS`: the indicator's left click now opens the
                // trend chart immediately, and the system resource viewer
                // (Task Manager) lives in the right-click menu. Keeping
                // `CS_DBLCLKS` would let Windows synthesize a
                // `WM_LBUTTONDBLCLK` after the trailing `WM_LBUTTONUP`,
                // which we no longer handle; dropping it here makes the
                // click pipeline honestly single-click only.
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                hInstance: hmodule.into(),
                hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH::default(),
                lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
                ..Default::default()
            };
            if RegisterClassExW(&wc) == 0 {
                return;
            }

            // The sentinel is a hidden top-level window that is deliberately
            // *not* owned by the taskbar: it must outlive explorer restarts to
            // receive the `TaskbarCreated` broadcast and rebuild the overlay.
            let taskbar_created =
                RegisterWindowMessageW(windows::core::PCWSTR(utf16("TaskbarCreated").as_ptr()));
            if taskbar_created != 0 {
                let _ = TASKBAR_CREATED.set(taskbar_created);
            } else {
                log_event("WARN", "TaskbarCreated registration failed");
            }
            let sentinel_class = utf16("SysVitalsSentinel");
            let sentinel_wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(sentinel_wndproc),
                hInstance: hmodule.into(),
                lpszClassName: windows::core::PCWSTR(sentinel_class.as_ptr()),
                ..Default::default()
            };
            if RegisterClassExW(&sentinel_wc) == 0 {
                log_event("WARN", "sentinel class registration failed");
            }
            let sentinel_title = utf16("SysVitalsSentinel");
            let sentinel = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                windows::core::PCWSTR(sentinel_class.as_ptr()),
                windows::core::PCWSTR(sentinel_title.as_ptr()),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(hmodule.into()),
                None,
            );
            match sentinel {
                Ok(sentinel) => {
                    SENTINEL_HWND.store(sentinel.0 as isize, Ordering::Relaxed);
                    let interval_ms = cfg.sample.interval_ms.clamp(500, 5000) as u32;
                    let _ = SetTimer(Some(sentinel), TIMER_SAMPLE, interval_ms, None);
                    let _ = SetTimer(Some(sentinel), TIMER_MOVE, MOVE_MS, None);
                    let _ = SetTimer(Some(sentinel), TIMER_CONFIG, CONFIG_POLL_MS, None);
                    let _ = SetTimer(Some(sentinel), TIMER_HISTORY, HISTORY_SYNC_MS, None);
                    let _ = SetTimer(Some(sentinel), TIMER_WATCH, WATCHDOG_MS, None);
                }
                Err(_) => log_event("WARN", "sentinel window creation failed"),
            }

            // Create one overlay per existing taskbar, then paint the first
            // sample. `reconcile_overlays` also runs every move tick, on
            // `TaskbarCreated` and on display changes, so taskbars that appear
            // later (secondary bars, hot-plug, explorer restart) get one too.
            reconcile_overlays();
            if LAST_SNAPSHOT.with(|s| s.borrow().is_none()) {
                sample_tick();
            }
            // The resident History framework is independent of any overlay.
            ensure_history_resident();

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

fn main() {
    #[cfg(windows)]
    win::run();
    #[cfg(not(windows))]
    println!("app-win builds its display layer on Windows only.");
}
