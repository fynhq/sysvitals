//! view: split out of the crate root (see `main.rs` for the module map).

use super::*;

use sysvitals_core::config_watch::ConfigWatcher;

#[cfg(windows)]
use sysvitals_panel::restore_window_size_after_move;

pub(crate) struct HistoryView {
    pub(crate) lang: Language,
    /// Created hidden by the resident launcher; do not claim activation on
    /// the first (offscreen) frame. The loop activates it after the first
    /// data load.
    pub(crate) hidden_start: bool,
    /// Logical size the anchored popup was laid out for. Windows creates the
    /// window on whichever monitor the OS picks and then moves it to the
    /// target display; crossing a DPI boundary makes the OS suggest a
    /// rectangle scaled by the DPI ratio, so a 680x390 popup opened from a
    /// 125% primary on a 100% secondary shrinks to 542x311. Re-assert the
    /// intended size once the move has settled.
    #[cfg(windows)]
    pub(crate) fixed_size: Option<gpui_kit::Size<Pixels>>,
    /// One-shot guard for [`Self::restore_popup_size`].
    #[cfg(windows)]
    pub(crate) size_fixed: bool,
    pub(crate) range: TimeRange,
    pub(crate) buckets: Vec<Bucket>,
    /// True while a wide background query is in flight; `buckets` is empty
    /// until it lands, so Copy must not export a header-only CSV.
    pub(crate) data_loading: bool,
    /// Window the current `buckets` were queried for; `points()` pads it to
    /// one slot per bucket so the category x axis stays time-linear.
    pub(crate) query: Option<VisibleQuery>,
    /// Generation ticket for wide background queries: a result whose ticket
    /// no longer matches the current one (the range switched mid-flight) is
    /// dropped instead of painting stale points under a new axis.
    pub(crate) load_seq: u64,
    /// One connection kept for the window's lifetime. Reopening SQLite every
    /// three seconds re-ran WAL/schema setup and contended with the
    /// indicator's writer for no benefit.
    pub(crate) store: Option<Store>,
    /// One alert per failure streak: a corrupt database would otherwise log
    /// every refresh tick (3 s), drowning the useful history.
    pub(crate) data_error_logged: bool,
    pub(crate) copied: bool,
    pub(crate) copy_failed: bool,
    /// Monotonic ticket for the copy confirmation timer. An older timer must
    /// not clear a newer copy's message.
    pub(crate) copy_feedback_seq: u64,
    /// Config-file watcher; the popup keeps language and CSV-scope settings
    /// live instead of freezing them at launch.
    pub(crate) config_watcher: ConfigWatcher,
    /// Last OS window title applied, so a language hot-reload can update the
    /// title (Mission Control / taskbar) without re-setting it every frame.
    pub(crate) last_title: Option<String>,
    /// Guards blur-close: only close after the window has been active at
    /// least once, so a transient "inactive" signal at open time (a macOS
    /// nonactivating panel that has not become key yet) cannot close the
    /// popup the instant it appears.
    pub(crate) ever_active: bool,
    /// Kept alive so the activation observer is not cancelled; dropped when
    /// the view is released.
    pub(crate) _activation: Option<Subscription>,
    /// Kept alive so the appearance observer is not cancelled. The panel
    /// always mirrors the OS light/dark theme (independent of the user's
    /// `display.theme` setting, which now only governs the indicator), so
    /// a live OS flip repaints this window too — see `observe_appearance`.
    pub(crate) _appearance: Option<Subscription>,
    /// Kept alive so the Esc / copy shortcut observer is not cancelled. Esc
    /// closes the popup; Ctrl+C (Cmd+C on macOS) copies the CSV exactly like
    /// the Copy button. This is an app-level keystroke observer, not a window
    /// paint-phase listener: registering from `render` with
    /// `Window::on_key_event` would touch the dispatch tree's active node,
    /// which does not exist yet for the root view on the first paint frame —
    /// GPUI unwraps it and panics, killing the process the moment the popup
    /// opens. This process owns a single window with no text fields, so a
    /// global keystroke observer is both safe and simpler.
    pub(crate) _shortcuts: Option<Subscription>,
    /// Last accent value seen by the live-refresh poll; the override only
    /// re-applies when this moves (see `start_accent_watch`). Seeded with
    /// `None` so the first tick always re-applies once (idempotent when the
    /// startup override already succeeded).
    pub(crate) last_accent: Option<u32>,
    /// Signals the resident launcher that the first database read finished.
    /// The window stays hidden until this flips, so the first visible frame
    /// already carries chart data.
    pub(crate) first_load: Arc<AtomicBool>,
    /// Shared command/status block in resident mode; `None` for the one-shot
    /// launch used on macOS and for the fallback path.
    pub(crate) shared: Option<Arc<HistoryShared>>,
    /// A draw-in is owed to the next visible frame with data. Set on open and
    /// on every range click; a pre-warmed hidden window must not consume it
    /// before it is actually shown.
    pub(crate) pending_draw: bool,
    /// Series motion currently playing, consumed by the render pass. Only one
    /// of the draw-in and live-refresh channels runs at a time.
    pub(crate) series_fx: Option<SeriesFx>,
    /// Generation woven into the animation state id. A fresh value makes the
    /// next sequence start from zero instead of inheriting a finished one.
    pub(crate) series_gen: u64,
    /// Data from the last settled load. A live refresh animates from this
    /// curve; range switches and empty windows clear it.
    pub(crate) last_series: Option<SeriesSnapshot>,
}

/// One settled chart window: the start it was queried for and the y values of
/// every dense slot in order. Only the values are kept; the timestamps are
/// implied by the query start and bucket width.
#[derive(Clone)]
pub(crate) struct SeriesSnapshot {
    pub(crate) from: i64,
    pub(crate) ys: Arc<Vec<(f64, f64)>>,
}

/// Series motion currently playing.
#[derive(Clone)]
pub(crate) enum SeriesFx {
    /// Reveal both lines from the left edge of the plot.
    Draw,
    /// Expand the index axis from the previous slot count to the current one
    /// and ease changed values from the previous snapshot.
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

/// Open the history database for a query without running the writer's
/// migration on this thread. A missing/zero-length file is created (schema
/// creation is cheap); an existing database is opened read-only and an old
/// schema simply stays in raw-only mode until the background writer heals it.
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

    /// Draw both series in on the next visible frame with data. The initial
    /// open starts with the flag already armed; every range click re-arms it
    /// so a manual switch reads as a fresh chart rather than a silent swap.
    pub(crate) fn begin_series_draw(&mut self) {
        self.pending_draw = true;
        self.series_fx = None;
        self.last_series = None;
    }

    /// Start the pending draw once its three gates are open: data loaded, the
    /// pre-warmed resident window has been activated (so the draw is not
    /// played while hidden), and at least one line segment exists. Reduced
    /// motion drops the effect instead of playing it.
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
        // A resident pre-warm window is hidden until its first activation.
        // The activation edge is the only reliable signal that the user can
        // see the sweep; without it the draw would play offscreen. The rare
        // show-without-activation fallback keeps a static chart instead.
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

    /// Record the window that just loaded and decide what moves next.
    ///
    /// A pending draw owns the next visible frame. A draw that is already
    /// playing is not restarted or cancelled by data that lands meanwhile.
    /// A live append or value update starts a refresh from the previous
    /// curve; every other case (range switch, empty data, a window-start
    /// jump) settles statically and becomes the baseline for the next load.
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
        // Values are compared exactly: an unchanged bucket produces the same
        // aggregate from the same raw rows, so any difference is real.
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

    /// Tell the resident indicator that this popup is gone. Called before
    /// every `remove_window`; a no-op for the one-shot launch.
    pub(crate) fn mark_hidden(&self) {
        if let Some(shared) = &self.shared {
            shared.mark_hidden();
        }
    }

    /// Poll the OS accent every 500 ms and re-apply the override when it
    /// moved. The appearance observer only fires on a light/dark MODE flip
    /// (the Windows platform layer filters `WM_SETTINGCHANGE` down to
    /// `ImmersiveColorSet` with an appearance change), so an accent-only
    /// change never re-runs `apply_accent` on its own. The probe runs inside
    /// `update`: on macOS `system_accent_hex` reads AppKit's
    /// `controlAccentColor`, which belongs to the main thread. The loop stops
    /// with the view entity, i.e. when the window closes. Same shape as the
    /// settings window's accent poll.
    pub(crate) fn start_accent_watch(&mut self, cx: &mut Context<Self>) {
        // Shared 500 ms accent poll (`sysvitals_panel`); it stops with the
        // view entity, i.e. when the window closes.
        spawn_accent_watch(
            cx,
            |view: &mut HistoryView| &mut view.last_accent,
            apply_theme_and_accent,
        );
    }

    /// Dismiss the popup when Esc is pressed; copy the CSV on Ctrl+C
    /// (Cmd+C on macOS), mirroring the Copy button including its
    /// confirmation flash and disabled states.
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

    /// Watch the window's OS-appearance changes and re-apply the system
    /// palette (the popup always follows the system; `display.theme` is
    /// indicator-only). Idempotent, and registered from the first render
    /// pass like `observe_activation` — never from a paint-phase callback,
    /// which would hit the empty active-node stack on the root view's first
    /// frame (see the crash note on `_shortcuts`).
    pub(crate) fn observe_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self._appearance.is_some() {
            return;
        }
        // The popup always mirrors the OS light/dark appearance; a live
        // system flip repaints it with the palette the rest of the desktop is
        // now using. Accent override must be re-applied because a palette
        // reload replaces every field. Shared registration lives in
        // `sysvitals_panel::observe_system_appearance`.
        self._appearance = Some(observe_system_appearance(cx, window, |cx| {
            apply_theme_and_accent(cx);
        }));
    }

    /// Re-assert the logical size the anchored popup was laid out for after
    /// the window has been moved to the indicator's display.
    ///
    /// Windows places the freshly created window on the OS's default monitor
    /// and `SetWindowPlacement` then moves it to the anchor's display. When
    /// that move crosses a DPI boundary, the `WM_DPICHANGED` suggested
    /// rectangle is derived from the *old* DPI, so a window already sized for
    /// the target DPI is scaled a second time (a 680x390 popup opened from a
    /// 125% primary on a 100% secondary came out 542x311, leaving a visible
    /// gap above the taskbar). GPUI applies that suggestion, so the intended
    /// size has to be re-applied once the move is done. A same-DPI open
    /// leaves the size within a pixel of the target and is left untouched.
    #[cfg(windows)]
    pub(crate) fn restore_popup_size(&mut self, window: &mut Window) {
        let Some(target) = self.fixed_size else {
            return;
        };
        // Shared cross-DPI size correction (`sysvitals_panel`).
        restore_window_size_after_move(window, &mut self.size_fixed, target);
    }

    /// Watch the window's activation and dismiss the popup when it loses
    /// focus after having been focused once. Idempotent.
    pub(crate) fn observe_activation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self._activation.is_some() {
            return;
        }
        // The observer only sees activation *changes*: a window created with
        // `focus: true` is already active when the first render registers it,
        // and that edge is not replayed. Seed the guard from the current
        // state, otherwise the first click-away would not close the popup.
        if window.is_window_active() {
            self.ever_active = true;
            #[cfg(windows)]
            self.restore_popup_size(window);
        }
        // On Windows the popup can open *without* ever being activated: the
        // indicator overlay was the foreground process when it launched this
        // child, and the foreground-lock rules can reject the startup
        // activation attempt — then `ever_active` never flips and a click away
        // never closes the window (the reported bug: a fast double click opens
        // the popup but clicking elsewhere leaves it stuck; a single click
        // usually activates fine). The one-shot launch therefore claims
        // activation through GPUI's own `activate_window` path (reassert
        // placement + inject an Alt press so `SetForegroundWindow` is
        // permitted) — but that call runs as a foreground task from the
        // launch site, never from this render pass: `activate_window`
        // internally calls `SetForegroundWindow`/`SetWindowPos`, which
        // synchronously dispatch WM_ACTIVATE/WM_MOVE into GPUI while the app
        // is already borrowed (the same re-entrancy documented on
        // `sysvitals_panel::preplace_hidden_window`). The resulting
        // WM_ACTIVATE edge flows through the observer below and arms the
        // blur-close. macOS is deliberately untouched: its panel is an
        // accessory app's window with its own key-window lifecycle,
        // unverified at runtime, and the bug was only ever reproduced on
        // Windows.
        let subscription = cx.observe_window_activation(window, |view, window, cx| {
            if window.is_window_active() {
                view.ever_active = true;
                #[cfg(windows)]
                view.restore_popup_size(window);
                // A pending draw waits for this edge on a pre-warmed hidden
                // window; make sure a frame runs to start it.
                cx.notify();
            } else if view.ever_active {
                // Clicked away: dismiss the popup like a native menu.
                view.mark_hidden();
                window.remove_window();
            }
        });
        self._activation = Some(subscription);
    }

    /// Reload the stored samples every [`REFRESH_INTERVAL`] while the window
    /// lives. The loop stops as soon as the view entity is released, i.e.
    /// when the window closes.
    /// Reload language when the config file changes while the popup is open.
    /// The theme is system-owned and the history retention is consumed by the
    /// writer, so language is the only setting this panel consumes. Returns
    /// whether anything visible changed so the static 1 d / 1 mo views can
    /// skip their three-second repaint.
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
            // Let the window paint its first frame before touching SQLite:
            // opening the database and aggregating buckets used to run inside
            // `HistoryView::new`, delaying the first visible paint.
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
                        // The window title follows `lang`; render applies it
                        // when it changes, including this hot-reload path.
                        // 1 d / 1 mo scan the whole retention window; keep
                        // them static while open. The range button loads them
                        // on switch (and re-clicking the active range reloads).
                        // Their static view also skips the repaint unless the
                        // config changed something visible.
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
