//! History data loading, bucketing, and CSV export.

use super::*;

use std::time::{SystemTime, UNIX_EPOCH};
use sysvitals_core::store::Store;

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

fn static_retry_delay(attempts: u32) -> Duration {
    const STEPS_MS: [u64; 4] = [3_000, 6_000, 12_000, 30_000];
    Duration::from_millis(STEPS_MS[(attempts as usize).min(STEPS_MS.len() - 1)])
}

impl HistoryView {
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

    pub(crate) fn log_data_error(&mut self, message: String) {
        if !self.data_error_logged {
            log_event("WARN", &message);
            self.data_error_logged = true;
        }
    }

    fn arm_static_retry(&mut self) {
        let delay = static_retry_delay(self.retry_attempts);
        self.retry_attempts = self.retry_attempts.saturating_add(1);
        self.retry_at = Some(Instant::now() + delay);
    }

    pub(crate) fn load_data(&mut self, cx: &mut Context<Self>) {
        let range_changed = self.query.is_none();
        if range_changed {
            self.retry_at = None;
            self.retry_attempts = 0;
        }
        let background_retry = !range_changed && self.retry_at.take().is_some();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let query = self.visible_query(now);
        self.query = Some(query);
        self.load_seq = self.load_seq.wrapping_add(1);
        if self.buckets.is_empty() && !background_retry {
            self.data_loading = true;
        }
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
                        let empty = buckets.is_empty();
                        view.buckets = buckets;
                        view.trim_leading_gap();
                        view.data_error_logged = false;
                        if !view.first_data_logged {
                            view.first_data_logged = true;
                            log_event(
                                "INFO",
                                &format!(
                                    "history: first data loaded at {}ms ({} points)",
                                    elapsed_ms(),
                                    view.points().len()
                                ),
                            );
                        }
                        if empty && !view.range.auto_refresh() {
                            view.arm_static_retry();
                        } else {
                            view.retry_at = None;
                            view.retry_attempts = 0;
                        }
                    }
                    Err(e) => {
                        view.log_data_error(format!("history: query failed: {e}"));
                        if !view.range.auto_refresh() {
                            view.arm_static_retry();
                        }
                    }
                }
                view.note_series_data();
                cx.notify();
            });
        })
        .detach();
    }

    fn trim_leading_gap(&mut self) {
        if let Some(query) = self.query.as_mut() {
            trim_leading_gap(query, &self.buckets);
        }
    }

    pub(crate) fn points(&self) -> Vec<ChartPoint> {
        self.query
            .map_or_else(Vec::new, |query| points(query, &self.buckets))
    }

    pub(crate) fn axis_labels(&self) -> Vec<(f32, String)> {
        self.query.map_or_else(Vec::new, |query| {
            axis_labels(self.range, query, &self.buckets, X_TICK_COUNT)
        })
    }

    pub(crate) fn copy_available(&self) -> bool {
        !self.data_loading && !self.buckets.is_empty()
    }

    pub(crate) fn copy_from_shortcut(&mut self, cx: &mut Context<Self>) {
        if self.copy_available() {
            self.copy_csv(cx);
        }
    }

    pub(crate) fn copy_csv(&mut self, cx: &mut Context<Self>) {
        self.copy_failed = false;
        let lang = self.lang;

        if self.data_loading {
            log_event(
                "INFO",
                "history: copy requested while the range is still loading",
            );
            self.copy_failed = true;
            self.schedule_copied_reset(cx);
            cx.notify();
            return;
        }
        self.finish_copy(self.buckets.clone(), lang, cx);
    }

    pub(crate) fn finish_copy(
        &mut self,
        buckets: Vec<Bucket>,
        lang: Language,
        cx: &mut Context<Self>,
    ) {
        let rows = buckets.len();
        let csv = build_csv(&buckets, lang);
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            // The clipboard can block while another process holds it open, so it
            // is written off the UI thread; the handle never spans an await.
            let written = executor
                .spawn(async move {
                    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
                    clipboard.set_text(csv).map_err(|e| e.to_string())
                })
                .await;
            let _ = this.update(cx, |view: &mut HistoryView, cx| {
                match written {
                    Ok(()) => {
                        view.copied = true;
                        view.copy_failed = false;
                        log_event("INFO", &format!("history: CSV copied ({rows} rows)"));
                    }
                    Err(e) => {
                        view.copied = false;
                        view.copy_failed = true;
                        log_event("WARN", &format!("history: clipboard write failed: {e}"));
                    }
                }
                view.schedule_copied_reset(cx);
                cx.notify();
            });
        })
        .detach();
    }

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

#[cfg(test)]
mod tests {
    use super::{open_store_for_read, Store};
    use std::path::PathBuf;

    fn temp_database(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "sysvitals-history-test-{tag}-{}-{stamp}.db",
            std::process::id()
        ))
    }

    fn remove_database(path: &std::path::Path) {
        let base = path.display().to_string();
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{base}{suffix}"));
        }
    }

    #[test]
    fn static_retry_delay_backs_off_and_caps() {
        use std::time::Duration;
        assert_eq!(super::static_retry_delay(0), Duration::from_secs(3));
        assert_eq!(super::static_retry_delay(1), Duration::from_secs(6));
        assert_eq!(super::static_retry_delay(2), Duration::from_secs(12));
        assert_eq!(super::static_retry_delay(3), Duration::from_secs(30));
        assert_eq!(super::static_retry_delay(99), Duration::from_secs(30));
    }

    #[test]
    fn a_missing_database_is_created_for_a_manual_launch() {
        let path = temp_database("missing");
        let store = open_store_for_read(&path).expect("a missing file opens as a new store");
        drop(store);
        assert!(path.exists(), "the chart needs a database to query");
        remove_database(&path);
    }

    #[test]
    fn an_existing_database_opens_without_a_write_migration() {
        let path = temp_database("existing");
        {
            let store = Store::open(&path).expect("create the database");
            drop(store);
        }
        let store = open_store_for_read(&path).expect("an existing file opens read-only");
        drop(store);
        remove_database(&path);
    }
}
