//! History popup entry: DPI awareness before the run loop; macOS headless, Windows one-shot.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use gpui_kit::component::button::*;
use gpui_kit::component::chart::AreaChart;
use gpui_kit::component::theme::Theme as UiTheme;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use sysvitals_core::cli::AnchorRect;
use sysvitals_core::config::{config_path, history_path, Config};
use sysvitals_core::i18n::{t, Language};
use sysvitals_core::store::{floor_to_grid, Bucket};
#[cfg(windows)]
use sysvitals_panel::set_per_monitor_dpi_aware;
use sysvitals_panel::{
    anchor_arg, anchor_monitor_arg, anchor_visible_bounds, backdrop_for, detect_material,
    layout_rect, log_event, log_sanitize_issues, observe_system_appearance, spawn_accent_watch,
    ConfigEvent, PanelSurface, CONTROL_RADIUS,
};

static START: OnceLock<Instant> = OnceLock::new();

static WINDOW_CREATED_MS: AtomicU64 = AtomicU64::new(0);

fn elapsed_ms() -> u128 {
    START
        .get()
        .map(|start| start.elapsed().as_millis())
        .unwrap_or(0)
}

fn write_ready_marker(path: Option<&std::path::Path>) {
    let Some(path) = path else {
        return;
    };
    if let Err(e) = std::fs::write(path, std::process::id().to_string()) {
        log_event("WARN", &format!("history: ready marker write failed: {e}"));
    }
}

mod chart;
mod placement;
mod theme;
mod view;
mod window;

pub(crate) use chart::*;
pub(crate) use placement::*;
pub(crate) use theme::*;
pub(crate) use view::*;
pub(crate) use window::*;

fn main() {
    sysvitals_panel::install_panic_hook();
    let _ = START.set(Instant::now());
    let started = START.get().copied().unwrap_or_else(Instant::now);

    #[cfg(windows)]
    set_per_monitor_dpi_aware();

    let app =
        sysvitals_panel::build_panel_app(cfg!(target_os = "macos"), QuitMode::LastWindowClosed);

    log_event(
        "INFO",
        &format!("history: application built at {}ms", elapsed_ms()),
    );

    app.run(move |cx| {
        sysvitals_panel::init_panel(cx, "history", started, apply_theme_and_accent);

        let anchor = anchor_arg();
        let monitor = anchor_monitor_arg().map(DisplayId::new);
        let ready_file = sysvitals_panel::ready_file_arg();
        log_event(
            "INFO",
            &format!(
                "history: popup starting (version={}, anchor={:?}, monitor={:?})",
                env!("CARGO_PKG_VERSION"),
                anchor.as_ref().map(|a| (a.left, a.top, a.right, a.bottom)),
                monitor
            ),
        );
        let Some(window_handle) = open_history_window(cx, anchor, monitor) else {
            cx.quit();
            return;
        };
        write_ready_marker(ready_file.as_deref());
        sysvitals_panel::log_first_frame(window_handle, cx, "history", started);
        #[cfg(windows)]
        cx.spawn(async move |cx| {
            let mut activated = false;
            for _ in 0..3 {
                let _ = cx.update(|app| {
                    window_handle.update(app, |_view, window, _| window.activate_window())
                });
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                activated = cx.update(|app| {
                    window_handle
                        .update(app, |view, window, _| {
                            if window.is_window_active() {
                                view.ever_active = true;
                                view.restore_popup_size(window);
                                true
                            } else {
                                false
                            }
                        })
                        .unwrap_or(false)
                });
                if activated {
                    return;
                }
            }
            let _ = cx.update(|app| {
                window_handle.update(app, |view, window, _| {
                    view.restore_popup_size(window);
                })
            });
            log_event(
                "WARN",
                "history: popup shown without activation; click-away is not armed",
            );
        })
        .detach();
    });
    log_event("INFO", "history: exited");
}
