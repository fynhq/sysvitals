//! History view state, data loading, and interactions.

use super::*;

use sysvitals_core::config_watch::ConfigWatcher;

#[cfg(windows)]
use sysvitals_panel::restore_window_size_after_move;

pub(crate) struct HistoryView {
    pub(crate) lang: Language,
    pub(crate) hidden_start: bool,
    #[cfg(windows)]
    pub(crate) fixed_size: Option<gpui_kit::Size<Pixels>>,
    #[cfg(windows)]
    pub(crate) size_fixed: bool,
    pub(crate) range: TimeRange,
    pub(crate) buckets: Vec<Bucket>,
    pub(crate) data_loading: bool,
    pub(crate) query: Option<VisibleQuery>,
    pub(crate) load_seq: u64,
    pub(crate) store: Option<Store>,
    pub(crate) data_error_logged: bool,
    pub(crate) copied: bool,
    pub(crate) copy_failed: bool,
    pub(crate) copy_feedback_seq: u64,
    pub(crate) config_watcher: ConfigWatcher,
    pub(crate) last_title: Option<String>,
    pub(crate) ever_active: bool,
    pub(crate) _activation: Option<Subscription>,
    pub(crate) _appearance: Option<Subscription>,
    // Keep the shortcut observer alive; dropping it silently disables Esc close and Cmd/Ctrl+C copy.
    pub(crate) _shortcuts: Option<Subscription>,
    pub(crate) last_accent: Option<u32>,
    pub(crate) first_load: Arc<AtomicBool>,
    pub(crate) shared: Option<Arc<HistoryShared>>,
    pub(crate) pending_draw: bool,
    pub(crate) series_fx: Option<SeriesFx>,
    pub(crate) series_gen: u64,
    pub(crate) last_series: Option<SeriesSnapshot>,
}

#[derive(Clone)]
pub(crate) struct SeriesSnapshot {
    pub(crate) from: i64,
    pub(crate) ys: Arc<Vec<(f64, f64)>>,
}

#[derive(Clone)]
pub(crate) enum SeriesFx {
    Draw,
    Refresh {
        prev_len: usize,
        prev_y: Arc<Vec<(f64, f64)>>,
    },
}

impl SeriesFx {
    pub(crate) fn duration(&self) -> Duration {
        match self {
            Self::Draw => SERIES_DRAW,
            Self::Refresh { .. } => SERIES_REFRESH,
        }
    }
}

impl HistoryView {
    pub(crate) fn new(
        lang: Language,
        hidden_start: bool,
        fixed_size: Option<gpui_kit::Size<Pixels>>,
        first_load: Arc<AtomicBool>,
        shared: Option<Arc<HistoryShared>>,
        cx: &mut Context<Self>,
    ) -> Self {
        #[cfg(not(windows))]
        let _ = fixed_size;
        let mut view = Self {
            lang,
            hidden_start,
            #[cfg(windows)]
            fixed_size,
            #[cfg(windows)]
            size_fixed: false,
            range: TimeRange::Min5,
            buckets: Vec::new(),
            data_loading: false,
            query: None,
            load_seq: 0,
            store: None,
            data_error_logged: false,
            copied: false,
            copy_failed: false,
            copy_feedback_seq: 0,
            config_watcher: ConfigWatcher::new().without_retry(),
            last_title: None,
            ever_active: false,
            _activation: None,
            _appearance: None,
            _shortcuts: None,
            last_accent: None,
            first_load,
            shared,
            pending_draw: true,
            series_fx: None,
            series_gen: 0,
            last_series: None,
        };
        view.start_refresh_loop(cx);
        view.start_shortcut_observer(cx);
        view.start_accent_watch(cx);
        view
    }

    pub(crate) fn begin_series_draw(&mut self) {
        self.pending_draw = true;
        self.series_fx = None;
        self.last_series = None;
    }

    pub(crate) fn start_pending_draw(&mut self, reduce_motion: bool) {
        if !self.pending_draw {
            return;
        }
        if reduce_motion {
            self.pending_draw = false;
            return;
        }
        if self.data_loading || self.query.is_none() || self.buckets.is_empty() {
            return;
        }
        if self.hidden_start && !self.ever_active {
            return;
        }
        if self.points().len() < 2 {
            return;
        }
        self.pending_draw = false;
        self.series_fx = Some(SeriesFx::Draw);
        self.series_gen = self.series_gen.wrapping_add(1);
    }

    pub(crate) fn note_series_data(&mut self) {
        let Some(query) = self.query else {
            self.last_series = None;
            self.series_fx = None;
            return;
        };
        if self.buckets.is_empty() {
            self.last_series = None;
            self.series_fx = None;
            return;
        }
        let points = self.points();
        let ys: Vec<(f64, f64)> = points.iter().map(|point| (point.cpu, point.mem)).collect();
        let current = SeriesSnapshot {
            from: query.from,
            ys: Arc::new(ys),
        };
        if self.pending_draw {
            self.last_series = Some(current);
            return;
        }
        if matches!(self.series_fx.as_ref(), Some(SeriesFx::Draw)) {
            self.last_series = Some(current);
            return;
        }
        let next_len = points.len();
        let prev = self.last_series.replace(current);
        let Some(prev) = prev else {
            return;
        };
        if !self.range.auto_refresh() {
            self.series_fx = None;
            return;
        }
        let prev_len = prev.ys.len();
        let values_changed = next_len == prev_len
            && points
                .iter()
                .zip(prev.ys.iter())
                .any(|(point, values)| point.cpu != values.0 || point.mem != values.1);
        if should_animate_live_refresh(prev.from, prev_len, query.from, next_len, values_changed) {
            self.series_fx = Some(SeriesFx::Refresh {
                prev_len,
                prev_y: prev.ys,
            });
            self.series_gen = self.series_gen.wrapping_add(1);
        } else {
            self.series_fx = None;
        }
    }

    pub(crate) fn mark_hidden(&self) {
        if let Some(shared) = &self.shared {
            shared.mark_hidden();
        }
    }

    // Accent-only changes do not fire the appearance observer; poll the OS accent every 500 ms and re-apply.
    pub(crate) fn start_accent_watch(&mut self, cx: &mut Context<Self>) {
        spawn_accent_watch(
            cx,
            |view: &mut HistoryView| &mut view.last_accent,
            apply_theme_and_accent,
        );
    }

    pub(crate) fn start_shortcut_observer(&mut self, cx: &mut Context<Self>) {
        if self._shortcuts.is_some() {
            return;
        }
        let subscription = cx.observe_keystrokes(|view, event, window, cx| {
            let keystroke = &event.keystroke;
            if keystroke.key == "escape" {
                view.mark_hidden();
                window.remove_window();
                return;
            }
            if keystroke.key == "c"
                && keystroke.modifiers.secondary(cx)
                && keystroke.modifiers.number_of_modifiers() == 1
            {
                view.copy_from_shortcut(cx);
            }
        });
        self._shortcuts = Some(subscription);
    }

    pub(crate) fn observe_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self._appearance.is_some() {
            return;
        }
        self._appearance = Some(observe_system_appearance(cx, window, |cx| {
            apply_theme_and_accent(cx);
        }));
    }

    #[cfg(windows)]
    pub(crate) fn restore_popup_size(&mut self, window: &mut Window) {
        let Some(target) = self.fixed_size else {
            return;
        };
        restore_window_size_after_move(window, &mut self.size_fixed, target);
    }

    pub(crate) fn observe_activation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self._activation.is_some() {
            return;
        }
        if window.is_window_active() {
            self.ever_active = true;
            #[cfg(windows)]
            self.restore_popup_size(window);
        }
        // A popup can open unactivated on Windows; only a real activation edge arms click-away, so a fast double-click can leave it stuck.
        let subscription = cx.observe_window_activation(window, |view, window, cx| {
            if window.is_window_active() {
                view.ever_active = true;
                #[cfg(windows)]
                view.restore_popup_size(window);
                cx.notify();
            } else if view.ever_active {
                view.mark_hidden();
                window.remove_window();
            }
        });
        self._activation = Some(subscription);
    }

    pub(crate) fn reload_config(&mut self) -> bool {
        match sysvitals_panel::poll_config(&mut self.config_watcher, "history: config reload") {
            ConfigEvent::Unchanged => false,
            ConfigEvent::Failed => false,
            ConfigEvent::Loaded { config, .. } => {
                let changed = self.lang != config.display.language;
                self.lang = config.display.language;
                changed
            }
        }
    }

    pub(crate) fn start_refresh_loop(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(16))
                .await;
            if this
                .update(cx, |view: &mut HistoryView, cx| {
                    view.reload_config();
                    view.load_data(cx);
                    view.first_load.store(true, Ordering::Release);
                    log_event(
                        "INFO",
                        &format!(
                            "history: window created at {}ms, first data {}ms later ({} points, hidden_start={})",
                            WINDOW_CREATED_MS.load(Ordering::Relaxed),
                            elapsed_ms().saturating_sub(WINDOW_CREATED_MS.load(Ordering::Relaxed) as u128),
                            view.buckets.len(),
                            view.hidden_start,
                        ),
                    );
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                if this
                    .update(cx, |view: &mut HistoryView, cx| {
                        let config_changed = view.reload_config();
                        let live = view.range.auto_refresh();
                        if live {
                            view.load_data(cx);
                        }
                        if live || config_changed {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }
}

mod data;
mod render;
