//! Pure window-placement math for the GPUI panels.
//!
//! The history popup and the settings window both place themselves against
//! the display's usable work area (taskbar/dock excluded). The arithmetic is
//! easy to get wrong on a secondary monitor, a narrow work area or a
//! negative-coordinate display, so it lives here and runs in the Linux
//! unit-test gate. `panel-support` converts GPUI `Bounds` into [`Rect`] and
//! the panels only construct their GPUI window bounds from the result.

/// A rectangle in logical pixels, with the same coordinate space as the work
/// area the caller measured (global desktop coordinates on Windows,
/// display-local on macOS).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Rect {
    pub const fn new(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    pub fn width(self) -> f32 {
        self.right - self.left
    }

    pub fn height(self) -> f32 {
        self.bottom - self.top
    }
}

/// Clamp a window size into a work area, keeping `margin` on every edge and
/// never going below `min_size` for either dimension. The minimum only raises
/// the cap, it does not grow a smaller requested size.
pub fn fit_size(
    requested_w: f32,
    requested_h: f32,
    work: Rect,
    margin: f32,
    min_size: f32,
) -> (f32, f32) {
    let max_w = (work.width() - 2.0 * margin).max(min_size);
    let max_h = (work.height() - 2.0 * margin).max(min_size);
    (requested_w.min(max_w), requested_h.min(max_h))
}

/// Origin for the history popup. The indicator sits inside the taskbar or
/// menu bar, which is *outside* the work area, so anchoring to the
/// indicator's own edge leaves almost no visual gap to the bar. Place the
/// popup relative to the work-area edge instead, with the same `gap` the
/// shell's own tray flyouts use. The bar side is inferred from the anchor
/// lying beyond that edge.
///
/// `size_w` and `size_h` are the popup's logical size (already fitted to the
/// work area by the caller; this function still clamps defensively).
pub fn anchor_origin(anchor: Rect, size_w: f32, size_h: f32, work: Rect, gap: f32) -> (f32, f32) {
    // Keep the same gap on the left/right screen edges: with the indicator
    // anchored at the taskbar's far left the centered popup would otherwise
    // be clamped flush against the monitor edge (x = 0). The width passed in
    // is already shrunk to leave room for both gaps on a narrow work area.
    let clamp_x = |x: f32| {
        let min = work.left + gap;
        let max = (work.right - size_w - gap).max(min);
        x.max(min).min(max)
    };
    let clamp_y = |y: f32| y.max(work.top).min((work.bottom - size_h).max(work.top));
    let center_x = (anchor.left + anchor.right) * 0.5;
    let center_y = (anchor.top + anchor.bottom) * 0.5;

    // The bar is below/above/left/right of the work area when the indicator
    // rect lies beyond that edge.
    let bar_below = anchor.top >= work.bottom - 1.0 && anchor.bottom > work.bottom;
    let bar_above = anchor.bottom <= work.top + 1.0 && anchor.top < work.top;
    let bar_left = anchor.right <= work.left + 1.0;
    let bar_right = anchor.left >= work.right - 1.0;

    if bar_below {
        return (
            clamp_x(center_x - size_w * 0.5),
            (work.bottom - gap - size_h).max(work.top),
        );
    }
    if bar_above {
        return (
            clamp_x(center_x - size_w * 0.5),
            (work.top + gap).min((work.bottom - size_h).max(work.top)),
        );
    }
    if bar_left {
        return (
            (work.left + gap).min(work.right - size_w),
            clamp_y(center_y - size_h * 0.5),
        );
    }
    if bar_right {
        return (
            (work.right - gap - size_w).max(work.left),
            clamp_y(center_y - size_h * 0.5),
        );
    }

    // Fallback for a free-floating indicator: prefer below, then above.
    let below = anchor.bottom + gap;
    let above = anchor.top - gap - size_h;
    let y = if below + size_h <= work.bottom {
        below
    } else if above >= work.top {
        above
    } else {
        (work.bottom - size_h).max(work.top)
    };
    (clamp_x(center_x - size_w * 0.5), y)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GAP: f32 = 8.0;
    const W: f32 = 680.0;
    const H: f32 = 390.0;

    fn rect(left: f32, top: f32, right: f32, bottom: f32) -> Rect {
        Rect::new(left, top, right, bottom)
    }

    #[test]
    fn rect_reports_width_and_height() {
        let r = rect(-10.0, 20.0, 90.0, 220.0);
        assert_eq!(r.width(), 100.0);
        assert_eq!(r.height(), 200.0);
    }

    #[test]
    fn anchor_below_the_work_area_uses_the_bottom_edge() {
        let work = rect(0.0, 0.0, 1920.0, 1040.0);
        let anchor = rect(900.0, 1040.0, 1000.0, 1080.0);
        assert_eq!(anchor_origin(anchor, W, H, work, GAP), (610.0, 642.0));
    }

    #[test]
    fn anchor_above_the_work_area_uses_the_top_edge() {
        let work = rect(0.0, 40.0, 1920.0, 1080.0);
        let anchor = rect(900.0, 0.0, 1000.0, 40.0);
        assert_eq!(anchor_origin(anchor, W, H, work, GAP), (610.0, 48.0));
    }

    #[test]
    fn anchor_left_of_the_work_area_uses_the_left_edge() {
        let work = rect(40.0, 0.0, 1920.0, 1080.0);
        let anchor = rect(0.0, 500.0, 40.0, 600.0);
        assert_eq!(anchor_origin(anchor, W, H, work, GAP), (48.0, 355.0));
    }

    #[test]
    fn anchor_right_of_the_work_area_uses_the_right_edge() {
        let work = rect(0.0, 0.0, 1880.0, 1080.0);
        let anchor = rect(1880.0, 500.0, 1920.0, 600.0);
        assert_eq!(anchor_origin(anchor, W, H, work, GAP), (1192.0, 355.0));
    }

    #[test]
    fn narrow_work_area_clamps_to_the_available_edges() {
        let work = rect(0.0, 0.0, 500.0, 400.0);
        let anchor = rect(200.0, 400.0, 300.0, 440.0);
        assert_eq!(anchor_origin(anchor, W, H, work, GAP), (8.0, 2.0));
    }

    #[test]
    fn negative_coordinates_are_clamped_in_the_same_space() {
        let work = rect(-1920.0, 0.0, 0.0, 1040.0);
        let anchor = rect(-960.0, 1040.0, -860.0, 1080.0);
        assert_eq!(anchor_origin(anchor, W, H, work, GAP), (-1250.0, 642.0));
    }

    #[test]
    fn floating_anchor_prefers_below_then_above() {
        let work = rect(0.0, 0.0, 1920.0, 1080.0);
        let below = rect(500.0, 500.0, 600.0, 550.0);
        assert_eq!(anchor_origin(below, W, H, work, GAP), (210.0, 558.0));

        let above = rect(500.0, 900.0, 600.0, 950.0);
        assert_eq!(anchor_origin(above, W, H, work, GAP), (210.0, 502.0));
    }

    #[test]
    fn no_room_clamps_the_popup_to_the_work_area() {
        let work = rect(0.0, 0.0, 1920.0, 300.0);
        let anchor = rect(500.0, 100.0, 600.0, 150.0);
        assert_eq!(anchor_origin(anchor, W, H, work, GAP), (210.0, 0.0));
    }

    #[test]
    fn fit_size_keeps_a_margin_and_minimum() {
        let work = rect(0.0, 0.0, 1280.0, 720.0);
        assert_eq!(fit_size(400.0, 720.0, work, 16.0, 200.0), (400.0, 688.0));
        assert_eq!(fit_size(400.0, 100.0, work, 16.0, 200.0), (400.0, 100.0));

        let tiny = rect(0.0, 0.0, 100.0, 100.0);
        assert_eq!(fit_size(400.0, 720.0, tiny, 16.0, 200.0), (200.0, 200.0));
    }
}
