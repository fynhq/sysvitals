//! Shared theme and window plumbing for the GPUI panels.

use gpui_kit::component::theme::Theme as UiTheme;
use gpui_kit::*;
#[cfg(target_os = "macos")]
use sysvitals_core::backdrop::choose_macos_material;
#[cfg(windows)]
use sysvitals_core::backdrop::choose_windows_material;
use sysvitals_core::backdrop::{
    material_tint_alpha, material_tint_alpha_macos, material_tint_alpha_settings, should_tint,
    Material,
};

pub use sysvitals_support::{
    acquire_instance_lock, anchor_arg, anchor_monitor_arg, clear_panel_request, install_panic_hook,
    log_event, log_sanitize_issues, parent_pid_arg, poll_config, prewarm_arg, ready_file_arg,
    take_panel_request, unix_ms, write_panel_request, ConfigEvent, InstanceLock, PanelAction,
    PanelRequest, PANEL_HISTORY, PANEL_SETTINGS, SV_BUILD_ID,
};
#[cfg(windows)]
pub use sysvitals_support::{hkcu_dword, os_build_number, taskbar_al, utf16, ParentProcess};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelSurface {
    Settings,
    History,
}

pub fn build_panel_app(headless: bool, quit_mode: QuitMode) -> Application {
    let app = gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .with_quit_mode(quit_mode);
    if headless {
        app.with_windowing(WindowingRequest::Headless)
    } else {
        app
    }
}

#[cfg(windows)]
pub fn trim_working_set() {
    use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
    // SAFETY: the pseudo-handle refers to this process; -1 sizes request a trim.
    unsafe {
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

#[cfg(not(windows))]
pub fn trim_working_set() {}

// Windows only grants SetForegroundWindow to a process that received the last input or is
// still starting up, so a long-lived host borrows the foreground thread's input state.
#[cfg(windows)]
pub fn take_window_foreground(hwnd: windows::Win32::Foundation::HWND) -> bool {
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, IsHungAppWindow, SetForegroundWindow,
    };
    // SAFETY: every handle comes from the OS in this scope and attach/detach stay paired.
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground == hwnd {
            return true;
        }
        // Attaching to a hung thread would stall this one, so leave the popup unfocused instead.
        if !foreground.is_invalid() && IsHungAppWindow(foreground).as_bool() {
            return false;
        }
        let current = GetCurrentThreadId();
        let owner = GetWindowThreadProcessId(foreground, None);
        if owner == 0 || owner == current || !AttachThreadInput(current, owner, true).as_bool() {
            return SetForegroundWindow(hwnd).as_bool();
        }
        let taken = SetForegroundWindow(hwnd).as_bool();
        let _ = AttachThreadInput(current, owner, false);
        taken
    }
}

// GPUI reports thread-local activation on Windows, so close rules must compare against the system foreground.
#[cfg(windows)]
pub fn window_has_foreground(hwnd: windows::Win32::Foundation::HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    // SAFETY: GetForegroundWindow returns a borrowed handle that is only compared.
    unsafe { GetForegroundWindow() == hwnd }
}

#[cfg(windows)]
pub fn foreground_window_description() -> String {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
    };
    // SAFETY: both text buffers belong to this call and the returned handles are only inspected.
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return "none".to_string();
        }
        let mut class = [0u16; 128];
        let class_len = GetClassNameW(hwnd, &mut class).max(0) as usize;
        let mut title = [0u16; 256];
        let title_len = GetWindowTextW(hwnd, &mut title).max(0) as usize;
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        format!(
            "hwnd=0x{:x} pid={pid} class='{}' title='{}'",
            hwnd.0 as isize,
            String::from_utf16_lossy(&class[..class_len]),
            String::from_utf16_lossy(&title[..title_len]),
        )
    }
}

pub fn window_display_id(window: &Window, cx: &App) -> Option<DisplayId> {
    window.display(cx).map(|display| display.id())
}

pub fn init_panel<F>(cx: &mut App, label: &'static str, started: std::time::Instant, apply_theme: F)
where
    F: FnOnce(&mut App),
{
    gpui_kit::init(cx);
    log_event(
        "INFO",
        &format!(
            "{label}: gpui init done at {}ms",
            started.elapsed().as_millis()
        ),
    );
    apply_theme(cx);
    watch_parent(cx, label);
}

// The second frame starts only after the first one was drawn, so the gap measures the first paint.
pub fn log_first_frames(
    window: AnyWindowHandle,
    cx: &mut App,
    label: &'static str,
    started: std::time::Instant,
) {
    let _ = window.update(cx, |_, window, _| {
        window.on_next_frame(move |window, _| {
            log_event(
                "INFO",
                &format!(
                    "{label}: first frame at {}ms",
                    started.elapsed().as_millis()
                ),
            );
            window.on_next_frame(move |_, _| {
                log_event(
                    "INFO",
                    &format!(
                        "{label}: second frame at {}ms",
                        started.elapsed().as_millis()
                    ),
                );
            });
        });
    });
}

#[cfg(windows)]
pub const CONTROL_RADIUS: f32 = 4.0;
#[cfg(not(windows))]
pub const CONTROL_RADIUS: f32 = 6.0;

#[cfg(windows)]
pub fn detect_material() -> Material {
    const PERSONALIZE_KEY: &str =
        "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
    let transparency = sysvitals_support::hkcu_dword(PERSONALIZE_KEY, "EnableTransparency")
        .map(|v| v != 0)
        .unwrap_or(true);
    choose_windows_material(sysvitals_support::os_build_number(), transparency)
}

#[cfg(target_os = "macos")]
pub fn detect_material() -> Material {
    choose_macos_material(true)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn detect_material() -> Material {
    Material::Opaque
}

pub fn backdrop_for(material: Material) -> WindowBackgroundAppearance {
    match material {
        Material::Mica => WindowBackgroundAppearance::MicaBackdrop,
        Material::Blur => WindowBackgroundAppearance::Blurred,
        Material::Opaque => WindowBackgroundAppearance::Opaque,
    }
}

#[cfg(windows)]
pub fn panel_font_family() -> Option<&'static str> {
    sysvitals_support::os_build_number()
        .filter(|build| *build >= 22000)
        .map(|_| "Segoe UI Variable")
}

#[cfg(not(windows))]
pub fn panel_font_family() -> Option<&'static str> {
    None
}

#[cfg(windows)]
pub fn system_accent_hex() -> Option<u32> {
    const ACCENT_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Accent";
    if let Some(abgr) = sysvitals_support::hkcu_dword(ACCENT_KEY, "AccentColorMenu") {
        return Some(sysvitals_core::color::abgr_to_rgb(abgr));
    }
    use windows::Win32::Graphics::Dwm::DwmGetColorizationColor;
    let mut argb = 0u32;
    if unsafe { DwmGetColorizationColor(&mut argb, &mut Default::default()) }.is_err() {
        return None;
    }
    Some(argb & 0x00FF_FFFF)
}

#[cfg(target_os = "macos")]
pub fn system_accent_hex() -> Option<u32> {
    use objc2::rc::Retained;
    use objc2_app_kit::{NSColor, NSColorSpace};
    let color = NSColor::controlAccentColor();
    let srgb = NSColorSpace::sRGBColorSpace();
    let converted: Retained<NSColor> = color.colorUsingColorSpace(&srgb)?;
    let (mut r, mut g, mut b, mut a) = (0f64, 0f64, 0f64, 0f64);
    unsafe { converted.getRed_green_blue_alpha(&mut r, &mut g, &mut b, &mut a) };
    let to8 = |x: f64| ((x.clamp(0.0, 1.0) * 255.0).round() as u32) & 0xFF;
    Some((to8(r) << 16) | (to8(g) << 8) | to8(b))
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn system_accent_hex() -> Option<u32> {
    None
}

fn rgb_hex_to_hsla(hex: u32) -> Hsla {
    let r = ((hex >> 16) & 0xFF) as f32 / 255.0;
    let g = ((hex >> 8) & 0xFF) as f32 / 255.0;
    let b = (hex & 0xFF) as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) * 0.5;
    if (max - min).abs() < 1e-6 {
        return hsla(0.0, 0.0, l, 1.0);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if (max - r).abs() < 1e-6 {
        ((g - b) / d + if g < b { 6.0 } else { 0.0 }) / 6.0
    } else if (max - g).abs() < 1e-6 {
        ((b - r) / d + 2.0) / 6.0
    } else {
        ((r - g) / d + 4.0) / 6.0
    };
    hsla(h, s, l, 1.0)
}

pub fn apply_accent(cx: &mut App) {
    let Some(hex) = system_accent_hex() else {
        return;
    };
    let base = rgb_hex_to_hsla(hex);
    #[cfg(target_os = "macos")]
    let foreground = hsla(0.0, 0.0, 1.0, 1.0);
    #[cfg(not(target_os = "macos"))]
    let foreground = if sysvitals_core::color::prefers_dark_foreground(hex) {
        hsla(0.0, 0.0, 0.08, 1.0)
    } else {
        hsla(0.0, 0.0, 0.97, 1.0)
    };
    UiTheme::update(cx, |theme| {
        let shift = |dl: f32| Hsla {
            l: (base.l + dl).clamp(0.05, 0.95),
            ..base
        };
        theme.colors.primary = base;
        theme.colors.button_primary = base;
        theme.colors.primary_hover = shift(0.06);
        theme.colors.button_primary_hover = shift(0.06);
        theme.colors.primary_active = shift(-0.06);
        theme.colors.button_primary_active = shift(-0.06);
        theme.colors.primary_foreground = foreground;
        theme.colors.button_primary_foreground = foreground;
        theme.colors.ring = base;
        theme.colors.list_active = Hsla {
            a: if theme.mode.is_dark() { 0.26 } else { 0.16 },
            ..base
        };
        theme.colors.list_active_border = base;
        theme.colors.selection = Hsla { a: 0.30, ..base };
        theme.colors.caret = base;
    });
}

fn apply_system_theme(cx: &mut App) {
    UiTheme::sync_system_appearance(None, cx);
}

pub fn apply_material(cx: &mut App, surface: PanelSurface) {
    let material = detect_material();
    if !should_tint(material) {
        return;
    }
    UiTheme::update(cx, |theme| {
        let dark = theme.mode.is_dark();
        let alpha = if cfg!(target_os = "macos") {
            material_tint_alpha_macos(dark)
        } else {
            match surface {
                PanelSurface::Settings => material_tint_alpha_settings(dark),
                PanelSurface::History => material_tint_alpha(dark),
            }
        };
        let opaque = |c: Hsla| Hsla { a: 1.0, ..c };
        theme.colors.background = opaque(theme.colors.background).opacity(alpha);
    });
}

pub fn apply_theme_and_accent(cx: &mut App, surface: PanelSurface) {
    apply_system_theme(cx);
    apply_accent(cx);
    apply_material(cx, surface);
    #[cfg(windows)]
    UiTheme::update(cx, |theme| {
        if let Some(family) = panel_font_family() {
            theme.font_family = family.into();
        }
    });
}

// Accent-only changes do not fire the appearance observer; poll the OS accent every 500 ms and re-apply.
pub fn spawn_accent_watch<V, S, R>(cx: &mut Context<V>, mut state: S, mut reload: R)
where
    V: 'static,
    S: FnMut(&mut V) -> &mut Option<u32> + 'static,
    R: FnMut(&mut App) + 'static,
{
    cx.spawn(async move |this, cx| loop {
        cx.background_executor()
            .timer(std::time::Duration::from_millis(500))
            .await;
        let alive = this
            .update(cx, |view: &mut V, cx| {
                let current = system_accent_hex();
                let slot = state(view);
                if current == *slot {
                    return true;
                }
                *slot = current;
                if current.is_some() {
                    apply_accent(cx);
                } else {
                    reload(cx);
                }
                cx.notify();
                true
            })
            .unwrap_or(false);
        if !alive {
            break;
        }
    })
    .detach();
}

pub fn observe_system_appearance<V, F>(
    cx: &mut Context<V>,
    window: &mut Window,
    mut on_change: F,
) -> Subscription
where
    V: 'static,
    F: FnMut(&mut App) + 'static,
{
    cx.observe_window_appearance(window, move |_view: &mut V, _window, cx| {
        on_change(cx);
        cx.notify();
    })
}

#[cfg(target_os = "macos")]
pub fn watch_parent(cx: &mut App, label: &'static str) {
    let Some(pid) = parent_pid_arg() else {
        return;
    };
    log_event("INFO", &format!("{label}: watching parent pid {pid}"));
    cx.spawn(async move |cx| loop {
        cx.background_executor()
            .timer(std::time::Duration::from_secs(1))
            .await;
        if unsafe { libc::kill(pid as i32, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            log_event("INFO", &format!("{label}: parent process gone; exiting"));
            cx.update(|app| app.quit());
            break;
        }
    })
    .detach();
}

#[cfg(windows)]
pub fn watch_parent(cx: &mut App, label: &'static str) {
    let Some(pid) = parent_pid_arg() else {
        return;
    };
    let parent = match ParentProcess::open(pid) {
        Ok(parent) => parent,
        Err(error) => {
            log_event(
                "WARN",
                &format!("{label}: parent process unavailable: {error}; exiting"),
            );
            cx.quit();
            return;
        }
    };
    log_event("INFO", &format!("{label}: watching parent pid {pid}"));
    cx.spawn(async move |cx| loop {
        cx.background_executor()
            .timer(std::time::Duration::from_secs(1))
            .await;
        if parent.exited() {
            log_event("INFO", &format!("{label}: parent process gone; exiting"));
            cx.update(|app| app.quit());
            break;
        }
    })
    .detach();
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn watch_parent(_cx: &mut App, _label: &'static str) {}

#[cfg(windows)]
pub fn window_hwnd(window: &Window) -> Option<windows::Win32::Foundation::HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::Win32(raw) = handle.as_raw() else {
        return None;
    };
    Some(HWND(raw.hwnd.get() as *mut _))
}

#[cfg(windows)]
pub fn window_is_visible(hwnd: windows::Win32::Foundation::HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;
    unsafe { IsWindowVisible(hwnd).as_bool() }
}

// Place a still-hidden panel on its target display before showing; a cross-DPI move rescales the logical size.
#[cfg(windows)]
pub fn preplace_hidden_window(hwnd: windows::Win32::Foundation::HWND, monitor: DisplayId) -> bool {
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO};
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, SWP_NOACTIVATE, SWP_NOREDRAW, SWP_NOSIZE, SWP_NOZORDER,
    };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(HMONITOR(u64::from(monitor) as _), &mut info) }.as_bool() {
        return false;
    }
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            info.rcWork.left,
            info.rcWork.top,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOREDRAW,
        )
    }
    .is_ok()
}

#[cfg(windows)]
pub fn set_per_monitor_dpi_aware() {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

pub fn restore_window_size_after_move(
    window: &mut Window,
    size_fixed: &mut bool,
    target: gpui_kit::Size<Pixels>,
) {
    if *size_fixed {
        return;
    }
    *size_fixed = true;
    let current = window.bounds().size;
    let width = f32::from(current.width);
    let height = f32::from(current.height);
    let target_width = f32::from(target.width);
    let target_height = f32::from(target.height);
    if (width - target_width).abs() <= 1.0 && (height - target_height).abs() <= 1.0 {
        return;
    }
    log_event(
        "INFO",
        &format!(
            "panel: restoring window size after moving displays \
             ({width}x{height} -> {target_width}x{target_height})"
        ),
    );
    window.resize(target);
}

pub fn anchor_visible_bounds(
    cx: &App,
    anchor: &sysvitals_core::cli::AnchorRect,
    monitor: Option<DisplayId>,
) -> Option<Bounds<Pixels>> {
    let display = monitor.and_then(|monitor| cx.displays().into_iter().find(|d| d.id() == monitor));
    let display = match display {
        Some(display) => Some(display),
        None => {
            let probe = point(
                px((anchor.left + anchor.right) * 0.5),
                px((anchor.top + anchor.bottom) * 0.5),
            );
            cx.displays()
                .into_iter()
                .find(|d| d.bounds().contains(&probe))
                .or_else(|| {
                    cx.displays()
                        .into_iter()
                        .find(|d| d.visible_bounds().contains(&probe))
                })
        }
    };
    display.map(|d| d.visible_bounds())
}

pub fn layout_rect(bounds: &Bounds<Pixels>) -> sysvitals_core::layout::Rect {
    sysvitals_core::layout::Rect::new(
        f32::from(bounds.origin.x),
        f32::from(bounds.origin.y),
        f32::from(bounds.origin.x) + f32::from(bounds.size.width),
        f32::from(bounds.origin.y) + f32::from(bounds.size.height),
    )
}
