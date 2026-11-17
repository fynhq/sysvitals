//! History popup window construction, anchoring, and show/hide.

use super::*;

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
    let cfg = match config_path() {
        Ok(path) => match Config::load(&path) {
            Ok((cfg, issues)) => {
                log_sanitize_issues("history: config repair", &issues);
                cfg
            }
            Err(e) => {
                log_event(
                    "WARN",
                    &format!("history: config load failed: {e}; using defaults"),
                );
                Config::default()
            }
        },
        Err(e) => {
            log_event(
                "WARN",
                &format!("history: config path unavailable: {e}; using defaults"),
            );
            Config::default()
        }
    };
    let lang = cfg.display.language;
    let window_title = format!("SysVitals {}", t(lang, "chart.title"));

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
        display_id: monitor,
        titlebar,
        kind: if anchored {
            WindowKind::PopUp
        } else {
            WindowKind::Normal
        },
        is_movable: !anchored,
        is_resizable: !anchored,
        window_background: backdrop_for(detect_material()),
        show: !hidden_start,
        focus: !hidden_start,
        ..Default::default()
    };

    let first_load = Arc::new(AtomicBool::new(false));
    let opened = gpui_kit::open_window(window_options, cx, {
        let first_load = first_load.clone();
        let shared = shared.clone();
        move |_window, cx| {
            cx.new(|cx| HistoryView::new(lang, hidden_start, layout_size, first_load, shared, cx))
        }
    });
    let (window_handle, _view) = match opened {
        Ok(opened) => opened,
        Err(e) => {
            log_event("WARN", &format!("history: open window failed: {e}"));
            return None;
        }
    };
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
    log_event(
        "INFO",
        &format!(
            "history: open_window returned at {}ms",
            WINDOW_CREATED_MS.load(Ordering::Relaxed)
        ),
    );
    let _ = window_handle.update(cx, |_, window, _| {
        window.on_next_frame(move |_, _| {
            log_event(
                "INFO",
                &format!("history: first frame at {}ms", elapsed_ms()),
            );
        });
    });
    Some((window_handle, first_load, layout_size))
}
