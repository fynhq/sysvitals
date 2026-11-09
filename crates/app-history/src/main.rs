//! SysVitals History — a GPUI trend chart popup.
//!
//! Launched by the platform overlay when the user clicks the indicator.
//! Shows CPU and memory usage over time as one merged chart, with a time
//! range selector and a Copy CSV button. When the launcher passes the
//! indicator rectangle via `--anchor`, the window opens as a topmost
//! borderless popup beside the indicator; otherwise it opens centered.
//! Closes on Esc or window blur.
//!
//! Module map: `view/mod` (state, lifecycle observers and config reload),
//! `view/data` (queries, CSV and chart point preparation), `view/render`
//! (Render pass and axis labels), `chart` (ranges/axis/colors), `placement`,
//! `resident` (Windows shared-memory loop), `theme` and `window`.
//! All items in this binary are crate-internal (`pub(crate)`).

// A GUI child process must not allocate a console window on Windows: without
// this attribute the binary is built as a console subsystem executable and a
// terminal flashes up beside the history window every time it launches.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::env;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
#[cfg(windows)]
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ, FILE_MAP_WRITE,
};
#[cfg(windows)]
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, SetProcessWorkingSetSize, WaitForSingleObject,
    PROCESS_SYNCHRONIZE,
};

use gpui_kit::base::plot::label::{PlotLabel, Text as PlotText};
use gpui_kit::component::button::*;
use gpui_kit::component::chart::AreaChart;
use gpui_kit::component::theme::Theme as UiTheme;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use sysvitals_core::cli::AnchorRect;
use sysvitals_core::config::{config_path, history_path, Config, CsvScope};
use sysvitals_core::i18n::{t, Language};
use sysvitals_core::store::{floor_to_grid, Bucket, Store};
#[cfg(windows)]
use sysvitals_panel::panel_font_family;
use sysvitals_panel::{
    anchor_visible_bounds, backdrop_for, detect_material, log_event, observe_system_appearance,
    spawn_accent_watch, PanelSurface, CONTROL_RADIUS,
};
#[cfg(windows)]
use sysvitals_panel::{preplace_hidden_window, set_per_monitor_dpi_aware, utf16, window_hwnd};

// --------------------------------------------------------------- diagnostics

/// Process-start instant for the popup's startup milestones. The launcher
/// already logs how long `CreateProcessW` took; these milestones cover the
/// other half of the wait — from process start to window creation, first
/// chart data, and (for a pre-warmed window) the reveal.
static START: OnceLock<Instant> = OnceLock::new();

/// Elapsed time at the moment the window was created, stored so the first
/// data-load log (which runs after the first frame, off the startup path)
/// can report it without writing to disk during startup.
static WINDOW_CREATED_MS: AtomicU64 = AtomicU64::new(0);

fn elapsed_ms() -> u128 {
    START
        .get()
        .map(|start| start.elapsed().as_millis())
        .unwrap_or(0)
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

    // GPUI does not declare DPI awareness itself. Without this the process
    // is treated as DPI-unaware and Windows bitmap-stretches the whole window
    // on a scaled display, which is what makes the text look soft.
    #[cfg(windows)]
    set_per_monitor_dpi_aware();

    // Windows resident mode: the overlay starts this process once and keeps
    // it alive with no window. macOS keeps the one-shot launch used by its
    // menu-bar overlay.
    #[cfg(windows)]
    let resident = env::args().any(|arg| arg == "--resident");

    // Register the bundled icon asset source before the run loop starts.
    // Without this the GPUI default asset source (`()`) returns `Ok(None)`
    // for every path, so every `IconName` renders as blank — this is exactly
    // what made the Copy / CircleCheck glyphs disappear from the copy button.
    //
    // macOS: this process is a transient panel of a menu-bar utility, so it
    // must not own foreground application UI (a Dock icon and a menu bar named
    // after the process). Requesting the headless windowing mode maps to the
    // AppKit accessory activation policy before the run loop starts, which
    // still opens windows normally. The default quit mode on macOS is explicit
    // (the process survives its last window), which would leave an invisible
    // accessory process running after the user closes this window, so the
    // process is asked to quit with its last window instead.
    //
    // Windows has no equivalent: a headless windowing request there refuses to
    // open windows, so the default windowed startup is left untouched there.
    #[cfg(target_os = "macos")]
    let app = gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .with_windowing(WindowingRequest::Headless)
        .with_quit_mode(QuitMode::LastWindowClosed);
    #[cfg(windows)]
    let app = if resident {
        // The resident process outlives every window; the indicator owns its
        // lifetime through the parent handle and explicit termination.
        gpui_kit::application()
            .with_assets(gpui_kit::assets::Assets)
            .with_quit_mode(QuitMode::Explicit)
    } else {
        // One-shot instance (manual launch): quit with the last window.
        // `QuitMode::Default` already means LastWindowClosed off macOS, but
        // spelling it out keeps the resident/one-shot split obvious at the
        // call site.
        gpui_kit::application()
            .with_assets(gpui_kit::assets::Assets)
            .with_quit_mode(QuitMode::LastWindowClosed)
    };
    #[cfg(not(any(windows, target_os = "macos")))]
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);

    app.run(move |cx| {
        gpui_kit::init(cx);
        // gpui-component's `theme::init` forces the light palette; sync
        // to the OS appearance before the window opens so the first paint
        // already has the right colors (the popup always follows the
        // system, independent of `display.theme`, which is indicator-only
        // now). The Windows accent override comes after, because a palette
        // reload wipes it.
        apply_theme_and_accent(cx);

        #[cfg(windows)]
        if resident {
            run_resident(cx);
            return;
        }

        #[cfg(target_os = "macos")]
        watch_parent(cx);

        let anchor = parse_anchor();
        let monitor = parse_anchor_monitor();
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
                // One-shot launch: the popup can land without activation when
                // the foreground lock refuses the launching process, so claim
                // activation as a foreground task — never inside render (see
                // `observe_activation`). Retry at the same cadence as the
                // resident loop, then restore the pre-move size even if no
                // activation edge ever arrives, so click-away and cross-DPI
                // sizing do not silently degrade.
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
