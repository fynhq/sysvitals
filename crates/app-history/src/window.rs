//! window: split out of the crate root (see `main.rs` for the module map).

use super::*;

/// Open one history window. The caller owns its lifetime: the macOS path
/// opens it visible and lets the process exit with it, while the resident
/// Windows loop opens it hidden, waits for the first data load, then reveals
/// it.
///
/// `hidden_start` is passed to the view so its activation observer does not
/// claim activation for the offscreen first frame: the resident launcher
/// activates the window itself once the chart data is ready.
///
/// Returns the intended logical size of the anchored popup alongside the
/// window, so the resident loop can restore it when the OS refused to
/// activate (and therefore never delivered the activation edge the view
/// uses for the same correction).
pub(crate) fn open_history_window(
    cx: &mut App,
    anchor: Option<AnchorRect>,
    monitor: Option<DisplayId>,
    hidden_start: bool,
    shared: Option<Arc<HistoryShared>>,
) -> Option<(
    AnyWindowHandle,
    Arc<AtomicBool>,
    Option<gpui_kit::Size<Pixels>>,
)> {
    let cfg = match config_path().and_then(|p| Config::load(&p).map(|(c, _)| c)) {
        Ok(cfg) => cfg,
        Err(e) => {
            log_event(
                "WARN",
                &format!("history: config load failed: {e}; using defaults"),
            );
            Config::default()
        }
    };
    let lang = cfg.display.language;
    let window_title = format!("SysVitals {}", t(lang, "chart.title"));

    // Popup geometry in GPUI logical pixels. Height cut 25% (520 -> 390):
    // a 3x shorter vertical span is still readable for a two-line trend,
    // and the popup sits closer to the indicator without dominating the
    // desktop. Anchor origin logic keeps `POPUP_GAP` on every edge it is
    // placed against, so a work area narrower than the default width
    // shrinks the popup instead of pinning it flush to a screen edge.
    let popup_size = size(px(680.), px(390.));
    let (window_bounds, layout_size) = match anchor.as_ref() {
        Some(a) => {
            let work = anchor_work_area(a, monitor, cx);
            let work_w = f32::from(work.size.width);
            let work_h = f32::from(work.size.height);
            let width = f32::from(popup_size.width).min((work_w - 2.0 * POPUP_GAP).max(1.0));
            let height = f32::from(popup_size.height).min((work_h - 2.0 * POPUP_GAP).max(200.0));
            let popup_size = size(px(width), px(height));
            (
                Bounds::new(anchor_origin(a, width, height, &work), popup_size),
                Some(popup_size),
            )
        }
        None => (Bounds::centered(None, popup_size, cx), None),
    };
    let anchored = anchor.is_some();
    // The entrance starts on the anchor side and settles into the final
    // placement. A centered fallback window has no direction and only fades.
    let intro = anchor
        .as_ref()
        .map(|anchor| intro_offsets(anchor, &window_bounds))
        .unwrap_or((0.0, 0.0));

    // Anchored popups behave like a menu: no title bar, no shadow of
    // a normal window, cannot be dragged or resized. When no anchor
    // was passed, fall back to a plain titled window so the process
    // is still usable if the indicator failed to compute a rect.
    //
    // macOS needs the anchored case to pass `titlebar: None`. The
    // transparent-titlebar path keeps `NSClosableWindowMask` while adding
    // `NSFullSizeContentViewWindowMask`, so AppKit still renders the
    // traffic-light buttons on top of content that now spans the whole
    // window — they collided with the chart's top-left. Dropping the
    // titlebar options removes the closable bit from the style mask, so no
    // window buttons are created. The library's only other lever,
    // `traffic_light_position`, merely relocates the buttons rather than
    // removing them. Windows ignores this field for its borderless popup,
    // so that platform keeps the previous options unchanged.
    #[cfg(target_os = "macos")]
    let titlebar = if anchored {
        None
    } else {
        Some(TitlebarOptions {
            title: Some(window_title.clone().into()),
            ..Default::default()
        })
    };
    #[cfg(not(target_os = "macos"))]
    let titlebar = Some(TitlebarOptions {
        title: Some(window_title.clone().into()),
        appears_transparent: anchored,
        ..Default::default()
    });

    let window_options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(window_bounds)),
        // Select the indicator's display explicitly. Without this GPUI
        // converts the logical bounds with the primary display's scale
        // factor, which places a popup on the wrong screen (or falls back to
        // the primary display) whenever the two monitors use different DPIs.
        display_id: monitor,
        titlebar,
        kind: if anchored {
            WindowKind::PopUp
        } else {
            WindowKind::Normal
        },
        is_movable: !anchored,
        is_resizable: !anchored,
        // System backdrop material (Win11 Mica / Win10-era acrylic blur /
        // macOS window blur, opaque fallback on anything older or when the
        // user disabled transparency effects). The palette tint applied in
        // `apply_theme_and_accent` is what lets it show through. This is
        // purely a compositor attribute — it does not change the window's
        // activation edges, so the blur-close contract is untouched.
        window_background: backdrop_for(detect_material()),
        show: !hidden_start,
        focus: !hidden_start,
        ..Default::default()
    };

    let first_load = Arc::new(AtomicBool::new(false));
    // Blur-close and appearance observation are registered from the first
    // render pass (both idempotent); Esc is handled by an app-level keystroke
    // observer set up in `HistoryView::new`. The build closure only needs to
    // create the view.
    let opened = gpui_kit::open_window(window_options, cx, {
        let first_load = first_load.clone();
        let shared = shared.clone();
        move |_window, cx| {
            cx.new(|cx| {
                HistoryView::new(
                    lang,
                    hidden_start,
                    layout_size,
                    intro,
                    first_load,
                    shared,
                    cx,
                )
            })
        }
    });
    let (window_handle, _view) = match opened {
        Ok(opened) => opened,
        Err(e) => {
            log_event("WARN", &format!("history: open window failed: {e}"));
            return None;
        }
    };
    // The resident path opens the window hidden. Crossing a display DPI
    // boundary during activation would make Windows resize it in view, so
    // move it onto the target display now; the resize then happens while the
    // window is still invisible. The move runs as a foreground task because
    // `SetWindowPos` must not be called while the app is borrowed (see
    // `sysvitals_panel::preplace_hidden_window`); the task still executes
    // well before the resident loop activates the window.
    #[cfg(windows)]
    if hidden_start && anchored {
        if let Some(monitor) = monitor {
            let hwnd = window_handle
                .update(cx, |_view, window, _| window_hwnd(&*window))
                .ok()
                .flatten();
            match hwnd {
                Some(hwnd) => {
                    cx.foreground_executor()
                        .spawn(async move {
                            if !preplace_hidden_window(hwnd, monitor) {
                                log_event(
                                    "WARN",
                                    &format!(
                                        "history: could not pre-place the popup on display 0x{:x}",
                                        u64::from(monitor)
                                    ),
                                );
                            }
                        })
                        .detach();
                }
                None => {
                    log_event(
                        "WARN",
                        "history: could not resolve the popup window for pre-placement",
                    );
                }
            }
        }
    }
    WINDOW_CREATED_MS.store(elapsed_ms() as u64, Ordering::Relaxed);
    Some((window_handle, first_load, layout_size))
}

/// Direction and distance of the popup's one-shot entrance, from the anchor
/// side toward the resting position. Vertical bars (taskbar left/right) slide
/// horizontally; the top/bottom taskbar and menu-bar cases slide vertically.
/// The popup is clamped to the work area, so a rectangle that overlaps the
/// anchor on both axes falls back to a fade only.
fn intro_offsets(anchor: &AnchorRect, bounds: &Bounds<Pixels>) -> (f32, f32) {
    const SLIDE: f32 = 6.0;
    let left = f32::from(bounds.origin.x);
    let top = f32::from(bounds.origin.y);
    let right = left + f32::from(bounds.size.width);
    let bottom = top + f32::from(bounds.size.height);

    if bottom <= anchor.top {
        // Popup above the anchor: start lower and settle upward.
        (0.0, SLIDE)
    } else if top >= anchor.bottom {
        // Popup below the anchor: start higher and settle downward.
        (0.0, -SLIDE)
    } else if right <= anchor.left {
        // Popup left of the anchor: start closer to it on the right.
        (SLIDE, 0.0)
    } else if left >= anchor.right {
        // Popup right of the anchor: start closer to it on the left.
        (-SLIDE, 0.0)
    } else {
        (0.0, 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::intro_offsets;
    use gpui_kit::{point, px, size, Bounds, Pixels};
    use sysvitals_core::cli::AnchorRect;

    fn bounds(left: f32, top: f32, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(left), px(top)), size(px(width), px(height)))
    }

    fn anchor(left: f32, top: f32, right: f32, bottom: f32) -> AnchorRect {
        AnchorRect {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn entrance_slides_from_the_anchor_side() {
        // Bottom taskbar: the popup sits above it and starts lower.
        let taskbar = anchor(900.0, 1040.0, 1000.0, 1080.0);
        assert_eq!(
            intro_offsets(&taskbar, &bounds(610.0, 642.0, 680.0, 390.0)),
            (0.0, 6.0)
        );

        // Top menu bar: the popup sits below it and starts higher.
        let menu_bar = anchor(900.0, 0.0, 1000.0, 40.0);
        assert_eq!(
            intro_offsets(&menu_bar, &bounds(610.0, 48.0, 680.0, 390.0)),
            (0.0, -6.0)
        );

        // Vertical bar on the left: the popup sits right of it and starts left.
        let bar_on_the_left = anchor(0.0, 400.0, 40.0, 500.0);
        assert_eq!(
            intro_offsets(&bar_on_the_left, &bounds(48.0, 300.0, 680.0, 390.0)),
            (-6.0, 0.0)
        );

        // Vertical bar on the right: the popup sits left of it and starts right.
        let bar_on_the_right = anchor(1880.0, 400.0, 1920.0, 500.0);
        assert_eq!(
            intro_offsets(&bar_on_the_right, &bounds(1152.0, 300.0, 680.0, 390.0)),
            (6.0, 0.0)
        );
    }

    #[test]
    fn entrance_falls_back_to_a_fade_when_placement_overlaps() {
        let anchor = anchor(150.0, 150.0, 180.0, 180.0);
        assert_eq!(
            intro_offsets(&anchor, &bounds(100.0, 100.0, 200.0, 200.0)),
            (0.0, 0.0)
        );
    }
}
