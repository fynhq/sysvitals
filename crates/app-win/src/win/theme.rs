//! theme: split out of `win` (see `win` for the module map).

use super::*;

pub unsafe fn refresh_theme() {
    // The taskbar alignment lives beside the theme values and can change
    // while the indicator is running; drop the cached probe so the next
    // placement reads it again.
    WIN11_CENTERED.store(u32::MAX, Ordering::Relaxed);
    AUTO_HIDE.store(u32::MAX, Ordering::Relaxed);
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

    // High contrast is an accessibility override: when it is on, the
    // taskbar no longer follows the normal light/dark palette, so keep the
    // system text color instead of our black/white choice.
    let mut hc = HIGHCONTRASTW::default();
    hc.cbSize = std::mem::size_of::<HIGHCONTRASTW>() as u32;
    let queried = SystemParametersInfoW(
        SPI_GETHIGHCONTRAST,
        hc.cbSize,
        Some(std::ptr::from_mut(&mut hc).cast::<std::ffi::c_void>()),
        windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    )
    .is_ok();
    let high_contrast = queried && hc.dwFlags.contains(HCF_HIGHCONTRASTON);
    HIGH_CONTRAST.store(high_contrast as u32, Ordering::Relaxed);
    if high_contrast {
        HC_TEXT.store(GetSysColor(COLOR_WINDOWTEXT), Ordering::Relaxed);
    } else {
        HC_TEXT.store(0, Ordering::Relaxed);
    }
}

/// Win11 with centered taskbar icons: the only case where the far-left
/// anchor is offered. Cached because placement runs every tick; the
/// setting-change / TaskbarCreated paths invalidate the cache.
pub unsafe fn win11_centered_taskbar() -> bool {
    let cached = WIN11_CENTERED.load(Ordering::Relaxed);
    if cached != u32::MAX {
        return cached == 1;
    }
    let centered = taskbar_al() == Some(1);
    WIN11_CENTERED.store(u32::from(centered), Ordering::Relaxed);
    centered
}

/// Whether the effective theme is light (config override wins over the
/// system flag).
pub fn theme_is_light() -> bool {
    match config().display.theme {
        Theme::Auto => LIGHT_THEME.load(Ordering::Relaxed) == 1,
        Theme::Light => true,
        Theme::Dark => false,
    }
}

/// Indicator glyph color. High contrast wins over both the system
/// light/dark flag and the user's `display.theme` override.
pub fn indicator_text_rgb() -> (u32, u32, u32) {
    if HIGH_CONTRAST.load(Ordering::Relaxed) == 1 {
        let bgr = HC_TEXT.load(Ordering::Relaxed);
        (bgr & 0xFF, (bgr >> 8) & 0xFF, (bgr >> 16) & 0xFF)
    } else if theme_is_light() {
        (0, 0, 0)
    } else {
        (255, 255, 255)
    }
}

// ------------------------------------------------------------------- fonts

// System UI face + pixel height, cached on the message thread. Read
// from `NONCLIENTMETRICS.lfMessageFont` so the indicator's font follows
// the system setting (face and size). `WM_SETTINGCHANGE` and
// `WM_DPICHANGED` invalidate this cache: text-size and per-monitor DPI
// changes must not require a restart.
thread_local! {
    pub static UI_FONT: RefCell<Option<(i32, Vec<u16>)>> = const { RefCell::new(None) };
}

pub fn ui_font() -> (i32, Vec<u16>) {
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

pub fn invalidate_ui_font() {
    UI_FONT.with(|slot| *slot.borrow_mut() = None);
}

/// DPI of the monitor hosting `hwnd`. `GetDpiForSystem` is only a fallback
/// when the window handle is unavailable.
pub fn dpi_for(hwnd: HWND) -> u32 {
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
pub fn effective_font_height_for(hwnd: HWND, vertical: bool) -> i32 {
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
pub fn pad_px(vertical: bool) -> i32 {
    if vertical {
        VERTICAL_PAD_PX
    } else {
        PAD_PX
    }
}

/// Width slack for the given orientation.
pub fn width_slack_px(vertical: bool) -> i32 {
    if vertical {
        VERTICAL_WIDTH_SLACK_PX
    } else {
        WIDTH_SLACK_PX
    }
}

/// Overlay font: the system UI face at the system UI size, grayscale AA.
/// Antialiased (not ClearType) because the glyphs are composited
/// per-pixel-alpha over the taskbar.
pub unsafe fn make_font_for(hwnd: HWND, vertical: bool) -> HFONT {
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
pub unsafe fn make_font_at_for(hwnd: HWND, scale: i32, vertical: bool) -> HFONT {
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
