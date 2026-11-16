//! History chart constants, palette, and display tuning.

use std::time::Duration;

use gpui_kit::{hsla, Hsla};

pub(crate) use sysvitals_core::chart::{
    axis_labels, build_csv, data_runs, fmt_date_time, format_ts_full, local_offset_at, points,
    query_buckets, should_animate_live_refresh, trim_leading_gap, ChartPoint, TimeRange,
    VisibleQuery,
};

pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(3);

pub(crate) const SERIES_DRAW: Duration = Duration::from_millis(360);

pub(crate) const SERIES_REFRESH: Duration = Duration::from_millis(280);

pub(crate) const ASYNC_MIN_BUCKET_SECS: i64 = 120;

#[cfg(windows)]
pub(crate) const ACTIVE_POLL_MS: u64 = 100;
#[cfg(windows)]
pub(crate) const IDLE_POLL_MS: u64 = 250;

pub(crate) const X_TICK_COUNT: usize = 6;

pub(crate) const AXIS_LABEL_SIZE: f32 = 12.0;

pub(crate) const X_AXIS_STRIP_H: f32 = 18.0;

pub(crate) const X_AXIS_GUTTER: f32 = 32.0;

pub(crate) const Y_AXIS_MIN: f64 = 0.0;
pub(crate) const Y_AXIS_MAX: f64 = 100.0;
pub(crate) const Y_TICK_COUNT: usize = 6;

pub(crate) const CPU_HUE: f32 = 210.0;
pub(crate) const MEM_HUE: f32 = 150.0;

pub(crate) fn cpu_color() -> Hsla {
    hsla(CPU_HUE / 360.0, 0.8, 0.55, 1.0)
}

pub(crate) fn mem_color() -> Hsla {
    hsla(MEM_HUE / 360.0, 0.7, 0.45, 1.0)
}

pub(crate) fn transparent_color() -> Hsla {
    hsla(0.0, 0.0, 0.0, 0.0)
}
