//! placement: split out of the crate root (see `main.rs` for the module map).

use super::*;

/// The usable area of the display an anchor sits on (taskbar/dock excluded),
/// used to flip and clamp the popup inside a screen the indicator is on.
/// Display selection is shared with the settings window
/// (`sysvitals_panel::anchor_visible_bounds`: the native monitor handle is
/// authoritative when passed, the rectangle midpoint probe is the fallback
/// for launches without it); this wrapper adds the last-resort defaults.
pub(crate) fn anchor_work_area(
    anchor: &AnchorRect,
    monitor: Option<DisplayId>,
    cx: &App,
) -> Bounds<Pixels> {
    anchor_visible_bounds(cx, anchor, monitor)
        .or_else(|| cx.primary_display().map(|display| display.visible_bounds()))
        .unwrap_or_else(|| Bounds::new(point(px(0.), px(0.)), size(px(1280.), px(720.))))
}

/// Logical breathing room between the popup and the edges it is placed
/// against: the taskbar/menu-bar side and, when the anchor sits close to a
/// monitor edge, the left/right work-area edges too.
pub(crate) const POPUP_GAP: f32 = 8.0;

/// Origin for the popup. The indicator sits inside the taskbar/menu bar,
/// which is *outside* the work area, so the popup is placed relative to the
/// work-area edge with the same breathing room the shell's own tray flyouts
/// use. The math lives in `core::layout` so it runs in the Linux unit-test
/// gate; this wrapper only adapts GPUI types.
pub(crate) fn anchor_origin(
    anchor: &AnchorRect,
    size_w: f32,
    size_h: f32,
    work: &Bounds<Pixels>,
) -> Point<Pixels> {
    let anchor =
        sysvitals_core::layout::Rect::new(anchor.left, anchor.top, anchor.right, anchor.bottom);
    let (x, y) =
        sysvitals_core::layout::anchor_origin(anchor, size_w, size_h, layout_rect(work), POPUP_GAP);
    point(px(x), px(y))
}

// The native window handle helper (`window_hwnd`) and the hidden-window
// pre-placement (`preplace_hidden_window`) live in `sysvitals_panel`, shared
// with the settings window. Pre-placement must run outside an App update:
// `SetWindowPos` synchronously delivers `WM_MOVE`/`WM_SIZE`, which re-enter
// GPUI's window callbacks while the app is already borrowed, panicking with
// "RefCell already borrowed".

#[cfg(test)]
mod tests {
    use super::*;

    fn work(width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(0.0), px(0.0)), size(px(width), px(height)))
    }

    fn indicator(left: f32, top: f32) -> AnchorRect {
        AnchorRect {
            left,
            top,
            right: left + 100.0,
            bottom: top + 40.0,
        }
    }

    #[test]
    fn a_bottom_taskbar_anchor_places_the_popup_above_the_work_area() {
        let origin = anchor_origin(
            &indicator(900.0, 1040.0),
            680.0,
            390.0,
            &work(1920.0, 1040.0),
        );
        assert_eq!((f32::from(origin.x), f32::from(origin.y)), (610.0, 642.0));
    }

    #[test]
    fn a_narrow_work_area_keeps_the_side_gap() {
        let origin = anchor_origin(&indicator(0.0, 1040.0), 200.0, 390.0, &work(220.0, 1040.0));
        assert_eq!(f32::from(origin.x), 8.0);
    }

    #[test]
    fn an_anchor_outside_the_work_area_still_returns_a_usable_origin() {
        let origin = anchor_origin(
            &indicator(2000.0, 500.0),
            680.0,
            390.0,
            &work(1920.0, 1040.0),
        );
        assert!(f32::from(origin.x) + 680.0 <= 1920.0);
        assert!(f32::from(origin.y) + 390.0 <= 1040.0);
    }
}
