use super::*;

fn open_store_for_read(path: &std::path::Path) -> Result<Store, String> {
    let has_db = path
        .metadata()
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false);
    if has_db {
        Store::open_read_only(path).map_err(|e| e.to_string())
    } else {
        Store::open(path).map_err(|e| e.to_string())
    }
}

impl HistoryView {
    /// The initial query window: a start floored to the range's label grid,
    /// the current time, and the fine curve bucket width. After a load, the
    /// chart may trim `from` forward to the first real bucket; the `Visible`
    /// CSV exports those real buckets rather than padding the gap.
    ///
    /// The bucket grid uses the offset at the *window start*, not at "now":
    /// across a DST transition the old sample side must keep the offset that
    /// was active when the window opened. Labels are converted per timestamp
    /// in `axis_labels`, so both sides of the transition read the correct
    /// local time.
    pub(crate) fn visible_query(&self, now: i64) -> VisibleQuery {
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
    pub(crate) fn ensure_store(&mut self) -> bool {
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
        // Never migrate an existing database on the UI thread. `Store::open`
        // runs the one-time rollup backfill, which can take seconds on a
        // large raw table; the background history writer performs (and heals)
        // that migration. A missing or zero-length database is created here
        // because schema creation on an empty file is cheap and lets a manual
        // launch show an empty chart instead of an error.
        let opened = open_store_for_read(&path);
        match opened {
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

    pub(crate) fn log_data_error(&mut self, message: String) {
        if !self.data_error_logged {
            log_event("WARN", &message);
            self.data_error_logged = true;
        }
    }

    pub(crate) fn load_data(&mut self, cx: &mut Context<Self>) {
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
        } // Wide ranges query in the background: the 1 d / 1 mo views scan the
          // whole retention window, and in the raw-only fallback (a failed
          // rollup backfill) that is the full raw table, which froze the popup
          // on slow disks. A fresh read-only connection is opened inside the
          // task because the view's `Store` is not `Send`; the generation
          // ticket drops a stale result when the range switched mid-flight.
          // Clear immediately so the previous range's points never render
          // under the new axis.
        self.buckets = Vec::new();
        self.data_loading = true;
        let seq = self.load_seq;
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let loaded = executor
                .spawn(async move {
                    let path = history_path().map_err(|e| e.to_string())?;
                    let store = open_store_for_read(&path).map_err(|e| e.to_string())?;
                    query_buckets(&store, query)
                })
                .await;
            let _ = this.update(cx, |view: &mut HistoryView, cx| {
                if view.load_seq != seq {
                    return;
                }
                view.data_loading = false;
                match loaded {
                    Ok(buckets) => {
                        view.buckets = buckets;
                        view.trim_leading_gap();
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
    pub(crate) fn load_data_sync(&mut self, query: VisibleQuery) {
        self.data_loading = false;
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
        self.trim_leading_gap();
    }

    /// Trim the visible window to the first stored sample. The math lives in
    /// `core::chart` so it runs in the Linux unit-test gate.
    fn trim_leading_gap(&mut self) {
        if let Some(query) = self.query.as_mut() {
            trim_leading_gap(query, &self.buckets);
        }
    }

    pub(crate) fn points(&self) -> Vec<ChartPoint> {
        // One dense slot per bucket keeps the library's index x scale
        // time-linear; missing slots repeat the previous real value and are
        // flagged for the tooltip. The math lives in `core::chart`.
        self.query
            .map_or_else(Vec::new, |query| points(query, &self.buckets))
    }

    /// Six axis labels spread over the time window, each floored to the
    /// range's clean local grid. The math lives in `core::chart`.
    pub(crate) fn axis_labels(&self) -> Vec<(f32, String)> {
        self.query.map_or_else(Vec::new, |query| {
            axis_labels(self.range, query, &self.buckets, X_TICK_COUNT)
        })
    }

    pub(crate) fn copy_csv(&mut self, cx: &mut Context<Self>) {
        self.copy_failed = false;
        let lang = self.lang;

        if self.csv_scope == CsvScope::Visible {
            if self.data_loading {
                // A wide range is still being queried and `buckets` is
                // intentionally empty; copying now would produce a
                // header-only file that looks like real data.
                log_event(
                    "INFO",
                    "history: copy requested while the range is still loading",
                );
                self.copy_failed = true;
                self.schedule_copied_reset(cx);
                cx.notify();
                return;
            }
            // The real buckets in the current window. The chart pads missing
            // slots to keep its index axis time-linear, but the CSV deliberately
            // exports measured rows only instead of fabricating values.
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
                    let store = open_store_for_read(&path).map_err(|e| e.to_string())?;
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
    pub(crate) fn finish_copy(
        &mut self,
        buckets: Vec<Bucket>,
        lang: Language,
        cx: &mut Context<Self>,
    ) {
        // The CSV format (UTF-8 BOM, locale header, Tab separator, local
        // timestamps and real buckets only) lives in `core::chart` so the
        // Linux gate can test it byte-for-byte.
        let csv = build_csv(&buckets, lang);

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
    pub(crate) fn schedule_copied_reset(&mut self, cx: &mut Context<Self>) {
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
