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

/// Quit when the indicator that launched this panel disappears without
/// running its quit path. The Windows job object and the macOS parent watch
/// both live in `sysvitals_panel` so the panels share one policy.
pub(crate) fn watch_parent(cx: &mut App) {
    sysvitals_panel::watch_parent(cx, "settings");
}

const SETTINGS_MARGIN: f32 = 16.0;
const SETTINGS_MIN_SIZE: f32 = 200.0;

/// Fit the requested window size into a display's usable work area. GPUI's
/// `check_given_bounds()` only verifies the window center, so a fixed
/// 400x740 settings window can otherwise spill above/below a short work area
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

#[cfg(test)]
mod tests {
    use super::*;

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
