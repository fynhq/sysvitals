//! Taskbar theme, font, and color resolution.

use super::*;

pub(crate) unsafe fn refresh_theme() {
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

    let mut hc = HIGHCONTRASTW {
        cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
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

pub(crate) unsafe fn win11_centered_taskbar() -> bool {
    let cached = WIN11_CENTERED.load(Ordering::Relaxed);
    if cached != u32::MAX {
        return cached == 1;
    }
    let centered = taskbar_al() == Some(1);
    WIN11_CENTERED.store(u32::from(centered), Ordering::Relaxed);
    centered
}

pub(crate) fn theme_is_light() -> bool {
    match config().display.theme {
        Theme::Auto => LIGHT_THEME.load(Ordering::Relaxed) == 1,
        Theme::Light => true,
        Theme::Dark => false,
    }
}

pub(crate) fn indicator_text_rgb() -> (u32, u32, u32) {
    if HIGH_CONTRAST.load(Ordering::Relaxed) == 1 {
        let bgr = HC_TEXT.load(Ordering::Relaxed);
        (bgr & 0xFF, (bgr >> 8) & 0xFF, (bgr >> 16) & 0xFF)
    } else if theme_is_light() {
        (0, 0, 0)
    } else {
        (255, 255, 255)
    }
}

thread_local! {
    pub(crate) static UI_FONT: RefCell<Option<(i32, Vec<u16>)>> = const { RefCell::new(None) };
}

pub(crate) fn ui_font() -> (i32, Vec<u16>) {
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

pub(crate) fn invalidate_ui_font() {
    UI_FONT.with(|slot| *slot.borrow_mut() = None);
}

pub(crate) fn dpi_for(hwnd: HWND) -> u32 {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    (if dpi == 0 {
        unsafe { GetDpiForSystem() }
    } else {
        dpi
    })
    .max(96)
}

pub(crate) fn effective_font_height_for(hwnd: HWND, vertical: bool) -> i32 {
    let (height, _) = ui_font();
    let system_dpi = (unsafe { GetDpiForSystem() }).max(96) as i32;
    let window_dpi = dpi_for(hwnd) as i32;
    let height = height * window_dpi / system_dpi;
    if vertical {
        (height * VERTICAL_FONT_HUNDREDTHS / 100).min(-1)
    } else {
        height
    }
}

pub(crate) fn pad_px(vertical: bool) -> i32 {
    if vertical {
        VERTICAL_PAD_PX
    } else {
        PAD_PX
    }
}

pub(crate) fn width_slack_px(vertical: bool) -> i32 {
    if vertical {
        VERTICAL_WIDTH_SLACK_PX
    } else {
        WIDTH_SLACK_PX
    }
}

pub(crate) unsafe fn make_font_for(hwnd: HWND, vertical: bool) -> HFONT {
    make_font_at_for(hwnd, 1, vertical)
}

pub(crate) unsafe fn make_font_at_for(hwnd: HWND, scale: i32, vertical: bool) -> HFONT {
    let (_, face) = ui_font();
    create_font(
        effective_font_height_for(hwnd, vertical) * scale.max(1),
        TEXT_WEIGHT.load(Ordering::Relaxed),
        &face,
    )
}

pub(crate) unsafe fn create_font(height: i32, weight: i32, face: &[u16]) -> HFONT {
    CreateFontW(
        height,
        0,
        0,
        0,
        weight,
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
