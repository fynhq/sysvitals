//! Settings window entry point: separate GPUI process with file-mtime handoff and DPI/parent watch.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::time::Instant;

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
    spawn_accent_watch, InstanceLock, PanelSurface, CONTROL_RADIUS,
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

fn main() {
    let started = Instant::now();
    sysvitals_panel::install_panic_hook();
    let _instance = match acquire_instance_lock(INSTANCE_LOCK_NAME) {
        InstanceLock::Acquired(guard) => Some(guard),
        InstanceLock::Busy => {
            log_event(
                "WARN",
                "settings: another settings window is already running; exiting",
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

    let (cfg, config_read_ok) = load_config_with_backup();

    // Settings stays a regular foreground app; only the history popup uses headless/accessory mode.
    #[cfg(target_os = "macos")]
    let app = gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .with_quit_mode(QuitMode::LastWindowClosed);
    #[cfg(not(target_os = "macos"))]
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);

    log_event(
        "INFO",
        &format!(
            "settings: application built at {}ms",
            started.elapsed().as_millis()
        ),
    );

    app.run(move |cx| {
        gpui_kit::init(cx);
        log_event(
            "INFO",
            &format!("settings: gpui init done at {}ms", started.elapsed().as_millis()),
        );
        apply_theme_and_accent(cx);
        watch_parent(cx);

        let anchor = anchor_arg();
        let monitor = anchor_monitor_arg().map(DisplayId::new);

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

        let window_bounds = settings_window_bounds(
            anchor.as_ref(),
            monitor,
            size(px(400.), px(740.)),
            cx,
        );
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

        let opened = gpui_kit::open_window(window_options, cx, move |window, cx| {
            cx.new(|cx| {
                SettingsView::new(cfg, config_read_ok, window, cx, initial_size)
            })
        });
        let (window_handle, _view) = match opened {
            Ok(opened) => opened,
            Err(e) => {
                log_event("WARN", &format!("settings: open window failed: {e}"));
                cx.quit();
                return;
            }
        };

        log_event(
            "INFO",
            &format!("settings: window built at {}ms", started.elapsed().as_millis()),
        );
        let _ = window_handle.update(cx, |_, window, _| {
            window.on_next_frame(move |_, _| {
                log_event(
                    "INFO",
                    &format!(
                        "settings: first frame at {}ms",
                        started.elapsed().as_millis()
                    ),
                );
            });
        });

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
                let deadline = Instant::now() + std::time::Duration::from_secs(4);
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
                                started.elapsed().as_millis()
                            ),
                        );
                        break;
                    }
                    if Instant::now() >= deadline {
                        break;
                    }
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(30))
                        .await;
                }
                if !shown {
                    log_event(
                        "WARN",
                        &format!(
                            "settings: window remained hidden after {}ms (activation attempts {activations}); exiting so the next click can retry",
                            started.elapsed().as_millis()
                        ),
                    );
                    if hwnd.is_some() {
                        cx.update(|app| app.quit());
                    }
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
                    .timer(std::time::Duration::from_millis(120))
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
    });
    log_event("INFO", "settings: exited");
}
