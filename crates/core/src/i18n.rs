//! Static English/Chinese UI strings.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    En,
    Zh,
}

impl Language {
    pub const ALL: [Language; 2] = [Language::En, Language::Zh];

    pub fn label(self) -> &'static str {
        match self {
            Language::En => "English",
            Language::Zh => "中文",
        }
    }

    fn strings(self) -> &'static HashMap<&'static str, &'static str> {
        static EN: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
        static ZH: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
        match self {
            Language::En => EN.get_or_init(|| parse(include_str!("../locales/en.conf"))),
            Language::Zh => ZH.get_or_init(|| parse(include_str!("../locales/zh.conf"))),
        }
    }
}

pub fn t(lang: Language, key: &str) -> &str {
    lang.strings().get(key).copied().unwrap_or(key)
}

fn parse(source: &'static str) -> HashMap<&'static str, &'static str> {
    entries(source).collect()
}

fn entries(source: &'static str) -> impl Iterator<Item = (&'static str, &'static str)> {
    source.lines().filter_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let (key, value) = line.split_once('=')?;
        Some((key.trim(), value.trim()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EN: &str = include_str!("../locales/en.conf");

    #[test]
    fn known_keys_differ_between_locales() {
        assert_ne!(
            t(Language::En, "settings.title"),
            t(Language::Zh, "settings.title")
        );
        assert_eq!(t(Language::En, "settings.title"), "SysVitals Settings");
        assert_eq!(t(Language::Zh, "settings.title"), "SysVitals 设置");
    }

    #[test]
    fn chart_empty_state_localized() {
        assert_eq!(t(Language::En, "chart.no_data"), "No data in this range");
        assert_eq!(t(Language::Zh, "chart.no_data"), "该时间范围内暂无数据");
    }

    #[test]
    fn csv_headers_localized() {
        assert_eq!(t(Language::En, "csv.header.cpu"), "cpu_pct");
        assert_eq!(t(Language::Zh, "csv.header.cpu"), "CPU占用");
        assert_eq!(t(Language::Zh, "csv.header.timestamp"), "时间");
    }

    #[test]
    fn menu_task_viewers_localized() {
        assert_eq!(t(Language::En, "menu.task_manager"), "Task Manager");
        assert_eq!(t(Language::Zh, "menu.task_manager"), "任务管理器");
        assert_eq!(t(Language::En, "menu.activity_monitor"), "Activity Monitor");
        assert_eq!(t(Language::Zh, "menu.activity_monitor"), "活动监视器");
    }

    #[test]
    fn unknown_key_returns_key() {
        assert_eq!(t(Language::En, "does.not.exist"), "does.not.exist");
    }

    #[test]
    fn language_default() {
        assert_eq!(Language::default(), Language::En);
    }

    #[test]
    fn every_key_resolves_in_both_locales() {
        for (key, value) in entries(EN) {
            assert_eq!(t(Language::En, key), value);
            assert_ne!(
                t(Language::Zh, key),
                key,
                "zh.conf is missing a value for {key}"
            );
        }
    }
}
