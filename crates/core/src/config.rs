//! Configuration model, defaults, sanitization, persistence and path
//! resolution. Persistence uses temp-file + atomic rename so a crash mid-write
//! never leaves a half-written config.
//!
//! Prefixes are stored exactly as the user set them and are independent of
//! `language`: switching language never rewrites them. Sanitization
//! only rejects control characters and caps length.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::i18n::Language;
use crate::model::{Anchor, Theme};

/// Maximum prefix length in characters.
pub const MAX_PREFIX_CHARS: usize = 8;

/// Allowed sample intervals in milliseconds. The settings UI offers exactly
/// these three choices (1s / 2s / 3s); any other value is snapped to the
/// nearest one.
pub const ALLOWED_INTERVAL_MS: [u64; 3] = [1000, 2000, 3000];

/// Default sample interval: 2 seconds.
pub const DEFAULT_INTERVAL_MS: u64 = 2000;

/// Allowed item-spacing values in pixels for the single-row layout. The
/// settings UI offers exactly these choices; any other value snaps to the
/// nearest one.
pub const ALLOWED_ITEM_SPACING_PX: [u32; 4] = [4, 8, 16, 32];

/// Default item spacing: 8 px.
pub const DEFAULT_ITEM_SPACING_PX: u32 = 8;

/// Allowed margin values in pixels. The setting offsets the indicator from
/// the far end of the taskbar: the bar's left edge on a horizontal bar under
/// the far-left anchor, and its top edge on a vertical (left/right) bar. The
/// settings UI offers exactly these choices; any other value snaps to the
/// nearest one.
pub const ALLOWED_LEFT_MARGIN_PX: [u32; 6] = [8, 16, 32, 64, 128, 256];

/// Default left margin: 16 px.
pub const DEFAULT_LEFT_MARGIN_PX: u32 = 16;

/// Upper bound for `history.retention_days`. A value large enough to make the
/// prune cutoff negative would disable pruning entirely and let the database
/// grow without bound; one year is far beyond the supported UI range.
pub const MAX_RETENTION_DAYS: u32 = 365;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub sample: SampleConfig,
    pub display: DisplayConfig,
    pub history: HistoryConfig,
    pub startup: StartupConfig,
    pub windows: WindowsConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SampleConfig {
    pub interval_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplayConfig {
    pub language: Language,
    /// CPU and MEM share one toggle: they are shown/hidden together.
    pub show_resources: bool,
    /// U and D share one toggle: they are shown/hidden together.
    pub show_network: bool,
    pub theme: Theme,
    /// Horizontal gap between adjacent metrics on the single-row layout, in
    /// pixels. One of [`ALLOWED_ITEM_SPACING_PX`]; anything else snaps to the
    /// nearest allowed value via `sanitize`.
    pub item_spacing: u32,
    /// Dual-row (2x2 quadrant) layout, on by default. Cross-platform:
    /// Windows renders it in the taskbar overlay; macOS renders it in the
    /// menu bar.
    pub dual_row: bool,
    pub prefix: PrefixConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrefixConfig {
    pub cpu: String,
    pub mem: String,
    pub up: String,
    pub down: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryConfig {
    pub retention_days: u32,
}

/// Login-startup preference, shared by both platforms. The settings window
/// owns it; the platform layer (Windows Run key / macOS `SMAppService`) applies
/// it. The legacy `[windows] auto_start` key is deliberately not read: it had
/// no consumer and defaulted to `true`, so honoring it would silently enable
/// auto-start for existing configs after this feature lands.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StartupConfig {
    pub auto_start: bool,
    /// Whether the user actually chose the auto-start value. A config that
    /// merely inherited the default (or was re-created after a corrupt file
    /// was backed up) must not remove a login item the user enabled in an
    /// earlier run. The settings window sets this only when the switch is
    /// clicked.
    pub auto_start_explicit: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowsConfig {
    pub anchor: Anchor,
    /// Offset (pixels) between the taskbar's far end and the indicator. On a
    /// horizontal bar under the far-left anchor this measures from the bar's
    /// left edge; on a vertical (left/right) bar it measures from the bar's
    /// top edge. One of [`ALLOWED_LEFT_MARGIN_PX`]; anything else snaps to the
    /// nearest allowed value via `sanitize`. Default 16.
    pub left_margin_px: u32,
}

impl Default for SampleConfig {
    fn default() -> Self {
        SampleConfig {
            interval_ms: DEFAULT_INTERVAL_MS,
        }
    }
}

impl Default for DisplayConfig {
    fn default() -> Self {
        DisplayConfig {
            language: Language::En,
            show_resources: true,
            show_network: true,
            theme: Theme::Auto,
            item_spacing: DEFAULT_ITEM_SPACING_PX,
            dual_row: true,
            prefix: PrefixConfig::default(),
        }
    }
}

impl DisplayConfig {
    /// Which of the four metrics are visible, given the two paired toggles.
    /// Resources (CPU/MEM) and network (U/D) can each be on or off together.
    pub fn metric_visible(&self, metric: crate::model::Metric) -> bool {
        use crate::model::Metric;
        match metric {
            Metric::Cpu | Metric::Mem => self.show_resources,
            Metric::Up | Metric::Down => self.show_network,
        }
    }
}

impl Default for PrefixConfig {
    fn default() -> Self {
        PrefixConfig {
            cpu: "CPU:".to_string(),
            mem: "MEM:".to_string(),
            up: "U:".to_string(),
            down: "D:".to_string(),
        }
    }
}

impl Default for HistoryConfig {
    fn default() -> Self {
        HistoryConfig { retention_days: 30 }
    }
}

impl Default for WindowsConfig {
    fn default() -> Self {
        WindowsConfig {
            anchor: Anchor::Right,
            left_margin_px: DEFAULT_LEFT_MARGIN_PX,
        }
    }
}

/// A recoverable problem found while sanitizing config. The value was fixed;
/// these are surfaced to the user (e.g. in the settings panel) but never
/// block startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizeIssue {
    pub field: &'static str,
    pub message: String,
}

/// Where a loaded config came from. A missing file yields defaults, but that
/// is not the same as the user explicitly choosing the default values: the
/// platform layers use this distinction to avoid applying side effects such
/// as removing a login item when the file is absent or has just been renamed
/// away as corrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    /// Loaded and parsed from an existing file.
    File,
    /// No file existed; defaults were used.
    Defaults,
}

impl Config {
    /// Clamp/repair an in-memory config into a valid state and report what was
    /// changed. Called after load and after any edit. Idempotent: a sanitized
    /// config yields no issues.
    pub fn sanitize(&mut self) -> Vec<SanitizeIssue> {
        let mut issues = Vec::new();

        if !ALLOWED_INTERVAL_MS.contains(&self.sample.interval_ms) {
            let old = self.sample.interval_ms;
            self.sample.interval_ms = *ALLOWED_INTERVAL_MS
                .iter()
                .min_by_key(|x| x.abs_diff(old))
                .unwrap_or(&DEFAULT_INTERVAL_MS);
            issues.push(SanitizeIssue {
                field: "sample.interval_ms",
                message: format!(
                    "{old} is not allowed, snapped to {}",
                    self.sample.interval_ms
                ),
            });
        }

        if self.history.retention_days == 0 || self.history.retention_days > MAX_RETENTION_DAYS {
            let old = self.history.retention_days;
            self.history.retention_days = if old == 0 { 30 } else { MAX_RETENTION_DAYS };
            issues.push(SanitizeIssue {
                field: "history.retention_days",
                message: format!(
                    "must be in 1..={MAX_RETENTION_DAYS}, reset to {} (was {old})",
                    self.history.retention_days
                ),
            });
        }

        if !ALLOWED_ITEM_SPACING_PX.contains(&self.display.item_spacing) {
            let old = self.display.item_spacing;
            self.display.item_spacing = snap_u32(old, &ALLOWED_ITEM_SPACING_PX);
            issues.push(SanitizeIssue {
                field: "display.item_spacing",
                message: format!("snapped to nearest allowed value (was {old})"),
            });
        }

        for (key, field) in [
            ("cpu", &mut self.display.prefix.cpu),
            ("mem", &mut self.display.prefix.mem),
            ("up", &mut self.display.prefix.up),
            ("down", &mut self.display.prefix.down),
        ] {
            if let Some(msg) = sanitize_prefix(field) {
                issues.push(SanitizeIssue {
                    field: match key {
                        "cpu" => "display.prefix.cpu",
                        "mem" => "display.prefix.mem",
                        "up" => "display.prefix.up",
                        _ => "display.prefix.down",
                    },
                    message: msg,
                });
            }
        }

        if !ALLOWED_LEFT_MARGIN_PX.contains(&self.windows.left_margin_px) {
            let old = self.windows.left_margin_px;
            self.windows.left_margin_px = snap_u32(old, &ALLOWED_LEFT_MARGIN_PX);
            issues.push(SanitizeIssue {
                field: "windows.left_margin_px",
                message: format!("snapped to nearest allowed value (was {old})"),
            });
        }
        // At least one metric group must stay visible: if both toggles are
        // off, re-enable resources (CPU/MEM) so the indicator never goes blank.
        if !self.display.show_resources && !self.display.show_network {
            self.display.show_resources = true;
            issues.push(SanitizeIssue {
                field: "display.show_resources",
                message: "at least one group must remain visible; resources re-enabled".to_string(),
            });
        }
        issues
    }

    /// Replace the fields owned by the settings UI with `ui`, leaving every
    /// other field (history retention and any future non-UI fields) untouched.
    /// Settings starts from the file it just read and overlays only these
    /// fields, so a manual edit outside the UI is not reverted by the next
    /// save.
    pub fn overlay_ui_from(&mut self, ui: &Config) {
        self.display = ui.display.clone();
        self.sample.interval_ms = ui.sample.interval_ms;
        self.startup = ui.startup.clone();
        self.windows = ui.windows.clone();
    }

    /// Serialize to TOML text.
    pub fn to_toml_string(&self) -> anyhow::Result<String> {
        toml::to_string_pretty(self).map_err(Into::into)
    }

    /// Parse TOML text, apply defaults for anything missing, then sanitize.
    ///
    /// A single unknown enum value (for example `theme = "Dark"` after a
    /// hand edit, or a variant from a newer build after a downgrade) must not
    /// discard the whole file: those fields are reset to their defaults and
    /// reported as sanitize issues before deserialization.
    pub fn from_toml_str(text: &str) -> anyhow::Result<(Config, Vec<SanitizeIssue>)> {
        let mut value: toml::Value = toml::from_str(text)?;
        let mut issues = Vec::new();
        repair_enum(
            &mut value,
            ("display", "language"),
            &["en", "zh"],
            "en",
            "display.language",
            &mut issues,
        );
        repair_enum(
            &mut value,
            ("display", "theme"),
            &["auto", "light", "dark"],
            "auto",
            "display.theme",
            &mut issues,
        );
        repair_enum(
            &mut value,
            ("windows", "anchor"),
            &["right", "left"],
            "right",
            "windows.anchor",
            &mut issues,
        );
        let mut cfg: Config = value.try_into()?;
        issues.extend(cfg.sanitize());
        Ok((cfg, issues))
    }

    /// Load from disk. A missing file yields defaults (not an error); other
    /// read/parse errors are returned so the caller can decide to back up.
    pub fn load(path: &PathBuf) -> anyhow::Result<(Config, Vec<SanitizeIssue>)> {
        Self::load_with_source(path).map(|(config, issues, _)| (config, issues))
    }

    /// Load from disk and report whether a file was actually read. Callers
    /// that apply side effects from the config (auto-start) must use the
    /// source to distinguish "the user saved defaults" from "the file is
    /// missing or was renamed away as corrupt".
    pub fn load_with_source(
        path: &PathBuf,
    ) -> anyhow::Result<(Config, Vec<SanitizeIssue>, ConfigSource)> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let (config, issues) = Self::from_toml_str(&text)?;
                Ok((config, issues, ConfigSource::File))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut cfg = Config::default();
                let issues = cfg.sanitize();
                Ok((cfg, issues, ConfigSource::Defaults))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Persist atomically: write a unique temp file (fsync), then rename over
    /// `path`. The temp name includes the process id so two settings windows
    /// launched by hand cannot interleave writes into the same file.
    pub fn save(&self, path: &PathBuf) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = {
            let mut name = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "config.toml".to_string());
            name.push_str(&format!(".{}.tmp", std::process::id()));
            dir_join(path.parent(), &name)
        };
        // A crashed writer leaves its pid-stamped temp file behind forever;
        // sweep stale ones (not our own, not a concurrent writer's fresh
        // file) before writing so the config directory stays clean.
        remove_orphan_tmp_files(path, &tmp);
        let text = self.to_toml_string()?;

        let write = (|| -> anyhow::Result<()> {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            drop(file);
            fs::rename(&tmp, path)?;
            // The rename itself is only durable once its directory entry is
            // synced. Windows does not allow opening a directory as a file;
            // the file sync above plus MoveFileEx is the best available
            // there.
            #[cfg(unix)]
            if let Some(parent) = path.parent() {
                if let Ok(dir) = fs::File::open(parent) {
                    let _ = dir.sync_all();
                }
            }
            Ok(())
        })();
        if write.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        write
    }
}

/// Snap an out-of-set `u32` value to the nearest entry of `allowed`. The
/// caller guarantees `allowed` is non-empty.
fn snap_u32(value: u32, allowed: &[u32]) -> u32 {
    *allowed
        .iter()
        .min_by_key(|x| x.abs_diff(value))
        .expect("allowed must be non-empty")
}

/// Reset one string-valued enum field to `fallback` when its value is not in
/// `allowed`, recording a sanitize issue. Missing fields and non-string values
/// are left for serde: a wrong type is a structural error, not a stale enum
/// name.
fn repair_enum(
    root: &mut toml::Value,
    path: (&str, &str),
    allowed: &[&str],
    fallback: &str,
    field: &'static str,
    issues: &mut Vec<SanitizeIssue>,
) {
    let Some(table) = root.get_mut(path.0).and_then(toml::Value::as_table_mut) else {
        return;
    };
    let Some(value) = table.get_mut(path.1) else {
        return;
    };
    let Some(current) = value.as_str().map(str::to_owned) else {
        return;
    };
    if allowed.contains(&current.as_str()) {
        return;
    }
    *value = toml::Value::String(fallback.to_string());
    issues.push(SanitizeIssue {
        field,
        message: format!("{current:?} is not a valid value, reset to {fallback:?}"),
    });
}

/// Best-effort cleanup of orphaned save temp files next to `path`. A crash
/// between temp write and rename leaves `config.toml.<pid>.tmp` behind, and
/// no live process ever removes it (the name embeds the dead writer's pid).
/// Only files older than five minutes are touched: a concurrently saving
/// process keeps its fresh temp file even across a short system sleep.
fn remove_orphan_tmp_files(path: &std::path::Path, own_tmp: &std::path::Path) {
    const ORPHAN_AGE_SECS: u64 = 300;
    let Some(dir) = path.parent() else {
        return;
    };
    let Some(base) = path.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let cutoff =
        std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(ORPHAN_AGE_SECS));
    for entry in entries.flatten() {
        if entry.path() == own_tmp {
            continue;
        }
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let Some(pid) = name
            .strip_prefix(&format!("{base}."))
            .and_then(|rest| rest.strip_suffix(".tmp"))
        else {
            continue;
        };
        if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .zip(cutoff)
            .is_some_and(|(modified, cutoff)| modified <= cutoff);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Trim outer whitespace, drop control chars, cap to MAX_PREFIX_CHARS.
/// Returns Some(message) if anything was changed.
fn sanitize_prefix(value: &mut String) -> Option<String> {
    let original = value.clone();
    let mut cleaned: String = value
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_PREFIX_CHARS)
        .collect();
    // Normalize internal runs of whitespace to a single space so prefixes stay
    // compact; the display layer always inserts exactly one space before value.
    while cleaned.contains("  ") {
        cleaned = cleaned.replace("  ", " ");
    }
    if cleaned != original {
        let msg = if original.chars().any(char::is_control) {
            "control characters removed"
        } else if original.chars().count() > MAX_PREFIX_CHARS {
            "truncated to 8 characters"
        } else {
            "whitespace normalized"
        };
        *value = cleaned;
        Some(msg.to_string())
    } else {
        None
    }
}

fn dir_join(parent: Option<&std::path::Path>, name: &str) -> PathBuf {
    match parent {
        Some(p) => p.join(name),
        None => PathBuf::from(name),
    }
}

/// Where the config file lives. Windows:
/// `%APPDATA%\SysVitals\config\config.toml`; macOS:
/// `~/Library/Application Support/SysVitals/config.toml` (the `directories`
/// crate applies the platform-specific project subdirectory). Linux dev
/// falls back to the same helper's config dir.
pub fn config_path() -> anyhow::Result<PathBuf> {
    let dirs = project_dirs()?;
    Ok(dirs.config_dir().join("config.toml"))
}

/// Where the history database lives. Windows:
/// `%APPDATA%\SysVitals\data\history.db`; macOS:
/// `~/Library/Application Support/SysVitals/history.db`. SQLite is the single
/// source of truth for historical data; CSV is export-only.
pub fn history_path() -> anyhow::Result<PathBuf> {
    let dirs = project_dirs()?;
    Ok(dirs.data_dir().join("history.db"))
}

/// The per-user data directory (`%APPDATA%\SysVitals\data` on Windows,
/// `~/Library/Application Support/SysVitals` on macOS). Log files live under
/// `<data_dir>/logs/` (see [`crate::logger`]).
pub fn data_dir() -> anyhow::Result<PathBuf> {
    let dirs = project_dirs()?;
    Ok(dirs.data_dir().to_path_buf())
}

/// Resolve the project directories once per process. Every path helper used
/// to construct `ProjectDirs` independently; logging calls `data_dir()` for
/// every line, so the resolution is cached.
fn project_dirs() -> anyhow::Result<&'static directories::ProjectDirs> {
    use std::sync::OnceLock;

    static DIRS: OnceLock<Option<directories::ProjectDirs>> = OnceLock::new();
    DIRS.get_or_init(|| directories::ProjectDirs::from("", "", "SysVitals"))
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("could not resolve project directories"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let c = Config::default();
        assert_eq!(c.sample.interval_ms, 2000);
        assert_eq!(c.display.language, Language::En);
        assert_eq!(c.display.prefix.cpu, "CPU:");
        assert_eq!(c.display.prefix.up, "U:");
        assert_eq!(c.windows.anchor, Anchor::Right);
        assert!(c.display.dual_row);
        assert_eq!(c.history.retention_days, 30);
        assert!(!c.startup.auto_start);
        assert!(!c.startup.auto_start_explicit);
        assert_eq!(c.display.item_spacing, DEFAULT_ITEM_SPACING_PX);
        assert_eq!(c.windows.left_margin_px, DEFAULT_LEFT_MARGIN_PX);
    }

    #[test]
    fn sanitize_default_reports_nothing() {
        let mut c = Config::default();
        assert!(c.sanitize().is_empty());
    }

    #[test]
    fn interval_snaps_to_nearest_allowed() {
        // 1/2/3 s are the only choices; 3000 is now legal and stays unchanged.
        let mut c = Config::default();
        c.sample.interval_ms = 3000;
        assert!(c.sanitize().iter().all(|i| i.field != "sample.interval_ms"));
        assert_eq!(c.sample.interval_ms, 3000);

        // 4000 is out of range: nearest of {1000,2000,3000} is 3000.
        let mut c2 = Config::default();
        c2.sample.interval_ms = 4000;
        let issues = c2.sanitize();
        assert_eq!(c2.sample.interval_ms, 3000);
        assert!(issues.iter().any(|i| i.field == "sample.interval_ms"));

        // 500 snaps up to the smallest allowed tier, 1000.
        let mut c3 = Config::default();
        c3.sample.interval_ms = 500;
        c3.sanitize();
        assert_eq!(c3.sample.interval_ms, 1000);
    }

    #[test]
    fn prefix_control_chars_and_length_capped() {
        let mut c = Config::default();
        c.display.prefix.cpu = "AB\u{7}CDEF".to_string(); // BEL in middle
        c.sanitize();
        assert_eq!(c.display.prefix.cpu, "ABCDEF");

        let mut c2 = Config::default();
        c2.display.prefix.mem = "TOOLONGPREFIX".to_string();
        let issues = c2.sanitize();
        assert_eq!(c2.display.prefix.mem.chars().count(), MAX_PREFIX_CHARS);
        assert!(issues.iter().any(|i| i.message.contains("truncated")));
    }

    #[test]
    fn empty_prefix_allowed() {
        let mut c = Config::default();
        c.display.prefix.down = "".to_string();
        assert!(c.sanitize().is_empty());
        assert_eq!(c.display.prefix.down, "");
    }

    #[test]
    fn toml_roundtrip_preserves_values() {
        let mut c = Config::default();
        c.display.language = Language::Zh;
        c.display.dual_row = true;
        c.startup.auto_start = true;
        c.startup.auto_start_explicit = true;
        c.windows.anchor = Anchor::Left;
        let text = c.to_toml_string().unwrap();
        let (back, _) = Config::from_toml_str(&text).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn missing_keys_fall_back_to_defaults() {
        // A hand-written partial config must still load with defaults filled in.
        let (c, _) = Config::from_toml_str("[display]\nlanguage = \"zh\"\n").unwrap();
        assert_eq!(c.display.language, Language::Zh);
        assert_eq!(c.display.prefix.cpu, "CPU:");
        assert_eq!(c.sample.interval_ms, 2000);
    }

    #[test]
    fn legacy_windows_auto_start_is_ignored() {
        // The old key defaulted to true and had no consumer. Reading it now
        // would turn auto-start on during an upgrade, so it must stay inert.
        let (c, _) = Config::from_toml_str("[windows]\nauto_start = true\n").unwrap();
        assert!(!c.startup.auto_start);
    }

    #[test]
    fn item_spacing_snaps_to_allowed_set() {
        // Allowed values pass through untouched.
        for v in ALLOWED_ITEM_SPACING_PX {
            let mut c = Config::default();
            c.display.item_spacing = v;
            let issues = c.sanitize();
            assert_eq!(c.display.item_spacing, v);
            assert!(issues.iter().all(|i| i.field != "display.item_spacing"));
        }
        // Out-of-set values snap to the nearest allowed entry.
        let mut low = Config::default();
        low.display.item_spacing = 1;
        let issues = low.sanitize();
        assert_eq!(low.display.item_spacing, 4);
        assert!(issues.iter().any(|i| i.field == "display.item_spacing"));

        let mut mid = Config::default();
        mid.display.item_spacing = 11;
        mid.sanitize();
        // 11 is equidistant from 8 and 16 (nearest on each side); the tie
        // resolves to the first (smaller) allowed value.
        assert_eq!(mid.display.item_spacing, 8);

        // Values above the max snap down to the max allowed entry.
        let mut above = Config::default();
        above.display.item_spacing = 100;
        above.sanitize();
        assert_eq!(above.display.item_spacing, 32);

        // Legacy space-count values from old configs snap sensibly.
        let mut legacy = Config::default();
        legacy.display.item_spacing = 2;
        legacy.sanitize();
        assert_eq!(legacy.display.item_spacing, 4);
    }

    #[test]
    fn left_margin_snaps_to_allowed_set() {
        for v in ALLOWED_LEFT_MARGIN_PX {
            let mut c = Config::default();
            c.windows.left_margin_px = v;
            let issues = c.sanitize();
            assert_eq!(c.windows.left_margin_px, v);
            assert!(issues.iter().all(|i| i.field != "windows.left_margin_px"));
        }
        // 4 snaps up to the smallest allowed value, 8.
        let mut c = Config::default();
        c.windows.left_margin_px = 4;
        let issues = c.sanitize();
        assert_eq!(c.windows.left_margin_px, 8);
        assert!(issues.iter().any(|i| i.field == "windows.left_margin_px"));

        // Removed set entry (48) snaps to the nearest survivor, 32.
        let mut old = Config::default();
        old.windows.left_margin_px = 48;
        old.sanitize();
        assert_eq!(old.windows.left_margin_px, 32);

        // Large values snap down to the max allowed entry.
        let mut big = Config::default();
        big.windows.left_margin_px = 500;
        big.sanitize();
        assert_eq!(big.windows.left_margin_px, 256);
    }

    #[test]
    fn paired_show_toggles_default_on_and_route_metrics() {
        use crate::model::Metric;
        let c = Config::default();
        assert!(c.display.show_resources && c.display.show_network);
        // Group membership: CPU/MEM follow show_resources, U/D follow
        // show_network — there is no per-metric switch anymore.
        assert!(c.display.metric_visible(Metric::Cpu) && c.display.metric_visible(Metric::Mem));
        assert!(c.display.metric_visible(Metric::Up) && c.display.metric_visible(Metric::Down));
        let mut off = Config::default();
        off.display.show_resources = false;
        assert!(!off.display.metric_visible(Metric::Cpu));
        assert!(!off.display.metric_visible(Metric::Mem));
        assert!(off.display.metric_visible(Metric::Up));
    }

    fn temp_config_path(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        let uniq = format!("sysvitals-cfgtest-{}-{}", tag, std::process::id());
        dir.push(uniq);
        let _ = fs::create_dir_all(&dir);
        dir.join("config.toml")
    }

    #[test]
    fn atomic_save_then_load() {
        let path = temp_config_path("save");
        let mut c = Config::default();
        c.display.prefix.cpu = "负载".to_string();
        c.save(&path).unwrap();
        let (back, _) = Config::load(&path).unwrap();
        assert_eq!(back.display.prefix.cpu, "负载");
        // No leftover temp file next to the config. `save` names the temp
        // file with the process id, so the old assertion against
        // `config.toml.tmp` never checked the real file.
        let tmp = path.with_file_name(format!("config.toml.{}.tmp", std::process::id()));
        assert!(!tmp.exists());
    }

    #[test]
    fn load_missing_file_yields_defaults() {
        let path = temp_config_path("missing").with_file_name("no-such-file.toml");
        let (c, issues) = Config::load(&path).unwrap();
        assert_eq!(c, Config::default());
        assert!(issues.is_empty());
    }

    #[test]
    fn save_sweeps_stale_orphan_tmp_files_but_not_fresh_ones() {
        let path = temp_config_path("orphans");
        // A crashed writer's stale temp file (backdated past the five-minute
        // guard) must be swept; a fresh temp file may belong to a concurrent
        // saver and must survive; unrelated files are never touched.
        let stale = path.with_file_name("config.toml.1111.tmp");
        let fresh = path.with_file_name("config.toml.2222.tmp");
        let non_pid = path.with_file_name("config.toml.backup.tmp");
        let decoy = path.with_file_name("notes.txt");
        for f in [&stale, &fresh, &non_pid, &decoy] {
            fs::write(f, b"x").unwrap();
        }
        let file = fs::File::options().write(true).open(&stale).unwrap();
        file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(600))
            .unwrap();
        let file = fs::File::options().write(true).open(&non_pid).unwrap();
        file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(600))
            .unwrap();
        Config::default().save(&path).unwrap();
        assert!(!stale.exists(), "stale pid-stamped temp file swept");
        assert!(fresh.exists(), "fresh temp file kept for its writer");
        assert!(non_pid.exists(), "non-pid temp file is not ours to sweep");
        assert!(decoy.exists(), "unrelated file untouched");
    }

    #[test]
    fn unknown_enum_values_reset_only_those_fields() {
        let text = r#"
[display]
language = "zh"
theme = "Dark"
item_spacing = 16
[windows]
anchor = "Middle"
[history]
retention_days = 14
"#;
        let (cfg, issues) = Config::from_toml_str(text).unwrap();
        assert_eq!(cfg.display.language, Language::Zh);
        assert_eq!(cfg.display.theme, Theme::Auto);
        assert_eq!(cfg.windows.anchor, Anchor::Right);
        // Unrelated values in the same file survive.
        assert_eq!(cfg.display.item_spacing, 16);
        assert_eq!(cfg.history.retention_days, 14);
        assert_eq!(issues.len(), 2, "theme and anchor reported");
        assert!(issues.iter().any(|i| i.field == "display.theme"));
        assert!(issues.iter().any(|i| i.field == "windows.anchor"));
    }

    #[test]
    fn legacy_hidden_config_keys_are_ignored() {
        // Old configs may still contain the removed debug/optional keys.
        // Deserialization ignores unknown fields, so they must not fail the
        // whole file or affect the retained settings.
        let text = r#"
[sample]
interval_ms = 3000
cpu_smoothing = 0.6
[history]
enabled = false
retention_days = 14
csv_scope = "last7d"
"#;
        let (cfg, issues) = Config::from_toml_str(text).unwrap();
        assert!(issues.is_empty());
        assert_eq!(cfg.sample.interval_ms, 3000);
        assert_eq!(cfg.history.retention_days, 14);
    }

    #[test]
    fn retention_days_is_capped() {
        let mut cfg = Config::default();
        cfg.history.retention_days = u32::MAX;
        let issues = cfg.sanitize();
        assert_eq!(cfg.history.retention_days, MAX_RETENTION_DAYS);
        assert!(issues.iter().any(|i| i.field == "history.retention_days"));
        cfg.history.retention_days = 0;
        cfg.sanitize();
        assert_eq!(cfg.history.retention_days, 30);
    }

    #[test]
    fn ui_overlay_preserves_non_ui_fields_from_disk() {
        let mut disk = Config::default();
        disk.history.retention_days = 7;
        disk.display.prefix.cpu = "DISK".to_string();

        let mut ui = Config::default();
        ui.display.language = Language::Zh;
        ui.display.prefix.cpu = "UI".to_string();
        ui.startup.auto_start = true;
        ui.sample.interval_ms = 3000;

        disk.overlay_ui_from(&ui);
        assert_eq!(disk.display.prefix.cpu, "UI");
        assert_eq!(disk.display.language, Language::Zh);
        assert_eq!(disk.sample.interval_ms, 3000);
        assert!(disk.startup.auto_start);
        // Non-UI fields survive.
        assert_eq!(disk.history.retention_days, 7);
    }

    #[test]
    fn load_source_distinguishes_a_missing_file_from_saved_defaults() {
        let path = temp_config_path("source").with_file_name("no-such-file.toml");
        let (_, _, missing) = Config::load_with_source(&path).unwrap();
        assert_eq!(missing, ConfigSource::Defaults);

        let mut cfg = Config::default();
        cfg.startup.auto_start = true;
        cfg.save(&path).unwrap();
        let (loaded, _, source) = Config::load_with_source(&path).unwrap();
        assert_eq!(source, ConfigSource::File);
        assert!(loaded.startup.auto_start);
    }
}
