//! Static UI strings. Two locales, `en` (default) and `zh`. The text lives in
//! `locales/*.conf` (one `key=value` per line) and is compiled into the binary
//! with `include_str!`; each locale is parsed lazily on first use. A
//! heavyweight i18n framework is not warranted for this fixed string set.
//!
//! Scope: these keys cover settings/panel/menu/CSV-header text. Status-bar
//! metric *prefixes* are user config and deliberately NOT here: changing
//! language must never rewrite a user's custom prefix.

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

    /// Native name shown in the language picker.
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

/// Translate `key` for `lang`. The return is valid for the lifetime of `key`;
/// callers always pass string literals. A missing key returns the key itself,
/// making a forgotten string visible during development rather than silently
/// empty.
pub fn t(lang: Language, key: &str) -> &str {
    lang.strings().get(key).copied().unwrap_or(key)
}

fn parse(source: &'static str) -> HashMap<&'static str, &'static str> {
    entries(source).collect()
}

/// Yield `(key, value)` pairs in file order. Full-line comments (`#`) and
/// blank lines are skipped; the first `=` separates key and value. Keys and
/// values are trimmed, so locale files must not rely on edge whitespace.
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
    use std::collections::HashSet;

    use super::*;

    const EN: &str = include_str!("../locales/en.conf");
    const ZH: &str = include_str!("../locales/zh.conf");

    /// Raw `(key, value)` slices before trimming, so the test can detect
    /// edge whitespace that the runtime parser would silently discard.
    fn raw_entries(source: &'static str) -> impl Iterator<Item = (&'static str, &'static str)> {
        source.lines().filter_map(|line| {
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                return None;
            }
            line.split_once('=')
        })
    }

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
        // Context-menu labels for the per-platform system resource viewer.
        // Both locales must resolve (no key-passthrough), and the two OSes
        // ship under different names so both pairs need their own keys.
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
    fn locale_files_have_matching_keys_in_order() {
        let en: Vec<_> = entries(EN).map(|(key, _)| key).collect();
        let zh: Vec<_> = entries(ZH).map(|(key, _)| key).collect();
        assert_eq!(en, zh, "en.conf and zh.conf keys must match in order");
    }

    #[test]
    fn locale_files_have_no_duplicate_keys() {
        for (name, source) in [("en.conf", EN), ("zh.conf", ZH)] {
            let mut seen = HashSet::new();
            for (key, _) in entries(source) {
                assert!(seen.insert(key), "{name}: duplicate key {key}");
            }
        }
    }

    #[test]
    fn locale_lines_have_no_edge_whitespace() {
        for (name, source) in [("en.conf", EN), ("zh.conf", ZH)] {
            for (key, value) in raw_entries(source) {
                assert!(!key.is_empty(), "{name}: empty key");
                assert!(!value.is_empty(), "{name}: empty value for {key}");
                assert_eq!(key, key.trim(), "{name}: edge whitespace in key {key:?}");
                assert_eq!(
                    value,
                    value.trim(),
                    "{name}: edge whitespace in value for {key}"
                );
            }
        }
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
