//! chart: split out of the crate root (see `main.rs` for the module map).

use super::*;

/// Time range options for the chart.
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
            TimeRange::Day1 => 86400,
            TimeRange::Month1 => 2_592_000,
        }
    }

    /// Fine bucket width shared by the chart curve and the CSV export:
    /// detailed enough to preserve resolution at each range while staying
    /// bounded (a month is ~720 rows, not 1.3M). The curve draws one point
    /// per fine bucket; only the sparse axis *labels* use the coarser clean
    /// step from [`Self::label_step_secs`].
    pub fn bucket_secs(self) -> i64 {
        match self {
            TimeRange::Min5 => 2,
            TimeRange::Hour1 => 10,
            TimeRange::Day1 => 120,
            TimeRange::Month1 => 3600,
        }
    }

    /// The clean wall-clock step the x-axis labels are floored to — the
    /// range picks the step so every label reads as a round local time:
    ///   5 min    = 60 s       (every local minute)
    ///   1 hour   = 600 s      (every :10 / :20 / ... local ten-minute mark)
    ///   1 day    = 7 200 s    (every 2 h at 00/02/.../22 local time)
    ///   1 month  = 432 000 s  (every 5 days at local midnight)
    /// Every grid is offset-aligned to the machine's local time zone; steps
    /// like 7200 s do not divide every real UTC offset (e.g. +5:30), so a
    /// zero-offset grid would read :30 instead of the intended marks.
    /// The curve is NOT drawn at this step: it uses [`Self::bucket_secs`] so
    /// the line keeps its detail. Each label point floors its own timestamp
    /// to this grid, so the six spread ticks read as clean times while the
    /// line stays fine-grained. The query window start is aligned down to
    /// this grid (see [`floor_to_grid`]), so the first label dates the window
    /// start.
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

    /// Whether the open popup should reload this range every few seconds.
    /// 5 m and 1 h read a small tail of the raw table and stay live; 1 d and
    /// 1 mo scan the whole retention window (the month aggregate is seconds
    /// at the 30-day cap), so they load once on open/range switch and stay
    /// static while open. Clicking the active range button reloads it.
    pub fn auto_refresh(self) -> bool {
        matches!(self, TimeRange::Min5 | TimeRange::Hour1)
    }
}

/// One dense chart slot. The x axis is an index axis, so the renderer must
/// emit one point per bucket in the visible window; otherwise missing
/// intervals compress time. `has_data` is false for padded slots (app not
/// running, sleep, history disabled); their values repeat the previous real
/// point so the path stays finite, and the tooltip reports `--`.
#[derive(Debug, Clone)]
pub struct ChartPoint {
    pub ts: i64,
    pub cpu: f64,
    pub mem: f64,
    pub has_data: bool,
}

/// The exact query window shared by the chart, the axis labels and the
/// `Visible` CSV export.
#[derive(Debug, Clone, Copy)]
pub struct VisibleQuery {
    pub from: i64,
    pub to: i64,
    pub bucket: i64,
}

/// How often a visible history window reloads the stored samples. The
/// indicator writes samples every sample interval, so redrawing every few
/// seconds makes the chart follow the indicator in near real time.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(3);

/// Bucket width at or above which a query runs off the UI thread. The 1 d
/// view (bucket 120 s) and 1 mo view (3600 s) scan the whole retention
/// window; in the raw-only fallback (a failed rollup backfill) that is the
/// full raw table. Narrower views read a few hundred rows and stay on the
/// in-view connection.
pub const ASYNC_MIN_BUCKET_SECS: i64 = 120;

/// Shared-state poll cadence. A visible popup still polls tighter than an idle
/// resident so the indicator's close request feels immediate; the no-window
/// loop only needs to notice a click and the parent's exit, so it wakes 4
/// times per second instead of 10. Only Windows has the resident loop.
#[cfg(windows)]
pub const ACTIVE_POLL_MS: u64 = 100;
#[cfg(windows)]
pub const IDLE_POLL_MS: u64 = 250;

/// How many x-axis tick labels the panel draws, spread evenly across the
/// visible time window (not across the data points, so gaps do not move
/// them).
pub const X_TICK_COUNT: usize = 6;

/// Axis label size. The chart library paints its tick labels at a fixed
/// 10 px (`gpui_base::plot::label::TEXT_SIZE`) with no public size knob, so
/// this panel asks the chart for no labels (`x_tick_count(0)` and empty
/// `y_tick_format` keep both axis lines and their space) and paints them
/// itself at this size.
pub const AXIS_LABEL_SIZE: f32 = 12.0;

/// The strip the chart reserves under its axis line for x labels: the
/// library's 10 px label size plus four 2 px gaps (`AXIS_GAP`).
pub const X_AXIS_STRIP_H: f32 = 18.0;

/// The value-axis gutter the chart keeps left of the plot (`VALUE_AXIS_GAP`,
/// never narrower than 32 px for the 0..=100 labels at its 10 px font). The
/// x scale starts this far inside the chart and runs to its right edge, so
/// the custom labels use the same origin to stay under their data points.
pub const X_AXIS_GUTTER: f32 = 32.0;

/// Y-axis domain and tick count. The library's y labels are suppressed the
/// same way as the x labels (empty tick text keeps the gutter and axis
/// line), so the overlay below reproduces these ticks at the same size as
/// the x labels.
pub const Y_AXIS_MIN: f64 = 0.0;
pub const Y_AXIS_MAX: f64 = 100.0;
pub const Y_TICK_COUNT: usize = 6;

/// Chart stroke colors shared by the series, their fills, and the legend.
pub const CPU_HUE: f32 = 210.0;
pub const MEM_HUE: f32 = 150.0;

pub fn cpu_color() -> Hsla {
    hsla(CPU_HUE / 360.0, 0.8, 0.55, 1.0)
}

pub fn mem_color() -> Hsla {
    hsla(MEM_HUE / 360.0, 0.7, 0.45, 1.0)
}

/// A fill that is invisible: the merged chart draws two zero-alpha areas,
/// which is exactly two lines with no shading.
pub fn transparent_color() -> Hsla {
    hsla(0.0, 0.0, 0.0, 0.0)
}

/// Local UTC offset (seconds east) at a specific timestamp. Unlike
/// `Local::now().offset()`, this stays correct on the old side of a DST
/// transition when formatting labels.
pub fn local_offset_at(ts: i64) -> i64 {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&Local).offset().local_minus_utc() as i64)
        .unwrap_or(0)
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

/// Format a unix timestamp as HH:MM in the machine's local time zone.
pub fn fmt_time(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&Local).format("%H:%M").to_string())
        .unwrap_or_default()
}

/// Format a unix timestamp as MM-DD, the terse date label for the month
/// view's x axis (year is redundant inside a 30-day window).
pub fn fmt_date(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&Local).format("%m-%d").to_string())
        .unwrap_or_default()
}

/// Format a unix timestamp as `MM-DD HH:MM`, used when a shorter view's x
/// axis crosses midnight and the tick that opens a new day must show both.
pub fn fmt_date_time(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&Local).format("%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// Local calendar date as a stable comparison key (`YYYY-MM-DD`). Two
/// timestamps match only if their wall-clock date in the user's zone
/// matches, which is the exact "day crossed" test the x-axis labels need.
/// A string key is used rather than an integer day count because it relies
/// on only the `%Y-%m-%d` format directive, avoiding a version-dependent
/// helper on `NaiveDate`.
pub fn day_of(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&Local).format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// Format a unix timestamp as YYYY-MM-DD HH:MM:SS in the machine's local
/// time zone. Used by both the CSV export and the hover tooltip so exported
/// rows and the point readout agree.
pub fn format_ts_full(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}
