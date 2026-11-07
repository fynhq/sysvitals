//! placement: split out of the crate root (see `main.rs` for the module map).

use super::*;

/// Parse `--anchor L T R B`; the pure parsing lives in `core::cli` (shared
/// with the settings panel). Absent, malformed, or degenerate rectangles
/// yield `None`, which keeps the old centered behavior — a bad argument must
/// never prevent the window from opening.
pub(crate) fn parse_anchor() -> Option<AnchorRect> {
    let args: Vec<String> = env::args().skip(1).collect();
    sysvitals_core::cli::parse_anchor(&args)
}

/// Parse `--anchor-monitor <handle>` from the process arguments: the native
/// display handle (HMONITOR on Windows, NSScreenNumber on macOS) that hosted
/// the indicator. `None` when absent, zero, or malformed. The pure parsing
/// lives in `core::cli` (unit-tested there).
pub(crate) fn parse_anchor_monitor() -> Option<DisplayId> {
    let args: Vec<String> = env::args().skip(1).collect();
    sysvitals_core::cli::parse_anchor_monitor(&args).map(DisplayId::new)
}

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
/// which is *outside* the work area, so anchoring to `anchor.top - gap`
/// leaves almost no visual gap to the bar. Place the popup relative to the
/// work-area edge instead, with the same breathing room the shell's own
/// tray flyouts use. The bar side is inferred from the anchor being outside
/// the work area on that edge.
pub(crate) fn anchor_origin(
    anchor: &AnchorRect,
    size_w: f32,
    size_h: f32,
    work: &Bounds<Pixels>,
) -> Point<Pixels> {
    let work_left = f32::from(work.origin.x);
    let work_right = work_left + f32::from(work.size.width);
    let work_top = f32::from(work.origin.y);
    let work_bottom = work_top + f32::from(work.size.height);
    // Keep the same gap on the left/right screen edges: with the indicator
    // anchored at the taskbar's far left the centered popup would otherwise
    // be clamped flush against the monitor edge (x = 0). The width passed in
    // is already shrunk to leave room for both gaps on a narrow work area.
    let clamp_x = |x: f32| {
        let min = work_left + POPUP_GAP;
        let max = (work_right - size_w - POPUP_GAP).max(min);
        x.max(min).min(max)
    };
    let clamp_y = |y: f32| y.max(work_top).min((work_bottom - size_h).max(work_top));
    let center_x = (anchor.left + anchor.right) * 0.5;
    let center_y = (anchor.top + anchor.bottom) * 0.5;

    // The bar is below/above/left/right of the work area when the indicator
    // rect lies beyond that edge.
    let bar_below = anchor.top >= work_bottom - 1.0 && anchor.bottom > work_bottom;
    let bar_above = anchor.bottom <= work_top + 1.0 && anchor.top < work_top;
    let bar_left = anchor.right <= work_left + 1.0;
    let bar_right = anchor.left >= work_right - 1.0;

    if bar_below {
        return point(
            px(clamp_x(center_x - size_w * 0.5)),
            px((work_bottom - POPUP_GAP - size_h).max(work_top)),
        );
    }
    if bar_above {
        return point(
            px(clamp_x(center_x - size_w * 0.5)),
            px((work_top + POPUP_GAP).min((work_bottom - size_h).max(work_top))),
        );
    }
    if bar_left {
        return point(
            px((work_left + POPUP_GAP).min(work_right - size_w)),
            px(clamp_y(center_y - size_h * 0.5)),
        );
    }
    if bar_right {
        return point(
            px((work_right - POPUP_GAP - size_w).max(work_left)),
            px(clamp_y(center_y - size_h * 0.5)),
        );
    }

    // Fallback for a free-floating indicator: prefer below, then above.
    let below = anchor.bottom + POPUP_GAP;
    let above = anchor.top - POPUP_GAP - size_h;
    let y = if below + size_h <= work_bottom {
        below
    } else if above >= work_top {
        above
    } else {
        (work_bottom - size_h).max(work_top)
    };
    point(px(clamp_x(center_x - size_w * 0.5)), px(y))
}

// The native window handle helper (`window_hwnd`) and the hidden-window
// pre-placement (`preplace_hidden_window`) live in `sysvitals_panel`, shared
// with the settings window. Pre-placement must run outside an App update:
// `SetWindowPos` synchronously delivers `WM_MOVE`/`WM_SIZE`, which re-enter
// GPUI's window callbacks while the app is already borrowed, panicking with
// "RefCell already borrowed".
