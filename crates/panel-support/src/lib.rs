//! Shared theme plumbing for the two GPUI panels (`app-settings` and
//! `app-history`).
//!
//! Both windows follow the system light/dark appearance, the OS accent and
//! the same backdrop-material fallback rules. Keeping the platform probes and
//! theme patches in one place removes a ~300-line copy that had to be fixed
//! twice whenever a token changed.

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

/// The Windows OS build number, or `None` if the version probe fails.
///
/// `RtlGetVersion` is the documented way to read the real build (the classic
/// `GetVersionEx` is version-helpered and lies on 8.1+).
#[cfg(windows)]
pub fn os_build_number() -> Option<u32> {
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
    let mut info = OSVERSIONINFOW::default();
    info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
    if unsafe { RtlGetVersion(&mut info) }.is_err() {
        return None;
    }
    Some(info.dwBuildNumber)
}

/// Read a DWORD from HKCU; `None` when the key or value is absent.
#[cfg(windows)]
pub fn hkcu_dword(key: &str, value: &str) -> Option<u32> {
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
        REG_VALUE_TYPE,
    };
    let utf16 = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
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
    if queried.is_ok() && size >= 4 {
        Some(u32::from_le_bytes(data))
    } else {
        None
    }
}

#[cfg(not(windows))]
pub fn hkcu_dword(_key: &str, _value: &str) -> Option<u32> {
    None
}

/// The backdrop material this Windows host supports, honoring the user's
/// transparency-effects preference.
#[cfg(windows)]
pub fn detect_material() -> Material {
    const PERSONALIZE_KEY: &str =
        "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
    let transparency = hkcu_dword(PERSONALIZE_KEY, "EnableTransparency")
        .map(|v| v != 0)
        .unwrap_or(true);
    choose_windows_material(os_build_number(), transparency)
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
    os_build_number()
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
    if let Some(abgr) = hkcu_dword(ACCENT_KEY, "AccentColorMenu") {
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
pub fn apply_system_theme(cx: &mut App) {
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
