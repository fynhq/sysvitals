//! Core value types shared across layers.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Metric {
    Cpu,
    Mem,
    Up,
    Down,
}

impl Metric {
    pub const ALL: [Metric; 4] = [Metric::Cpu, Metric::Mem, Metric::Up, Metric::Down];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Anchor {
    #[default]
    Right,
    Left,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Auto,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Snapshot {
    pub ts: i64,
    pub cpu_pct: f64,
    pub mem_pct: f64,
    pub up_bps: f64,
    pub down_bps: f64,
}

impl Snapshot {
    pub fn value(self, metric: Metric) -> f64 {
        match metric {
            Metric::Cpu => self.cpu_pct,
            Metric::Mem => self.mem_pct,
            Metric::Up => self.up_bps,
            Metric::Down => self.down_bps,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_value_routes_by_metric() {
        let s = Snapshot {
            ts: 0,
            cpu_pct: 12.0,
            mem_pct: 34.0,
            up_bps: 5.0,
            down_bps: 6.0,
        };
        assert_eq!(s.value(Metric::Cpu), 12.0);
        assert_eq!(s.value(Metric::Mem), 34.0);
        assert_eq!(s.value(Metric::Up), 5.0);
        assert_eq!(s.value(Metric::Down), 6.0);
    }

    #[test]
    fn anchor_and_theme_defaults() {
        assert_eq!(Anchor::default(), Anchor::Right);
        assert_eq!(Theme::default(), Theme::Auto);
    }

    #[test]
    fn serde_roundtrip_lowercase() {
        let a: Anchor = serde_json_free(r#" "left" "#);
        assert_eq!(a, Anchor::Left);
        let t: Theme = serde_json_free(r#" "dark" "#);
        assert_eq!(t, Theme::Dark);
        let m: Metric = serde_json_free(r#" "up" "#);
        assert_eq!(m, Metric::Up);
    }

    fn serde_json_free<T: serde::de::DeserializeOwned>(s: &str) -> T {
        let toml_str = format!("v = {}", s.trim());
        let de: Wrap<T> = toml::from_str(&toml_str).unwrap();
        de.v
    }

    #[derive(Deserialize)]
    struct Wrap<T> {
        v: T,
    }
}
