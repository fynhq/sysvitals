//! Settings view state and control event handling.

use super::*;

#[cfg(windows)]
use sysvitals_panel::restore_window_size_after_move;

fn px_items(values: &[u32]) -> Vec<String> {
    values.iter().map(|px| format!("{px} px")).collect()
}

pub(crate) fn spacing_items() -> Vec<String> {
    px_items(&ALLOWED_ITEM_SPACING_PX)
}

pub(crate) fn margin_items() -> Vec<String> {
    px_items(&ALLOWED_LEFT_MARGIN_PX)
}

pub(crate) fn anchor_items(lang: Language) -> Vec<String> {
    vec![
        t(lang, "anchor.right").to_string(),
        t(lang, "anchor.left").to_string(),
    ]
}

#[cfg(windows)]
pub(crate) type StringSelectState = SelectState<SearchableVec<String>>;

pub(crate) struct SettingsView {
    pub(crate) cfg: Config,
    pub(crate) prefix_cpu: Entity<InputState>,
    pub(crate) prefix_mem: Entity<InputState>,
    pub(crate) prefix_up: Entity<InputState>,
    pub(crate) prefix_down: Entity<InputState>,
    pub(crate) dual_row: bool,
    pub(crate) show_resources: bool,
    pub(crate) show_network: bool,
    pub(crate) auto_start: bool,
    pub(crate) auto_start_touched: bool,
    pub(crate) lang_idx: usize,
    pub(crate) interval_idx: usize,
    pub(crate) spacing_idx: usize,
    pub(crate) margin_idx: usize,
    pub(crate) theme_idx: usize,
    pub(crate) anchor_idx: usize,
    #[cfg(windows)]
    pub(crate) spacing_select: Entity<StringSelectState>,
    #[cfg(windows)]
    pub(crate) margin_select: Entity<StringSelectState>,
    #[cfg(windows)]
    pub(crate) anchor_select: Entity<StringSelectState>,
    pub(crate) layout_available: bool,
    pub(crate) vertical_taskbar: bool,
    pub(crate) last_accent: Option<u32>,
    pub(crate) save_failed: std::cell::Cell<bool>,
    pub(crate) config_read_failed: bool,
    pub(crate) last_saved_toml: Option<String>,
    pub(crate) last_taskbar_probe: Option<Instant>,
    #[cfg(windows)]
    pub(crate) initial_size: gpui_kit::Size<Pixels>,
    #[cfg(windows)]
    pub(crate) size_fixed: bool,
    pub(crate) _subscriptions: Vec<Subscription>,
    pub(crate) _deactivation: Option<Subscription>,
    pub(crate) _appearance: Option<Subscription>,
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
                        cx.notify();
                    }
                }
            }));
        }
        for state in [&prefix_cpu, &prefix_mem, &prefix_up, &prefix_down] {
            _subscriptions.push(cx.subscribe_in(
                state,
                &*window,
                |this: &mut Self, state, ev: &InputEvent, window, cx| match ev {
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
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            ));
        }

        // GPUI does not blur focused inputs when a window is removed, so pending edits are saved before close.
        let this = cx.weak_entity();
        window.on_window_should_close(cx, move |_, cx| {
            let _ = this.update(cx, |view, cx| view.save(cx));
            true
        });

        #[cfg(target_os = "macos")]
        let window_shortcuts = cx.observe_keystrokes(|view, event, window, cx| {
            let keystroke = &event.keystroke;
            if keystroke.modifiers.platform
                && (keystroke.key.as_str() == "w" || keystroke.key.as_str() == "q")
            {
                view.save(cx);
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
        spawn_accent_watch(
            cx,
            |view: &mut SettingsView| &mut view.last_accent,
            apply_theme_and_accent,
        );
        view
    }

    #[cfg(windows)]
    pub(crate) fn restore_window_size(&mut self, window: &mut Window) {
        restore_window_size_after_move(window, &mut self.size_fixed, self.initial_size);
    }

    pub(crate) fn current_lang(&self) -> Language {
        Language::ALL[self.lang_idx]
    }

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

    pub(crate) fn save(&mut self, cx: &App) {
        if self.config_read_failed {
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
            let _ = (window, cx);
        }
    }
}

mod render;

#[cfg(test)]
mod tests {
    use super::{
        anchor_items, margin_items, spacing_items, t, Language, ALLOWED_ITEM_SPACING_PX,
        ALLOWED_LEFT_MARGIN_PX,
    };

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
