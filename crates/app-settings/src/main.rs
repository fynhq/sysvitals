//! SysVitals Settings — a modern GPUI-based settings window.
//!
//! Launched by the platform overlay (app-win / app-mac) when the user opens
//! settings. Reads the shared config, presents a modern grouped UI, and writes
//! every change back atomically as it happens: there are no dialog buttons,
//! the overlay watches the config file and hot-reloads it. The window itself
//! is closed with the system title-bar close button.
//!
//! Visual theme: the panel always follows the system light/dark appearance
//! (the `display.theme` setting only governs the resident indicator). On
//! Windows and macOS the primary color follows the system accent, so the
//! segmented controls match what the rest of the shell uses.

// A GUI child process must not allocate a console window on Windows: without
// this attribute the binary is built as a console subsystem executable and a
// terminal flashes up beside the settings window every time it launches.
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
// `.when(cond, |b| ...)` on builders comes from this trait; the glob above
// does not re-export it (app-history imports it the same way).
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use sysvitals_core::cli::AnchorRect;
use sysvitals_core::config::{
    ALLOWED_INTERVAL_MS, ALLOWED_ITEM_SPACING_PX, ALLOWED_LEFT_MARGIN_PX, MAX_PREFIX_CHARS,
};
use sysvitals_core::{t, Anchor, Config, Language, Theme};
use sysvitals_panel::{
    anchor_visible_bounds, backdrop_for, detect_material, log_event, observe_system_appearance,
    spawn_accent_watch, PanelSurface, CONTROL_RADIUS,
};
#[cfg(windows)]
use sysvitals_panel::{panel_font_family, preplace_hidden_window, taskbar_al, utf16, window_hwnd};

mod config;
#[cfg(target_os = "macos")]
mod macos_select;
mod platform;
mod theme;
mod view;
mod widgets;

pub use config::*;
#[cfg(target_os = "macos")]
pub use macos_select::*;
pub use platform::*;
pub use theme::*;
pub use view::*;
pub use widgets::*;

fn main() {
    sysvitals_panel::install_panic_hook();
    if !acquire_single_instance() {
        log_event(
            "WARN",
            "settings: another settings window is already running; exiting",
        );
        return;
    }
    log_event(
        "INFO",
        &format!(
            "settings: starting (version={}, build={}, parent_pid={:?})",
            env!("CARGO_PKG_VERSION"),
            SV_BUILD_ID,
            parse_parent_pid()
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
    // GPUI does not declare DPI awareness itself. Without this the process
    // is treated as DPI-unaware and Windows bitmap-stretches the whole window
    // on a scaled display, which is what makes the text look soft.
    #[cfg(windows)]
    sysvitals_panel::set_per_monitor_dpi_aware();

    let cfg = load_config_with_backup();

    // macOS: run as a regular foreground app so opening the settings window
    // puts an icon in the Dock (user requirement), and quit with the last
    // window so closing it leaves no invisible process behind. Two platform
    // mechanics make this work: the binary lives inside
    // `SysVitals.app/Contents/MacOS`, so `+[NSBundle mainBundle]` resolves the
    // enclosing bundle and AppKit reads its `CFBundleIconFile` (AppIcon.icns)
    // for the Dock tile; and the default `Regular` policy is re-applied via
    // `setActivationPolicy_` at `did_finish_launching`, which overrides the
    // bundle's `LSUIElement` agent default at runtime. Whether the tile shows
    // the app icon or a generic one is an on-device check.
    //
    // The history popup deliberately keeps the headless request (Accessory
    // policy = no Dock icon): a transient chart panel should not announce
    // itself in the Dock.
    //
    // Windows has no equivalent concerns: the default windowed startup
    // already gives the window a taskbar entry via the icon embedded by the
    // build script.
    //
    // `with_assets(Assets)` registers the bundled Lucide icon catalog as the
    // GPUI asset source. Without it, the default source (`()`) returns
    // `Ok(None)` for every path, so every `IconName` — including the Select
    // dropdown's ChevronDown arrow — renders as blank.
    #[cfg(target_os = "macos")]
    let app = gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .with_quit_mode(QuitMode::LastWindowClosed);
    #[cfg(not(target_os = "macos"))]
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);

    app.run(move |cx| {
        gpui_kit::init(cx);
        // `gpui_component::theme::init` hardcodes `Theme::change(Light)`, so
        // the panel must sync to the system appearance before the window is
        // opened — otherwise a dark-mode desktop shows a light settings
        // panel for the first frame. The panel always follows the system,
        // independent of `display.theme` (which now only governs the
        // indicator). The OS accent override and the backdrop tint come with
        // it (all three are theme-color writes and must be sequenced
        // together).
        apply_theme_and_accent(cx);
        watch_parent(cx);

        // The indicator passes its rect so the window opens on the same
        // monitor (multi-monitor); an absent or invalid anchor keeps the
        // default centered placement.
        let anchor = parse_anchor();
        let monitor = parse_anchor_monitor();

        let titlebar = {
            #[cfg(target_os = "macos")]
            {
                // Full-size content: the tinted material continues under the
                // titlebar and a custom 34px header draws the title, so the
                // window no longer shows a separate native titlebar shade.
                TitlebarOptions {
                    // Keep the native window title for Mission Control and
                    // accessibility; `appears_transparent` hides it visually
                    // and the header below draws the visible copy.
                    title: Some(t(cfg.display.language, "settings.title").to_string().into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(9.), px(9.))),
                }
            }
            #[cfg(windows)]
            {
                // A client-area title bar keeps the caption on the same Mica
                // surface as the page; the component owns the caption drag
                // area and the minimize/maximize/close hit boxes.
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
            // 400x720: leave room for the launch-at-login switch added to
            // the General card; 680px did not account for the extra row.
            size(px(400.), px(720.)),
            cx,
        );
        // Logical size after fitting the work area; the view restores it
        // once if Windows' cross-DPI placement rescaled the window.
        let initial_size = window_bounds.get_bounds().size;
        let window_options = WindowOptions {
            window_bounds: Some(window_bounds),
            // Select the indicator's display explicitly; see
            // `settings_window_bounds` for why the rectangle alone is not
            // enough on mixed-DPI setups.
            display_id: monitor,
            titlebar: Some(titlebar),
            // System backdrop material (Win11 Mica / Win10-era acrylic blur /
            // macOS window blur, with an opaque fallback on anything older or
            // when the user disabled transparency effects). The palette tint
            // applied above is what lets this show through.
            window_background: backdrop_for(detect_material()),
            // Windows opens hidden so the cross-DPI placement and size
            // correction happen off-screen (see the spawn after open_window);
            // macOS keeps the previous visible-with-focus behavior.
            #[cfg(windows)]
            show: false,
            #[cfg(windows)]
            focus: false,
            #[cfg(not(windows))]
            focus: true,
            ..Default::default()
        };

        // A failed window creation must not panic the process (the history
        // popup already degrades this way): log and quit instead.
        let opened = gpui_kit::open_window(window_options, cx, move |window, cx| {
            cx.new(|cx| SettingsView::new(cfg, window, cx, initial_size))
        });
        let (window_handle, _view) = match opened {
            Ok(opened) => opened,
            Err(e) => {
                log_event("WARN", &format!("settings: open window failed: {e}"));
                cx.quit();
                return;
            }
        };

        // Windows: the window was created hidden on the OS default monitor.
        // Move it to the anchor's display while hidden, apply the intended
        // size, then activate it — activation applies the saved placement and
        // shows the window, so the cross-DPI rescale never reaches the
        // screen. All three steps run as foreground tasks: native window
        // calls must not run inside an App borrow (see
        // `preplace_hidden_window`).
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
                let resized = cx.update(|app| {
                    window_handle
                        .update(app, |_, window, _| window.resize(initial_size))
                        .is_ok()
                });
                let activated = cx.update(|app| {
                    window_handle
                        .update(app, |_, window, _| window.activate_window())
                        .is_ok()
                });
                if resized && activated {
                    log_event("INFO", "settings: window shown after hidden placement");
                } else {
                    log_event(
                        "WARN",
                        &format!(
                            "settings: show after hidden placement failed (resize={resized}, activate={activated})"
                        ),
                    );
                }
            })
            .detach();
        }

        // macOS: a child process spawned by the menu-bar indicator starts in
        // the background. Creating the window with `focus: true` only asks
        // AppKit for key status; it does not activate the app, so the panel
        // can land behind the frontmost window until the user clicks the Dock
        // icon. Activate the app and raise the window explicitly, then
        // reassert once the launch transition has completed (activation
        // during `applicationDidFinishLaunching` is not always honored).
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
