//! Cross-check locale keys against Rust source references.
//!
//! The locale files are the source of truth for UI strings, while Rust call
//! sites decide which keys are still needed. This test fails when a key used
//! in source is missing from the locale files, or when a defined key is no
//! longer referenced. It runs as part of `cargo test`, so CI enforces the
//! check without a separate repository script.
//!
//! Keys are discovered from literal arguments to `t(...)` and from string
//! literals sharing a namespace already present in the locale files. Dynamic
//! construction via `format!` is not supported; add such keys as literals.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

const EN: &str = include_str!("../locales/en.conf");
const ZH: &str = include_str!("../locales/zh.conf");

/// Literals in source that share a translation-key namespace but are not UI
/// strings.
const NON_I18N_LITERALS: &[&str] = &["settings.lock"];

#[derive(Default)]
struct Scan {
    referenced: BTreeSet<String>,
    missing: BTreeMap<String, BTreeSet<String>>,
}

fn valid_segment(segment: &str, first: bool) -> bool {
    let mut bytes = segment.bytes();
    let Some(first_byte) = bytes.next() else {
        return false;
    };
    let valid_first = if first {
        first_byte.is_ascii_lowercase()
    } else {
        first_byte.is_ascii_lowercase() || first_byte.is_ascii_digit() || first_byte == b'_'
    };
    valid_first
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn is_key(value: &str) -> bool {
    let mut parts = value.split('.');
    let Some(first) = parts.next() else {
        return false;
    };
    if !valid_segment(first, true) {
        return false;
    }
    let mut segments = 0;
    for part in parts {
        if !valid_segment(part, false) {
            return false;
        }
        segments += 1;
    }
    segments > 0
}

fn parse_keys(source: &'static str, name: &str) -> Vec<&'static str> {
    let mut keys = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, raw) in source.lines().enumerate() {
        let number = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        assert_eq!(raw, raw.trim(), "{name}:{number}: edge whitespace");
        let (key, value) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("{name}:{number}: expected key=value"));
        assert_eq!(key, key.trim(), "{name}:{number}: whitespace around key");
        assert_eq!(
            value,
            value.trim(),
            "{name}:{number}: whitespace around value"
        );
        assert!(!key.is_empty(), "{name}:{number}: empty key");
        assert!(!value.is_empty(), "{name}:{number}: empty value for {key}");
        assert!(is_key(key), "{name}:{number}: invalid key {key}");
        assert!(seen.insert(key), "{name}:{number}: duplicate key {key}");
        keys.push(key);
    }
    assert!(!keys.is_empty(), "{name}: no keys");
    keys
}

fn strip_line_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut escaped = false;
    let mut chars = line.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
        } else if character == '"' {
            in_string = true;
        } else if character == '/' && chars.peek().is_some_and(|(_, next)| *next == '/') {
            return &line[..index];
        }
    }
    line
}

fn string_literals(line: &str) -> Vec<&str> {
    let mut literals = Vec::new();
    let mut start = None;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        match start {
            None => {
                if character == '"' {
                    start = Some(index + 1);
                    escaped = false;
                }
            }
            Some(begin) => {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    literals.push(&line[begin..index]);
                    start = None;
                }
            }
        }
    }
    literals
}

fn t_call_literals(line: &str) -> Vec<&str> {
    let bytes = line.as_bytes();
    let mut literals = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else if byte == b'"' {
            in_string = true;
        } else if byte == b't'
            && (index == 0
                || !(bytes[index - 1].is_ascii_alphanumeric() || bytes[index - 1] == b'_'))
        {
            let mut open = index + 1;
            while open < bytes.len() && bytes[open].is_ascii_whitespace() {
                open += 1;
            }
            if open < bytes.len() && bytes[open] == b'(' {
                if let Some(comma) = line[open + 1..].find(',') {
                    let second = line[open + 1 + comma + 1..].trim_start();
                    if let Some(rest) = second.strip_prefix('"') {
                        if let Some(end) = rest.find('"') {
                            literals.push(&rest[..end]);
                        }
                    }
                }
            }
        }
        index += 1;
    }
    literals
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|error| panic!("cannot resolve workspace root: {error}"))
}

fn collect_rs_files(directory: &Path, files: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()));
    for entry in entries {
        let path = entry
            .unwrap_or_else(|error| panic!("cannot read entry in {}: {error}", directory.display()))
            .path();
        if path.is_dir() {
            collect_rs_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

fn scan_sources(defined: &BTreeSet<String>) -> Scan {
    let repo = workspace_root();
    let i18n_impl = repo.join("crates/core/src/i18n.rs");
    let mut files = Vec::new();
    collect_rs_files(&repo.join("crates"), &mut files);
    let prefixes: BTreeSet<&str> = defined
        .iter()
        .filter_map(|key| key.split_once('.').map(|(prefix, _)| prefix))
        .collect();

    let mut scan = Scan::default();
    for path in files {
        // The implementation file contains a deliberate unknown-key test.
        if path == i18n_impl {
            continue;
        }
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        let relative = path
            .strip_prefix(&repo)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");

        for (index, raw) in text.lines().enumerate() {
            let line = strip_line_comment(raw);
            let location = format!("{relative}:{}", index + 1);

            for literal in string_literals(line) {
                if defined.contains(literal) {
                    scan.referenced.insert(literal.to_string());
                } else if is_key(literal)
                    && literal
                        .split_once('.')
                        .is_some_and(|(prefix, _)| prefixes.contains(prefix))
                    && !NON_I18N_LITERALS.contains(&literal)
                {
                    scan.missing
                        .entry(literal.to_string())
                        .or_default()
                        .insert(location.clone());
                }
            }

            for key in t_call_literals(line) {
                if !defined.contains(key) {
                    scan.missing
                        .entry(key.to_string())
                        .or_default()
                        .insert(location.clone());
                }
            }
        }
    }
    scan
}

fn defined_keys() -> BTreeSet<String> {
    parse_keys(EN, "en.conf")
        .into_iter()
        .map(str::to_string)
        .collect()
}

#[test]
fn locale_files_have_matching_keys_in_order() {
    let en = parse_keys(EN, "en.conf");
    let zh = parse_keys(ZH, "zh.conf");
    assert_eq!(en, zh, "en.conf and zh.conf keys must match in order");
}

#[test]
fn source_keys_are_defined() {
    let defined = defined_keys();
    let scan = scan_sources(&defined);
    if scan.missing.is_empty() {
        return;
    }

    let mut message = String::from("keys referenced by source but missing from locales:\n");
    for (key, locations) in &scan.missing {
        let locations = locations.iter().cloned().collect::<Vec<_>>().join(", ");
        writeln!(&mut message, "  {key} ({locations})").expect("write to String");
    }
    panic!("{message}");
}

#[test]
fn locale_keys_are_referenced() {
    let defined = defined_keys();
    let scan = scan_sources(&defined);
    let unused: Vec<_> = defined.difference(&scan.referenced).collect();
    if unused.is_empty() {
        return;
    }

    let mut message = String::from("keys defined in locales but not referenced by source:\n");
    for key in unused {
        writeln!(&mut message, "  {key}").expect("write to String");
    }
    panic!("{message}");
}
