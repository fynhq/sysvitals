//! Windows display layer: one topmost layered overlay per taskbar.
//!
//! Module map:
//! - `overlay`  — per-taskbar overlay state, geometry, visibility/topmost,
//!   reconciliation and the UIA clock anchor cache.
//! - `render`   — measurement, GDI rasterization, 2x supersampling and the
//!   periodic sample tick.
//! - `children` — Settings/History process lifetime, job object and the
//!   shared-memory history state.
//! - `wndproc`  — the sentinel window procedure and the message loop.
//! - `menu`     — the native context menu.
//! - `config`   — config load/save, mtime polling and auto-start.
//! - `theme`    — fonts, DPI and OS theme/high-contrast probes.
//! - `crash`    — panic hook and the SEH filter.
//!
//! Items are `pub` so siblings can reach them through `use super::*`, but
//! `win` itself is private to this crate, so nothing here widens the crate's
//! real API.

mod children;
mod config;
mod crash;
mod menu;
mod overlay;
mod render;
mod theme;
mod wndproc;

pub(crate) use children::*;
pub(crate) use config::*;
pub(crate) use crash::*;
pub(crate) use menu::*;
pub(crate) use overlay::*;
pub(crate) use render::*;
pub(crate) use theme::*;
pub(crate) use wndproc::*;

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sysvitals_core::format::FormattedRow;
use sysvitals_core::{
    format_frame, format_frame_vertical, t, Config, ConfigPoll, ConfigWatcher, HistoryWriterSlot,
    Language, Sampler, Theme,
};
// Shared process plumbing (daily-file logging, panic hook, HKCU probes)
// lives in `sysvitals-support` so the overlay and the GPUI panels cannot
// drift apart.
use sysvitals_support::{hkcu_dword, log_event, os_build_number, taskbar_al, utf16};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW, GetDC,
    GetMonitorInfoW, GetSysColor, MonitorFromWindow, ReleaseDC, SelectObject, SetBkMode,
    SetTextColor, AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, BLENDFUNCTION, CLIP_DEFAULT_PRECIS, COLOR_WINDOWTEXT, DEFAULT_CHARSET, DEFAULT_PITCH,
    DIB_RGB_COLORS, DT_CALCRECT, DT_CENTER, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER,
    FF_SWISS, FW_REGULAR, FW_SEMIBOLD, HFONT, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Diagnostics::Debug::{SetUnhandledExceptionFilter, EXCEPTION_POINTERS};
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
    RegCloseKey, RegDeleteKeyValueW, RegOpenKeyExW, RegQueryValueExW, RegSetKeyValueW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, REG_SZ,
};
use windows::Win32::System::Threading::{
    CreateMutexW, CreateProcessW, GetCurrentProcessId, GetExitCodeProcess, TerminateProcess,
    WaitForSingleObject, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows::Win32::UI::HiDpi::{
    GetDpiForSystem, GetDpiForWindow, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Shell::{
    IVirtualDesktopManager, SHAppBarMessage, ShellExecuteW, VirtualDesktopManager, ABM_GETSTATE,
    ABS_AUTOHIDE, APPBARDATA,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, FindWindowExW, FindWindowW, GetClassNameW, GetCursorPos, GetForegroundWindow,
    GetMessageW, GetShellWindow, GetSystemMetrics, GetWindowLongPtrW, GetWindowRect,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, KillTimer, MoveWindow,
    PostMessageW, PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SetForegroundWindow,
    SetTimer, SetWindowPos, ShowWindow, SystemParametersInfoW, TrackPopupMenu, TranslateMessage,
    UpdateLayeredWindow, WindowFromPoint, CS_HREDRAW, CS_VREDRAW, GWL_EXSTYLE, HWND_TOPMOST,
    MF_SEPARATOR, MF_STRING, MSG, NONCLIENTMETRICSW, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SPI_GETHIGHCONTRAST,
    SPI_GETNONCLIENTMETRICS, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE,
    SW_SHOWNORMAL, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, ULW_ALPHA, WM_DESTROY,
    WM_DISPLAYCHANGE, WM_DPICHANGED, WM_ENDSESSION, WM_LBUTTONUP, WM_NULL, WM_QUERYENDSESSION,
    WM_RBUTTONUP, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

// ------------------------------------------------------------------ consts

/// Build stamp emitted by build.rs: `v{semver}` on a tagged commit,
/// `v{semver}_{short-hash} (dev)` otherwise. Kept in step with the
/// settings footer and the macOS startup log.
const SV_BUILD_ID: &str = match option_env!("SV_BUILD_ID") {
    Some(id) => id,
    None => concat!("v", env!("CARGO_PKG_VERSION")),
};

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
/// Posted by the background settings-launch thread once the child handle
/// is stored (or the launch failed).
const WM_APP_SETTINGS_READY: u32 = 0x8000 + 4;
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
/// Per-user login-startup list. The value is our executable path; the
/// default-off `[startup] auto_start` setting is applied here.
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "SysVitals";

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
/// High-contrast mode flag (0/1). When set, the indicator ignores the
/// light/dark override and paints with the system high-contrast text color.
static HIGH_CONTRAST: AtomicU32 = AtomicU32::new(0);
/// `GetSysColor(COLOR_WINDOWTEXT)` in high-contrast mode (0x00BBGGRR).
static HC_TEXT: AtomicU32 = AtomicU32::new(0);
/// Last applied login-startup value; `u32::MAX` means "not applied yet".
static APPLIED_AUTO_START: AtomicU32 = AtomicU32::new(u32::MAX);
static CONFIG: OnceLock<RwLock<Config>> = OnceLock::new();
/// True when the initial config read fell back to defaults because the
/// file was unreadable or malformed. Auto-start is not applied in that
/// case: a broken file must not silently remove a login item the user
/// enabled earlier.
static CONFIG_LOAD_FAILED: AtomicBool = AtomicBool::new(false);
/// True when the currently loaded config came from an existing file.
/// A missing file is not an explicit "auto-start off" choice: the login
/// item is left untouched until the user saves a config through Settings.
static CONFIG_FROM_FILE: AtomicBool = AtomicBool::new(false);
/// Cache of the Win11 centered-taskbar preference. `u32::MAX` means
/// "probe again"; refreshed on WM_SETTINGCHANGE and TaskbarCreated so the
/// per-tick placement code avoids a registry read.
static WIN11_CENTERED: AtomicU32 = AtomicU32::new(u32::MAX);
/// Cache of the shell auto-hide preference. `SHAppBarMessage` is a
/// synchronous shell call, so probing it on every geometry tick could
/// block the message thread if Explorer is busy. Invalidate on setting
/// changes, TaskbarCreated, and the 5s watchdog.
static AUTO_HIDE: AtomicU32 = AtomicU32::new(u32::MAX);
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
/// Settings launch/replace worker in flight. `CreateProcessW` and the
/// wait for a replaced Settings instance's mutex can take seconds, so
/// both happen off the message thread.
static SETTINGS_LAUNCHING: AtomicBool = AtomicBool::new(false);
/// Monitor (HMONITOR as isize) the live settings window was opened on.
/// A Settings click on a different indicator moves the window there by
/// closing and relaunching it; a click on the same screen is a no-op
/// (there is no IPC to raise an existing GPUI window).
static SETTINGS_MONITOR: AtomicIsize = AtomicIsize::new(0);
/// Settings process handle; stored as an `isize` because the launch
/// happens on a worker thread (`HANDLE` is not `Send`). The UI thread
/// reaps and closes it.
static SETTINGS_PROCESS: Mutex<Option<isize>> = Mutex::new(None);
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
/// Whether `TIMER_SAMPLE` is currently armed. A failed initial `SetTimer`
/// leaves the overlay frozen on its startup frame; the watchdog retries it.
static SAMPLE_TIMER_OK: AtomicBool = AtomicBool::new(false);
fn sentinel_hwnd() -> HWND {
    HWND(SENTINEL_HWND.load(Ordering::Relaxed) as *mut std::ffi::c_void)
}

thread_local! {
    static SAMPLER: RefCell<Option<Sampler>> = const { RefCell::new(None) };
    /// Config-file mtime cache and parse-error throttle, message-thread only.
    static CONFIG_WATCHER: RefCell<ConfigWatcher> = RefCell::new(ConfigWatcher::new());
    /// Raw sample shared by every overlay, so a config/DPI/theme change can
    /// rebuild each layout without taking another reading (which used to
    /// inject a millisecond-scale network spike on every save).
    static LAST_SNAPSHOT: RefCell<Option<sysvitals_core::Snapshot>> = const { RefCell::new(None) };
    /// One entry per visible taskbar; the registry the sentinel drives.
    static OVERLAYS: RefCell<Vec<Overlay>> = const { RefCell::new(Vec::new()) };
    /// Background SQLite writer. Created on the first enabled sample so a
    /// missing data dir or a failed spawn never blocks the indicator; the
    /// slot owns the lazy-spawn retry policy.
    static HISTORY_WRITER: RefCell<HistoryWriterSlot> = const { RefCell::new(HistoryWriterSlot::new()) };
    /// Last occlusion probe; the diagnostic is throttled to 5 s because
    /// `WindowFromPoint` is not free and an occluder is not actionable.
    static LAST_OCCLUSION_PROBE: std::cell::Cell<Option<Instant>> =
        const { std::cell::Cell::new(None) };
    /// Shared command/status block between this overlay and the resident
    /// history process. The mapping handle is kept alive for the process
    /// lifetime.
    static HISTORY_STATE: RefCell<Option<(*mut sysvitals_core::HistoryState, HANDLE)>> = const { RefCell::new(None) };
    // COM virtual-desktop manager, created on demand the first time the
    // indicator is found cloaked. All access is on the message thread.
    static VD_MANAGER: RefCell<Option<IVirtualDesktopManager>> = const { RefCell::new(None) };
}
