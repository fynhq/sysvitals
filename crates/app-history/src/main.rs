//! History popup entry: DPI awareness before the run loop; macOS headless/accessory, Windows resident bootstrap.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::env;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use windows::Win32::Foundation::CloseHandle;
#[cfg(windows)]
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ, FILE_MAP_WRITE,
};
#[cfg(windows)]
use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};

use gpui_kit::base::plot::label::{PlotLabel, Text as PlotText};
use gpui_kit::component::button::*;
use gpui_kit::component::chart::AreaChart;
use gpui_kit::component::theme::Theme as UiTheme;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use sysvitals_core::cli::AnchorRect;
use sysvitals_core::config::{config_path, history_path, Config};
use sysvitals_core::i18n::{t, Language};
use sysvitals_core::store::{floor_to_grid, Bucket, Store};
use sysvitals_panel::{
    anchor_arg, anchor_monitor_arg, anchor_visible_bounds, backdrop_for, detect_material,
    layout_rect, log_event, log_sanitize_issues, observe_system_appearance, spawn_accent_watch,
    ConfigEvent, PanelSurface, CONTROL_RADIUS,
};
#[cfg(windows)]
use sysvitals_panel::{preplace_hidden_window, set_per_monitor_dpi_aware, utf16, window_hwnd};

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
mod resident;
mod theme;
mod view;
mod window;

pub(crate) use chart::*;
pub(crate) use placement::*;
pub(crate) use resident::*;
pub(crate) use theme::*;
pub(crate) use view::*;
pub(crate) use window::*;

fn main() {
    sysvitals_panel::install_panic_hook();
    let _ = START.set(Instant::now());

    #[cfg(windows)]
    set_per_monitor_dpi_aware();

    #[cfg(windows)]
    let resident = env::args().any(|arg| arg == "--resident");

    #[cfg(target_os = "macos")]
    let app = gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .with_windowing(WindowingRequest::Headless)
        .with_quit_mode(QuitMode::LastWindowClosed);
    #[cfg(windows)]
    let app = if resident {
        gpui_kit::application()
            .with_assets(gpui_kit::assets::Assets)
            .with_quit_mode(QuitMode::Explicit)
    } else {
        gpui_kit::application()
            .with_assets(gpui_kit::assets::Assets)
            .with_quit_mode(QuitMode::LastWindowClosed)
    };
    #[cfg(not(any(windows, target_os = "macos")))]
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);

    log_event(
        "INFO",
        &format!("history: application built at {}ms", elapsed_ms()),
    );

    app.run(move |cx| {
        gpui_kit::init(cx);
        log_event(
            "INFO",
            &format!("history: gpui init done at {}ms", elapsed_ms()),
        );
        apply_theme_and_accent(cx);

        #[cfg(windows)]
        if resident {
            run_resident(cx);
            return;
        }

        #[cfg(target_os = "macos")]
        watch_parent(cx);

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
        match open_history_window(cx, anchor, monitor, false, None) {
            Some((_window_handle, _ready, _size)) => {
                write_ready_marker(ready_file.as_deref());
                #[cfg(windows)]
                cx.spawn(async move |cx| {
                    let Some(window_handle) = _window_handle.downcast::<HistoryView>() else {
                        return;
                    };
                    for _ in 0..3 {
                        let _ = cx.update(|app| {
                            window_handle.update(app, |_view, window, _| {
                                window.activate_window();
                            })
                        });
                        cx.background_executor()
                            .timer(Duration::from_millis(500))
                            .await;
                        let active = cx.update(|app| {
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
                        if active {
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
                        "history: one-shot popup shown without activation; click-away is not armed",
                    );
                })
                .detach();
            }
            None => cx.quit(),
        }
    });
    log_event("INFO", "history: exited");
}
