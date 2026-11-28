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

pub(crate) const X_TICK_COUNT: usize = 6;

pub(crate) const AXIS_LABEL_SIZE: f32 = 12.0;

pub(crate) const X_AXIS_STRIP_H: f32 = 18.0;

pub(crate) const X_AXIS_GUTTER: f32 = 32.0;

pub(crate) const Y_AXIS_MIN: f64 = 0.0;
pub(crate) const Y_AXIS_MAX: f64 = 100.0;
pub(crate) const Y_TICK_COUNT: usize = 6;

pub(crate) const CPU_HUE: f32 = 210.0;
pub(crate) const MEM_HUE: f32 = 150.0;

// Light-mode strokes keep >=3:1 non-text contrast; dark mode keeps the brighter values.
pub(crate) fn cpu_color(dark: bool) -> Hsla {
    hsla(CPU_HUE / 360.0, 0.8, if dark { 0.55 } else { 0.45 }, 1.0)
}

pub(crate) fn mem_color(dark: bool) -> Hsla {
    hsla(MEM_HUE / 360.0, 0.7, if dark { 0.45 } else { 0.32 }, 1.0)
}

pub(crate) fn transparent_color() -> Hsla {
    hsla(0.0, 0.0, 0.0, 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_mode_strokes_are_darker_than_dark_mode() {
        assert!(cpu_color(false).l < cpu_color(true).l);
        assert!(mem_color(false).l < mem_color(true).l);
    }
}
