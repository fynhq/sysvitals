//! Settings window: one-shot GPUI process, entered from the SysVitalsPanel host.

use std::time::{Duration, Instant};

use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
#[cfg(windows)]
use gpui_kit::component::searchable_list::SearchableVec;
#[cfg(windows)]
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::theme::Theme as UiTheme;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use sysvitals_core::cli::AnchorRect;
use sysvitals_core::config::{
    ALLOWED_INTERVAL_MS, ALLOWED_ITEM_SPACING_PX, ALLOWED_LEFT_MARGIN_PX, MAX_PREFIX_CHARS,
};
use sysvitals_core::{t, Anchor, Config, Language, Theme};
use sysvitals_panel::{
    acquire_instance_lock, anchor_arg, anchor_monitor_arg, anchor_visible_bounds, backdrop_for,
    detect_material, layout_rect, log_event, observe_system_appearance, parent_pid_arg,
    spawn_accent_watch, take_panel_request, window_display_id, InstanceLock, PanelAction,
    PanelSurface, CONTROL_RADIUS, PANEL_REQUEST_MAX_AGE_MS, PANEL_REQUEST_POLL_MS, PANEL_SETTINGS,
};
#[cfg(windows)]
use sysvitals_panel::{preplace_hidden_window, taskbar_al, window_hwnd, window_is_visible};

mod config;
#[cfg(target_os = "macos")]
mod macos_select;
mod platform;
mod theme;
mod view;
mod widgets;

pub(crate) use config::*;
#[cfg(target_os = "macos")]
pub(crate) use macos_select::*;
pub(crate) use platform::*;
pub(crate) use theme::*;
pub(crate) use view::*;
pub(crate) use widgets::*;

#[cfg(windows)]
const INSTANCE_LOCK_NAME: &str = "Local\\SysVitalsSettings_SingleInstance";
#[cfg(not(windows))]
const INSTANCE_LOCK_NAME: &str = "settings.lock";

pub fn run() {
    let started = Instant::now();
    sysvitals_panel::install_panic_hook();
    let _instance = match acquire_instance_lock(INSTANCE_LOCK_NAME) {
        InstanceLock::Acquired(guard) => Some(guard),
        InstanceLock::Busy => {
            log_event(
                "WARN",
                "settings: another settings host is already running; exiting",
            );
            return;
        }
        InstanceLock::Unavailable(error) => {
            log_event(
                "WARN",
                &format!("settings: instance guard unavailable: {error}; continuing"),
            );
            None
        }
    };
    log_event(
        "INFO",
        &format!(
            "settings: starting (version={}, build={}, parent_pid={:?})",
            env!("CARGO_PKG_VERSION"),
            SV_BUILD_ID,
            parent_pid_arg()
        ),
    );
    #[cfg(windows)]
    log_event(
        "INFO",
        &format!(
            "settings: taskbar vertical={} win11_centered={}",
            taskbar_is_vertical(),
            layout_available()
        ),
    );
    #[cfg(windows)]
    sysvitals_panel::set_per_monitor_dpi_aware();

    // Settings is one-shot on both platforms: it exists only while its window is open.
    let app =
        sysvitals_panel::build_panel_app(cfg!(target_os = "macos"), QuitMode::LastWindowClosed);

    log_event(
        "INFO",
        &format!(
            "settings: application built at {}ms",
            started.elapsed().as_millis()
        ),
    );

    app.run(move |cx| {
        log_event(
            "INFO",
            &format!(
                "settings: app run entered at {}ms",
                started.elapsed().as_millis()
            ),
        );
        sysvitals_panel::init_panel(cx, "settings", started, apply_theme_and_accent);

        let anchor = anchor_arg();
        let monitor = anchor_monitor_arg().map(DisplayId::new);
        if open_settings_window(cx, anchor, monitor, Instant::now()).is_none() {
            cx.quit();
            return;
        }
        start_request_loop(cx);
    });
    log_event("INFO", "settings: exited");
}

fn open_settings_window(
    cx: &mut App,
    anchor: Option<AnchorRect>,
    monitor: Option<DisplayId>,
    request_started: Instant,
) -> Option<AnyWindowHandle> {
    let (cfg, config_read_ok) = load_config_with_backup();
    log_event(
        "INFO",
        &format!(
            "settings: config loaded at {}ms",
            request_started.elapsed().as_millis()
        ),
    );

    let titlebar = {
        #[cfg(target_os = "macos")]
        {
            TitlebarOptions {
                title: Some(t(cfg.display.language, "settings.title").to_string().into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(9.), px(9.))),
            }
        }
        #[cfg(windows)]
        {
            TitlebarOptions {
                title: Some(t(cfg.display.language, "settings.title").to_string().into()),
                ..TitleBar::title_bar_options()
            }
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            TitlebarOptions {
                title: Some(t(cfg.display.language, "settings.title").to_string().into()),
                ..Default::default()
            }
        }
    };

    let window_bounds =
        settings_window_bounds(anchor.as_ref(), monitor, size(px(400.), px(740.)), cx);
    let initial_size = window_bounds.get_bounds().size;
    let window_options = WindowOptions {
        window_bounds: Some(window_bounds),
        display_id: monitor,
        titlebar: Some(titlebar),
        window_background: backdrop_for(detect_material()),
        #[cfg(windows)]
        show: false,
        #[cfg(windows)]
        focus: false,
        #[cfg(not(windows))]
        focus: true,
        ..Default::default()
    };

    #[cfg(target_os = "macos")]
    set_macos_activation_policy(objc2_app_kit::NSApplicationActivationPolicy::Regular);

    let opened = gpui_kit::open_window(window_options, cx, move |window, cx| {
        cx.new(|cx| SettingsView::new(cfg, config_read_ok, window, cx, initial_size))
    });
    let (window_handle, _view) = match opened {
        Ok(opened) => opened,
        Err(e) => {
            log_event("WARN", &format!("settings: open window failed: {e}"));
            return None;
        }
    };

    log_event(
        "INFO",
        &format!(
            "settings: window built at {}ms",
            request_started.elapsed().as_millis()
        ),
    );
    sysvitals_panel::log_first_frames(window_handle, cx, "settings", request_started);

    #[cfg(windows)]
    {
        let hwnd = window_handle
            .update(cx, |_, window, _| window_hwnd(&*window))
            .ok()
            .flatten();
        cx.spawn(async move |cx| {
            if let (Some(hwnd), Some(monitor)) = (hwnd, monitor) {
                if !preplace_hidden_window(hwnd, monitor) {
                    log_event(
                        "WARN",
                        &format!(
                            "settings: could not pre-place the window on display 0x{:x}",
                            u64::from(monitor)
                        ),
                    );
                }
            }
            // GPUI shows the window on its first frame, so polling beats a fixed retry sleep.
            let deadline = Instant::now() + Duration::from_secs(4);
            let mut polls = 0u32;
            let mut activations = 0u32;
            let mut activated = false;
            let mut shown = false;
            loop {
                polls += 1;
                if polls == 1 || polls.is_multiple_of(4) {
                    activations += 1;
                    let _ = cx.update(|app| {
                        window_handle
                            .update(app, |_, window, _| window.resize(initial_size))
                            .is_ok()
                    });
                    activated |= cx.update(|app| {
                        window_handle
                            .update(app, |_, window, _| window.activate_window())
                            .is_ok()
                    });
                }
                let visible = hwnd.is_some_and(window_is_visible);
                let active = cx.update(|app| {
                    window_handle
                        .update(app, |_, window, _| window.is_window_active())
                        .unwrap_or(false)
                });
                if visible || active || (hwnd.is_none() && activated) {
                    shown = true;
                    log_event(
                        "INFO",
                        &format!(
                            "settings: window shown after hidden placement (attempt {activations}, polls {polls}, visible={visible}, active={active}, elapsed={}ms)",
                            request_started.elapsed().as_millis()
                        ),
                    );
                    break;
                }
                if Instant::now() >= deadline {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(30))
                    .await;
            }
            if !shown {
                log_event(
                    "WARN",
                    &format!(
                        "settings: window remained hidden after {}ms (activation attempts {activations}); exiting so the next click can retry",
                        request_started.elapsed().as_millis()
                    ),
                );
                cx.update(|app| app.quit());
            }
        })
        .detach();
    }

    #[cfg(target_os = "macos")]
    {
        cx.activate(true);
        let _ = window_handle.update(cx, |_, window, _| window.activate_window());
        cx.spawn(async move |cx| {
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            cx.update(|app| {
                app.activate(true);
                let _ = window_handle.update(app, |_, window, _| window.activate_window());
            });
        })
        .detach();
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = window_handle;

    Some(window_handle)
}

fn show_settings(
    cx: &mut App,
    anchor: Option<AnchorRect>,
    monitor: Option<DisplayId>,
    request_started: Instant,
) {
    let existing = cx.windows().first().copied();
    if let Some(handle) = existing {
        let existing_display = handle
            .update(cx, |_, window, cx| window_display_id(window, cx))
            .ok()
            .flatten();
        if monitor.is_none() || existing_display == monitor {
            #[cfg(target_os = "macos")]
            set_macos_activation_policy(objc2_app_kit::NSApplicationActivationPolicy::Regular);
            let _ = handle.update(cx, |_, window, _| window.activate_window());
            #[cfg(target_os = "macos")]
            cx.activate(true);
            log_event("INFO", "settings: existing window focused");
            return;
        }
        // The one-shot host quits with its last window, so open the
        // replacement before removing the old window; a failed open keeps the
        // existing window instead of leaving no window at all.
        log_event(
            "INFO",
            "settings: moving the window to the requested display",
        );
        if open_settings_window(cx, anchor, monitor, request_started).is_some() {
            let removed = handle
                .update(cx, |_, window, _| window.remove_window())
                .is_ok();
            if !removed {
                log_event("WARN", "settings: remove_window failed after the move");
            }
        } else {
            log_event(
                "WARN",
                "settings: keeping the existing window; open on the requested display failed",
            );
        }
        return;
    }
    open_settings_window(cx, anchor, monitor, request_started);
}

fn start_request_loop(cx: &mut App) {
    cx.spawn(async move |cx| loop {
        cx.background_executor()
            .timer(Duration::from_millis(PANEL_REQUEST_POLL_MS))
            .await;
        let request = match take_panel_request(PANEL_SETTINGS) {
            Ok(Some(request)) => request,
            Ok(None) => continue,
            Err(error) => {
                log_event("WARN", &format!("settings: {error}"));
                continue;
            }
        };
        if request.is_stale(PANEL_REQUEST_MAX_AGE_MS) {
            log_event("INFO", "settings: stale panel request ignored");
            continue;
        }
        if request.action != PanelAction::Open {
            log_event("WARN", "settings: unsupported panel request ignored");
            continue;
        }
        let anchor = request.anchor;
        let monitor = request.monitor.map(DisplayId::new);
        let request_started = Instant::now();
        cx.update(|app| show_settings(app, anchor, monitor, request_started));
    })
    .detach();
}

#[cfg(target_os = "macos")]
fn set_macos_activation_policy(policy: objc2_app_kit::NSApplicationActivationPolicy) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;

    if let Some(mtm) = MainThreadMarker::new() {
        NSApplication::sharedApplication(mtm).setActivationPolicy(policy);
    }
}
