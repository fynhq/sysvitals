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

#[cfg(windows)]
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};

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

use sysvitals_core::config::{
    ALLOWED_INTERVAL_MS, ALLOWED_ITEM_SPACING_PX, ALLOWED_LEFT_MARGIN_PX,
};
use sysvitals_core::{Anchor, Config, Language, Theme, t};
use sysvitals_panel::{PanelSurface, backdrop_for, detect_material, system_accent_hex};
#[cfg(windows)]
use sysvitals_panel::{hkcu_dword, panel_font_family};

// ---------------------------------------------------------------------------
// Platform gate for the layout section
// ---------------------------------------------------------------------------

/// HKCU explorer keys, mirroring what the overlay reads: the Win11 centered-
/// taskbar flag decides whether the far-left anchor is offered, so the
/// settings window must consult the very same value.
#[cfg(windows)]
const TASKBAR_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced";
#[cfg(windows)]
const TASKBAR_VALUE: &str = "TaskbarAl";

#[cfg(windows)]
fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Whether the indicator's layout section is offered on this machine: the
/// far-left anchor exists only on Windows 11 with a centered taskbar.
#[cfg(windows)]
fn layout_available() -> bool {
    hkcu_dword(TASKBAR_KEY, TASKBAR_VALUE) == Some(1)
}

#[cfg(not(windows))]
fn layout_available() -> bool {
    false
}

/// Whether the taskbar is currently docked left or right. On such a bar the
/// margin value offsets the indicator from the bar's top edge, and that knob
/// must be editable even where the anchor section is not offered, so the
/// settings window probes the same app-bar position the overlay reads.
/// `false` anywhere the probe fails (macOS, hidden taskbar, pre-registered
/// shell) keeps the previous UI surface untouched.
#[cfg(windows)]
fn taskbar_is_vertical() -> bool {
    use windows::Win32::UI::Shell::{
        ABE_LEFT, ABE_RIGHT, ABM_GETTASKBARPOS, APPBARDATA, SHAppBarMessage,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    let mut abd = APPBARDATA {
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        ..Default::default()
    };
    if unsafe { SHAppBarMessage(ABM_GETTASKBARPOS, &mut abd) } == 0 {
        return false;
    }
    // Auto-hidden bars report a rect parked off-screen; then the orientation
    // does not matter for what is visible right now. Validate against the
    // virtual desktop: the main taskbar can sit on a secondary monitor with
    // negative or beyond-primary coordinates.
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if abd.rc.bottom <= vy || abd.rc.top >= vy + vh || abd.rc.right <= vx || abd.rc.left >= vx + vw
    {
        return false;
    }
    abd.uEdge == ABE_LEFT || abd.uEdge == ABE_RIGHT
}

#[cfg(not(windows))]
fn taskbar_is_vertical() -> bool {
    false
}

// ---------------------------------------------------------------------------
// Native macOS dropdown menus
// ---------------------------------------------------------------------------

/// A system menu for the three string dropdowns on macOS.
///
/// The GPUI `Select` popover is a deferred element inside the settings
/// window, so it can never blur the desktop behind it: a translucent surface
/// would reveal the settings rows underneath, not the window material. Native
/// menus own their own vibrant window, which is the platform-correct way to
/// get the blur (and the keyboard/dismiss behavior) users expect from a
/// dropdown on macOS.
#[cfg(target_os = "macos")]
mod macos_select {
    use std::cell::Cell;

    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
    use objc2_app_kit::{
        NSAppearanceCustomization, NSApplication, NSControlStateValueOn, NSEvent, NSMenu,
        NSMenuItem, NSView,
    };
    use objc2_foundation::{NSObject, NSObjectProtocol, NSString};

    thread_local! {
        static MENU_SELECTION: Cell<Option<isize>> = const { Cell::new(None) };
    }

    define_class!(
        // SAFETY: NSObject has no subclassing requirements; no Drop impl.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "SysVitalsSettingsMenuTarget"]
        struct MenuTarget;

        impl MenuTarget {
            #[unsafe(method(selectItem:))]
            fn select_item(&self, sender: Option<&NSMenuItem>) {
                if let Some(item) = sender {
                    MENU_SELECTION.with(|slot| slot.set(Some(item.tag())));
                }
            }
        }

        unsafe impl NSObjectProtocol for MenuTarget {}
    );

    impl MenuTarget {
        fn new(mtm: MainThreadMarker) -> Retained<Self> {
            let this = Self::alloc(mtm);
            // SAFETY: MenuTarget does not override init; this dispatches to
            // NSObject's init, avoiding the super() wrapper's RetainSemantics
            // issues with Allocated<T> in objc2 0.6.5.
            unsafe { msg_send![this, init] }
        }
    }

    /// Show `items` at the pointer as a native menu and return the chosen
    /// index. The current item carries the system checkmark. `None` means the
    /// menu was dismissed without a selection (or no window was available).
    pub fn show(items: &[String], selected: usize) -> Option<usize> {
        if items.is_empty() {
            return None;
        }
        let mtm = MainThreadMarker::new()?;
        let app = NSApplication::sharedApplication(mtm);
        let Some(window) = app.keyWindow().or_else(|| app.mainWindow()) else {
            eprintln!("app-settings: native select menu has no window");
            return None;
        };
        let Some(view) = window.contentView() else {
            eprintln!("app-settings: native select menu has no content view");
            return None;
        };

        let menu = NSMenu::new(mtm);
        // Every item has an explicit target/action below; automatic enabling
        // would otherwise walk the responder chain and gray them out.
        menu.setAutoenablesItems(false);
        // A menu raised from a vibrant window can inherit the button's
        // appearance and render dark in light mode. Follow the app's
        // effective appearance, like the indicator's context menu.
        let appearance = app.effectiveAppearance();
        menu.setAppearance(Some(&appearance));

        let target = MenuTarget::new(mtm);
        let target_ref: &MenuTarget = &target;
        for (index, label) in items.iter().enumerate() {
            let item = NSMenuItem::new(mtm);
            item.setTitle(&NSString::from_str(label));
            // SAFETY: the selector is implemented by MenuTarget above.
            unsafe { item.setAction(Some(sel!(selectItem:))) };
            // SAFETY: target_ref stays alive through the nested tracking loop.
            unsafe { item.setTarget(Some(target_ref)) };
            item.setTag(index as isize);
            if index == selected {
                item.setState(NSControlStateValueOn);
            }
            menu.addItem(&item);
        }

        // `mouseLocation` is in screen coordinates; convert through the
        // window into its content view, the space popUpMenuPositioningItem
        // expects. The menu's upper-left lands at the click point, like a
        // context menu raised from the trigger.
        let screen_point = NSEvent::mouseLocation();
        let window_point = window.convertPointFromScreen(screen_point);
        let view_point = view.convertPoint_fromView(window_point, None);
        let view_ref: &NSView = &view;

        MENU_SELECTION.with(|slot| slot.set(None));
        let _ = menu.popUpMenuPositioningItem_atLocation_inView(None, view_point, Some(view_ref));
        MENU_SELECTION
            .with(|slot| slot.take())
            .and_then(|tag| usize::try_from(tag).ok())
    }
}

#[cfg(target_os = "macos")]
#[derive(Copy, Clone)]
struct MacSelectStyle {
    background: Hsla,
    foreground: Hsla,
    border: Hsla,
    caret: Hsla,
    radius: Pixels,
}

/// The macOS dropdown trigger: the same filled control look as the GPUI
/// Select, but clicking it raises a native menu instead of an in-window
/// popover. Width is owned by the caller so all three dropdowns keep their
/// existing layout slots.
#[cfg(target_os = "macos")]
fn mac_select_trigger(
    id: &'static str,
    label: String,
    style: MacSelectStyle,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    h_flex()
        .id(id)
        .w_full()
        .h(px(32.))
        .px(px(10.))
        .items_center()
        .justify_between()
        .gap_1()
        .bg(style.background)
        .text_color(style.foreground)
        .border_1()
        .border_color(style.border)
        .rounded(style.radius)
        .cursor_pointer()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .truncate()
                .child(label),
        )
        .child(Icon::new(IconName::ChevronDown).text_color(style.caret))
        .on_click(on_click)
}

// ---------------------------------------------------------------------------
// Window backdrop material (Win11 Mica / macOS blur, with fallbacks)
// ---------------------------------------------------------------------------

/// Re-apply the shared theme/accent/material stack, then the settings-only
/// typography and token overrides.
fn apply_theme_and_accent(cx: &mut App) {
    sysvitals_panel::apply_theme_and_accent(cx, PanelSurface::Settings);
    // Typography: keep the library's 16px rem base so its 0.875rem controls
    // (buttons, inputs, selects, dropdown rows) land on 14px, matching the
    // option labels on the left — the render root pins body text to that
    // 14px size. The focus ring is disabled because its 2px outer ring reads
    // as a heavy border on these compact controls, and clicking outside a
    // control is handled separately by blurring the window.
    UiTheme::update(cx, |theme| {
        theme.font_size = px(16.);
        theme.focus_ring = false;
        #[cfg(target_os = "macos")]
        {
            // Keep the switch thumb white in both appearances, on both the
            // unchecked track and the accent track. Option buttons keep their
            // original library styling; only the selected tile is
            // accent-tinted by `apply_accent`.
            theme.colors.switch_thumb = hsla(0., 0., 1., 1.);
        }
        #[cfg(windows)]
        {
            if let Some(family) = panel_font_family() {
                theme.font_family = family.into();
            }
            let dark = theme.mode.is_dark();
            // The library's switch thumb falls back to the theme background:
            // white in light mode, but neutral-950 in dark mode, which nearly
            // disappears against the #404040 track. WinUI keeps the thumb
            // white in both appearances; the checked track still follows the
            // system accent through `tokens.primary` (apply_accent).
            if dark {
                theme.colors.switch_thumb = hsla(0., 0., 1., 1.);
            }
            // WinUI control strokes and flyout surfaces. The library's
            // neutral-200/neutral-950 pair is much flatter than the Windows
            // Settings controls, which draw a visible stroke around a
            // subtly-filled input and a lighter flyout than the page behind
            // it.
            theme.colors.input = if dark {
                hsla(0., 0., 1., 0.30)
            } else {
                hsla(0., 0., 0., 0.26)
            };
            theme.colors.popover = if dark {
                hsla(0., 0., 0.17, 1.)
            } else {
                hsla(0., 0., 0.976, 1.)
            };
            theme.colors.popover_foreground = theme.colors.foreground;
        }
    });
}

// ---------------------------------------------------------------------------
// Dropdown item builders
// ---------------------------------------------------------------------------

/// Pixel-choice items carry their unit so the dropdown does not read as a
/// bare count. "px" is a unit symbol shared by both locales, so the list never
/// needs a language refresh.
fn px_items(values: &[u32]) -> Vec<String> {
    values.iter().map(|v| format!("{v} px")).collect()
}

/// Anchor items are translated text (the choice labels), rebuilt whenever the
/// selected language changes. Index 0 = right of the taskbar anchor space
/// (left of the tray), index 1 = far left.
fn anchor_items(lang: Language) -> Vec<String> {
    vec![
        t(lang, "anchor.right").to_string(),
        t(lang, "anchor.left").to_string(),
    ]
}

/// Every dropdown here is a single-choice list of strings (pixel values
/// rendered as decimal text, or translated anchor labels). macOS uses native
/// menus instead, so this state exists only on Windows.
#[cfg(windows)]
type StringSelectState = SelectState<SearchableVec<String>>;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct SettingsView {
    cfg: Config,
    // Input states for prefixes
    prefix_cpu: Entity<InputState>,
    prefix_mem: Entity<InputState>,
    prefix_up: Entity<InputState>,
    prefix_down: Entity<InputState>,
    // Toggle states
    dual_row: bool,
    show_resources: bool,
    show_network: bool,
    // Index-based selections
    lang_idx: usize,
    interval_idx: usize,
    spacing_idx: usize,
    margin_idx: usize,
    theme_idx: usize,
    anchor_idx: usize,
    // Dropdown states (item text may carry the selected language). Windows
    // renders the library Select; macOS raises a native menu from the index
    // fields instead.
    #[cfg(windows)]
    spacing_select: Entity<StringSelectState>,
    #[cfg(windows)]
    margin_select: Entity<StringSelectState>,
    #[cfg(windows)]
    anchor_select: Entity<StringSelectState>,
    /// Whether the layout section is offered on this machine (see
    /// `layout_available`). False hides it entirely.
    layout_available: bool,
    /// Whether the taskbar is docked left/right (see `taskbar_is_vertical`),
    /// re-probed at most once a second while the window is open. On such a
    /// bar the margin row is shown unconditionally (the value offsets the
    /// indicator from the bar's top edge), while the anchor dropdown — which
    /// selects between the tray end and the far-left end of a HORIZONTAL bar
    /// — would be a dead control and stays hidden.
    vertical_taskbar: bool,
    /// Last accent value seen by the live-refresh poll; the override only
    /// re-applies when this moves (see `new`).
    last_accent: Option<u32>,
    /// Whether the most recent config write failed. Shown in the footer so a
    /// read-only/full disk does not silently swallow every setting change.
    save_failed: std::cell::Cell<bool>,
    /// Serialized form last written (or loaded) so a focus loss with no edits
    /// does not touch the file and trigger an indicator reload.
    last_saved_toml: Option<String>,
    /// Taskbar-shape probes are syscalls/registry reads; refresh them at most
    /// once a second on interaction-driven renders.
    last_taskbar_probe: Option<Instant>,
    /// Keep event subscriptions alive for the lifetime of the view.
    _subscriptions: Vec<Subscription>,
    /// Flush guard: saves the current state whenever the window loses focus,
    /// so a prefix typed right before closing with the title-bar button is
    /// not lost (its field may never have received an explicit blur).
    _deactivation: Option<Subscription>,
    /// Kept alive so the appearance observer is not cancelled: a live OS
    /// light/dark flip reloads the system palette (the panel always follows
    /// the system, independent of the indicator-only `display.theme`).
    _appearance: Option<Subscription>,
    /// Kept alive so the Cmd+W / Cmd+Q observer is not cancelled. macOS has
    /// no default window menu here, so those keys need an explicit handler;
    /// both close this window (the indicator is a separate process and keeps
    /// running).
    #[cfg(target_os = "macos")]
    _shortcuts: Option<Subscription>,
}

impl SettingsView {
    fn new(cfg: Config, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let prefix_cpu =
            cx.new(|cx| InputState::new(window, cx).default_value(&cfg.display.prefix.cpu));
        let prefix_mem =
            cx.new(|cx| InputState::new(window, cx).default_value(&cfg.display.prefix.mem));
        let prefix_up =
            cx.new(|cx| InputState::new(window, cx).default_value(&cfg.display.prefix.up));
        let prefix_down =
            cx.new(|cx| InputState::new(window, cx).default_value(&cfg.display.prefix.down));

        let lang_idx = Language::ALL
            .iter()
            .position(|l| *l == cfg.display.language)
            .unwrap_or(0);
        let interval_idx = ALLOWED_INTERVAL_MS
            .iter()
            .position(|v| *v == cfg.sample.interval_ms)
            .unwrap_or(0);
        let spacing_idx = ALLOWED_ITEM_SPACING_PX
            .iter()
            .position(|v| *v == cfg.display.item_spacing)
            .unwrap_or(0);
        let margin_idx = ALLOWED_LEFT_MARGIN_PX
            .iter()
            .position(|v| *v == cfg.windows.left_margin_px)
            .unwrap_or(0);
        let anchor_idx = if cfg.windows.anchor == Anchor::Left {
            1
        } else {
            0
        };
        let theme_idx = match cfg.display.theme {
            Theme::Auto => 0,
            Theme::Light => 1,
            Theme::Dark => 2,
        };

        #[cfg(windows)]
        let lang = Language::ALL[lang_idx];
        #[cfg(windows)]
        let spacing_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(px_items(&ALLOWED_ITEM_SPACING_PX)),
                Some(IndexPath::new(spacing_idx)),
                window,
                cx,
            )
        });
        #[cfg(windows)]
        let margin_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(px_items(&ALLOWED_LEFT_MARGIN_PX)),
                Some(IndexPath::new(margin_idx)),
                window,
                cx,
            )
        });
        #[cfg(windows)]
        let anchor_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(anchor_items(lang)),
                Some(IndexPath::new(anchor_idx)),
                window,
                cx,
            )
        });

        // Every change is applied immediately: dropdown selections commit on
        // confirm, text fields commit on blur or Enter.
        let mut _subscriptions: Vec<Subscription> = Vec::new();
        #[cfg(windows)]
        {
            _subscriptions.push(cx.subscribe(&spacing_select, |this: &mut Self, _, ev, cx| {
                if let SelectEvent::Confirm(Some(value)) = ev {
                    let items = px_items(&ALLOWED_ITEM_SPACING_PX);
                    if let Some(i) = items.iter().position(|s| s == value) {
                        this.spacing_idx = i;
                        this.save(cx);
                        cx.notify();
                    }
                }
            }));
            _subscriptions.push(cx.subscribe(&margin_select, |this: &mut Self, _, ev, cx| {
                if let SelectEvent::Confirm(Some(value)) = ev {
                    let items = px_items(&ALLOWED_LEFT_MARGIN_PX);
                    if let Some(i) = items.iter().position(|s| s == value) {
                        this.margin_idx = i;
                        this.save(cx);
                        cx.notify();
                    }
                }
            }));
            _subscriptions.push(cx.subscribe(&anchor_select, |this: &mut Self, _, ev, cx| {
                if let SelectEvent::Confirm(Some(value)) = ev {
                    let items = anchor_items(this.current_lang());
                    if let Some(i) = items.iter().position(|s| s == value) {
                        this.anchor_idx = i;
                        this.save(cx);
                        // The margin row appears only for the far-left anchor.
                        cx.notify();
                    }
                }
            }));
        }
        for state in [&prefix_cpu, &prefix_mem, &prefix_up, &prefix_down] {
            _subscriptions.push(
                cx.subscribe(state, |this: &mut Self, _, ev: &InputEvent, cx| {
                    match ev {
                        // Prefix text commits when the field loses focus or
                        // the user presses Enter, not on every keystroke.
                        // Safe to read the input states here: events are
                        // dispatched through the app's effect queue, so the
                        // emitting entity's borrow is already released.
                        InputEvent::Blur | InputEvent::PressEnter { .. } => this.save(cx),
                        _ => {}
                    }
                }),
            );
        }

        // macOS has no default application menu in this child process, so
        // Cmd+W / Cmd+Q need an explicit handler; both close this window and
        // leave the menu-bar indicator (a separate process) running.
        #[cfg(target_os = "macos")]
        let window_shortcuts = cx.observe_keystrokes(|_view, event, window, _cx| {
            let keystroke = &event.keystroke;
            if keystroke.modifiers.platform
                && (keystroke.key.as_str() == "w" || keystroke.key.as_str() == "q")
            {
                window.remove_window();
            }
        });

        let last_saved_toml = cfg.to_toml_string().ok();
        let view = Self {
            dual_row: cfg.display.dual_row,
            show_resources: cfg.display.show_resources,
            show_network: cfg.display.show_network,
            cfg,
            prefix_cpu,
            prefix_mem,
            prefix_up,
            prefix_down,
            lang_idx,
            interval_idx,
            spacing_idx,
            margin_idx,
            theme_idx,
            anchor_idx,
            #[cfg(windows)]
            spacing_select,
            #[cfg(windows)]
            margin_select,
            #[cfg(windows)]
            anchor_select,
            layout_available: layout_available(),
            vertical_taskbar: taskbar_is_vertical(),
            // Seed with `None` (a sentinel), not the live accent: if the
            // startup `apply_accent` were ever overwritten by a later
            // palette load, a real seed would make the poll see "no change"
            // and never re-apply. `None` forces one guaranteed re-apply on the
            // first 500 ms tick (idempotent when startup already succeeded),
            // after which it tracks the real value.
            last_accent: None,
            save_failed: std::cell::Cell::new(false),
            last_saved_toml,
            last_taskbar_probe: Some(Instant::now()),
            _subscriptions,
            _deactivation: None,
            _appearance: None,
            #[cfg(target_os = "macos")]
            _shortcuts: Some(window_shortcuts),
        };
        // OS accent live-refresh. The appearance observer registered in
        // `render` only fires when the light/dark MODE flips (the platform
        // layer filters `WM_SETTINGCHANGE` to `ImmersiveColorSet` with a mode
        // change); the user changing the accent in the system Settings app
        // does not re-run it, so the panel would keep the old primary color
        // until it reopens. Poll the accent source every 500 ms and re-apply
        // the override only when the value actually moved; the loop stops
        // with the view entity, i.e. when the window closes. Same shape as
        // the history popup's data-refresh loop. The probe runs inside
        // `update` on purpose: on macOS `system_accent_hex` reads AppKit
        // (`controlAccentColor`), which belongs to the main thread — the same
        // thread `update` runs on.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(500))
                    .await;
                let alive = this
                    .update(cx, |view: &mut SettingsView, cx| {
                        let current = system_accent_hex();
                        if current == view.last_accent {
                            return true;
                        }
                        view.last_accent = current;
                        if current.is_some() {
                            sysvitals_panel::apply_accent(cx);
                        } else {
                            // The OS moved back to "multicolor" (or the accent
                            // probe failed): reload the palette so the old accent
                            // override is removed.
                            apply_theme_and_accent(cx);
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        })
        .detach();
        view
    }

    fn current_lang(&self) -> Language {
        Language::ALL[self.lang_idx]
    }

    /// Enforce "at least one indicator group stays on": when the user turns
    /// off the last remaining group, re-enable the other one instead, so the
    /// indicator is never left with nothing to show.
    fn set_resources(&mut self, checked: bool) {
        self.show_resources = checked;
        if !self.show_resources && !self.show_network {
            self.show_network = true;
        }
    }

    fn set_network(&mut self, checked: bool) {
        self.show_network = checked;
        if !self.show_network && !self.show_resources {
            self.show_resources = true;
        }
    }

    fn collect_config(&self, cx: &App) -> Config {
        let mut cfg = self.cfg.clone();
        cfg.display.language = Language::ALL[self.lang_idx];
        cfg.sample.interval_ms = ALLOWED_INTERVAL_MS[self.interval_idx];
        cfg.display.item_spacing = ALLOWED_ITEM_SPACING_PX[self.spacing_idx];
        cfg.windows.left_margin_px = ALLOWED_LEFT_MARGIN_PX[self.margin_idx];
        cfg.windows.anchor = if self.anchor_idx == 1 {
            Anchor::Left
        } else {
            Anchor::Right
        };
        cfg.display.theme = match self.theme_idx {
            1 => Theme::Light,
            2 => Theme::Dark,
            _ => Theme::Auto,
        };
        cfg.display.dual_row = self.dual_row;
        cfg.display.show_resources = self.show_resources;
        cfg.display.show_network = self.show_network;
        cfg.display.prefix.cpu = self.prefix_cpu.read(cx).value().to_string();
        cfg.display.prefix.mem = self.prefix_mem.read(cx).value().to_string();
        cfg.display.prefix.up = self.prefix_up.read(cx).value().to_string();
        cfg.display.prefix.down = self.prefix_down.read(cx).value().to_string();
        cfg.sanitize();
        cfg
    }

    /// Persist the UI-owned fields. The settings model is a startup snapshot,
    /// so a blind whole-file write would silently revert fields this window
    /// does not edit (`history.*`, `sample.cpu_smoothing`,
    /// `windows.auto_start`) after a manual edit. Re-read the current file
    /// and carry those fields forward; only UI-owned fields come from the
    /// view. Also skip the write when the serialized result is unchanged, so
    /// merely focusing away does not bump the mtime and make the indicator
    /// reload/re-sample.
    fn save(&mut self, cx: &App) {
        let mut cfg = self.collect_config(cx);
        let Ok(path) = sysvitals_core::config::config_path() else {
            self.save_failed.set(true);
            return;
        };
        if let Ok((disk, _)) = Config::load(&path) {
            cfg.history = disk.history;
            cfg.sample.cpu_smoothing = disk.sample.cpu_smoothing;
            cfg.windows.auto_start = disk.windows.auto_start;
        }
        let text = match cfg.to_toml_string() {
            Ok(text) => text,
            Err(e) => {
                eprintln!("app-settings: config serialize failed: {e}");
                self.save_failed.set(true);
                return;
            }
        };
        if self.last_saved_toml.as_deref() == Some(text.as_str()) {
            return;
        }
        let result = cfg.save(&path);
        if let Err(e) = &result {
            eprintln!("app-settings: config save failed: {e}");
        } else {
            self.cfg = cfg;
            self.last_saved_toml = Some(text);
        }
        self.save_failed.set(result.is_err());
    }

    /// Rebuild the language-dependent anchor dropdown after a language flip,
    /// keeping the current selection. The dropdown entity and its
    /// subscription stay in place; only the items are swapped.
    fn refresh_anchor_items(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(windows)]
        {
            let items = anchor_items(self.current_lang());
            let idx = self.anchor_idx;
            self.anchor_select.update(cx, |state, cx| {
                state.set_items(SearchableVec::new(items), window, cx);
                state.set_selected_index(Some(IndexPath::new(idx)), window, cx);
            });
        }
        #[cfg(not(windows))]
        {
            // macOS builds the native menu from `anchor_items` on every
            // click, so there is no persistent list to refresh here.
            let _ = (window, cx);
        }
    }
}

/// A section heading: WinUI group headers are 12px semibold and muted.
fn section_label(text: String, dim: Hsla) -> impl IntoElement {
    div().text_xs().font_semibold().text_color(dim).child(text)
}

/// Windows uses WinUI's `ControlCornerRadius` (4px) inside its 8px card
/// (`OverlayCornerRadius`), so the nested corners keep a clear hierarchy
/// instead of competing with the card's radius. macOS keeps the library's 6px
/// controls inside its 10px card.
#[cfg(windows)]
const CONTROL_RADIUS: f32 = 4.0;
#[cfg(not(windows))]
const CONTROL_RADIUS: f32 = 6.0;

/// A grouped option surface. Windows keeps the WinUI card (8px radius plus
/// an optional hairline when the opaque light fill would melt into a bright
/// Mica page); macOS uses a flatter grouped-list card (10px radius). Rows
/// touch on both hosts and are separated by [`card_divider`], like a native
/// grouped list rather than a stack of floating controls.
fn settings_card(bg: Hsla, border: Option<Hsla>) -> Div {
    v_flex()
        .w_full()
        .rounded(px(if cfg!(target_os = "macos") { 10. } else { 8. }))
        .bg(bg)
        .when(border.is_some(), |this| {
            this.border_1()
                .border_color(border.unwrap_or_else(|| hsla(0., 0., 0., 0.)))
        })
        .p_3()
}

/// Grouped-list separator between option rows, one device pixel thick.
///
/// Windows uses WinUI's `DividerStrokeColorDefault` (`#0F000000` in light,
/// `#15FFFFFF` in dark). macOS keeps `NSColor.separatorColor` (10% black /
/// white), which is what System Settings paints. The line is deliberately a
/// hairline in physical pixels: one logical point would be two pixels on a
/// Retina display (or 1.25–1.5px at Windows fractional scaling) and read
/// heavier than either platform's grouped lists.
fn card_divider(dark: bool, scale_factor: f32) -> Div {
    let color = if cfg!(windows) {
        if dark {
            hsla(0., 0., 1., 0x15 as f32 / 255.)
        } else {
            hsla(0., 0., 0., 0x0F as f32 / 255.)
        }
    } else if dark {
        hsla(0., 0., 1., 0.10)
    } else {
        hsla(0., 0., 0., 0.10)
    };
    let hairline = 1.0 / scale_factor.max(1.0);
    div().h(px(hairline)).w_full().bg(color)
}

/// A section heading paired with its card at the tight 4px label gap.
fn settings_section(title: String, card: impl IntoElement, dim: Hsla) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(section_label(title, dim))
        .child(card)
}

/// Blur the window when a mouse-down occurs outside `element`. Used around
/// inputs/selects so their focus border disappears when the user clicks any
/// other part of the settings page (GPUI keeps focus otherwise).
fn blur_on_outside(element: Div) -> Div {
    element.on_mouse_down_out(|_, window, cx| window.blur(cx))
}

/// A labelled control row: title on the left, a wrapped tile group on the
/// right. Centered vertically like a WinUI settings row.
fn labeled_row(title: String, control: impl IntoElement) -> impl IntoElement {
    h_flex()
        .justify_between()
        .items_center()
        .gap_4()
        .py_2()
        .child(title)
        .child(control)
}

/// A labelled on/off row with the WinUI row height.
fn switch_row(title: String, control: impl IntoElement) -> impl IntoElement {
    h_flex()
        .justify_between()
        .items_center()
        .py_2()
        .child(title)
        .child(control)
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Re-probe the taskbar shape at most once a second: the user can move
        // or auto-hide the bar while this window stays open, but a registry
        // read plus `SHAppBarMessage` on every keystroke-triggered repaint is
        // needless I/O on the UI thread.
        let probe_due = self
            .last_taskbar_probe
            .is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(1));
        if probe_due {
            self.vertical_taskbar = taskbar_is_vertical();
            self.layout_available = layout_available();
            self.last_taskbar_probe = Some(Instant::now());
        }
        // Deactivation flush (registered once): persist the current state
        // whenever the window loses focus, covering the case where a prefix
        // field still holds the last edit when the window is closed or the
        // user clicks away.
        if self._deactivation.is_none() {
            let sub = cx.observe_window_activation(window, |this: &mut Self, window, cx| {
                if !window.is_window_active() {
                    this.save(cx);
                }
            });
            self._deactivation = Some(sub);
        }

        // Live OS appearance flip (registered once): the settings panel
        // always mirrors the system light/dark theme (the `display.theme`
        // setting is indicator-only), so a system flip repaints this window
        // immediately. Same first-render registration pattern as the
        // activation observer (never from a paint-phase API — the root
        // view's empty active-node stack would panic the unwrap inside
        // `window.on_key_event`).
        if self._appearance.is_none() {
            let sub = cx.observe_window_appearance(window, |_this: &mut Self, _w, cx| {
                apply_theme_and_accent(cx);
                cx.notify();
            });
            self._appearance = Some(sub);
        }

        // Every label comes from the core i18n table, keyed on the
        // *currently selected* language, so switching language repaints the
        // whole window immediately instead of only after saving.
        let lang = self.current_lang();

        // Theme colors are read once up front so the conditional `.when`
        // closure below never touches `cx` (keeps the borrow checker out of
        // the builder-chain argument positions).
        // Secondary text targets WCAG AA at the 12px sizes used here. The
        // previous 0.5/0.4 pair read too light on the tinted page and cards.
        let dim = cx.theme().foreground.opacity(0.6);
        let faint = cx.theme().foreground.opacity(0.55);

        // --- Flattened (segmented) option rows for small choice sets: every
        // choice is a visible tile, the active one carries the primary style.
        // Spacing/margin/anchor use dropdowns instead (longer option lists).
        let mut lang_row = h_flex().gap_1().flex_wrap();
        for i in 0..Language::ALL.len() {
            let active = i == self.lang_idx;
            lang_row = lang_row.child(
                Button::new(format!("lang-{i}"))
                    .small()
                    .rounded(px(CONTROL_RADIUS))
                    .when(active, |b| b.primary())
                    .label(Language::ALL[i].label().to_string())
                    .on_click(cx.listener(
                        move |this: &mut Self,
                              _: &ClickEvent,
                              window: &mut Window,
                              cx: &mut Context<Self>| {
                            this.lang_idx = i;
                            // The anchor choices are translated text: rebuild
                            // them so the dropdown repaints in the new
                            // language, then persist immediately.
                            this.refresh_anchor_items(window, cx);
                            // The custom titlebar repaints from `lang`, but the
                            // platform title (taskbar / Mission Control /
                            // accessibility) needs an explicit update.
                            window.set_window_title(t(Language::ALL[i], "settings.title"));
                            this.save(cx);
                            cx.notify();
                        },
                    )),
            );
        }

        let mut interval_row = h_flex().gap_1().flex_wrap();
        for (i, interval_ms) in ALLOWED_INTERVAL_MS.iter().enumerate() {
            let active = i == self.interval_idx;
            let secs = *interval_ms / 1000;
            let text = format!("{secs} {}", t(lang, "settings.unit_seconds"));
            interval_row = interval_row.child(
                Button::new(format!("interval-{i}"))
                    .small()
                    .rounded(px(CONTROL_RADIUS))
                    .when(active, |b| b.primary())
                    .label(text)
                    .on_click(cx.listener(
                        move |this: &mut Self,
                              _: &ClickEvent,
                              _: &mut Window,
                              cx: &mut Context<Self>| {
                            this.interval_idx = i;
                            this.save(cx);
                            cx.notify();
                        },
                    )),
            );
        }

        // Theme choices render as icons (globe = follow system, sun = light,
        // moon = dark) with the translated name kept as the tooltip. All
        // three glyphs live in the bundled default asset set, so they paint
        // through the registered `Assets` source — a catalog name outside
        // that set (e.g. Monitor) would silently render blank.
        const THEME_KEYS: [&str; 3] = ["theme.auto", "theme.light", "theme.dark"];
        const THEME_ICONS: [IconName; 3] = [IconName::Globe, IconName::Sun, IconName::Moon];
        let mut theme_row = h_flex().gap_1();
        for i in 0..THEME_KEYS.len() {
            let active = i == self.theme_idx;
            let tip = t(lang, THEME_KEYS[i]).to_string();
            theme_row = theme_row.child(
                Button::new(format!("theme-{i}"))
                    .small()
                    .rounded(px(CONTROL_RADIUS))
                    .when(active, |b| b.primary())
                    .icon(THEME_ICONS[i].clone())
                    .tooltip(tip)
                    .on_click(cx.listener(
                        move |this: &mut Self,
                              _: &ClickEvent,
                              _: &mut Window,
                              cx: &mut Context<Self>| {
                            this.theme_idx = i;
                            // The choice is persisted for the indicator to
                            // hot-reload. It deliberately does NOT re-theme
                            // this window or the history popup: those always
                            // follow the system appearance, so changing it
                            // must not restyle the panels the user is
                            // looking at.
                            this.save(cx);
                            cx.notify();
                        },
                    )),
            );
        }

        // Style the dropdown triggers/inputs like Windows Settings: a filled
        // control brush, a visible 1px stroke and the WinUI 4px corner. The
        // per-element overrides win over the component's internal appearance
        // (its style refinement is applied last). Other hosts keep the
        // library look.
        let control: Option<(Hsla, Hsla, f32)> = {
            #[cfg(windows)]
            {
                let dark = cx.theme().mode.is_dark();
                let (bg, border) = if dark {
                    (hsla(0., 0., 1., 0.06), hsla(0., 0., 1., 0.30))
                } else {
                    (hsla(0., 0., 1., 0.72), hsla(0., 0., 0., 0.26))
                };
                Some((bg, border, CONTROL_RADIUS))
            }
            #[cfg(not(windows))]
            {
                None
            }
        };
        #[cfg(windows)]
        let mut spacing_select = Select::new(&self.spacing_select);
        #[cfg(windows)]
        let mut margin_select = Select::new(&self.margin_select);
        #[cfg(windows)]
        let mut anchor_select = Select::new(&self.anchor_select);
        #[cfg(windows)]
        if let Some((bg, border, radius)) = control {
            spacing_select = spacing_select
                .bg(bg)
                .border_color(border)
                .rounded(px(radius));
            margin_select = margin_select
                .bg(bg)
                .border_color(border)
                .rounded(px(radius));
            anchor_select = anchor_select
                .bg(bg)
                .border_color(border)
                .rounded(px(radius));
        }

        // macOS raises the three dropdowns as native menus. The trigger keeps
        // the same 160/240px slots and filled-control look, so the layout
        // below is shared with Windows.
        #[cfg(target_os = "macos")]
        let mac_style = {
            let theme = cx.theme();
            MacSelectStyle {
                background: theme.input_background(),
                foreground: theme.foreground,
                border: theme.input,
                caret: theme.muted_foreground,
                radius: theme.radius,
            }
        };
        #[cfg(windows)]
        let spacing_control = blur_on_outside(div().w(px(160.))).child(spacing_select);
        #[cfg(windows)]
        let margin_control = blur_on_outside(div().w(px(160.))).child(margin_select);
        #[cfg(windows)]
        let anchor_control = blur_on_outside(div().w(px(160.))).child(anchor_select);
        #[cfg(target_os = "macos")]
        let spacing_control = div().w(px(160.)).child(mac_select_trigger(
            "spacing-trigger",
            px_items(&ALLOWED_ITEM_SPACING_PX)[self.spacing_idx].clone(),
            mac_style,
            cx.listener(|this, _, _, cx| {
                let items = px_items(&ALLOWED_ITEM_SPACING_PX);
                if let Some(index) = macos_select::show(&items, this.spacing_idx) {
                    this.spacing_idx = index;
                    this.save(cx);
                    cx.notify();
                }
            }),
        ));
        #[cfg(target_os = "macos")]
        let margin_control = div().w(px(160.)).child(mac_select_trigger(
            "margin-trigger",
            px_items(&ALLOWED_LEFT_MARGIN_PX)[self.margin_idx].clone(),
            mac_style,
            cx.listener(|this, _, _, cx| {
                let items = px_items(&ALLOWED_LEFT_MARGIN_PX);
                if let Some(index) = macos_select::show(&items, this.margin_idx) {
                    this.margin_idx = index;
                    this.save(cx);
                    cx.notify();
                }
            }),
        ));
        #[cfg(target_os = "macos")]
        let anchor_control = div().w(px(160.)).child(mac_select_trigger(
            "anchor-trigger",
            anchor_items(self.current_lang())[self.anchor_idx].clone(),
            mac_style,
            cx.listener(|this, _, _, cx| {
                let items = anchor_items(this.current_lang());
                if let Some(index) = macos_select::show(&items, this.anchor_idx) {
                    this.anchor_idx = index;
                    this.save(cx);
                    cx.notify();
                }
            }),
        ));

        let dark = cx.theme().mode.is_dark();
        // Native separators are hairlines: one device pixel. GPUI lays out in
        // logical pixels, so divide by the display scale to land on one
        // physical pixel on Retina instead of drawing a two-pixel line.
        let scale_factor = window.scale_factor();
        // Grouped-card fill. Windows paints an opaque WinUI card tone: the
        // platform renderer accumulates layer alpha additively, so a
        // translucent card over the translucent page tint saturates to alpha 1
        // and composites as an opaque gray — the Mica lift never reaches it.
        // #FBFBFB / #2B2B2B are the opaque equivalents of WinUI's 70% / 5%
        // white card over the light / dark page. macOS uses a near-white card
        // in light mode (the
        // old 0.55 white over the gray blur read as muddy gray next to System
        // Settings) and a slightly stronger dark lift; the separators below
        // do the row grouping that the Windows gap used to do.
        let card_bg = {
            #[cfg(target_os = "macos")]
            {
                if dark {
                    hsla(0., 0., 1., 0.10)
                } else {
                    hsla(0., 0., 1., 0.82)
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                if dark {
                    // Opaque equivalent of WinUI's #0DFFFFFF (5% white) card
                    // over the dark settings page (~#2B2B2B).
                    hsla(0., 0., 0.17, 1.)
                } else {
                    // Opaque equivalent of WinUI's #B3FFFFFF (70% white) card
                    // over the light settings page (~#FBFBFB).
                    hsla(0., 0., 0.984, 1.)
                }
            }
        };
        // WinUI's CardStrokeColorDefault is 6% black in light and 10% black
        // in dark. The light stroke is what keeps the near-white card readable
        // on a bright Mica page; the dark stroke is the system's own edge.
        let card_border = {
            #[cfg(target_os = "macos")]
            {
                None
            }
            #[cfg(not(target_os = "macos"))]
            {
                if dark {
                    Some(hsla(0., 0., 0., 0.10))
                } else {
                    Some(hsla(0., 0., 0., 0.06))
                }
            }
        };

        let general_card = settings_card(card_bg, card_border)
            .child(labeled_row(
                t(lang, "settings.language").to_string(),
                lang_row,
            ))
            .child(card_divider(dark, scale_factor))
            .child(labeled_row(
                t(lang, "settings.sample_interval").to_string(),
                interval_row,
            ))
            .when(cfg!(not(target_os = "macos")), |this| {
                // Offered only where the choice is actually honored. On
                // macOS the indicator title is drawn by AppKit, which picks
                // its own color from the menu-bar appearance.
                this.child(card_divider(dark, scale_factor))
                    .child(labeled_row(
                        t(lang, "settings.theme").to_string(),
                        theme_row,
                    ))
            });

        let display_card = settings_card(card_bg, card_border)
            .child(switch_row(
                t(lang, "settings.dual_row").to_string(),
                Switch::new("dual-row")
                    .checked(self.dual_row)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.dual_row = *checked;
                        this.save(cx);
                        cx.notify();
                    })),
            ))
            .child(card_divider(dark, scale_factor))
            .child(switch_row(
                t(lang, "settings.group_resources").to_string(),
                Switch::new("show-res")
                    .checked(self.show_resources)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.set_resources(*checked);
                        this.save(cx);
                        cx.notify();
                    })),
            ))
            .child(card_divider(dark, scale_factor))
            .child(switch_row(
                t(lang, "settings.group_network").to_string(),
                Switch::new("show-net")
                    .checked(self.show_network)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.set_network(*checked);
                        this.save(cx);
                        cx.notify();
                    })),
            ))
            .child(card_divider(dark, scale_factor))
            .child(labeled_row(
                t(lang, "settings.item_spacing").to_string(),
                spacing_control,
            ));

        // One layout card holds the anchor and margin rows together; either
        // row may be hidden by the platform/taskbar shape, but they read as a
        // single group when both apply.
        let mut layout_card = settings_card(card_bg, card_border);
        let mut show_layout = false;
        if self.layout_available && !self.vertical_taskbar {
            layout_card = layout_card.child(labeled_row(
                t(lang, "settings.anchor").to_string(),
                anchor_control,
            ));
            show_layout = true;
        }
        let show_margin = (self.layout_available && self.anchor_idx == 1 && !self.vertical_taskbar)
            || self.vertical_taskbar;
        if show_margin {
            if show_layout {
                layout_card = layout_card.child(card_divider(dark, scale_factor));
            }
            layout_card = layout_card.child(labeled_row(
                if self.vertical_taskbar {
                    t(lang, "settings.top_margin").to_string()
                } else {
                    t(lang, "settings.left_margin").to_string()
                },
                margin_control,
            ));
            show_layout = true;
        }

        let prefix_card = settings_card(card_bg, card_border)
            .child(
                // The 2x2 prefix grid uses spacing rather than a divider
                // between its two rows (2026-10-26, user request); the other
                // cards keep their row separators.
                v_flex()
                    .gap_3()
                    .child(
                        h_flex()
                            .gap_4()
                            .child(prefix_cell(
                                t(lang, "settings.cpu").to_string(),
                                &self.prefix_cpu,
                                dim,
                                control,
                            ))
                            .child(prefix_cell(
                                t(lang, "settings.mem").to_string(),
                                &self.prefix_mem,
                                dim,
                                control,
                            )),
                    )
                    .child(
                        h_flex()
                            .gap_4()
                            .child(prefix_cell(
                                t(lang, "settings.upload").to_string(),
                                &self.prefix_up,
                                dim,
                                control,
                            ))
                            .child(prefix_cell(
                                t(lang, "settings.download").to_string(),
                                &self.prefix_down,
                                dim,
                                control,
                            )),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(faint)
                    .pt_2()
                    .child(t(lang, "settings.prefix_hint").to_string()),
            );

        let body = v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .text_size(px(14.))
            .overflow_y_scrollbar()
            .child(settings_section(
                t(lang, "settings.general").to_string(),
                general_card,
                dim,
            ))
            .child(settings_section(
                t(lang, "settings.display").to_string(),
                display_card,
                dim,
            ))
            .when(show_layout, |this| {
                this.child(settings_section(
                    t(lang, "settings.layout").to_string(),
                    layout_card,
                    dim,
                ))
            })
            .child(settings_section(
                t(lang, "settings.prefixes").to_string(),
                prefix_card,
                dim,
            ))
            .when(self.save_failed.get(), |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(faint)
                        .text_center()
                        .child(t(lang, "settings.save_failed").to_string()),
                )
            })
            // Version footer: pinned to the bottom of the scrollable column
            // and built from `SV_BUILD_ID` (see the const below).
            .child(
                div()
                    .mt_auto()
                    .pt_2()
                    .text_xs()
                    .text_color(faint)
                    .text_center()
                    .child(SV_BUILD_ID),
            );

        // Both desktop panels draw into a transparent client area so the
        // tinted material continues behind the caption. macOS keeps its
        // centered 34px header and native traffic lights; Windows renders the
        // component title bar (caption drag + window controls) with a
        // transparent background, making the caption part of the page.
        #[cfg(target_os = "macos")]
        {
            let title = t(lang, "settings.title").to_string();
            v_flex()
                .size_full()
                .child(
                    h_flex()
                        .h(px(34.))
                        .w_full()
                        .flex_shrink_0()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .text_size(px(13.))
                                .font_semibold()
                                .text_color(cx.theme().foreground.opacity(0.9))
                                .child(title),
                        ),
                )
                .child(div().flex_1().overflow_hidden().child(body))
        }
        #[cfg(windows)]
        {
            let title = t(lang, "settings.title").to_string();
            v_flex()
                .size_full()
                .child(
                    TitleBar::new()
                        .w_full()
                        .pl(px(16.))
                        .bg(hsla(0., 0., 0., 0.))
                        .border_color(hsla(0., 0., 0., 0.))
                        .child(
                            div()
                                .text_size(px(13.))
                                .font_semibold()
                                .text_color(cx.theme().foreground.opacity(0.9))
                                .child(title),
                        ),
                )
                .child(div().flex_1().overflow_hidden().child(body))
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            body
        }
    }
}

/// Version banner shown in the settings footer — see the footer for how it
/// is produced by the build script.
const SV_BUILD_ID: &str = match option_env!("SV_BUILD_ID") {
    Some(id) => id,
    None => concat!("v", env!("CARGO_PKG_VERSION")),
};

/// One prefix cell of the settings 2x2 grid: a dim caption above a flexible
/// text input. Takes the entity by reference so the render borrow survives
/// the child call.
fn prefix_cell(
    label: String,
    input: &Entity<InputState>,
    label_color: Hsla,
    control: Option<(Hsla, Hsla, f32)>,
) -> impl IntoElement {
    let mut field = Input::new(input).w_full();
    if let Some((bg, border, radius)) = control {
        field = field.bg(bg).border_color(border).rounded(px(radius));
    }
    v_flex()
        .flex_1()
        .gap_1()
        .child(div().text_xs().text_color(label_color).child(label))
        .child(blur_on_outside(div().w_full()).child(field))
}

/// Load the config, backing up a malformed file before falling back to
/// defaults. Without the backup the first instant-save would overwrite the
/// user's (hand-edited) file with defaults.
fn load_config_with_backup() -> Config {
    let Ok(path) = sysvitals_core::config::config_path() else {
        return Config::default();
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Config::default(),
        Err(e) => {
            // A sharing violation/permission error is transient: do not move
            // the user's file aside (the next save would then overwrite the
            // canonical path with defaults). Only a malformed file is backed
            // up below.
            eprintln!("app-settings: config read failed: {e}; using defaults");
            return Config::default();
        }
    };
    match Config::from_toml_str(&text) {
        Ok((cfg, _)) => cfg,
        Err(e) => {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let backup = path.with_file_name(format!("config.toml.corrupt-{stamp}"));
            let _ = std::fs::rename(&path, &backup);
            eprintln!(
                "app-settings: config parse failed: {e}; backed up to {}",
                backup.display()
            );
            Config::default()
        }
    }
}

/// Whether another settings process already owns the instance lock. A
/// second window has its own startup snapshot and would overwrite the first
/// one's edits on its next save, so duplicate launches exit quietly.
#[cfg(windows)]
fn acquire_single_instance() -> bool {
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;
    let name = utf16("Local\\SysVitalsSettings_SingleInstance");
    match unsafe { CreateMutexW(None, false, windows::core::PCWSTR(name.as_ptr())) } {
        Ok(handle) => {
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                false
            } else {
                *SETTINGS_INSTANCE.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(handle.0 as isize);
                true
            }
        }
        // A mutex failure must not keep the panel from opening; the parent
        // launch slot remains the primary duplicate guard.
        Err(_) => true,
    }
}

#[cfg(target_os = "macos")]
fn acquire_single_instance() -> bool {
    use std::os::fd::AsRawFd;
    let Ok(dir) = sysvitals_core::config::data_dir() else {
        return true;
    };
    let _ = std::fs::create_dir_all(&dir);
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("settings.lock"))
    else {
        return true;
    };
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return false;
    }
    *SETTINGS_INSTANCE_FILE
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(file);
    true
}

#[cfg(not(any(windows, target_os = "macos")))]
fn acquire_single_instance() -> bool {
    true
}

#[cfg(windows)]
static SETTINGS_INSTANCE: std::sync::Mutex<Option<isize>> = std::sync::Mutex::new(None);
#[cfg(target_os = "macos")]
static SETTINGS_INSTANCE_FILE: std::sync::Mutex<Option<std::fs::File>> =
    std::sync::Mutex::new(None);

fn parse_parent_pid() -> Option<u32> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--parent-pid" {
            return args.next()?.parse().ok();
        }
    }
    None
}

/// Quit when the indicator that launched this panel disappears without
/// running its quit path (crash, Force Quit). The job object covers Windows
/// ordinary exits, but a monitored parent also catches a child launched
/// before the job was configured and mirrors the mac behavior.
#[cfg(windows)]
fn watch_parent(cx: &mut App) {
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    let Some(pid) = parse_parent_pid() else {
        return;
    };
    let raw = match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        Ok(handle) => handle.0 as isize,
        Err(e) => {
            eprintln!("app-settings: parent process unavailable: {e}");
            cx.quit();
            return;
        }
    };
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(1))
                .await;
            let exited = unsafe { WaitForSingleObject(HANDLE(raw as *mut std::ffi::c_void), 0) }
                != WAIT_TIMEOUT;
            if exited {
                cx.update(|app| app.quit());
                break;
            }
        }
        let _ = unsafe { CloseHandle(HANDLE(raw as *mut std::ffi::c_void)) };
    })
    .detach();
}

#[cfg(target_os = "macos")]
fn watch_parent(cx: &mut App) {
    let Some(pid) = parse_parent_pid() else {
        return;
    };
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(1))
                .await;
            if unsafe { libc::kill(pid as i32, 0) } != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                cx.update(|app| app.quit());
                break;
            }
        }
    })
    .detach();
}

#[cfg(not(any(windows, target_os = "macos")))]
fn watch_parent(_cx: &mut App) {}

fn main() {
    if !acquire_single_instance() {
        eprintln!("app-settings: another settings window is already running; exiting");
        return;
    }
    // GPUI does not declare DPI awareness itself. Without this the process
    // is treated as DPI-unaware and Windows bitmap-stretches the whole window
    // on a scaled display, which is what makes the text look soft.
    #[cfg(windows)]
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

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

        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(400.), px(680.)), cx)),
            titlebar: Some(titlebar),
            // System backdrop material (Win11 Mica / Win10-era acrylic blur /
            // macOS window blur, with an opaque fallback on anything older or
            // when the user disabled transparency effects). The palette tint
            // applied above is what lets this show through.
            window_background: backdrop_for(detect_material()),
            focus: true,
            ..Default::default()
        };

        let (window_handle, _view) =
            gpui_kit::open_window(window_options, cx, move |window, cx| {
                cx.new(|cx| SettingsView::new(cfg, window, cx))
            })
            .expect("Failed to open settings window");

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
        #[cfg(not(target_os = "macos"))]
        let _ = window_handle;
    });
}
