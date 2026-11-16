//! Pure history-chart ranges, buckets, and axis logic.

use std::collections::HashMap;

use chrono::{DateTime, FixedOffset, Local};

use crate::i18n::{t, Language};
use crate::store::{floor_to_grid, Bucket, Store};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeRange {
    Min5,
    Hour1,
    Day1,
    Month1,
}

impl TimeRange {
    pub const ALL: [TimeRange; 4] = [
        TimeRange::Min5,
        TimeRange::Hour1,
        TimeRange::Day1,
        TimeRange::Month1,
    ];

    pub fn seconds(self) -> i64 {
        match self {
            TimeRange::Min5 => 300,
            TimeRange::Hour1 => 3600,
            TimeRange::Day1 => 86_400,
            TimeRange::Month1 => 2_592_000,
        }
    }

    pub fn bucket_secs(self) -> i64 {
        match self {
            TimeRange::Min5 => 2,
            TimeRange::Hour1 => 10,
            TimeRange::Day1 => 120,
            TimeRange::Month1 => 3600,
        }
    }

    // Axis labels use a clean wall-clock step at local time; the curve keeps the finer bucket step.
    pub fn label_step_secs(self) -> i64 {
        match self {
            TimeRange::Min5 => 60,
            TimeRange::Hour1 => 600,
            TimeRange::Day1 => 7_200,
            TimeRange::Month1 => 432_000,
        }
    }

    pub fn label(self, lang: Language) -> &'static str {
        match self {
            TimeRange::Min5 => t(lang, "chart.range.5m"),
            TimeRange::Hour1 => t(lang, "chart.range.1h"),
            TimeRange::Day1 => t(lang, "chart.range.1d"),
            TimeRange::Month1 => t(lang, "chart.range.1mo"),
        }
    }

    pub fn auto_refresh(self) -> bool {
        matches!(self, TimeRange::Min5 | TimeRange::Hour1)
    }
}

#[derive(Debug, Clone)]
pub struct ChartPoint {
    pub ts: i64,
    pub cpu: f64,
    pub mem: f64,
    pub has_data: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct VisibleQuery {
    pub from: i64,
    pub to: i64,
    pub bucket: i64,
}

pub fn points(query: VisibleQuery, buckets: &[Bucket]) -> Vec<ChartPoint> {
    let Some(first) = buckets.first().copied() else {
        return Vec::new();
    };
    let mut by_ts: HashMap<i64, Bucket> =
        buckets.iter().map(|bucket| (bucket.ts, *bucket)).collect();
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

pub fn data_runs(points: &[ChartPoint]) -> Vec<std::ops::Range<usize>> {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    for (index, point) in points.iter().enumerate() {
        if point.has_data {
            if start.is_none() {
                start = Some(index);
            }
        } else if let Some(first) = start.take() {
            runs.push(first..index);
        }
    }
    if let Some(first) = start {
        runs.push(first..points.len());
    }
    runs
}

// Animate only a live refresh with an unchanged start, a non-shrinking line, and a visible value change.
pub fn should_animate_live_refresh(
    prev_from: i64,
    prev_len: usize,
    next_from: i64,
    next_len: usize,
    values_changed: bool,
) -> bool {
    prev_len >= 2
        && next_len >= prev_len
        && prev_from == next_from
        && (next_len > prev_len || values_changed)
}

pub fn trim_leading_gap(query: &mut VisibleQuery, buckets: &[Bucket]) {
    if let Some(first) = buckets.first() {
        query.from = query.from.max(first.ts);
    }
}

pub fn axis_labels(
    range: TimeRange,
    query: VisibleQuery,
    buckets: &[Bucket],
    tick_count: usize,
) -> Vec<(f32, String)> {
    axis_labels_with_offset(range, query, buckets, tick_count, local_offset_at)
}

fn axis_labels_with_offset<F>(
    range: TimeRange,
    query: VisibleQuery,
    buckets: &[Bucket],
    tick_count: usize,
    offset_at: F,
) -> Vec<(f32, String)>
where
    F: Fn(i64) -> i64,
{
    if buckets.is_empty() || query.to <= query.from {
        return Vec::new();
    }
    let month_view = range == TimeRange::Month1;
    let step = range.label_step_secs();
    let span = (query.to - query.from) as f64;
    let mut labels = Vec::new();
    let mut prev_day: Option<String> = None;
    let mut last_shown: Option<String> = None;
    for k in 0..tick_count {
        let frac = if tick_count <= 1 {
            0.5
        } else {
            k as f32 / (tick_count - 1) as f32
        };
        let ts = query.from + (span * f64::from(frac)).round() as i64;
        let clean = floor_to_grid(ts, step, offset_at(ts)).max(query.from);
        let offset = offset_at(clean);
        let day = day_of_at(clean, offset);
        let crosses = prev_day.as_ref().is_none_or(|previous| *previous != day);
        prev_day = Some(day);
        let text = if month_view {
            fmt_date_at(clean, offset)
        } else if crosses {
            fmt_date_time_at(clean, offset)
        } else {
            fmt_time_at(clean, offset)
        };
        if last_shown.as_deref() == Some(text.as_str()) {
            continue;
        }
        last_shown = Some(text.clone());
        labels.push((frac, text));
    }
    labels
}

pub fn build_csv(buckets: &[Bucket], lang: Language) -> String {
    build_csv_with_offset(buckets, lang, local_offset_at)
}

fn build_csv_with_offset<F>(buckets: &[Bucket], lang: Language, offset_at: F) -> String
where
    F: Fn(i64) -> i64,
{
    let mut csv = String::from("\u{FEFF}");
    let header = format!(
        "{}\t{}\t{}",
        t(lang, "csv.header.timestamp"),
        t(lang, "csv.header.cpu"),
        t(lang, "csv.header.mem")
    );
    csv.push_str(&header);
    csv.push('\n');
    for bucket in buckets {
        let ts_str = format_ts_full_at(bucket.ts, offset_at(bucket.ts));
        csv.push_str(&format!("{ts_str}\t{:.1}\t{:.1}\n", bucket.cpu, bucket.mem));
    }
    csv
}

pub fn local_offset_at(ts: i64) -> i64 {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&Local).offset().local_minus_utc() as i64)
        .unwrap_or(0)
}

pub fn fmt_date_time(ts: i64) -> String {
    fmt_date_time_at(ts, local_offset_at(ts))
}

pub fn format_ts_full(ts: i64) -> String {
    format_ts_full_at(ts, local_offset_at(ts))
}

pub fn query_buckets(store: &Store, query: VisibleQuery) -> Result<Vec<Bucket>, String> {
    store
        .buckets_offset(
            query.from,
            query.to,
            query.bucket,
            local_offset_at(query.from),
        )
        .map_err(|e| e.to_string())
}

fn fixed_datetime(ts: i64, offset_secs: i64) -> Option<DateTime<FixedOffset>> {
    let offset = FixedOffset::east_opt(offset_secs as i32)?;
    DateTime::from_timestamp(ts, 0).map(|dt| dt.with_timezone(&offset))
}

fn fmt_time_at(ts: i64, offset: i64) -> String {
    fixed_datetime(ts, offset)
        .map(|dt| dt.format("%H:%M").to_string())
        .unwrap_or_default()
}

fn fmt_date_at(ts: i64, offset: i64) -> String {
    fixed_datetime(ts, offset)
        .map(|dt| dt.format("%m-%d").to_string())
        .unwrap_or_default()
}

fn fmt_date_time_at(ts: i64, offset: i64) -> String {
    fixed_datetime(ts, offset)
        .map(|dt| dt.format("%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

fn day_of_at(ts: i64, offset: i64) -> String {
    fixed_datetime(ts, offset)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

fn format_ts_full_at(ts: i64, offset: i64) -> String {
    fixed_datetime(ts, offset)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(ts: i64, cpu: f64, mem: f64) -> Bucket {
        Bucket { ts, cpu, mem }
    }

    #[test]
    fn time_ranges_match_their_grids() {
        for (range, seconds, bucket_secs, label_step) in [
            (TimeRange::Min5, 300, 2, 60),
            (TimeRange::Hour1, 3600, 10, 600),
            (TimeRange::Day1, 86_400, 120, 7_200),
            (TimeRange::Month1, 2_592_000, 3600, 432_000),
        ] {
            assert_eq!(range.seconds(), seconds);
            assert_eq!(range.bucket_secs(), bucket_secs);
            assert_eq!(range.label_step_secs(), label_step);
        }
        assert_eq!(TimeRange::ALL.len(), 4);
        assert!(TimeRange::Min5.auto_refresh());
        assert!(TimeRange::Hour1.auto_refresh());
        assert!(!TimeRange::Day1.auto_refresh());
        assert!(!TimeRange::Month1.auto_refresh());
        assert_eq!(TimeRange::Hour1.label(Language::En), "1 hour");
    }

    #[test]
    fn points_fill_missing_slots_with_the_previous_real_value() {
        let buckets = vec![
            bucket(0, 10.0, 20.0),
            bucket(20, 30.0, 40.0),
            bucket(40, 50.0, 60.0),
        ];
        let query = VisibleQuery {
            from: 0,
            to: 40,
            bucket: 10,
        };
        let points = points(query, &buckets);
        assert_eq!(points.len(), 5);
        assert_eq!(points[0].ts, 0);
        assert!(points[0].has_data);
        assert_eq!(points[0].cpu, 10.0);
        assert_eq!(points[1].ts, 10);
        assert!(!points[1].has_data);
        assert_eq!(points[1].cpu, 10.0);
        assert_eq!(points[1].mem, 20.0);
        assert_eq!(points[2].ts, 20);
        assert!(points[2].has_data);
        assert_eq!(points[2].cpu, 30.0);
        assert_eq!(points[3].ts, 30);
        assert!(!points[3].has_data);
        assert_eq!(points[3].cpu, 30.0);
        assert_eq!(points[4].ts, 40);
        assert!(points[4].has_data);
        assert_eq!(points[4].cpu, 50.0);
    }

    #[test]
    fn points_are_empty_without_data() {
        let query = VisibleQuery {
            from: 0,
            to: 40,
            bucket: 10,
        };
        assert!(points(query, &[]).is_empty());
    }

    #[test]
    fn data_runs_split_at_padded_slots() {
        let query = VisibleQuery {
            from: 0,
            to: 40,
            bucket: 10,
        };
        let points = points(query, &[bucket(10, 10.0, 20.0), bucket(30, 30.0, 40.0)]);
        assert_eq!(data_runs(&points), vec![1..2, 3..4]);
        assert!(data_runs(&[]).is_empty());
        assert!(data_runs(&points[..1]).is_empty());
    }

    #[test]
    fn live_refresh_animation_requires_a_stable_growing_window() {
        assert!(should_animate_live_refresh(0, 150, 0, 151, false));
        assert!(should_animate_live_refresh(0, 150, 0, 150, true));
        assert!(!should_animate_live_refresh(0, 150, 0, 150, false));
        assert!(!should_animate_live_refresh(0, 150, 60, 151, false));
        assert!(!should_animate_live_refresh(0, 150, 0, 149, true));
        assert!(!should_animate_live_refresh(0, 1, 0, 2, true));
    }

    #[test]
    fn trim_leading_gap_starts_at_the_first_real_bucket() {
        let buckets = vec![bucket(20, 30.0, 40.0), bucket(40, 50.0, 60.0)];
        let mut query = VisibleQuery {
            from: 0,
            to: 40,
            bucket: 10,
        };
        trim_leading_gap(&mut query, &buckets);
        assert_eq!(query.from, 20);
        let points = points(query, &buckets);
        assert_eq!(points.len(), 3);
        assert_eq!(points[0].ts, 20);
        assert!(points[0].has_data);
        assert_eq!(points[1].ts, 30);
        assert!(!points[1].has_data);
        assert_eq!(points[2].ts, 40);
        assert!(points[2].has_data);
    }

    #[test]
    fn axis_labels_are_spread_and_read_clean_local_times() {
        let buckets = vec![bucket(0, 0.0, 0.0)];
        let query = VisibleQuery {
            from: 0,
            to: 3600,
            bucket: 10,
        };
        let labels = axis_labels_with_offset(TimeRange::Hour1, query, &buckets, 6, |_| 0);
        let texts: Vec<&str> = labels.iter().map(|(_, text)| text.as_str()).collect();
        assert_eq!(
            texts,
            ["01-01 00:00", "00:10", "00:20", "00:30", "00:40", "01:00"]
        );
        assert_eq!(labels[0].0, 0.0);
        assert_eq!(labels[5].0, 1.0);
    }

    #[test]
    fn axis_labels_drop_duplicates_after_flooring() {
        let buckets = vec![bucket(0, 0.0, 0.0)];
        let query = VisibleQuery {
            from: 0,
            to: 300,
            bucket: 60,
        };
        let labels = axis_labels_with_offset(TimeRange::Month1, query, &buckets, 6, |_| 0);
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].1, "01-01");
    }

    #[test]
    fn axis_labels_are_empty_without_data_or_a_window() {
        let buckets = vec![bucket(0, 0.0, 0.0)];
        let query = VisibleQuery {
            from: 0,
            to: 3600,
            bucket: 10,
        };
        assert!(axis_labels_with_offset(TimeRange::Hour1, query, &[], 6, |_| 0).is_empty());
        let empty_window = VisibleQuery {
            from: 0,
            to: 0,
            bucket: 10,
        };
        assert!(
            axis_labels_with_offset(TimeRange::Hour1, empty_window, &buckets, 6, |_| 0).is_empty()
        );
    }

    #[test]
    fn axis_labels_use_the_offset_at_each_timestamp() {
        let buckets = vec![bucket(0, 0.0, 0.0)];
        let query = VisibleQuery {
            from: 0,
            to: 600,
            bucket: 2,
        };
        let labels = axis_labels_with_offset(TimeRange::Min5, query, &buckets, 2, |ts| {
            if ts >= 300 {
                3600
            } else {
                0
            }
        });
        let texts: Vec<&str> = labels.iter().map(|(_, text)| text.as_str()).collect();
        assert_eq!(texts, ["01-01 00:00", "01:10"]);
    }

    #[test]
    fn csv_has_bom_tabs_and_only_real_rows() {
        let buckets = vec![bucket(0, 12.34, 56.78), bucket(10, 90.0, 80.0)];
        let csv = build_csv_with_offset(&buckets, Language::En, |_| 0);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "\u{FEFF}timestamp\tcpu_pct\tmem_pct");
        assert_eq!(lines[1], "1970-01-01 00:00:00\t12.3\t56.8");
        assert_eq!(lines[2], "1970-01-01 00:00:10\t90.0\t80.0");
    }

    #[test]
    fn fixed_offsets_handle_negative_utc_offsets() {
        assert_eq!(fmt_time_at(0, -5 * 3600), "19:00");
        assert_eq!(fmt_date_at(0, -5 * 3600), "12-31");
        assert_eq!(fmt_date_time_at(0, -5 * 3600), "12-31 19:00");
        assert_eq!(day_of_at(0, -5 * 3600), "1969-12-31");
        assert_eq!(format_ts_full_at(0, -5 * 3600), "1969-12-31 19:00:00");
    }

    #[test]
    fn local_offset_is_a_plausible_utc_offset() {
        let offset = local_offset_at(1_700_000_000);
        assert!(offset.abs() <= 14 * 3600);
    }
}
