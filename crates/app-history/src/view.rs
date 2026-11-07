//! view: split out of the crate root (see `main.rs` for the module map).

use super::*;

use sysvitals_panel::restore_window_size_after_move;

pub struct HistoryView {
    pub lang: Language,
    /// Created hidden by the resident launcher; do not claim activation on
    /// the first (offscreen) frame. The loop activates it after the first
    /// data load.
    pub hidden_start: bool,
    /// Logical size the anchored popup was laid out for. Windows creates the
    /// window on whichever monitor the OS picks and then moves it to the
    /// target display; crossing a DPI boundary makes the OS suggest a
    /// rectangle scaled by the DPI ratio, so a 680x390 popup opened from a
    /// 125% primary on a 100% secondary shrinks to 542x311. Re-assert the
    /// intended size once the move has settled.
    #[cfg(windows)]
    pub fixed_size: Option<gpui_kit::Size<Pixels>>,
    /// One-shot guard for [`Self::restore_popup_size`].
    #[cfg(windows)]
    pub size_fixed: bool,
    pub range: TimeRange,
    pub buckets: Vec<Bucket>,
    /// Window the current `buckets` were queried for; `points()` pads it to
    /// one slot per bucket so the category x axis stays time-linear.
    pub query: Option<VisibleQuery>,
    /// Generation ticket for wide background queries: a result whose ticket
    /// no longer matches the current one (the range switched mid-flight) is
    /// dropped instead of painting stale points under a new axis.
    pub load_seq: u64,
    /// One connection kept for the window's lifetime. Reopening SQLite every
    /// three seconds re-ran WAL/schema setup and contended with the
    /// indicator's writer for no benefit.
    pub store: Option<Store>,
    /// One alert per failure streak: a corrupt database would otherwise log
    /// every refresh tick (3 s), drowning the useful history.
    pub data_error_logged: bool,
    pub csv_scope: CsvScope,
    pub copied: bool,
    pub copy_failed: bool,
    /// Monotonic ticket for the copy confirmation timer. An older timer must
    /// not clear a newer copy's message.
    pub copy_feedback_seq: u64,
    /// Last config mtime applied, in nanoseconds; the popup keeps language and
    /// CSV-scope settings live instead of freezing them at launch.
    pub config_mtime: u64,
    /// Last OS window title applied, so a language hot-reload can update the
    /// title (Mission Control / taskbar) without re-setting it every frame.
    pub last_title: Option<String>,
    /// Guards blur-close: only close after the window has been active at
    /// least once, so a transient "inactive" signal at open time (a macOS
    /// nonactivating panel that has not become key yet) cannot close the
    /// popup the instant it appears.
    pub ever_active: bool,
    /// Kept alive so the activation observer is not cancelled; dropped when
    /// the view is released.
    pub _activation: Option<Subscription>,
    /// Kept alive so the appearance observer is not cancelled. The panel
    /// always mirrors the OS light/dark theme (independent of the user's
    /// `display.theme` setting, which now only governs the indicator), so
    /// a live OS flip repaints this window too — see `observe_appearance`.
    pub _appearance: Option<Subscription>,
    /// Kept alive so the Esc observer is not cancelled. Esc closes the popup.
    /// This is an app-level keystroke observer, not a window paint-phase
    /// listener: registering from `render` with `Window::on_key_event` would
    /// touch the dispatch tree's active node, which does not exist yet for
    /// the root view on the first paint frame — GPUI unwraps it and panics,
    /// killing the process the moment the popup opens. This process owns a
    /// single window with no text fields, so a global keystroke observer is
    /// both safe and simpler.
    pub _escape: Option<Subscription>,
    /// Last accent value seen by the live-refresh poll; the override only
    /// re-applies when this moves (see `start_accent_watch`). Seeded with
    /// `None` so the first tick always re-applies once (idempotent when the
    /// startup override already succeeded).
    pub last_accent: Option<u32>,
    /// Signals the resident launcher that the first database read finished.
    /// The window stays hidden until this flips, so the first visible frame
    /// already carries chart data.
    pub first_load: Arc<AtomicBool>,
    /// Shared command/status block in resident mode; `None` for the one-shot
    /// launch used on macOS and for the fallback path.
    pub shared: Option<Arc<HistoryShared>>,
}

impl HistoryView {
    pub fn new(
        lang: Language,
        csv_scope: CsvScope,
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
            range: TimeRange::Hour1,
            buckets: Vec::new(),
            query: None,
            load_seq: 0,
            store: None,
            data_error_logged: false,
            csv_scope,
            copied: false,
            copy_failed: false,
            copy_feedback_seq: 0,
            config_mtime: 0,
            last_title: None,
            ever_active: false,
            _activation: None,
            _appearance: None,
            _escape: None,
            last_accent: None,
            first_load,
            shared,
        };
        view.start_refresh_loop(cx);
        view.start_escape_observer(cx);
        view.start_accent_watch(cx);
        view
    }

    /// Tell the resident indicator that this popup is gone. Called before
    /// every `remove_window`; a no-op for the one-shot launch.
    pub fn mark_hidden(&self) {
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
    pub fn start_accent_watch(&mut self, cx: &mut Context<Self>) {
        // Shared 500 ms accent poll (`sysvitals_panel`); it stops with the
        // view entity, i.e. when the window closes.
        spawn_accent_watch(
            cx,
            |view: &mut HistoryView| &mut view.last_accent,
            |cx| apply_theme_and_accent(cx),
        );
    }

    /// Dismiss the popup when Esc is pressed.
    pub fn start_escape_observer(&mut self, cx: &mut Context<Self>) {
        if self._escape.is_some() {
            return;
        }
        let subscription = cx.observe_keystrokes(|view, event, window, _cx| {
            if event.keystroke.key == "escape" {
                view.mark_hidden();
                window.remove_window();
            }
        });
        self._escape = Some(subscription);
    }

    /// Watch the window's OS-appearance changes and re-apply the system
    /// palette (the popup always follows the system; `display.theme` is
    /// indicator-only). Idempotent, and registered from the first render
    /// pass like `observe_activation` — never from a paint-phase callback,
    /// which would hit the empty active-node stack on the root view's first
    /// frame (see the crash note on `_escape`).
    pub fn observe_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    pub fn restore_popup_size(&mut self, window: &mut Window) {
        let Some(target) = self.fixed_size else {
            return;
        };
        // Shared cross-DPI size correction (`sysvitals_panel`).
        restore_window_size_after_move(window, &mut self.size_fixed, target);
    }

    /// Watch the window's activation and dismiss the popup when it loses
    /// focus after having been focused once. Idempotent.
    pub fn observe_activation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        let subscription = cx.observe_window_activation(window, |view, window, _| {
            if window.is_window_active() {
                view.ever_active = true;
                #[cfg(windows)]
                view.restore_popup_size(window);
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
    /// Reload language and CSV scope when the config file changes while the
    /// popup is open. The theme is system-owned; only these two settings are
    /// consumed by this panel. Returns whether anything visible changed so
    /// the static 1 d / 1 mo views can skip their three-second repaint.
    pub fn reload_config(&mut self) -> bool {
        let Ok(path) = config_path() else {
            return false;
        };
        let mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        if mtime == self.config_mtime {
            return false;
        }
        match Config::load(&path) {
            Ok((cfg, _)) => {
                self.config_mtime = mtime;
                let changed =
                    self.lang != cfg.display.language || self.csv_scope != cfg.history.csv_scope;
                self.lang = cfg.display.language;
                self.csv_scope = cfg.history.csv_scope;
                changed
            }
            Err(e) => {
                // Retry only when the file changes again, and keep the last
                // good values in the meantime.
                self.config_mtime = mtime;
                log_event("WARN", &format!("history: config reload failed: {e}"));
                false
            }
        }
    }

    pub fn start_refresh_loop(&self, cx: &mut Context<Self>) {
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

    /// The exact query window the chart is showing: a start floored to the
    /// range's label grid, the current time, and the fine curve bucket width.
    /// CSV export and the chart both use this so "copy what you see" is true
    /// at the window boundary too.
    ///
    /// The bucket grid uses the offset at the *window start*, not at "now":
    /// across a DST transition the old sample side must keep the offset that
    /// was active when the window opened. Labels are converted per timestamp
    /// in `axis_labels`, so both sides of the transition read the correct
    /// local time.
    pub fn visible_query(&self, now: i64) -> VisibleQuery {
        let step = self.range.label_step_secs();
        let start = now - self.range.seconds();
        let offset = local_offset_at(start);
        VisibleQuery {
            from: floor_to_grid(start, step, offset),
            to: now,
            bucket: self.range.bucket_secs(),
        }
    }

    /// Keep one SQLite connection for the window. Reopening every refresh
    /// re-ran WAL/schema setup and could fail with `SQLITE_BUSY` while the
    /// indicator was writing, leaving the chart empty.
    pub fn ensure_store(&mut self) -> bool {
        if self.store.is_some() {
            return true;
        }
        let path = match history_path() {
            Ok(path) => path,
            Err(e) => {
                self.log_data_error(format!("history: cannot resolve the store path: {e}"));
                return false;
            }
        };
        match Store::open(&path) {
            Ok(store) => {
                self.store = Some(store);
                true
            }
            Err(e) => {
                self.log_data_error(format!(
                    "history: store open failed: {}: {e}",
                    path.display()
                ));
                false
            }
        }
    }

    pub fn log_data_error(&mut self, message: String) {
        if !self.data_error_logged {
            log_event("WARN", &message);
            self.data_error_logged = true;
        }
    }

    pub fn load_data(&mut self, cx: &mut Context<Self>) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let query = self.visible_query(now);
        self.query = Some(query);
        // Every load advances the ticket: a pending wide query whose range
        // was replaced (by another wide range or by a narrow synchronous
        // load) is dropped when it delivers.
        self.load_seq = self.load_seq.wrapping_add(1);
        if query.bucket < ASYNC_MIN_BUCKET_SECS {
            self.load_data_sync(query);
            return;
        }
        // Wide ranges query in the background: the 1 d / 1 mo views scan the
        // whole retention window, and in the raw-only fallback (a failed
        // rollup backfill) that is the full raw table, which froze the popup
        // on slow disks. A fresh read-only connection is opened inside the
        // task because the view's `Store` is not `Send`; the generation
        // ticket drops a stale result when the range switched mid-flight.
        // Clear immediately so the previous range's points never render
        // under the new axis.
        self.buckets = Vec::new();
        let seq = self.load_seq;
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let loaded = executor
                .spawn(async move {
                    let path = history_path().map_err(|e| e.to_string())?;
                    let store = Store::open(&path).map_err(|e| e.to_string())?;
                    query_buckets(&store, query)
                })
                .await;
            let _ = this.update(cx, |view: &mut HistoryView, cx| {
                if view.load_seq != seq {
                    return;
                }
                match loaded {
                    Ok(buckets) => {
                        view.buckets = buckets;
                        view.data_error_logged = false;
                    }
                    Err(e) => view.log_data_error(format!("history: query failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Synchronous path for narrow buckets: reuse the window's long-lived
    /// connection (see `ensure_store`). Clears on failure so a range switch
    /// never shows the previous range's points under the new axis.
    pub fn load_data_sync(&mut self, query: VisibleQuery) {
        // The curve draws at the fine CSV bucket width; axis labels are
        // generated independently from the time window (see `axis_labels`).
        // One query serves both — no coarse pass.
        let mut buckets = Vec::new();
        if self.ensure_store() {
            let result = self.store.as_ref().map(|store| query_buckets(store, query));
            match result {
                Some(Ok(loaded)) => {
                    buckets = loaded;
                    self.data_error_logged = false;
                }
                Some(Err(e)) => {
                    self.log_data_error(format!("history: query failed: {e}"));
                    // A broken connection should be retried on the next
                    // refresh rather than kept forever.
                    self.store = None;
                }
                None => {}
            }
        }
        self.buckets = buckets;
    }

    pub fn points(&self) -> Vec<ChartPoint> {
        // The chart library's x scale is an index scale (`ScalePoint`), so
        // one point per bucket is required for a time-linear geometry. The
        // query returns only buckets that contain samples; pad the missing
        // slots here. Missing values repeat the previous real point so the
        // path stays finite; `has_data` lets the tooltip say "--" instead of
        // presenting a fabricated value.
        let Some(query) = self.query else {
            return Vec::new();
        };
        let Some(first) = self.buckets.first().copied() else {
            return Vec::new();
        };
        let mut by_ts: HashMap<i64, Bucket> = self
            .buckets
            .iter()
            .map(|bucket| (bucket.ts, *bucket))
            .collect();
        let bucket = query.bucket.max(1);
        let mut last = first;
        let mut points = Vec::new();
        let mut ts = query.from;
        while ts <= query.to {
            if let Some(bucket) = by_ts.remove(&ts) {
                last = bucket;
                points.push(ChartPoint {
                    ts,
                    cpu: bucket.cpu,
                    mem: bucket.mem,
                    has_data: true,
                });
            } else {
                points.push(ChartPoint {
                    ts,
                    cpu: last.cpu,
                    mem: last.mem,
                    has_data: false,
                });
            }
            ts = match ts.checked_add(bucket) {
                Some(next) => next,
                None => break,
            };
        }
        points
    }

    /// Six axis labels spread over the **time window**, not the data index.
    /// Each label floors its own timestamp to the range's clean grid using
    /// the offset at that timestamp, so a window crossing a DST transition
    /// still reads the correct local time on both sides.
    pub fn axis_labels(&self) -> Vec<(f32, String)> {
        let Some(query) = self.query else {
            return Vec::new();
        };
        if self.buckets.is_empty() {
            return Vec::new();
        }
        if query.to <= query.from {
            return Vec::new();
        }
        let month_view = self.range == TimeRange::Month1;
        let step = self.range.label_step_secs();
        let span = (query.to - query.from) as f64;
        let mut labels = Vec::new();
        let mut prev_day: Option<String> = None;
        let mut last_shown: Option<String> = None;
        for k in 0..X_TICK_COUNT {
            let frac = if X_TICK_COUNT <= 1 {
                0.5
            } else {
                k as f32 / (X_TICK_COUNT - 1) as f32
            };
            let ts = query.from + (span * f64::from(frac)).round() as i64;
            let clean = floor_to_grid(ts, step, local_offset_at(ts));
            let day = day_of(clean);
            let crosses = prev_day.as_ref().is_none_or(|previous| *previous != day);
            prev_day = Some(day);
            let text = if month_view {
                fmt_date(clean)
            } else if crosses {
                fmt_date_time(clean)
            } else {
                fmt_time(clean)
            };
            if last_shown.as_deref() == Some(text.as_str()) {
                continue;
            }
            last_shown = Some(text.clone());
            labels.push((frac, text));
        }
        labels
    }

    pub fn copy_csv(&mut self, cx: &mut Context<Self>) {
        self.copy_failed = false;
        let lang = self.lang;

        if self.csv_scope == CsvScope::Visible {
            // Exactly the points currently drawn, including the grid-aligned
            // window start and the local-time bucket phase.
            self.finish_copy(self.buckets.clone(), lang, cx);
            return;
        }

        // The longer scopes scan the whole retention window; a month of raw
        // samples on the UI thread visibly froze the popup. Query in the
        // background and only build the clipboard text back on the UI thread.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let (seconds, bucket_secs) = match self.csv_scope {
            CsvScope::Visible => unreachable!(),
            CsvScope::Last24h => (86_400, 120),
            CsvScope::Last7d => (604_800, 600),
            CsvScope::Last30d => (2_592_000, 3600),
        };
        let start = now - seconds;
        let from = floor_to_grid(start, bucket_secs, local_offset_at(start));
        let query = VisibleQuery {
            from,
            to: now,
            bucket: bucket_secs,
        };
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let loaded = executor
                .spawn(async move {
                    let path = history_path().map_err(|e| e.to_string())?;
                    let store = Store::open(&path).map_err(|e| e.to_string())?;
                    query_buckets(&store, query)
                })
                .await;
            let _ = this.update(cx, |view: &mut HistoryView, cx| match loaded {
                Ok(buckets) => view.finish_copy(buckets, lang, cx),
                Err(e) => {
                    log_event("WARN", &format!("history: CSV query failed: {e}"));
                    view.copied = false;
                    view.copy_failed = true;
                    view.schedule_copied_reset(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Build the clipboard text, write it, and arm the confirmation reset.
    pub fn finish_copy(&mut self, buckets: Vec<Bucket>, lang: Language, cx: &mut Context<Self>) {
        // Tab-separated rows paste straight into spreadsheet cells (WPS /
        // Excel split on tabs on paste), and unlike a space separator they
        // do not collide with the internal space of the "YYYY-MM-DD HH:MM:SS"
        // timestamp column. The UTF-8 BOM keeps CJK headers readable in Excel.
        let mut csv = String::from("\u{FEFF}");
        // Header labels come from the core i18n table (single source of truth
        // for the CSV columns), not a local copy, so a header edit lives in
        // exactly one place.
        let header = format!(
            "{}\t{}\t{}",
            t(lang, "csv.header.timestamp"),
            t(lang, "csv.header.cpu"),
            t(lang, "csv.header.mem")
        );
        csv.push_str(&header);
        csv.push('\n');
        for b in &buckets {
            let ts_str = format_ts_full(b.ts);
            csv.push_str(&format!("{ts_str}\t{:.1}\t{:.1}\n", b.cpu, b.mem));
        }

        match arboard::Clipboard::new() {
            Ok(mut clipboard) => match clipboard.set_text(csv) {
                Ok(()) => {
                    self.copied = true;
                    self.copy_failed = false;
                    log_event(
                        "INFO",
                        &format!("history: CSV copied ({} rows)", buckets.len()),
                    );
                }
                Err(e) => {
                    self.copied = false;
                    self.copy_failed = true;
                    log_event("WARN", &format!("history: clipboard write failed: {e}"));
                }
            },
            Err(e) => {
                self.copied = false;
                self.copy_failed = true;
                log_event("WARN", &format!("history: clipboard unavailable: {e}"));
            }
        }
        self.schedule_copied_reset(cx);
        cx.notify();
    }

    /// Clear the "copied" confirmation once the history window has had time
    /// to show it. Each copy takes a new ticket; an older timer that fires
    /// later must not clear a newer confirmation.
    pub fn schedule_copied_reset(&mut self, cx: &mut Context<Self>) {
        self.copy_feedback_seq = self.copy_feedback_seq.wrapping_add(1);
        let seq = self.copy_feedback_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1200))
                .await;
            let _ = this.update(cx, |view: &mut HistoryView, cx| {
                if view.copy_feedback_seq == seq {
                    view.copied = false;
                    view.copy_failed = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

impl Render for HistoryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = self.lang;
        let title = format!("SysVitals {}", t(lang, "chart.title"));
        if self.last_title.as_deref() != Some(title.as_str()) {
            window.set_window_title(&title);
            self.last_title = Some(title);
        }
        let points = self.points();
        let empty = self.buckets.is_empty();
        // The chart itself paints no x labels (`x_tick_count(0)` below); this
        // panel draws them at a readable size instead. Labels are generated
        // from the time window (six evenly spaced positions), not from data
        // indices, so a data gap does not move the axis text.
        let axis_labels = self.axis_labels();
        let copied = self.copied;
        let copy_failed = self.copy_failed;
        let current_range = self.range;

        // Register the blur-close observer and the live appearance observer
        // exactly once. Both are registered here (not from `new`) because
        // neither is available before the window exists; and neither uses a
        // paint-phase API, so the empty-dispatch-tree root-view trap that
        // killed the original Esc handler does not apply.
        self.observe_activation(window, cx);
        self.observe_appearance(window, cx);

        // Build range buttons individually to avoid borrow issues with cx.listener.
        let mut range_buttons = h_flex().gap_1();
        for r in TimeRange::ALL {
            let is_active = r == current_range;
            let label = r.label(lang).to_string();
            let btn = Button::new(format!("range-{:?}", r))
                .small()
                .rounded(px(CONTROL_RADIUS))
                .when(is_active, |b| b.primary())
                .label(label)
                .on_click(cx.listener(move |this: &mut Self, _: &ClickEvent, _, cx| {
                    this.range = r;
                    this.load_data(cx);
                    cx.notify();
                }));
            range_buttons = range_buttons.child(btn);
        }

        // Icon-only copy button: a green check flashes for ~1.2s after a
        // successful copy, then the button returns to the copy icon. The
        // tooltip keeps the textual hint in the current language.
        let copy_tip = if copied {
            t(lang, "chart.copied").to_string()
        } else if copy_failed {
            t(lang, "chart.copy_failed").to_string()
        } else {
            t(lang, "chart.copy_csv").to_string()
        };

        let copy_btn = Button::new("copy-csv")
            .small()
            .rounded(px(CONTROL_RADIUS))
            .tooltip(copy_tip)
            .when(copied, |b| {
                b.icon(IconName::CircleCheck)
                    .text_color(hsla(150.0 / 360.0, 0.7, 0.45, 1.0))
            })
            .when(!copied, |b| b.icon(IconName::Copy))
            .when(copy_failed, |b| b.text_color(hsla(0.0, 0.7, 0.5, 1.0)))
            .on_click(cx.listener(|this: &mut Self, _: &ClickEvent, _, cx| {
                this.copy_csv(cx);
            }));

        let header = h_flex()
            .justify_between()
            .items_center()
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .child(t(lang, "chart.title").to_string()),
            )
            .child(h_flex().gap_2().child(range_buttons).child(copy_btn));
        // `theme.border` resolves to a near-black neutral in dark mode, so
        // on the dark tinted popup the hairline read as a black line. Derive
        // it from the foreground instead: light on dark, dark on light, like
        // the system separators.
        let header_divider = div()
            .h(px(1.))
            .w_full()
            .bg(cx.theme().foreground.opacity(0.12));

        // One merged chart for both series. AreaChart accumulates one entry
        // per `y`/`stroke`/`fill`/`name` call in order, so two of each give
        // two lines; the zero-alpha fills keep the look of plain lines.
        // Percentages share one 0..=100 axis, which is what makes merging
        // sane.
        //
        // `appear(false)`: the library's default 1s left-to-right draw-in
        // is what the user experiences as "slow to show" — the popup's
        // job is to be readable the instant it opens, so the sweep is off.
        //
        // Y axis: `y_domain(0, 100)` + `y_tick_count(6)` + `y_padding(0, 0)`.
        // The library spaces the ticks evenly over the *full* plot height but
        // maps the pinned domain only into the height shrunk by the top
        // padding (10 px default), so the labels would read the padded
        // positions (e.g. 20.8 / 41.6 / ... instead of 20 / 40 / ...). With
        // zero padding the tick grid and the domain grid coincide and the
        // six labels land exactly on 0 / 20 / 40 / 60 / 80 / 100; the series
        // clips at the domain edges as before (pinned-plot mask). Its tick
        // labels are also painted by the overlay: `y_tick_format` returns
        // empty text, which keeps the axis line and the 32 px value-axis
        // gutter (the minimum gutter is fixed) while hiding the library's
        // 10 px labels.
        //
        // X axis: `x_tick_count(0)` keeps the axis line and the space the
        // library reserves for tick labels, but suppresses the labels: it
        // paints them at a fixed 10 px with no public size knob, so the
        // labels are drawn at `AXIS_LABEL_SIZE` by the overlay added below.
        // `points()` still labels the same six evenly spread indices
        // (mirroring the library's selection rule) with clean-time text, so
        // the overlay reads exactly like the axis the chart would have drawn,
        // only larger.
        //
        // `tooltip_title` overrides the axis label with the full
        // `YYYY-MM-DD HH:MM:SS` (with seconds), because the axis labels are
        // deliberately terse and a hover should give exact timing.
        //
        // Both label sets are painted by one canvas over the chart. The
        // canvas receives the chart's final bounds at paint time, so the
        // labels reproduce the library's plot geometry (the 32 px value-axis
        // gutter and the 18 px strip under the x axis) without re-deriving
        // the layout. The canvas paints before the chart, keeping a hover
        // tooltip above the labels.
        let y_labels: Vec<String> = (0..Y_TICK_COUNT)
            .map(|k| {
                let value =
                    Y_AXIS_MAX - (Y_AXIS_MAX - Y_AXIS_MIN) * k as f64 / (Y_TICK_COUNT - 1) as f64;
                format!("{value:.0}")
            })
            .collect();
        let label_color = cx.theme().muted_foreground;
        let axis_overlay = canvas(
            |_bounds, _window, _cx| {},
            move |bounds, _, window, cx| {
                let plot_left = X_AXIS_GUTTER;
                let plot_h = (f32::from(bounds.size.height) - X_AXIS_STRIP_H).max(0.0);
                let plot_w = (f32::from(bounds.size.width) - plot_left).max(0.0);
                let mut texts = Vec::with_capacity(axis_labels.len() + y_labels.len());
                for (i, (frac, text)) in axis_labels.iter().enumerate() {
                    let first = i == 0;
                    let last = i + 1 == axis_labels.len();
                    let align = if first {
                        TextAlign::Left
                    } else if last {
                        TextAlign::Right
                    } else {
                        TextAlign::Center
                    };
                    texts.push(
                        PlotText::new(
                            text.clone(),
                            point(px(plot_left + plot_w * *frac), px(plot_h + 3.0)),
                            label_color,
                        )
                        .font_size(px(AXIS_LABEL_SIZE))
                        .align(align),
                    );
                }
                let last_tick = (y_labels.len() - 1).max(1) as f32;
                for (k, text) in y_labels.iter().enumerate() {
                    let tick_y = plot_h * k as f32 / last_tick;
                    let top = (tick_y - AXIS_LABEL_SIZE * 0.5)
                        .clamp(0.0, (plot_h - AXIS_LABEL_SIZE).max(0.0));
                    texts.push(
                        PlotText::new(
                            text.clone(),
                            point(px(plot_left - 4.0), px(top)),
                            label_color,
                        )
                        .font_size(px(AXIS_LABEL_SIZE))
                        .align(TextAlign::Right),
                    );
                }
                PlotLabel::new(texts).paint(&bounds, window, cx);
            },
        )
        .absolute()
        .size_full();
        let empty_color = cx.theme().foreground.opacity(0.6);
        let merged_chart = div()
            .flex_1()
            .relative()
            .child(axis_overlay)
            .child(
                AreaChart::new(points)
                    // The x value is only used for axis/tooltip text; the
                    // library's ScalePoint projects by index. `points()`
                    // makes that index correspond to a bucket slot.
                    .x(|p: &ChartPoint| p.ts.to_string())
                    .y(|p: &ChartPoint| p.cpu)
                    .stroke(cpu_color())
                    .fill(transparent_color())
                    .name(t(lang, "chart.cpu").to_string())
                    .y(|p: &ChartPoint| p.mem)
                    .stroke(mem_color())
                    .fill(transparent_color())
                    .name(t(lang, "chart.mem").to_string())
                    .y_domain(Y_AXIS_MIN, Y_AXIS_MAX)
                    .y_padding(0.0, 0.0)
                    .x_axis(true)
                    .x_tick_count(0)
                    .y_axis(true)
                    .y_tick_format(|_| "")
                    .y_tick_count(Y_TICK_COUNT)
                    .grid(true)
                    .appear(false)
                    .tooltip_title(|p: &ChartPoint| format_ts_full(p.ts).into())
                    .tooltip_value(|p: &ChartPoint, _series: usize, v: f64| {
                        if p.has_data {
                            format!("{v:.2}%").into()
                        } else {
                            "--".into()
                        }
                    }),
            )
            .when(empty, |this| {
                this.child(
                    h_flex()
                        .absolute()
                        .size_full()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .text_sm()
                                .text_color(empty_color)
                                .child(t(lang, "chart.no_data").to_string()),
                        ),
                )
            });

        // Shared legend: dot + label per series, aligned with the chart's
        // own colors.
        let legend = h_flex()
            .gap_4()
            .items_center()
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(div().w(px(8.)).h(px(8.)).rounded(px(4.)).bg(cpu_color()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().foreground.opacity(0.7))
                            .child(t(lang, "chart.cpu").to_string()),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(div().w(px(8.)).h(px(8.)).rounded(px(4.)).bg(mem_color()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().foreground.opacity(0.7))
                            .child(t(lang, "chart.mem").to_string()),
                    ),
            );

        v_flex()
            .size_full()
            .p_4()
            .gap_2()
            .child(header)
            .child(header_divider)
            .child(legend)
            .child(merged_chart)
    }
}
