//! History popup window construction and anchoring.

use super::*;

pub(crate) fn open_history_window(
    cx: &mut App,
    anchor: Option<AnchorRect>,
    monitor: Option<DisplayId>,
) -> Option<AnyWindowHandle> {
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
        show: true,
        focus: true,
        ..Default::default()
    };

    let opened = gpui_kit::open_window(window_options, cx, move |_window, cx| {
        cx.new(|cx| HistoryView::new(lang, layout_size, cx))
    });
    let (window_handle, _view) = match opened {
        Ok(opened) => opened,
        Err(e) => {
            log_event("WARN", &format!("history: open window failed: {e}"));
            return None;
        }
    };
    WINDOW_CREATED_MS.store(elapsed_ms() as u64, Ordering::Relaxed);
    log_event(
        "INFO",
        &format!(
            "history: open_window returned at {}ms",
            WINDOW_CREATED_MS.load(Ordering::Relaxed)
        ),
    );
    Some(window_handle)
}
