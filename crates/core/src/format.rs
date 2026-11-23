//! Pure percentage/rate formatting and layout strings.

use crate::config::Config;
use crate::model::{Metric, Snapshot};

pub const KB: f64 = 1024.0;
pub const MB: f64 = 1024.0 * KB;
pub const GB: f64 = 1024.0 * MB;

pub const PERCENT_RESERVE: &str = "100%";

pub const RATE_RESERVE: &str = "8888 M/s";

pub fn format_percent(value: f64) -> String {
    if !value.is_finite() {
        return "--%".to_string();
    }
    let v = value.clamp(0.0, 100.0).round();
    format!("{v:.0}%")
}

// Rate field is fixed width: 4 chars, adapt unit/decimals so rounding to 10.00/100.0 never widens the slot.
fn format_rate_with(bps: f64, with_per_sec: bool) -> String {
    if !bps.is_finite() {
        return "--".to_string();
    }
    let bps = bps.max(0.0);
    let (scaled, letter) = if bps < KB {
        (bps, "B")
    } else if bps < MB {
        (bps / KB, "K")
    } else if bps < GB {
        (bps / MB, "M")
    } else {
        (bps / GB, "G")
    };
    let mut decimals = if scaled >= 100.0 {
        0
    } else if scaled >= 10.0 {
        1
    } else {
        2
    };
    let mut value = format!("{scaled:.decimals$}");
    while decimals > 0 && value.len() > 4 {
        decimals -= 1;
        value = format!("{scaled:.decimals$}");
    }
    if with_per_sec {
        format!("{value} {letter}/s")
    } else {
        format!("{value} {letter}")
    }
}

pub fn join_prefix(prefix: &str, value: &str) -> String {
    if prefix.is_empty() {
        value.to_string()
    } else {
        format!("{prefix} {value}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub left: String,
    pub right: String,
}

impl Line {
    pub fn single(text: impl Into<String>) -> Self {
        Line {
            left: text.into(),
            right: String::new(),
        }
    }

    pub fn render(&self) -> String {
        if self.right.is_empty() {
            self.left.clone()
        } else {
            format!("{}  {}", self.left, self.right)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormattedRow {
    pub lines: Vec<Line>,
    pub items: Vec<String>,
}

pub fn metric_prefix(metric: Metric, cfg: &Config) -> &str {
    match metric {
        Metric::Cpu => &cfg.display.prefix.cpu,
        Metric::Mem => &cfg.display.prefix.mem,
        Metric::Up => &cfg.display.prefix.up,
        Metric::Down => &cfg.display.prefix.down,
    }
}

pub fn metric_visible(metric: Metric, cfg: &Config) -> bool {
    cfg.display.metric_visible(metric)
}

pub fn visible_metrics(cfg: &Config) -> Vec<Metric> {
    Metric::ALL
        .into_iter()
        .filter(|metric| metric_visible(*metric, cfg))
        .collect()
}

pub fn column_metrics(cfg: &Config) -> (Vec<Metric>, Vec<Metric>) {
    let resources = [Metric::Cpu, Metric::Mem];
    let network = [Metric::Up, Metric::Down];
    let show_resources = cfg.display.show_resources;
    let show_network = cfg.display.show_network;
    if show_resources && show_network {
        (resources.to_vec(), network.to_vec())
    } else if show_resources {
        (resources.to_vec(), Vec::new())
    } else if show_network {
        (network.to_vec(), Vec::new())
    } else {
        (Vec::new(), Vec::new())
    }
}

pub fn format_value(metric: Metric, snapshot: &Snapshot) -> String {
    format_value_impl(metric, snapshot, true)
}

pub fn format_value_compact(metric: Metric, snapshot: &Snapshot) -> String {
    format_value_impl(metric, snapshot, false)
}

fn format_value_impl(metric: Metric, snapshot: &Snapshot, with_per_sec: bool) -> String {
    let value = snapshot.value(metric);
    match metric {
        Metric::Cpu | Metric::Mem => format_percent(value),
        Metric::Up | Metric::Down => format_rate_with(value, with_per_sec),
    }
}

fn metric_cells(metrics: Vec<Metric>, cfg: &Config, snapshot: &Snapshot) -> Vec<(Metric, String)> {
    metrics
        .into_iter()
        .map(|metric| {
            (
                metric,
                join_prefix(metric_prefix(metric, cfg), &format_value(metric, snapshot)),
            )
        })
        .collect()
}

pub fn format_frame(snapshot: &Snapshot, cfg: &Config) -> FormattedRow {
    if cfg.display.dual_row {
        let (left_metrics, right_metrics) = column_metrics(cfg);
        let left = metric_cells(left_metrics, cfg, snapshot);
        let right = metric_cells(right_metrics, cfg, snapshot);

        if left.is_empty() || right.is_empty() {
            let cells = if left.is_empty() { &right } else { &left };
            return FormattedRow {
                lines: vec![
                    Line::single(cell_at(cells, 0)),
                    Line::single(cell_at(cells, 1)),
                ],
                items: Vec::new(),
            };
        }

        FormattedRow {
            lines: vec![
                Line {
                    left: cell_at(&left, 0),
                    right: cell_at(&right, 0),
                },
                Line {
                    left: cell_at(&left, 1),
                    right: cell_at(&right, 1),
                },
            ],
            items: Vec::new(),
        }
    } else {
        let enabled = visible_metrics(cfg);
        let items: Vec<String> = enabled
            .iter()
            .map(|m| join_prefix(metric_prefix(*m, cfg), &format_value(*m, snapshot)))
            .collect();
        let text = items.join("  ");
        FormattedRow {
            lines: vec![Line::single(text)],
            items,
        }
    }
}

fn cell_at(cells: &[(Metric, String)], idx: usize) -> String {
    cells.get(idx).map(|(_, s)| s.clone()).unwrap_or_default()
}

pub fn format_frame_vertical(snapshot: &Snapshot, cfg: &Config) -> FormattedRow {
    let enabled = visible_metrics(cfg);
    let mut lines: Vec<Line> = Vec::with_capacity(enabled.len() * 2);
    for (i, m) in enabled.iter().enumerate() {
        if i > 0 {
            lines.push(Line::single(""));
        }
        let prefix = metric_prefix(*m, cfg);
        if !prefix.is_empty() {
            lines.push(Line::single(prefix.to_string()));
        }
        lines.push(Line::single(format_value_compact(*m, snapshot)));
    }
    FormattedRow {
        lines,
        items: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Snapshot;

    fn snap(cpu: f64, mem: f64, up: f64, down: f64) -> Snapshot {
        Snapshot {
            ts: 0,
            cpu_pct: cpu,
            mem_pct: mem,
            up_bps: up,
            down_bps: down,
        }
    }

    #[test]
    fn percent_clamps_and_rounds() {
        assert_eq!(format_percent(29.4), "29%");
        assert_eq!(format_percent(29.6), "30%");
        assert_eq!(format_percent(-3.0), "0%");
        assert_eq!(format_percent(140.0), "100%");
        assert_eq!(format_percent(f64::NAN), "--%");
        assert_eq!(format_percent(f64::INFINITY), "--%");
    }

    #[test]
    fn rate_uses_binary_units_and_decimal_rules() {
        assert_eq!(format_rate_with(314.0, true), "314 B/s");
        assert_eq!(format_rate_with(0.0, true), "0.00 B/s");
        assert_eq!(format_rate_with(1023.0, true), "1023 B/s");
        assert_eq!(format_rate_with(1024.0, true), "1.00 K/s");
        assert_eq!(format_rate_with(314.0 * KB, true), "314 K/s");
        assert_eq!(format_rate_with(12.0 * KB + 512.0, true), "12.5 K/s");
        assert_eq!(format_rate_with(1.2 * MB, true), "1.20 M/s");
        assert_eq!(format_rate_with(100.0 * MB, true), "100 M/s");
        assert_eq!(format_rate_with(1.5 * GB, true), "1.50 G/s");
        assert_eq!(format_rate_with(1024.0 * MB, true), "1.00 G/s");
        assert_eq!(format_rate_with(120.0 * GB, true), "120 G/s");
        assert_eq!(format_rate_with(f64::NAN, true), "--");
        assert_eq!(format_rate_with(f64::INFINITY, true), "--");
        assert_eq!(format_rate_with(f64::NAN, false), "--");
    }

    #[test]
    fn rate_numeric_field_never_exceeds_four_chars() {
        for bps in [
            9.996 * KB,
            99.96 * KB,
            999.6 * KB,
            1023.9 * KB,
            9.996 * MB,
            99.96 * MB,
            999.6 * MB,
            1023.9 * MB,
            1023.9 * GB,
        ] {
            let rendered = format_rate_with(bps, true);
            let numeric = rendered.split(' ').next().unwrap_or_default();
            assert!(
                numeric.len() <= 4,
                "{rendered} has a {} character numeric field",
                numeric.len()
            );
            let compact = format_rate_with(bps, false);
            let numeric = compact.split(' ').next().unwrap_or_default();
            assert!(numeric.len() <= 4, "{compact}");
        }
        assert_eq!(format_rate_with(9.996 * KB, true), "10.0 K/s");
        assert_eq!(format_rate_with(99.96 * KB, true), "100 K/s");
        assert_eq!(format_rate_with(1023.9 * KB, true), "1024 K/s");
    }

    #[test]
    fn rate_nan_is_no_reading_and_negative_is_zero() {
        assert_eq!(format_rate_with(f64::NAN, true), "--");
        assert_eq!(format_rate_with(f64::NEG_INFINITY, true), "--");
        assert_eq!(format_rate_with(-5.0, true), "0.00 B/s");
        assert_eq!(format_rate_with(f64::INFINITY, true), "--");
    }

    #[test]
    fn rate_compact_drops_per_sec_suffix() {
        assert_eq!(format_rate_with(314.0, false), "314 B");
        assert_eq!(format_rate_with(0.0, false), "0.00 B");
        assert_eq!(format_rate_with(1024.0, false), "1.00 K");
        assert_eq!(format_rate_with(12.0 * KB + 512.0, false), "12.5 K");
        assert_eq!(format_rate_with(1.2 * MB, false), "1.20 M");
        assert_eq!(format_rate_with(1.5 * GB, false), "1.50 G");
        assert_eq!(format_rate_with(f64::NAN, false), "--");
    }

    #[test]
    fn prefix_join_inserts_one_space() {
        assert_eq!(join_prefix("CPU", "29%"), "CPU 29%");
        assert_eq!(join_prefix("", "29%"), "29%");
    }

    #[test]
    fn single_row_layout() {
        let mut cfg = Config::default();
        cfg.display.dual_row = false;
        let f = format_frame(&snap(29.0, 68.0, 1.2 * MB, 314.0 * KB), &cfg);
        assert_eq!(f.lines.len(), 1);
        assert_eq!(
            f.lines[0].render(),
            "CPU: 29%  MEM: 68%  U: 1.20 M/s  D: 314 K/s"
        );
    }

    #[test]
    fn single_row_exposes_items_for_pixel_spacing() {
        let mut cfg = Config::default();
        cfg.display.dual_row = false;
        let f = format_frame(&snap(29.0, 68.0, 1024.0, 2048.0), &cfg);
        assert_eq!(
            f.items,
            vec!["CPU: 29%", "MEM: 68%", "U: 1.00 K/s", "D: 2.00 K/s"]
        );
        let mut cfg2 = Config::default();
        cfg2.display.dual_row = false;
        cfg2.display.item_spacing = 24;
        let f2 = format_frame(&snap(29.0, 68.0, 1024.0, 2048.0), &cfg2);
        assert_eq!(f2.items, f.items);
        assert_eq!(f2.lines[0].render(), f.lines[0].render());
    }

    #[test]
    fn dual_row_has_no_items() {
        let mut cfg = Config::default();
        cfg.display.dual_row = true;
        let f = format_frame(&snap(29.0, 68.0, 1024.0, 2048.0), &cfg);
        assert!(f.items.is_empty());
        assert_eq!(f.lines.len(), 2);
    }

    #[test]
    fn paired_toggles_hide_groups() {
        let mut cfg = Config::default();
        cfg.display.dual_row = false;
        cfg.display.show_resources = false;
        let f = format_frame(&snap(29.0, 68.0, 1024.0, 2048.0), &cfg);
        let text = f.lines[0].render();
        assert!(!text.contains("CPU") && !text.contains("MEM"));
        assert!(text.starts_with("U: "));

        let mut cfg2 = Config::default();
        cfg2.display.dual_row = false;
        cfg2.display.show_network = false;
        let f2 = format_frame(&snap(29.0, 68.0, 1024.0, 2048.0), &cfg2);
        assert_eq!(f2.lines[0].render(), "CPU: 29%  MEM: 68%");
    }

    #[test]
    fn dual_row_exposes_unpadded_cells_for_pixel_alignment() {
        let mut cfg = Config::default();
        cfg.display.dual_row = true;
        let f = format_frame(&snap(29.0, 68.0, 1.2 * MB, 314.0 * KB), &cfg);
        assert_eq!(f.lines.len(), 2);
        assert_eq!(f.lines[0].left, "CPU: 29%");
        assert_eq!(f.lines[1].left, "MEM: 68%");
        assert_eq!(f.lines[0].right, "U: 1.20 M/s");
        assert_eq!(f.lines[1].right, "D: 314 K/s");
        assert!(
            f.lines
                .iter()
                .all(|line| !line.left.ends_with(' ') && !line.right.ends_with(' ')),
            "column alignment belongs to the renderer, not to the formatter"
        );
    }

    #[test]
    fn nan_values_show_placeholder() {
        let mut cfg = Config::default();
        cfg.display.dual_row = false;
        let f = format_frame(&snap(f64::NAN, f64::NAN, f64::NAN, f64::NAN), &cfg);
        assert_eq!(f.lines[0].render(), "CPU: --%  MEM: --%  U: --  D: --");
        let mut c2 = Config::default();
        c2.display.dual_row = false;
        c2.display.show_network = false;
        let f2 = format_frame(&snap(f64::NAN, f64::NAN, f64::NAN, f64::NAN), &c2);
        assert_eq!(f2.lines[0].render(), "CPU: --%  MEM: --%");
    }

    #[test]
    fn dual_row_keeps_two_lines_when_a_column_empty() {
        let mut cfg = Config::default();
        cfg.display.dual_row = true;
        cfg.display.show_network = false;
        let f = format_frame(&snap(29.0, 68.0, 0.0, 0.0), &cfg);
        assert_eq!(f.lines.len(), 2);
        assert_eq!(f.lines[0].render(), "CPU: 29%");
        assert_eq!(f.lines[1].render(), "MEM: 68%");
    }

    #[test]
    fn custom_and_empty_prefix() {
        let mut cfg = Config::default();
        cfg.display.dual_row = false;
        cfg.display.prefix.cpu = "".to_string();
        cfg.display.prefix.up = "↑".to_string();
        let f = format_frame(&snap(29.0, 68.0, 1024.0, 0.0), &cfg);
        assert_eq!(
            f.lines[0].render(),
            "29%  MEM: 68%  ↑ 1.00 K/s  D: 0.00 B/s"
        );
    }

    #[test]
    fn vertical_layout_stacks_label_and_value_per_metric() {
        let cfg = Config::default();
        let f = format_frame_vertical(&snap(29.0, 68.0, 1.2 * MB, 314.0 * KB), &cfg);
        let texts: Vec<&str> = f.lines.iter().map(|l| l.left.as_str()).collect();
        assert_eq!(
            texts,
            vec!["CPU:", "29%", "", "MEM:", "68%", "", "U:", "1.20 M", "", "D:", "314 K"]
        );
        assert!(f.items.is_empty());
    }

    #[test]
    fn vertical_layout_keeps_widest_line_below_joined_width() {
        let cfg = Config::default();
        let f = format_frame_vertical(&snap(90.0, 60.0, 1.56 * MB, 100.0 * KB), &cfg);
        for l in &f.lines {
            assert!(
                l.left.chars().count() <= "1.56 M".chars().count(),
                "line {:?} exceeds the compact value-only width",
                l.left
            );
        }
    }

    #[test]
    fn column_metrics_matches_the_paired_toggle_matrix() {
        let mut cfg = Config::default();
        assert_eq!(
            column_metrics(&cfg),
            (
                vec![Metric::Cpu, Metric::Mem],
                vec![Metric::Up, Metric::Down]
            )
        );
        cfg.display.show_network = false;
        assert_eq!(
            column_metrics(&cfg),
            (vec![Metric::Cpu, Metric::Mem], Vec::new())
        );
        let mut cfg = Config::default();
        cfg.display.show_resources = false;
        assert_eq!(
            column_metrics(&cfg),
            (vec![Metric::Up, Metric::Down], Vec::new())
        );
        let mut cfg = Config::default();
        cfg.display.show_resources = false;
        cfg.display.show_network = false;
        assert_eq!(column_metrics(&cfg), (Vec::new(), Vec::new()));
    }

    #[test]
    fn visible_metrics_and_prefixes_match_the_formatter() {
        let mut cfg = Config::default();
        assert_eq!(
            visible_metrics(&cfg),
            vec![Metric::Cpu, Metric::Mem, Metric::Up, Metric::Down]
        );
        cfg.display.show_resources = false;
        assert_eq!(visible_metrics(&cfg), vec![Metric::Up, Metric::Down]);
        assert_eq!(metric_prefix(Metric::Cpu, &cfg), "CPU:");
        assert_eq!(metric_prefix(Metric::Down, &cfg), "D:");
    }

    #[test]
    fn fixed_slot_reserve_strings_cover_the_formatter() {
        assert_eq!(format_percent(100.0), PERCENT_RESERVE);
        assert_eq!(format_rate_with(8888.0 * MB, true), "8.68 G/s");
        let numeric = RATE_RESERVE.split(' ').next().unwrap();
        assert_eq!(numeric.len(), 4);
        assert!(RATE_RESERVE.split(' ').nth(1).unwrap().len() >= 3);
    }

    #[test]
    fn vertical_layout_respects_toggles() {
        let mut cfg = Config::default();
        cfg.display.show_network = false;
        let f = format_frame_vertical(&snap(29.0, 68.0, 1024.0, 2048.0), &cfg);
        let texts: Vec<&str> = f.lines.iter().map(|l| l.left.as_str()).collect();
        assert_eq!(texts, vec!["CPU:", "29%", "", "MEM:", "68%"]);
    }

    #[test]
    fn vertical_layout_empty_prefix_shows_value_only() {
        let mut cfg = Config::default();
        cfg.display.prefix.cpu = "".to_string();
        let f = format_frame_vertical(&snap(29.0, 68.0, 1024.0, 2048.0), &cfg);
        let texts: Vec<&str> = f.lines.iter().map(|l| l.left.as_str()).collect();
        assert_eq!(
            texts,
            vec!["29%", "", "MEM:", "68%", "", "U:", "1.00 K", "", "D:", "2.00 K"]
        );
    }

    #[test]
    fn format_value_compact_matches_vertical_frame_values() {
        let s = snap(37.0, 61.0, 2.0 * KB, 3.0 * MB);
        assert_eq!(format_value_compact(Metric::Cpu, &s), "37%");
        assert_eq!(format_value_compact(Metric::Mem, &s), "61%");
        assert_eq!(format_value_compact(Metric::Up, &s), "2.00 K");
        assert_eq!(format_value_compact(Metric::Down, &s), "3.00 M");
        assert_eq!(format_value(Metric::Up, &s), "2.00 K/s");
        assert_eq!(format_value(Metric::Down, &s), "3.00 M/s");
        let nan = snap(f64::NAN, f64::NAN, f64::NAN, f64::NAN);
        assert_eq!(format_value_compact(Metric::Up, &nan), "--");
        assert_eq!(format_value_compact(Metric::Cpu, &nan), "--%");
    }
}
