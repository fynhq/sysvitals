//! chart: UI-only chart constants and palette.
//!
//! The pure data/time logic (ranges, query windows, bucket padding, axis
//! labels, CSV text) lives in `sysvitals_core::chart` so the Linux quality
//! gate can test it; this module keeps the GPUI-facing layout constants and
//! colors, and re-exports the core items the rest of this binary uses.

use std::time::Duration;

use gpui_kit::{hsla, Hsla};

pub(crate) use sysvitals_core::chart::{
    axis_labels, build_csv, data_runs, fmt_date_time, format_ts_full, local_offset_at, points,
    query_buckets, trim_leading_gap, ChartPoint, TimeRange, VisibleQuery,
};

/// How often a visible history window reloads the stored samples. The
/// indicator writes samples every sample interval, so redrawing every few
/// seconds makes the chart follow the indicator in near real time.
pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(3);

/// Bucket width at or above which a query runs off the UI thread. The 1 d
/// view (bucket 120 s) and 1 mo view (3600 s) scan the whole retention
/// window; in the raw-only fallback (a failed rollup backfill) that is the
/// full raw table. Narrower views read a few hundred rows and stay on the
/// in-view connection.
pub(crate) const ASYNC_MIN_BUCKET_SECS: i64 = 120;

/// Shared-state poll cadence. A visible popup still polls tighter than an idle
/// resident so the indicator's close request feels immediate; the no-window
/// loop only needs to notice a click and the parent's exit, so it wakes 4
/// times per second instead of 10. Only Windows has the resident loop.
#[cfg(windows)]
pub(crate) const ACTIVE_POLL_MS: u64 = 100;
#[cfg(windows)]
pub(crate) const IDLE_POLL_MS: u64 = 250;

/// How many x-axis tick labels the panel draws, spread evenly across the
/// visible time window (not across the data points, so gaps do not move
/// them).
pub(crate) const X_TICK_COUNT: usize = 6;

/// Axis label size. The chart library paints its tick labels at a fixed
/// 10 px (`gpui_base::plot::label::TEXT_SIZE`) with no public size knob, so
/// this panel asks the chart for no labels (`x_tick_count(0)` and empty
/// `y_tick_format` keep both axis lines and their space) and paints them
/// itself at this size.
pub(crate) const AXIS_LABEL_SIZE: f32 = 12.0;

/// The strip the chart reserves under its axis line for x labels: the
/// library's 10 px label size plus four 2 px gaps (`AXIS_GAP`).
pub(crate) const X_AXIS_STRIP_H: f32 = 18.0;

/// The value-axis gutter the chart keeps left of the plot (`VALUE_AXIS_GAP`,
/// never narrower than 32 px for the 0..=100 labels at its 10 px font). The
/// x scale starts this far inside the chart and runs to its right edge, so
/// the custom labels use the same origin to stay under their data points.
pub(crate) const X_AXIS_GUTTER: f32 = 32.0;

/// Y-axis domain and tick count. The library's y labels are suppressed the
/// same way as the x labels (empty tick text keeps the gutter and axis
/// line), so the overlay below reproduces these ticks at the same size as
/// the x labels.
pub(crate) const Y_AXIS_MIN: f64 = 0.0;
pub(crate) const Y_AXIS_MAX: f64 = 100.0;
pub(crate) const Y_TICK_COUNT: usize = 6;

/// Chart stroke colors shared by the series, their fills, and the legend.
pub(crate) const CPU_HUE: f32 = 210.0;
pub(crate) const MEM_HUE: f32 = 150.0;

pub(crate) fn cpu_color() -> Hsla {
    hsla(CPU_HUE / 360.0, 0.8, 0.55, 1.0)
}

pub(crate) fn mem_color() -> Hsla {
    hsla(MEM_HUE / 360.0, 0.7, 0.45, 1.0)
}

/// A fill that is invisible: the merged chart draws two zero-alpha areas,
/// which is exactly two lines with no shading.
pub(crate) fn transparent_color() -> Hsla {
    hsla(0.0, 0.0, 0.0, 0.0)
}
