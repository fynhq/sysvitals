//! config: split out of `win` (see `win` for the module map).

use super::*;

// ------------------------------------------------------------ config access

pub(crate) fn config() -> Config {
    CONFIG
        .get_or_init(|| {
            // A missing or broken config file must not stop the indicator:
            // fall back to defaults and keep running, but leave a trace so
            // a parse error is diagnosable.
            let loaded = match sysvitals_core::config::config_path() {
                Ok(p) => match Config::load_with_source(&p) {
                    Ok((c, issues, source)) => {
                        log_config_issues(&issues);
                        CONFIG_FROM_FILE.store(
                            source == sysvitals_core::config::ConfigSource::File,
                            Ordering::Relaxed,
                        );
                        c
                    }
                    Err(e) => {
                        log_event(
                            "WARN",
                            &format!("config load failed ({}): {e}; using defaults", p.display()),
                        );
                        CONFIG_LOAD_FAILED.store(true, Ordering::Relaxed);
                        CONFIG_FROM_FILE.store(false, Ordering::Relaxed);
                        Config::default()
                    }
                },
                Err(e) => {
                    log_event("WARN", &format!("config path unavailable: {e}"));
                    CONFIG_LOAD_FAILED.store(true, Ordering::Relaxed);
                    CONFIG_FROM_FILE.store(false, Ordering::Relaxed);
                    Config::default()
                }
            };
            RwLock::new(loaded)
        })
        .read()
        .expect("config lock")
        .clone()
}

pub(crate) fn store_config(new: Config) {
    if let Some(slot) = CONFIG.get() {
        *slot.write().expect("config lock") = new;
    }
}

pub(crate) fn log_config_issues(issues: &[sysvitals_core::config::SanitizeIssue]) {
    for issue in issues {
        log_event(
            "WARN",
            &format!("config repair {}: {}", issue.field, issue.message),
        );
    }
}

// ----------------------------------------------------------------- logging

// Logging (`log_event`, `prune_old_logs`) comes from `sysvitals-support`
// so every SysVitals process shares one implementation.

// ------------------------------------------------------------------ helpers

/// Read a REG_SZ value from HKCU; `None` when absent or unreadable. Used
/// only to avoid rewriting/logging an unchanged Run entry.
pub(crate) unsafe fn hkcu_string(key: &str, value: &str) -> Option<String> {
    let key = utf16(key);
    let value = utf16(value);
    let mut hkey = HKEY::default();
    if RegOpenKeyExW(
        HKEY_CURRENT_USER,
        windows::core::PCWSTR(key.as_ptr()),
        None,
        KEY_READ,
        &mut hkey,
    )
    .is_err()
    {
        return None;
    }
    let mut data = [0u16; 2048];
    let mut size = (data.len() * 2) as u32;
    let queried = RegQueryValueExW(
        hkey,
        windows::core::PCWSTR(value.as_ptr()),
        None,
        None,
        Some(data.as_mut_ptr().cast()),
        Some(&mut size),
    );
    let _ = RegCloseKey(hkey);
    if queried.is_err() || size < 2 {
        return None;
    }
    let wide = &data[..(size as usize / 2)];
    let text = String::from_utf16_lossy(wide);
    Some(text.trim_end_matches('\0').to_string())
}

/// Write or remove the per-user Run entry. Returns whether the registry
/// actually changed. The value is the quoted path to this executable, so
/// moving the folder and launching from the new copy repairs the entry on
/// the next start.
pub(crate) unsafe fn set_auto_start(enabled: bool) -> Result<bool, String> {
    let key = utf16(RUN_KEY);
    let value = utf16(RUN_VALUE);
    if enabled {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let command = format!("\"{}\"", exe.display());
        if hkcu_string(RUN_KEY, RUN_VALUE).as_deref() == Some(command.as_str()) {
            return Ok(false);
        }
        let data: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
        let result = RegSetKeyValueW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(key.as_ptr()),
            windows::core::PCWSTR(value.as_ptr()),
            REG_SZ.0,
            Some(data.as_ptr().cast()),
            (data.len() * 2) as u32,
        );
        if result.is_ok() {
            Ok(true)
        } else {
            Err(format!("registry write error {}", result.0))
        }
    } else {
        if hkcu_string(RUN_KEY, RUN_VALUE).is_none() {
            return Ok(false);
        }
        let result = RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(key.as_ptr()),
            windows::core::PCWSTR(value.as_ptr()),
        );
        // No value is the desired end state, not a failure.
        if result.is_ok() || result == ERROR_FILE_NOT_FOUND {
            Ok(true)
        } else {
            Err(format!("registry delete error {}", result.0))
        }
    }
}

/// Apply the login-startup preference once per value change. The settings
/// window writes the config; this process owns the registry side effect.
pub(crate) fn apply_auto_start(cfg: &Config) {
    // A `false` that merely came from the default (or a config re-created
    // after a corrupt file was backed up) is not an explicit user choice,
    // so it must not remove a login item. A legacy `true` is still safe
    // to apply: it can only repair/add the entry, never remove one.
    if !cfg.startup.auto_start_explicit && !cfg.startup.auto_start {
        // Leave the system state alone, but forget the last applied bit
        // so a later explicit/legacy `true` can still repair the entry.
        APPLIED_AUTO_START.store(u32::MAX, Ordering::Relaxed);
        return;
    }
    let desired = cfg.startup.auto_start;
    let desired_bits = desired as u32;
    if APPLIED_AUTO_START.load(Ordering::Relaxed) == desired_bits {
        return;
    }
    match unsafe { set_auto_start(desired) } {
        Ok(changed) => {
            APPLIED_AUTO_START.store(desired_bits, Ordering::Relaxed);
            if changed {
                log_event(
                    "INFO",
                    &format!(
                        "auto-start {}",
                        if desired { "enabled" } else { "disabled" }
                    ),
                );
            }
        }
        Err(e) => log_event(
            "WARN",
            &format!(
                "auto-start {} failed: {e}",
                if desired { "enable" } else { "disable" }
            ),
        ),
    }
}

// ------------------------------------------------- config file watching

/// Read the config file's last-modified time as nanoseconds since UNIX
/// epoch. Returns 0 when the file does not exist or cannot be read.
pub(crate) fn config_mtime() -> u64 {
    let Ok(path) = sysvitals_core::config::config_path() else {
        return 0;
    };
    let Ok(meta) = std::fs::metadata(&path) else {
        return 0;
    };
    let Ok(modified) = meta.modified() else {
        return 0;
    };
    modified
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Called on each TIMER_CONFIG tick. Compares the config file's mtime
/// against the cached value; on change, reloads the config, re-seeds the
/// sample timer (interval may have changed), and re-lays out every overlay.
pub(crate) fn poll_config_change() {
    let current = config_mtime();
    let previous = CONFIG_MTIME.load(Ordering::Relaxed);
    if current == previous {
        return;
    }
    let loaded = sysvitals_core::config::config_path().and_then(|p| Config::load_with_source(&p));
    let (loaded, source) = match loaded {
        Ok((c, issues, source)) => {
            log_config_issues(&issues);
            (c, source)
        }
        Err(e) => {
            // Keep the last good config. Do not advance CONFIG_MTIME, so a
            // fixed file is picked up on the next poll; throttle the log
            // to one line per broken mtime.
            if CONFIG_ERROR_MTIME.swap(current, Ordering::Relaxed) != current {
                log_event("WARN", &format!("config reload failed: {e}"));
            }
            return;
        }
    };
    // The sampler fixes `cpu_smoothing` at construction; detect a change
    // before swapping in the new config so it can rebuild afterwards.
    let smoothing_changed = config().sample.cpu_smoothing != loaded.sample.cpu_smoothing;
    CONFIG_MTIME.store(current, Ordering::Relaxed);
    CONFIG_ERROR_MTIME.store(0, Ordering::Relaxed);
    CONFIG_FROM_FILE.store(
        source == sysvitals_core::config::ConfigSource::File,
        Ordering::Relaxed,
    );
    store_config(loaded.clone());
    if smoothing_changed {
        // Drop the sampler; the next tick rebuilds it with the new factor.
        // Rebuilding re-primes the CPU EMA and re-seeds the network
        // baseline, i.e. one "--" frame — the same cost as a restart.
        SAMPLER.with(|s| *s.borrow_mut() = None);
        log_event(
            "INFO",
            &format!(
                "cpu_smoothing changed to {}: sampler rebuilds on the next tick",
                loaded.sample.cpu_smoothing
            ),
        );
    }
    // A missing file (for example after saving a corrupt config aside)
    // is not an explicit "auto-start off" choice. Only an existing,
    // parsed file may apply the platform side effect.
    if source == sysvitals_core::config::ConfigSource::File {
        apply_auto_start(&loaded);
    } else {
        // Let a later re-created config apply its value again, even if it
        // matches the last one we applied.
        APPLIED_AUTO_START.store(u32::MAX, Ordering::Relaxed);
    }
    log_event(
        "INFO",
        &format!(
            "config file change detected: interval={}ms language={:?} theme={:?} \
                 dual_row={} show_resources={} show_network={} item_spacing={}px \
                 anchor={:?} left_margin={}px auto_start={}",
            loaded.sample.interval_ms,
            loaded.display.language,
            loaded.display.theme,
            loaded.display.dual_row,
            loaded.display.show_resources,
            loaded.display.show_network,
            loaded.display.item_spacing,
            loaded.windows.anchor,
            loaded.windows.left_margin_px,
            loaded.startup.auto_start,
        ),
    );
    // Re-seed the sample timer in case the interval changed. SetTimer
    // with the same hwnd+id replaces the running timer, so no KillTimer
    // is needed; if the replacement fails the old timer keeps ticking at
    // the previous interval instead of sampling stopping entirely.
    unsafe {
        let sentinel = sentinel_hwnd();
        if SetTimer(
            Some(sentinel),
            TIMER_SAMPLE,
            loaded.sample.interval_ms.clamp(500, 5000) as u32,
            None,
        ) == 0
        {
            log_event(
                "WARN",
                "config reload: SetTimer failed, keeping the previous sample interval",
            );
        }
    }
    // Reset the grow-only widths so a narrower layout takes effect, then
    // re-lay out every overlay from the last sample (no re-sample spike).
    OVERLAYS.with(|slot| {
        for ov in slot.borrow_mut().iter_mut() {
            ov.win_w = 0;
        }
    });
    relayout_all();
}
