//! History view state, data loading, and interactions.

use super::*;

use std::sync::Arc;
use sysvitals_core::config_watch::ConfigWatcher;

#[cfg(windows)]
use sysvitals_panel::restore_window_size_after_move;

const BLUR_CLOSE_DELAY: Duration = Duration::from_millis(100);
const FOREGROUND_POLL_INTERVAL: Duration = Duration::from_millis(50);

fn window_is_foreground(window: &Window) -> bool {
    #[cfg(windows)]
    {
        window_hwnd(window).is_some_and(window_has_foreground)
    }
    #[cfg(not(windows))]
    {
        window.is_window_active()
    }
}

fn foreground_note() -> String {
    #[cfg(windows)]
    {
        format!(" (foreground: {})", foreground_window_description())
    }
    #[cfg(not(windows))]
    {
        String::new()
    }
}

pub(crate) struct HistoryView {
    pub(crate) lang: Language,
    #[cfg(windows)]
    pub(crate) fixed_size: Option<gpui_kit::Size<Pixels>>,
    #[cfg(windows)]
    pub(crate) size_fixed: bool,
    pub(crate) range: TimeRange,
    pub(crate) buckets: Vec<Bucket>,
    pub(crate) data_loading: bool,
    pub(crate) query: Option<VisibleQuery>,
    pub(crate) load_seq: u64,
    pub(crate) retry_at: Option<Instant>,
    pub(crate) retry_attempts: u32,
    pub(crate) data_error_logged: bool,
    pub(crate) first_data_logged: bool,
    pub(crate) copied: bool,
    pub(crate) copy_failed: bool,
    pub(crate) copy_feedback_seq: u64,
    pub(crate) config_watcher: ConfigWatcher,
    pub(crate) last_title: Option<String>,
    pub(crate) ever_active: bool,
    pub(crate) foreground_watch: bool,
    pub(crate) _appearance: Option<Subscription>,
    // Keep the shortcut observer alive; dropping it silently disables Esc close and Cmd/Ctrl+C copy.
    pub(crate) _shortcuts: Option<Subscription>,
    pub(crate) last_accent: Option<u32>,
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
        fixed_size: Option<gpui_kit::Size<Pixels>>,
        cx: &mut Context<Self>,
    ) -> Self {
        #[cfg(not(windows))]
        let _ = fixed_size;
        let mut view = Self {
            lang,
            #[cfg(windows)]
            fixed_size,
            #[cfg(windows)]
            size_fixed: false,
            range: TimeRange::Min5,
            buckets: Vec::new(),
            data_loading: false,
            query: None,
            load_seq: 0,
            retry_at: None,
            retry_attempts: 0,
            data_error_logged: false,
            first_data_logged: false,
            copied: false,
            copy_failed: false,
            copy_feedback_seq: 0,
            config_watcher: ConfigWatcher::new().without_retry(),
            last_title: None,
            ever_active: false,
            foreground_watch: false,
            _appearance: None,
            _shortcuts: None,
            last_accent: None,
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

    fn note_foreground(&mut self, window: &mut Window) {
        #[cfg(not(windows))]
        let _ = window;
        self.ever_active = true;
        #[cfg(windows)]
        self.restore_popup_size(window);
    }

    // Windows does not reliably deliver WM_ACTIVATE(WA_INACTIVE) to the popup, so poll
    // the real foreground while the window is open instead of trusting activation edges.
    pub(crate) fn watch_foreground(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.foreground_watch {
            return;
        }
        self.foreground_watch = true;
        if window_is_foreground(window) {
            self.note_foreground(window);
        }
        cx.spawn_in(window, async move |this, cx| {
            let mut away_since: Option<Instant> = None;
            loop {
                cx.background_executor()
                    .timer(FOREGROUND_POLL_INTERVAL)
                    .await;
                let state = this
                    .update_in(cx, |view, window, _| {
                        let foreground = window_is_foreground(window);
                        if foreground && !view.ever_active {
                            view.note_foreground(window);
                        }
                        (foreground, view.ever_active)
                    })
                    .ok();
                let Some((foreground, armed)) = state else {
                    return;
                };
                if foreground {
                    if away_since.take().is_some() {
                        log_event(
                            "INFO",
                            "history: popup kept open; the foreground returned during the close grace",
                        );
                    }
                    continue;
                }
                if !armed {
                    continue;
                }
                let away = away_since.get_or_insert_with(Instant::now).elapsed();
                if away < BLUR_CLOSE_DELAY {
                    continue;
                }
                log_event(
                    "INFO",
                    &format!(
                        "history: popup closed after {}ms without the foreground{}",
                        away.as_millis(),
                        foreground_note()
                    ),
                );
                let _ = this.update_in(cx, |_view, window, _| window.remove_window());
                return;
            }
        })
        .detach();
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
                    log_event(
                        "INFO",
                        &format!(
                            "history: window created at {}ms",
                            WINDOW_CREATED_MS.load(Ordering::Relaxed)
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
                        let retry_due =
                            !live && view.retry_at.is_some_and(|at| Instant::now() >= at);
                        if live || retry_due {
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
