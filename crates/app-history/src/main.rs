//! History popup entry: a warm GPUI host on Windows, a one-shot popup on macOS.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
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
use sysvitals_panel::parent_pid_arg;
#[cfg(windows)]
use sysvitals_panel::set_per_monitor_dpi_aware;
use sysvitals_panel::{
    acquire_instance_lock, anchor_arg, anchor_monitor_arg, anchor_visible_bounds, backdrop_for,
    detect_material, layout_rect, log_event, log_sanitize_issues, observe_system_appearance,
    prewarm_arg, set_gpu_windowed, spawn_accent_watch, take_panel_request, trim_working_set,
    window_display_id, ConfigEvent, InstanceLock, PanelAction, PanelSurface, CONTROL_RADIUS,
    PANEL_HISTORY,
};

static START: OnceLock<Instant> = OnceLock::new();
static OPEN_STARTED: Mutex<Option<Instant>> = Mutex::new(None);

static WINDOW_CREATED_MS: AtomicU64 = AtomicU64::new(0);

fn begin_open() -> Instant {
    let now = Instant::now();
    *OPEN_STARTED
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(now);
    now
}

fn elapsed_ms() -> u128 {
    let open = *OPEN_STARTED
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    open.unwrap_or_else(|| START.get().copied().unwrap_or_else(Instant::now))
        .elapsed()
        .as_millis()
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

#[cfg(windows)]
const INSTANCE_LOCK_NAME: &str = "Local\\SysVitalsHistory_SingleInstance";
#[cfg(not(windows))]
const INSTANCE_LOCK_NAME: &str = "history.lock";
const REQUEST_MAX_AGE_MS: u64 = 10_000;
const REQUEST_POLL_MS: u64 = 50;

fn main() {
    sysvitals_panel::install_panic_hook();
    let _ = START.set(Instant::now());
    let started = START.get().copied().unwrap_or_else(Instant::now);

    let _instance = match acquire_instance_lock(INSTANCE_LOCK_NAME) {
        InstanceLock::Acquired(guard) => Some(guard),
        InstanceLock::Busy => {
            log_event(
                "WARN",
                "history: another history host is already running; exiting",
            );
            return;
        }
        InstanceLock::Unavailable(error) => {
            log_event(
                "WARN",
                &format!("history: instance guard unavailable: {error}; continuing"),
            );
            None
        }
    };

    #[cfg(windows)]
    set_per_monitor_dpi_aware();

    #[cfg(windows)]
    let resident = parent_pid_arg().is_some();
    #[cfg(not(windows))]
    let resident = false;
    let prewarm = prewarm_arg();
    let ready_file = sysvitals_panel::ready_file_arg();
    let ready_file_for_exit = ready_file.clone();
    let app = sysvitals_panel::build_panel_app(
        cfg!(target_os = "macos"),
        if resident {
            QuitMode::Explicit
        } else {
            QuitMode::LastWindowClosed
        },
    );

    log_event(
        "INFO",
        &format!("history: application built at {}ms", elapsed_ms()),
    );

    app.run(move |cx| {
        sysvitals_panel::init_panel(cx, "history", started, apply_theme_and_accent);

        let anchor = anchor_arg();
        let monitor = anchor_monitor_arg().map(DisplayId::new);
        log_event(
            "INFO",
            &format!(
                "history: popup starting (version={}, anchor={:?}, monitor={:?})",
                env!("CARGO_PKG_VERSION"),
                anchor.as_ref().map(|a| (a.left, a.top, a.right, a.bottom)),
                monitor
            ),
        );
        #[cfg(windows)]
        let hosted = resident;
        #[cfg(not(windows))]
        let hosted = true;
        if hosted {
            start_history_host(cx, resident, prewarm, anchor, monitor, ready_file);
            return;
        }
        if open_history(cx, anchor, monitor, ready_file.as_deref()).is_none() {
            cx.quit();
        }
    });
    if !resident {
        if let Some(path) = ready_file_for_exit {
            let _ = std::fs::remove_file(path);
        }
    }
    log_event("INFO", "history: exited");
}

fn open_history(
    cx: &mut App,
    anchor: Option<AnchorRect>,
    monitor: Option<DisplayId>,
    ready_file: Option<&std::path::Path>,
) -> Option<AnyWindowHandle> {
    let request_started = begin_open();
    let window_handle = open_history_window(cx, anchor, monitor)?;
    write_ready_marker(ready_file);
    sysvitals_panel::log_first_frame(window_handle, cx, "history", request_started);
    #[cfg(windows)]
    spawn_history_activation(cx, window_handle);
    Some(window_handle)
}

#[cfg(windows)]
fn spawn_history_activation(cx: &mut App, window_handle: AnyWindowHandle) {
    cx.spawn(async move |cx| {
        let Some(window_handle) = window_handle.downcast::<HistoryView>() else {
            return;
        };
        for _ in 0..3 {
            let _ = cx.update(|app| {
                window_handle.update(app, |_view, window, _| window.activate_window())
            });
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            let activated = cx.update(|app| {
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
}

fn start_history_host(
    cx: &mut App,
    resident: bool,
    prewarm: bool,
    anchor: Option<AnchorRect>,
    monitor: Option<DisplayId>,
    ready_file: Option<std::path::PathBuf>,
) {
    if prewarm {
        log_event("INFO", "history: prewarmed without a window");
    } else if open_history(cx, anchor, monitor, ready_file.as_deref()).is_none() {
        log_event(
            "WARN",
            "history: initial window failed to open; exiting so the next click can retry",
        );
        cx.quit();
        return;
    }
    cx.spawn(async move |cx| {
        let mut idle_since: Option<Instant> = None;
        let mut idle_handled = false;
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(REQUEST_POLL_MS))
                .await;
            let empty = cx.update(|app| app.windows().is_empty());
            if resident {
                if empty {
                    if idle_since.is_none() {
                        idle_since = Some(Instant::now());
                        idle_handled = false;
                    }
                    if !idle_handled
                        && idle_since.is_some_and(|at| at.elapsed() >= Duration::from_secs(2))
                    {
                        let _ = set_gpu_windowed(cx, false).await;
                        trim_working_set();
                        idle_handled = true;
                        log_event("INFO", "history: resident host is idle");
                    }
                } else {
                    idle_since = None;
                    idle_handled = false;
                }
            }

            let Some(request) = take_panel_request(PANEL_HISTORY) else {
                continue;
            };
            if request.is_stale(REQUEST_MAX_AGE_MS) {
                log_event("INFO", "history: stale panel request ignored");
                continue;
            }
            if !set_gpu_windowed(cx, true).await {
                log_event(
                    "WARN",
                    "history: DirectX device unavailable; exiting so the next click can retry",
                );
                cx.update(|app| app.quit());
                return;
            }
            let result = cx.update(|app| {
                apply_history_request(
                    app,
                    request.action,
                    request.anchor,
                    request.monitor.map(DisplayId::new),
                    ready_file.as_deref(),
                )
            });
            if let Err(error) = result {
                log_event("WARN", &format!("history: panel request failed: {error}"));
                cx.update(|app| app.quit());
                return;
            }
            idle_since = None;
            idle_handled = false;
        }
    })
    .detach();
}

fn apply_history_request(
    cx: &mut App,
    action: PanelAction,
    anchor: Option<AnchorRect>,
    monitor: Option<DisplayId>,
    ready_file: Option<&std::path::Path>,
) -> Result<(), String> {
    let existing = cx.windows().first().copied();
    if let Some(handle) = existing {
        let existing_display = handle
            .update(cx, |_, window, cx| window_display_id(window, cx))
            .ok()
            .flatten();
        let same_monitor = monitor.is_none() || existing_display == monitor;
        if action == PanelAction::Toggle && same_monitor {
            let removed = handle
                .update(cx, |_, window, _| window.remove_window())
                .is_ok();
            if removed {
                log_event("INFO", "history: window closed by indicator");
                return Ok(());
            }
            log_event("WARN", "history: remove_window failed");
            return Ok(());
        }
        if action == PanelAction::Open && same_monitor {
            let _ = handle.update(cx, |_, window, _| window.activate_window());
            return Ok(());
        }
        #[cfg(target_os = "macos")]
        {
            // The one-shot macOS host quits with its last window: open the
            // replacement first so a display move never empties the app. The
            // old popup also closes itself when the new window takes focus.
            log_event("INFO", "history: moving the popup to the requested display");
            if open_history(cx, anchor, monitor, None).is_some() {
                if cx.windows().contains(&handle)
                    && handle
                        .update(cx, |_, window, _| window.remove_window())
                        .is_err()
                {
                    log_event("WARN", "history: old popup did not close after the move");
                }
            } else {
                log_event(
                    "WARN",
                    "history: keeping the existing popup; open on the requested display failed",
                );
            }
            return Ok(());
        }
        #[cfg(not(target_os = "macos"))]
        {
            let removed = handle
                .update(cx, |_, window, _| window.remove_window())
                .is_ok();
            if !removed {
                log_event("WARN", "history: remove_window failed before the move");
            }
            log_event("INFO", "history: moving the popup to the requested display");
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                cx.update(|app| {
                    if open_history(app, anchor, monitor, None).is_none() {
                        log_event("WARN", "history: reopen on the requested display failed");
                    }
                });
            })
            .detach();
            return Ok(());
        }
    }

    open_history(cx, anchor, monitor, ready_file)
        .map(|_| ())
        .ok_or_else(|| "open_history_window returned no window".to_string())
}
