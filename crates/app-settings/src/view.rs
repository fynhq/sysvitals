//! view: split out of the crate root (see `main.rs` for the module map).

use super::*;

#[cfg(windows)]
use sysvitals_panel::restore_window_size_after_move;

// ---------------------------------------------------------------------------
// Dropdown item builders
// ---------------------------------------------------------------------------

/// Pixel-choice items carry their unit so the dropdown does not read as a
/// bare count. "px" is a unit symbol shared by both locales, so the list never
/// needs a language refresh.
pub fn px_items(values: &[u32]) -> Vec<String> {
    values.iter().map(|v| format!("{v} px")).collect()
}

/// Anchor items are translated text (the choice labels), rebuilt whenever the
/// selected language changes. Index 0 = right of the taskbar anchor space
/// (left of the tray), index 1 = far left.
pub fn anchor_items(lang: Language) -> Vec<String> {
    vec![
        t(lang, "anchor.right").to_string(),
        t(lang, "anchor.left").to_string(),
    ]
}

/// Every dropdown here is a single-choice list of strings (pixel values
/// rendered as decimal text, or translated anchor labels). macOS uses native
/// menus instead, so this state exists only on Windows.
#[cfg(windows)]
pub type StringSelectState = SelectState<SearchableVec<String>>;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

pub struct SettingsView {
    pub cfg: Config,
    // Input states for prefixes
    pub prefix_cpu: Entity<InputState>,
    pub prefix_mem: Entity<InputState>,
    pub prefix_up: Entity<InputState>,
    pub prefix_down: Entity<InputState>,
    // Toggle states
    pub dual_row: bool,
    pub show_resources: bool,
    pub show_network: bool,
    pub auto_start: bool,
    /// Set only when the user actually clicks the launch-at-login switch.
    /// A config value inherited from defaults must not become an explicit
    /// platform side effect merely because another setting was saved.
    pub auto_start_touched: bool,
    // Index-based selections
    pub lang_idx: usize,
    pub interval_idx: usize,
    pub spacing_idx: usize,
    pub margin_idx: usize,
    pub theme_idx: usize,
    pub anchor_idx: usize,
    // Dropdown states (item text may carry the selected language). Windows
    // renders the library Select; macOS raises a native menu from the index
    // fields instead.
    #[cfg(windows)]
    pub spacing_select: Entity<StringSelectState>,
    #[cfg(windows)]
    pub margin_select: Entity<StringSelectState>,
    #[cfg(windows)]
    pub anchor_select: Entity<StringSelectState>,
    /// Whether the layout section is offered on this machine (see
    /// `layout_available`). False hides it entirely.
    pub layout_available: bool,
    /// Whether the taskbar is docked left/right (see `taskbar_is_vertical`),
    /// re-probed at most once a second while the window is open. On such a
    /// bar the margin row is shown unconditionally (the value offsets the
    /// indicator from the bar's top edge), while the anchor dropdown — which
    /// selects between the tray end and the far-left end of a HORIZONTAL bar
    /// — would be a dead control and stays hidden.
    pub vertical_taskbar: bool,
    /// Last accent value seen by the live-refresh poll; the override only
    /// re-applies when this moves (see `new`).
    pub last_accent: Option<u32>,
    /// Whether the most recent config write failed. Shown in the footer so a
    /// read-only/full disk does not silently swallow every setting change.
    pub save_failed: std::cell::Cell<bool>,
    /// Serialized form last written (or loaded) so a focus loss with no edits
    /// does not touch the file and trigger an indicator reload.
    pub last_saved_toml: Option<String>,
    /// Taskbar-shape probes are syscalls/registry reads; refresh them at most
    /// once a second on interaction-driven renders.
    pub last_taskbar_probe: Option<Instant>,
    /// The logical size the window was opened at. Windows derives a
    /// `WM_DPICHANGED` suggested rectangle from the *previous* display's DPI
    /// when the window is placed on a monitor with a different scale factor;
    /// the window is resized to this size once before it is shown, with an
    /// activation-edge check as the fallback (same correction as the history
    /// popup).
    #[cfg(windows)]
    pub initial_size: gpui_kit::Size<Pixels>,
    /// One-shot guard for [`Self::restore_window_size`], so a user resize
    /// after the window has settled is never overwritten.
    #[cfg(windows)]
    pub size_fixed: bool,
    /// Keep event subscriptions alive for the lifetime of the view.
    pub _subscriptions: Vec<Subscription>,
    /// Flush guard: saves the current state whenever the window loses focus,
    /// so a prefix typed right before closing with the title-bar button is
    /// not lost (its field may never have received an explicit blur).
    pub _deactivation: Option<Subscription>,
    /// Kept alive so the appearance observer is not cancelled: a live OS
    /// light/dark flip reloads the system palette (the panel always follows
    /// the system, independent of the indicator-only `display.theme`).
    pub _appearance: Option<Subscription>,
    /// Kept alive so the Cmd+W / Cmd+Q observer is not cancelled. macOS has
    /// no default window menu here, so those keys need an explicit handler;
    /// both close this window (the indicator is a separate process and keeps
    /// running).
    #[cfg(target_os = "macos")]
    pub _shortcuts: Option<Subscription>,
}

impl SettingsView {
    pub fn new(
        cfg: Config,
        window: &mut Window,
        cx: &mut Context<Self>,
        initial_size: gpui_kit::Size<Pixels>,
    ) -> Self {
        #[cfg(not(windows))]
        let _ = initial_size;
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
            _subscriptions.push(cx.subscribe_in(
                state,
                &*window,
                |this: &mut Self, state, ev: &InputEvent, window, cx| {
                    match ev {
                        // Truncate only on commit. Doing it on every Change
                        // would rewrite the field while an IME composition is
                        // active and can break Chinese/Japanese input.
                        // Blur/Enter is after composition has ended, and the
                        // updated value is what `save` reads immediately.
                        InputEvent::Blur | InputEvent::PressEnter { .. } => {
                            let current = state.read(cx).value().to_string();
                            let cleaned: String = current.chars().take(MAX_PREFIX_CHARS).collect();
                            if cleaned != current {
                                state.update(cx, |state, cx| {
                                    state.set_value(cleaned, window, cx);
                                });
                            }
                            this.save(cx);
                        }
                        _ => {}
                    }
                },
            ));
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
            auto_start: cfg.startup.auto_start,
            auto_start_touched: false,
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
            #[cfg(windows)]
            initial_size,
            #[cfg(windows)]
            size_fixed: false,
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
        // until it reopens. The shared poll (`sysvitals_panel`) re-applies
        // the override only when the value actually moved and stops with the
        // view entity, i.e. when the window closes.
        spawn_accent_watch(
            cx,
            |view: &mut SettingsView| &mut view.last_accent,
            |cx| apply_theme_and_accent(cx),
        );
        view
    }

    /// Re-assert the logical size the window was opened at after Windows
    /// placed it on the indicator's display.
    ///
    /// The window is created on the OS's default monitor and then moved to
    /// the anchor's display. A cross-DPI move makes Windows derive the
    /// suggested rectangle from the *previous* DPI, so a 400x720 panel
    /// opened from a 125% primary on a 100% secondary came out 318x575.
    /// GPUI applies that suggestion; restore the intended size once, after
    /// the move (shared cross-DPI correction in `sysvitals_panel`).
    #[cfg(windows)]
    pub fn restore_window_size(&mut self, window: &mut Window) {
        restore_window_size_after_move(window, &mut self.size_fixed, self.initial_size);
    }

    pub fn current_lang(&self) -> Language {
        Language::ALL[self.lang_idx]
    }

    /// Enforce "at least one indicator group stays on": when the user turns
    /// off the last remaining group, re-enable the other one instead, so the
    /// indicator is never left with nothing to show.
    pub fn set_resources(&mut self, checked: bool) {
        self.show_resources = checked;
        if !self.show_resources && !self.show_network {
            self.show_network = true;
        }
    }

    pub fn set_network(&mut self, checked: bool) {
        self.show_network = checked;
        if !self.show_network && !self.show_resources {
            self.show_resources = true;
        }
    }

    pub fn collect_config(&self, cx: &App) -> Config {
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
        cfg.startup.auto_start = self.auto_start;
        cfg.startup.auto_start_explicit =
            self.auto_start_touched || cfg.startup.auto_start_explicit;
        cfg.display.prefix.cpu = self.prefix_cpu.read(cx).value().to_string();
        cfg.display.prefix.mem = self.prefix_mem.read(cx).value().to_string();
        cfg.display.prefix.up = self.prefix_up.read(cx).value().to_string();
        cfg.display.prefix.down = self.prefix_down.read(cx).value().to_string();
        cfg.sanitize();
        cfg
    }

    /// Persist the UI-owned fields. The settings model is a startup snapshot,
    /// so a blind whole-file write would silently revert fields this window
    /// does not edit (`history.*`, `sample.cpu_smoothing`) after a manual edit.
    /// Re-read the current file and carry those fields forward; only UI-owned
    /// fields come from the view. Also skip the write when the serialized
    /// result is unchanged, so merely focusing away does not bump the mtime
    /// and make the indicator reload/re-sample.
    pub fn save(&mut self, cx: &App) {
        let ui = self.collect_config(cx);
        let Ok(path) = sysvitals_core::config::config_path() else {
            log_event(
                "WARN",
                "settings: cannot resolve the config path; save skipped",
            );
            self.save_failed.set(true);
            return;
        };
        // Start from the file currently on disk so every field this window
        // does not own (history.*, sample.cpu_smoothing, and any future
        // non-UI field) survives. Only the UI-owned fields are overlaid.
        let mut cfg = match Config::load(&path) {
            Ok((disk, issues)) => {
                for issue in &issues {
                    log_event(
                        "WARN",
                        &format!(
                            "settings: config merge repair {}: {}",
                            issue.field, issue.message
                        ),
                    );
                }
                disk
            }
            Err(e) => {
                log_event(
                    "WARN",
                    &format!("settings: config merge read failed: {e}; keeping UI values"),
                );
                self.cfg.clone()
            }
        };
        let disk_auto_start_explicit = cfg.startup.auto_start_explicit;
        cfg.overlay_ui_from(&ui);
        // A manual edit that already marked the preference explicit must not
        // be downgraded just because this window did not touch the switch.
        cfg.startup.auto_start_explicit |= disk_auto_start_explicit;
        cfg.sanitize();
        let text = match cfg.to_toml_string() {
            Ok(text) => text,
            Err(e) => {
                log_event("WARN", &format!("settings: config serialize failed: {e}"));
                self.save_failed.set(true);
                return;
            }
        };
        if self.last_saved_toml.as_deref() == Some(text.as_str()) {
            return;
        }
        let result = cfg.save(&path);
        if let Err(e) = &result {
            log_event("WARN", &format!("settings: config save failed: {e}"));
        } else {
            log_event(
                "INFO",
                &format!(
                    "settings: config saved (interval={}ms, language={:?}, theme={:?}, \
                     dual_row={}, show_resources={}, show_network={}, item_spacing={}px, \
                     auto_start={})",
                    cfg.sample.interval_ms,
                    cfg.display.language,
                    cfg.display.theme,
                    cfg.display.dual_row,
                    cfg.display.show_resources,
                    cfg.display.show_network,
                    cfg.display.item_spacing,
                    cfg.startup.auto_start,
                ),
            );
            self.cfg = cfg;
            self.last_saved_toml = Some(text);
        }
        self.save_failed.set(result.is_err());
    }

    /// Rebuild the language-dependent anchor dropdown after a language flip,
    /// keeping the current selection. The dropdown entity and its
    /// subscription stay in place; only the items are swapped.
    pub fn refresh_anchor_items(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                if window.is_window_active() {
                    // The first frame may have rendered before activation
                    // settled; retry the one-shot correction here.
                    #[cfg(windows)]
                    this.restore_window_size(window);
                } else {
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
            self._appearance = Some(observe_system_appearance(cx, window, |cx| {
                apply_theme_and_accent(cx);
            }));
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
        // Hover color for the footer version link: the system accent is the
        // only affordance this GPUI snapshot offers for link text (there is
        // no underline styling on elements).
        let link = cx.theme().primary;

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
                mac_select_popup(
                    cx,
                    px_items(&ALLOWED_ITEM_SPACING_PX),
                    this.spacing_idx,
                    |view, index| view.spacing_idx = index,
                );
            }),
        ));
        #[cfg(target_os = "macos")]
        let margin_control = div().w(px(160.)).child(mac_select_trigger(
            "margin-trigger",
            px_items(&ALLOWED_LEFT_MARGIN_PX)[self.margin_idx].clone(),
            mac_style,
            cx.listener(|this, _, _, cx| {
                mac_select_popup(
                    cx,
                    px_items(&ALLOWED_LEFT_MARGIN_PX),
                    this.margin_idx,
                    |view, index| view.margin_idx = index,
                );
            }),
        ));
        #[cfg(target_os = "macos")]
        let anchor_control = div().w(px(160.)).child(mac_select_trigger(
            "anchor-trigger",
            anchor_items(self.current_lang())[self.anchor_idx].clone(),
            mac_style,
            cx.listener(|this, _, _, cx| {
                mac_select_popup(
                    cx,
                    anchor_items(this.current_lang()),
                    this.anchor_idx,
                    |view, index| view.anchor_idx = index,
                );
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
                IconName::Globe,
                lang_row,
                dim,
            ))
            .child(card_divider(dark, scale_factor))
            .child(labeled_row(
                t(lang, "settings.sample_interval").to_string(),
                IconName::RefreshCw,
                interval_row,
                dim,
            ))
            .child(card_divider(dark, scale_factor))
            .child(switch_row(
                t(lang, "settings.auto_start").to_string(),
                IconName::Play,
                Switch::new("auto-start")
                    .checked(self.auto_start)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.auto_start = *checked;
                        this.auto_start_touched = true;
                        this.save(cx);
                        cx.notify();
                    })),
                dim,
            ))
            .when(cfg!(not(target_os = "macos")), |this| {
                // Offered only where the choice is actually honored. On
                // macOS the indicator title is drawn by AppKit, which picks
                // its own color from the menu-bar appearance.
                this.child(card_divider(dark, scale_factor))
                    .child(labeled_row(
                        t(lang, "settings.theme").to_string(),
                        IconName::Palette,
                        theme_row,
                        dim,
                    ))
            });

        let display_card = settings_card(card_bg, card_border)
            .child(switch_row(
                t(lang, "settings.dual_row").to_string(),
                IconName::GalleryVerticalEnd,
                Switch::new("dual-row")
                    .checked(self.dual_row)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.dual_row = *checked;
                        this.save(cx);
                        cx.notify();
                    })),
                dim,
            ))
            .child(card_divider(dark, scale_factor))
            .child(switch_row(
                t(lang, "settings.group_resources").to_string(),
                IconName::Cpu,
                Switch::new("show-res")
                    .checked(self.show_resources)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.set_resources(*checked);
                        this.save(cx);
                        cx.notify();
                    })),
                dim,
            ))
            .child(card_divider(dark, scale_factor))
            .child(switch_row(
                t(lang, "settings.group_network").to_string(),
                IconName::Network,
                Switch::new("show-net")
                    .checked(self.show_network)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.set_network(*checked);
                        this.save(cx);
                        cx.notify();
                    })),
                dim,
            ))
            .child(card_divider(dark, scale_factor))
            .child(labeled_row(
                t(lang, "settings.item_spacing").to_string(),
                IconName::Frame,
                spacing_control,
                dim,
            ));

        // One layout card holds the anchor and margin rows together; either
        // row may be hidden by the platform/taskbar shape, but they read as a
        // single group when both apply.
        let mut layout_card = settings_card(card_bg, card_border);
        let mut show_layout = false;
        if self.layout_available && !self.vertical_taskbar {
            layout_card = layout_card.child(labeled_row(
                t(lang, "settings.anchor").to_string(),
                IconName::PanelLeft,
                anchor_control,
                dim,
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
                if self.vertical_taskbar {
                    IconName::ArrowUp
                } else {
                    IconName::ArrowLeft
                },
                margin_control,
                dim,
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
                                IconName::Cpu,
                                &self.prefix_cpu,
                                dim,
                                control,
                            ))
                            .child(prefix_cell(
                                t(lang, "settings.mem").to_string(),
                                IconName::MemoryStick,
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
                                IconName::ArrowUp,
                                &self.prefix_up,
                                dim,
                                control,
                            ))
                            .child(prefix_cell(
                                t(lang, "settings.download").to_string(),
                                IconName::ArrowDown,
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
                IconName::Settings,
                general_card,
                dim,
            ))
            .child(settings_section(
                t(lang, "settings.display").to_string(),
                IconName::LayoutDashboard,
                display_card,
                dim,
            ))
            .when(show_layout, |this| {
                this.child(settings_section(
                    t(lang, "settings.layout").to_string(),
                    IconName::PanelBottom,
                    layout_card,
                    dim,
                ))
            })
            .child(settings_section(
                t(lang, "settings.prefixes").to_string(),
                IconName::CaseSensitive,
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
            // and built from the product name plus `SV_BUILD_ID` (see the
            // const below). The whole stamp doubles as a single link to the
            // project repository; only the text itself is interactive, so the
            // surrounding footer stays inert.
            .child(
                h_flex().mt_auto().pt_2().justify_center().child(
                    div()
                        .id("version-link")
                        .text_xs()
                        .text_color(faint)
                        .cursor_pointer()
                        .hover(move |style| style.text_color(link))
                        .on_click(cx.listener(
                            |_: &mut Self,
                             _: &ClickEvent,
                             _: &mut Window,
                             cx: &mut Context<Self>| {
                                cx.open_url(REPO_URL);
                            },
                        ))
                        .child(format!("SysVitals {SV_BUILD_ID}")),
                ),
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
