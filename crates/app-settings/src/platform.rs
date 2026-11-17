//! Platform gates for settings layout options.

use super::*;

#[cfg(windows)]
pub(crate) fn layout_available() -> bool {
    taskbar_al() == Some(1)
}

#[cfg(not(windows))]
pub(crate) fn layout_available() -> bool {
    false
}

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

const SETTINGS_MARGIN: f32 = 16.0;
const SETTINGS_MIN_SIZE: f32 = 200.0;

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

pub(crate) fn settings_window_bounds(
    anchor: Option<&AnchorRect>,
    monitor: Option<DisplayId>,
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

#[cfg(test)]
mod tests {
    use super::{centered_on_work, fit_window_size, point, px, size, Bounds, Pixels, WindowBounds};

    fn work(width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(0.0), px(0.0)), size(px(width), px(height)))
    }

    #[test]
    fn a_short_work_area_shrinks_the_settings_window() {
        let fitted = fit_window_size(size(px(400.0), px(740.0)), work(1280.0, 600.0));
        assert_eq!(f32::from(fitted.width), 400.0);
        assert_eq!(f32::from(fitted.height), 568.0);
    }

    #[test]
    fn centering_keeps_the_window_inside_the_work_area() {
        let bounds = centered_on_work(work(1280.0, 600.0), size(px(400.0), px(740.0)));
        let WindowBounds::Windowed(bounds) = bounds else {
            panic!("the settings window is always windowed");
        };
        assert_eq!(f32::from(bounds.origin.x), 440.0);
        assert_eq!(f32::from(bounds.origin.y), 16.0);
        assert_eq!(f32::from(bounds.size.height), 568.0);
    }
}
