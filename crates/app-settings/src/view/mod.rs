//! view: split out of the crate root (see `main.rs` for the module map).

use super::*;

#[cfg(windows)]
use sysvitals_panel::restore_window_size_after_move;

// ---------------------------------------------------------------------------
// Dropdown item builders
// ---------------------------------------------------------------------------

/// Render a list of pixel presets. "px" is a unit symbol shared by both
/// locales, so the list needs no translation.
fn px_items(values: &[u32]) -> Vec<String> {
    values.iter().map(|px| format!("{px} px")).collect()
}

/// Item-spacing presets, shown as raw pixel values.
pub(crate) fn spacing_items() -> Vec<String> {
    px_items(&ALLOWED_ITEM_SPACING_PX)
}

/// Margin presets, shown like [`spacing_items`].
pub(crate) fn margin_items() -> Vec<String> {
    px_items(&ALLOWED_LEFT_MARGIN_PX)
}

/// Anchor items are translated text (the choice labels), rebuilt whenever the
/// selected language changes. Index 0 = right of the taskbar anchor space
/// (left of the tray), index 1 = far left.
pub(crate) fn anchor_items(lang: Language) -> Vec<String> {
    vec![
        t(lang, "anchor.right").to_string(),
        t(lang, "anchor.left").to_string(),
    ]
}

/// Every dropdown here is a single-choice list of strings (raw pixel presets
/// or translated anchor labels). macOS uses native menus instead, so this
/// state exists only on Windows.
#[cfg(windows)]
pub(crate) type StringSelectState = SelectState<SearchableVec<String>>;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

pub(crate) struct SettingsView {
    pub(crate) cfg: Config,
    // Input states for prefixes
    pub(crate) prefix_cpu: Entity<InputState>,
    pub(crate) prefix_mem: Entity<InputState>,
    pub(crate) prefix_up: Entity<InputState>,
    pub(crate) prefix_down: Entity<InputState>,
    // Toggle states
    pub(crate) dual_row: bool,
    pub(crate) show_resources: bool,
    pub(crate) show_network: bool,
    pub(crate) auto_start: bool,
    /// Set only when the user actually clicks the launch-at-login switch.
    /// A config value inherited from defaults must not become an explicit
    /// platform side effect merely because another setting was saved.
    pub(crate) auto_start_touched: bool,
    // Index-based selections
    pub(crate) lang_idx: usize,
    pub(crate) interval_idx: usize,
    pub(crate) spacing_idx: usize,
    pub(crate) margin_idx: usize,
    pub(crate) theme_idx: usize,
    pub(crate) anchor_idx: usize,
    // Dropdown states (item text may carry the selected language). Windows
    // renders the library Select; macOS raises a native menu from the index
    // fields instead.
    #[cfg(windows)]
    pub(crate) spacing_select: Entity<StringSelectState>,
    #[cfg(windows)]
    pub(crate) margin_select: Entity<StringSelectState>,
    #[cfg(windows)]
    pub(crate) anchor_select: Entity<StringSelectState>,
    /// Whether the layout section is offered on this machine (see
    /// `layout_available`). False hides it entirely.
    pub(crate) layout_available: bool,
    /// Whether the taskbar is docked left/right (see `taskbar_is_vertical`),
    /// re-probed at most once a second while the window is open. On such a
    /// bar the margin row is shown unconditionally (the value offsets the
    /// indicator from the bar's top edge), while the anchor dropdown — which
    /// selects between the tray end and the far-left end of a HORIZONTAL bar
    /// — would be a dead control and stays hidden.
    pub(crate) vertical_taskbar: bool,
    /// Last accent value seen by the live-refresh poll; the override only
    /// re-applies when this moves (see `new`).
    pub(crate) last_accent: Option<u32>,
    /// Whether the most recent config write failed. Shown in the footer so a
    /// read-only/full disk does not silently swallow every setting change.
    pub(crate) save_failed: std::cell::Cell<bool>,
    /// The startup config could not be read for a transient reason. Saving is
    /// disabled for this window: the UI shows defaults, and writing them would
    /// overwrite the user's real file. Reopening the window retries the read.
    pub(crate) config_read_failed: bool,
    /// Serialized form last written (or loaded) so a focus loss with no edits
    /// does not touch the file and trigger an indicator reload.
    pub(crate) last_saved_toml: Option<String>,
    /// Taskbar-shape probes are syscalls/registry reads; refresh them at most
    /// once a second on interaction-driven renders.
    pub(crate) last_taskbar_probe: Option<Instant>,
    /// The logical size the window was opened at. Windows derives a
    /// `WM_DPICHANGED` suggested rectangle from the *previous* display's DPI
    /// when the window is placed on a monitor with a different scale factor;
    /// the window is resized to this size once before it is shown, with an
    /// activation-edge check as the fallback (same correction as the history
    /// popup).
    #[cfg(windows)]
    pub(crate) initial_size: gpui_kit::Size<Pixels>,
    /// One-shot guard for [`Self::restore_window_size`], so a user resize
    /// after the window has settled is never overwritten.
    #[cfg(windows)]
    pub(crate) size_fixed: bool,
    /// Keep event subscriptions alive for the lifetime of the view.
    pub(crate) _subscriptions: Vec<Subscription>,
    /// Flush guard: saves the current state whenever the window loses focus,
    /// so a prefix typed right before closing with the title-bar button is
    /// not lost (its field may never have received an explicit blur).
    pub(crate) _deactivation: Option<Subscription>,
    /// Kept alive so the appearance observer is not cancelled: a live OS
    /// light/dark flip reloads the system palette (the panel always follows
    /// the system, independent of the indicator-only `display.theme`).
    pub(crate) _appearance: Option<Subscription>,
    /// Kept alive so the Cmd+W / Cmd+Q observer is not cancelled. macOS has
    /// no default window menu here, so those keys need an explicit handler;
    /// both close this window (the indicator is a separate process and keeps
    /// running).
    #[cfg(target_os = "macos")]
    pub(crate) _shortcuts: Option<Subscription>,
}

impl SettingsView {
    pub(crate) fn new(
        cfg: Config,
        config_read_ok: bool,
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

        // Empty prefix fields read as broken; show the conventional fallback
        // as a hint without changing the stored value. The arrows are plain
        // Unicode, not emoji.
        for (state, placeholder) in [
            (&prefix_cpu, "CPU"),
            (&prefix_mem, "MEM"),
            (&prefix_up, "↑"),
            (&prefix_down, "↓"),
        ] {
            state.update(cx, |state, cx| {
                state.set_placeholder(placeholder, window, cx);
            });
        }

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
        let spacing_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(spacing_items()),
                Some(IndexPath::new(spacing_idx)),
                window,
                cx,
            )
        });
        #[cfg(windows)]
        let margin_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(margin_items()),
                Some(IndexPath::new(margin_idx)),
                window,
                cx,
            )
        });
        #[cfg(windows)]
        let anchor_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(anchor_items(Language::ALL[lang_idx])),
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
                    let items = spacing_items();
                    if let Some(i) = items.iter().position(|s| s == value) {
                        this.spacing_idx = i;
                        this.save(cx);
                        cx.notify();
                    }
                }
            }));
            _subscriptions.push(cx.subscribe(&margin_select, |this: &mut Self, _, ev, cx| {
                if let SelectEvent::Confirm(Some(value)) = ev {
                    let items = margin_items();
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
                        // Repaint so the parent-computed `n/8` counter
                        // tracks every edit. The value is only *read* here;
                        // truncation still happens on commit so an active IME
                        // composition is never rewritten mid-keystroke.
                        InputEvent::Change => cx.notify(),
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
            config_read_failed: !config_read_ok,
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
            apply_theme_and_accent,
        );
        view
    }

    /// Re-assert the logical size the window was opened at after Windows
    /// placed it on the indicator's display.
    ///
    /// The window is created on the OS's default monitor and then moved to
    /// the anchor's display. A cross-DPI move makes Windows derive the
    /// suggested rectangle from the *previous* DPI, so a 400x740 panel
    /// opened from a 125% primary on a 100% secondary came out 318x575.
    /// GPUI applies that suggestion; restore the intended size once, after
    /// the move (shared cross-DPI correction in `sysvitals_panel`).
    #[cfg(windows)]
    pub(crate) fn restore_window_size(&mut self, window: &mut Window) {
        restore_window_size_after_move(window, &mut self.size_fixed, self.initial_size);
    }

    pub(crate) fn current_lang(&self) -> Language {
        Language::ALL[self.lang_idx]
    }

    /// Apply a display-group choice. The UI disables the switch for the last
    /// visible group, so this never needs to flip the other switch behind the
    /// user's back; `sanitize` still guarantees a valid config if a caller
    /// bypasses the UI.
    pub(crate) fn set_resources(&mut self, checked: bool) {
        self.show_resources = checked;
    }

    pub(crate) fn set_network(&mut self, checked: bool) {
        self.show_network = checked;
    }

    pub(crate) fn collect_config(&self, cx: &App) -> Config {
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
    /// does not edit (`history.retention_days`) after a manual edit.
    /// Re-read the current file and carry those fields forward; only UI-owned
    /// fields come from the view. Also skip the write when the serialized
    /// result is unchanged, so merely focusing away does not bump the mtime
    /// and make the indicator reload/re-sample.
    pub(crate) fn save(&mut self, cx: &App) {
        if self.config_read_failed {
            // The startup snapshot is defaults, not the user's file. Writing
            // it would overwrite the real config; leave the file untouched
            // and let reopening the window retry the read.
            return;
        }
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
        // does not own (history.retention_days, and any future non-UI field)
        // survives. Only the UI-owned fields are overlaid. A read failure
        // aborts the save: falling back to the startup snapshot could write
        // defaults over a file that is temporarily unreadable.
        let mut cfg = match Config::load(&path) {
            Ok((disk, issues)) => {
                sysvitals_panel::log_sanitize_issues("settings: config merge repair", &issues);
                disk
            }
            Err(e) => {
                log_event(
                    "WARN",
                    &format!("settings: config merge read failed: {e}; save skipped"),
                );
                self.save_failed.set(true);
                return;
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

    /// Rebuild the translated anchor list after a language flip, keeping the
    /// current selection. Spacing and margin are pure pixel values, so those
    /// lists need no rebuild.
    pub(crate) fn refresh_localized_items(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
            // macOS builds the native menu from the localized list on every
            // click, so there is no persistent list to refresh here.
            let _ = (window, cx);
        }
    }
}

mod render;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_presets_match_the_allowed_values() {
        let spacing = spacing_items();
        assert_eq!(spacing.len(), ALLOWED_ITEM_SPACING_PX.len());
        for (item, value) in spacing.iter().zip(ALLOWED_ITEM_SPACING_PX) {
            assert_eq!(item, &format!("{value} px"));
        }

        let margins = margin_items();
        assert_eq!(margins.len(), ALLOWED_LEFT_MARGIN_PX.len());
        for (item, value) in margins.iter().zip(ALLOWED_LEFT_MARGIN_PX) {
            assert_eq!(item, &format!("{value} px"));
        }
    }

    #[test]
    fn anchor_labels_follow_the_selected_language() {
        let en = anchor_items(Language::En);
        let zh = anchor_items(Language::Zh);
        assert_eq!(en[0], t(Language::En, "anchor.right"));
        assert_eq!(zh[1], t(Language::Zh, "anchor.left"));
        assert_ne!(en, zh, "the two renders must not share one translation");
    }
}
