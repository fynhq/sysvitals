//! Windows taskbar overlay implementation, sentinel timers, and overlay registry.

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
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sysvitals_core::format::FormattedRow;
use sysvitals_core::{
    format_frame, format_frame_vertical, t, Config, ConfigWatcher, HistoryWriterSlot, Language,
    Sampler, Theme,
};
use sysvitals_support::{
    acquire_instance_lock, hkcu_dword, log_event, log_sanitize_issues, os_build_number, taskbar_al,
    utf16, AutoStartController, AutoStartOutcome, ConfigEvent, InstanceLock, SV_BUILD_ID,
};
use windows::core::{Interface, BOOL, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
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
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, IPersistFile, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED, STGM_READ,
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
use windows::Win32::System::Registry::{RegDeleteKeyValueW, HKEY_CURRENT_USER};
use windows::Win32::System::Threading::{
    CreateProcessW, GetCurrentProcessId, GetProcessId, TerminateProcess, WaitForSingleObject,
    PROCESS_INFORMATION, STARTF_FORCEOFFFEEDBACK, STARTUPINFOW,
};
use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows::Win32::UI::HiDpi::{
    GetDpiForSystem, GetDpiForWindow, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Shell::{
    FOLDERID_Startup, IShellLinkW, IVirtualDesktopManager, SHAppBarMessage, SHGetKnownFolderPath,
    ShellExecuteW, ShellLink, VirtualDesktopManager, ABM_GETSTATE, ABS_AUTOHIDE, APPBARDATA,
    KF_FLAG_DEFAULT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, EnumWindows, FindWindowExW, FindWindowW, GetClassNameW, GetCursorPos,
    GetForegroundWindow, GetMessageW, GetShellWindow, GetSystemMetrics, GetWindowLongPtrW,
    GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, KillTimer,
    LoadCursorW, MoveWindow, PostMessageW, PostQuitMessage, RegisterClassExW,
    RegisterWindowMessageW, SetCursor, SetForegroundWindow, SetTimer, SetWindowPos, ShowWindow,
    SystemParametersInfoW, TrackPopupMenu, TranslateMessage, UpdateLayeredWindow, WindowFromPoint,
    CS_HREDRAW, CS_VREDRAW, GWL_EXSTYLE, HTCLIENT, HWND_TOPMOST, IDC_ARROW, IDC_WAIT, MF_SEPARATOR,
    MF_STRING, MSG, NONCLIENTMETRICSW, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SPI_GETHIGHCONTRAST,
    SPI_GETNONCLIENTMETRICS, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE,
    SW_SHOWNORMAL, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, ULW_ALPHA, WM_DESTROY,
    WM_DISPLAYCHANGE, WM_DPICHANGED, WM_ENDSESSION, WM_LBUTTONUP, WM_NULL, WM_QUERYENDSESSION,
    WM_RBUTTONUP, WM_SETCURSOR, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

const TIMER_SAMPLE: usize = 0x515;
const TIMER_MOVE: usize = 0x516;
const TIMER_CONFIG: usize = 0x517;
const MOVE_MS: u32 = 1_000;
const CONFIG_POLL_MS: u32 = 500;
const WM_APP_HISTORY_READY: u32 = 0x8000 + 3;
const WM_APP_SETTINGS_READY: u32 = 0x8000 + 4;
const TIMER_CURSOR: usize = 0x519;
const CURSOR_POLL_MS: u32 = 250;
const TIMER_REBIND: usize = 0x51A;
const REBIND_RETRY_MS: u32 = 500;
const TIMER_WATCH: usize = 0x51B;
const WATCHDOG_MS: u32 = 5_000;
const PAD_PX: i32 = 6;
const WIDTH_SLACK_PX: i32 = 8;
const VERTICAL_FONT_HUNDREDTHS: i32 = 75;
const VERTICAL_PAD_PX: i32 = 3;
const VERTICAL_WIDTH_SLACK_PX: i32 = 4;
const THEME_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
const THEME_VALUE: &str = "SystemUsesLightTheme";
const STARTUP_LINK_NAME: &str = "SysVitals.lnk";
const LEGACY_RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const LEGACY_RUN_VALUE: &str = "SysVitals";

const IDM_SETTINGS: usize = 901;
const IDM_QUIT: usize = 902;
const IDM_RESTART: usize = 903;
const IDM_TASKMGR: usize = 904;

static LIGHT_THEME: AtomicU32 = AtomicU32::new(0);
static TEXT_WEIGHT: AtomicI32 = AtomicI32::new(600);
static HIGH_CONTRAST: AtomicU32 = AtomicU32::new(0);
static HC_TEXT: AtomicU32 = AtomicU32::new(0);
static CONFIG: OnceLock<RwLock<Config>> = OnceLock::new();
static CONFIG_LOAD_FAILED: AtomicBool = AtomicBool::new(false);
static CONFIG_FROM_FILE: AtomicBool = AtomicBool::new(false);
static WIN11_CENTERED: AtomicU32 = AtomicU32::new(u32::MAX);
static AUTO_HIDE: AtomicU32 = AtomicU32::new(u32::MAX);
// A kill-on-close job owns every panel child, so a crash or quit cannot leave an orphan.
static CHILD_JOB: AtomicIsize = AtomicIsize::new(0);
static QUITTING: AtomicBool = AtomicBool::new(false);
// One hidden sentinel owns every timer and the TaskbarCreated broadcast, so timers survive Explorer restarts.
static SENTINEL_HWND: AtomicIsize = AtomicIsize::new(0);
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
static SAMPLE_TIMER_OK: AtomicBool = AtomicBool::new(false);
fn sentinel_hwnd() -> HWND {
    HWND(SENTINEL_HWND.load(Ordering::Relaxed) as *mut std::ffi::c_void)
}

thread_local! {
    static SAMPLER: RefCell<Option<Sampler>> = const { RefCell::new(None) };
    static CONFIG_WATCHER: RefCell<ConfigWatcher> = RefCell::new(ConfigWatcher::new());
    static LAST_SNAPSHOT: RefCell<Option<sysvitals_core::Snapshot>> = const { RefCell::new(None) };
    static OVERLAYS: RefCell<Vec<Overlay>> = const { RefCell::new(Vec::new()) };
    static HISTORY_WRITER: RefCell<HistoryWriterSlot> = const { RefCell::new(HistoryWriterSlot::new()) };
    static LAST_OCCLUSION_PROBE: std::cell::Cell<Option<Instant>> =
        const { std::cell::Cell::new(None) };
    static VD_MANAGER: RefCell<Option<IVirtualDesktopManager>> = const { RefCell::new(None) };
}
