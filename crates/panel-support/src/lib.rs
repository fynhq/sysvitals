//! Shared theme and window plumbing for the two GPUI panels (`app-settings`
//! and `app-history`).
//!
//! Both windows follow the system light/dark appearance, the OS accent and
//! the same backdrop-material fallback rules, and both need the same hidden-
//! window placement, DPI and parent-watch mechanics. Keeping the probes,
//! theme patches and window helpers in one place removes copy-paste that
//! would otherwise have to be fixed twice whenever a token changes.
//!
//! GPUI-free process plumbing (logging, the panic hook, registry probes)
//! lives in `sysvitals-support` and is re-exported here so panel call sites
//! have a single import surface.

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

// Process plumbing shared by every panel (and, directly, by the platform
// indicators): daily-file logging, the crash hook and the Windows probes.
#[cfg(windows)]
pub use sysvitals_support::{hkcu_dword, os_build_number, taskbar_al, utf16};
pub use sysvitals_support::{install_panic_hook, log_event, prune_old_logs};

/// Which panel is asking for the material tint. The history popup sits
/// directly on the chart and keeps the standard tint; the settings page is
/// made of opaque cards and exposes Mica at full strength.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelSurface {
    Settings,
    History,
}

/// Corner radius shared by every panel control (buttons, inputs, dropdowns).
/// Windows uses WinUI's `ControlCornerRadius` (4px), which also stays visually
/// nested inside its 8px cards; macOS keeps the library's 6px.
#[cfg(windows)]
pub const CONTROL_RADIUS: f32 = 4.0;
#[cfg(not(windows))]
pub const CONTROL_RADIUS: f32 = 6.0;

/// The backdrop material this Windows host supports, honoring the user's
/// transparency-effects preference.
#[cfg(windows)]
pub fn detect_material() -> Material {
    const PERSONALIZE_KEY: &str =
        "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
    let transparency = sysvitals_support::hkcu_dword(PERSONALIZE_KEY, "EnableTransparency")
        .map(|v| v != 0)
        .unwrap_or(true);
    choose_windows_material(sysvitals_support::os_build_number(), transparency)
}

/// macOS always requests the blur material: `NSVisualEffectView` exists on
/// every supported host, and AppKit automatically flattens it when the user
/// enables "reduce transparency".
#[cfg(target_os = "macos")]
pub fn detect_material() -> Material {
    choose_macos_material(true)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn detect_material() -> Material {
    Material::Opaque
}

/// Map the chosen material onto GPUI's window-background request.
pub fn backdrop_for(material: Material) -> WindowBackgroundAppearance {
    match material {
        Material::Mica => WindowBackgroundAppearance::MicaBackdrop,
        Material::Blur => WindowBackgroundAppearance::Blurred,
        Material::Opaque => WindowBackgroundAppearance::Opaque,
    }
}

/// Windows 11 ships Segoe UI Variable, whose optical size is tuned for UI
/// text. Older builds keep the system-resolved font.
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

/// The system accent color packed as `0x00RRGGBB`, or `None` when it should
/// not be used at all.
///
/// Windows reads Explorer's `AccentColorMenu` (ABGR), which changes as soon
/// as the picker is confirmed; DWM colorization is only a fallback because it
/// can lag and is darkened by the transparency mix. macOS always follows
/// `NSColor.controlAccentColor`, which resolves both an explicit pick and the
/// default multicolor / wallpaper-derived accent.
#[cfg(windows)]
pub fn system_accent_hex() -> Option<u32> {
    const ACCENT_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Accent";
    if let Some(abgr) = sysvitals_support::hkcu_dword(ACCENT_KEY, "AccentColorMenu") {
        return Some(sysvitals_core::color::abgr_to_rgb(abgr));
    }
    use windows::Win32::Graphics::Dwm::DwmGetColorizationColor;
    let mut argb = 0u32;
    // `Default::default()` avoids naming the `BOOL` type: in windows-core
    // 0.62 that is a plain `pub type BOOL = i32` alias, so `BOOL(0)` would
    // compile as a tuple-struct constructor (E0423). The opaque-blend flag is
    // not read — the accent color is meaningful either way.
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

/// Convert a packed 24-bit RGB hex to GPUI's `Hsla`.
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

/// Push the OS accent color into the GPUI theme so primary-tinted controls
/// match the system picker.
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

/// Sync the window palette to the OS light/dark appearance.
fn apply_system_theme(cx: &mut App) {
    UiTheme::sync_system_appearance(None, cx);
}

/// Tint the root surface so the backdrop reads through. `Opaque` is a no-op,
/// preserving the pre-material rendering exactly.
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

/// Theme sync, accent override and material tint, in that order. A palette
/// reload wipes every runtime patch, so the panels must call this after every
/// appearance change and apply their own extras afterwards.
pub fn apply_theme_and_accent(cx: &mut App, surface: PanelSurface) {
    apply_system_theme(cx);
    apply_accent(cx);
    apply_material(cx, surface);
}

/// Poll the OS accent color and re-apply the theme patch when it changes.
///
/// `state` reaches into the view's cached last-seen accent; `reload` re-runs
/// the panel's full theme patch when the accent disappears (the OS fell back
/// to multicolor, or the probe failed) — a palette reload is the only way to
/// drop the old override. The loop stops with the view entity, i.e. when the
/// window closes. The probe runs inside `update` on purpose: on macOS
/// `system_accent_hex` reads AppKit (`controlAccentColor`), which belongs to
/// the main thread — the same thread `update` runs on.
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

/// Re-apply the panel palette when the OS light/dark appearance flips live.
/// `on_change` must re-run the panel's full theme patch (a palette reload
/// wipes every runtime override). Returns the subscription the caller keeps
/// for the window's lifetime.
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

/// Quit when the launching indicator dies without running its quit path
/// (crash, Force Quit). The platform job object covers Windows ordinary
/// exits; this macOS watch also catches a child launched before the job was
/// configured. `label` names the panel in the log lines.
#[cfg(target_os = "macos")]
pub fn watch_parent(cx: &mut App, label: &'static str) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(pid) = sysvitals_core::cli::parse_parent_pid(&args) else {
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

/// Native window handle of a GPUI window, if it has one.
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

/// Move a still-hidden panel window onto the display it will open on.
///
/// Windows creates the window on the OS default monitor and GPUI moves it
/// to the anchor's display when the window is shown. A cross-DPI move makes
/// Windows re-scale the window from the previous DPI, which used to run
/// while the panel was already visible (the "growing" flash on a 4K
/// secondary display). Moving the hidden window first lets the DPI change
/// happen off-screen; activation then only applies the final placement.
///
/// Must run outside an App update: `SetWindowPos` synchronously delivers
/// `WM_MOVE`/`WM_SIZE`, and those messages re-enter GPUI's window callbacks
/// (`AsyncApp::update`) while the app is already borrowed, panicking with
/// "RefCell already borrowed".
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

/// GPUI does not declare DPI awareness itself. Without this the process is
/// treated as DPI-unaware and Windows bitmap-stretches the whole window on a
/// scaled display, which is what makes the text look soft.
#[cfg(windows)]
pub fn set_per_monitor_dpi_aware() {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Re-assert the logical size a panel window was laid out for after Windows
/// moved it onto the indicator's display.
///
/// Windows places a freshly created window on the OS default monitor and
/// `SetWindowPlacement` then moves it to the anchor display. When that move
/// crosses a DPI boundary, the `WM_DPICHANGED` suggested rectangle is
/// derived from the *old* DPI, so a window already sized for the target DPI
/// is scaled a second time (a 680x390 popup opened from a 125% primary on a
/// 100% secondary came out 542x311, leaving a visible gap above the
/// taskbar). GPUI applies that suggestion, so the intended size is
/// re-applied once, after the move. A same-DPI move stays within a pixel of
/// the target and is untouched. `size_fixed` is the caller's one-shot latch.
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

/// The usable bounds (taskbar/dock excluded) of the display hosting the
/// indicator anchor, or `None` when no display matches.
///
/// The native display handle is authoritative when the launcher passed one:
/// GPUI scales each monitor's logical space by its own factor, so on
/// mixed-DPI setups the anchor rectangle alone can fall inside two displays'
/// bounds and the probe would pick the wrong one. The rectangle midpoint
/// probe remains the fallback for launches without a handle (older companion
/// process, macOS).
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
