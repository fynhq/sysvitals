//! config: split out of `win` (see `win` for the module map).

use super::*;
use std::path::{Path, PathBuf};

// ------------------------------------------------------------ config access

pub(crate) fn config() -> Config {
    CONFIG
        .get_or_init(|| {
            // A missing or broken config file must not stop the indicator:
            // fall back to defaults and keep running, but leave a trace so
            // a parse error is diagnosable. The watcher seeds its mtime only
            // on a successful load; a failed initial read is retried by the
            // normal 500ms poll instead of pinning defaults forever.
            let loaded = match CONFIG_WATCHER.with(|watcher| watcher.borrow_mut().load_initial()) {
                Ok((c, issues, source)) => {
                    log_sanitize_issues("config repair", &issues);
                    CONFIG_LOAD_FAILED.store(false, Ordering::Relaxed);
                    CONFIG_FROM_FILE.store(
                        source == sysvitals_core::config::ConfigSource::File,
                        Ordering::Relaxed,
                    );
                    c
                }
                Err(e) => {
                    log_event("WARN", &format!("config load failed: {e}; using defaults"));
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

// ----------------------------------------------------------------- logging

// Logging (`log_event`) comes from `sysvitals-support` so every SysVitals
// process shares one implementation.

// ------------------------------------------------------------------ helpers

/// Resolve the per-user Startup folder through the shell so redirected
/// profiles (roaming or OneDrive AppData) land where Explorer reads it.
fn startup_dir() -> Result<PathBuf, String> {
    let raw = unsafe { SHGetKnownFolderPath(&FOLDERID_Startup, KF_FLAG_DEFAULT, None) }
        .map_err(|e| format!("startup folder lookup failed: {e}"))?;
    let text = unsafe { raw.to_string() };
    unsafe { CoTaskMemFree(Some(raw.0 as *const std::ffi::c_void)) };
    let text = text.map_err(|e| format!("startup folder path invalid: {e}"))?;
    Ok(PathBuf::from(text))
}

fn startup_link_path() -> Result<PathBuf, String> {
    Ok(startup_dir()?.join(STARTUP_LINK_NAME))
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

/// Read the target of an existing shortcut so an unchanged entry is not
/// rewritten on every launch. Missing, foreign, or unreadable links yield
/// `None` and are simply recreated.
unsafe fn shortcut_target(link_path: &Path) -> Option<PathBuf> {
    let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
    let persist: IPersistFile = link.cast().ok()?;
    let link_w = utf16(&link_path.to_string_lossy());
    persist.Load(PCWSTR(link_w.as_ptr()), STGM_READ).ok()?;
    let mut buf = [0u16; 1024];
    link.GetPath(&mut buf, std::ptr::null_mut(), 0).ok()?;
    let len = buf.iter().position(|&c| c == 0)?;
    if len == 0 {
        return None;
    }
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

/// Write `SysVitals.lnk` in the Startup folder pointing at this executable.
unsafe fn write_startup_shortcut(link_path: &Path, exe: &Path) -> Result<(), String> {
    let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
        .map_err(|e| format!("shortcut COM unavailable: {e}"))?;
    let exe_w = utf16(&exe.to_string_lossy());
    link.SetPath(PCWSTR(exe_w.as_ptr()))
        .map_err(|e| format!("shortcut target rejected: {e}"))?;
    if let Some(dir) = exe.parent() {
        let dir_w = utf16(&dir.to_string_lossy());
        // Best effort: the working directory does not affect startup.
        let _ = link.SetWorkingDirectory(PCWSTR(dir_w.as_ptr()));
    }
    let title = utf16("SysVitals");
    let _ = link.SetDescription(PCWSTR(title.as_ptr()));
    let _ = link.SetIconLocation(PCWSTR(exe_w.as_ptr()), 0);
    let persist: IPersistFile = link
        .cast()
        .map_err(|e| format!("shortcut persist interface unavailable: {e}"))?;
    let link_w = utf16(&link_path.to_string_lossy());
    persist
        .Save(PCWSTR(link_w.as_ptr()), true)
        .map_err(|e| format!("shortcut save failed ({}): {e}", link_path.display()))
}

/// Remove a Run value written by builds that used the registry. The shortcut
/// is authoritative now; a leftover value would launch a second copy at
/// login, so cleanup failure is logged but never masks the shortcut result.
unsafe fn remove_legacy_run_value() -> bool {
    let key = utf16(LEGACY_RUN_KEY);
    let value = utf16(LEGACY_RUN_VALUE);
    let result = RegDeleteKeyValueW(
        HKEY_CURRENT_USER,
        PCWSTR(key.as_ptr()),
        PCWSTR(value.as_ptr()),
    );
    if result.is_ok() {
        log_event("INFO", "auto-start: removed legacy Run entry");
        true
    } else if result != ERROR_FILE_NOT_FOUND {
        log_event(
            "WARN",
            &format!(
                "auto-start: legacy Run cleanup failed (registry error {})",
                result.0
            ),
        );
        false
    } else {
        false
    }
}

/// Create or remove the per-user Startup-folder shortcut. Returns whether the
/// system state actually changed. The link carries the current executable
/// path, so moving the folder and launching the new copy repairs the entry on
/// the next start.
pub(crate) unsafe fn set_auto_start(enabled: bool) -> Result<bool, String> {
    let link_path = startup_link_path()?;
    let mut changed = if enabled {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        if shortcut_target(&link_path).is_some_and(|target| same_path(&target, &exe)) {
            false
        } else {
            write_startup_shortcut(&link_path, &exe)?;
            true
        }
    } else {
        match std::fs::remove_file(&link_path) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => {
                return Err(format!(
                    "shortcut remove failed ({}): {e}",
                    link_path.display()
                ))
            }
        }
    };
    if remove_legacy_run_value() {
        changed = true;
    }
    Ok(changed)
}

thread_local! {
    /// Shared login-startup policy; the platform side effect is the Startup
    /// shortcut below. The controller owns the explicit-bit rule, the applied
    /// cache and the retry cooldown.
    static AUTO_START: RefCell<AutoStartController> = RefCell::new(AutoStartController::new());
}

fn set_auto_start_outcome(desired: bool) -> Result<AutoStartOutcome, String> {
    unsafe { set_auto_start(desired) }.map(|changed| {
        if changed {
            AutoStartOutcome::Changed { detail: None }
        } else {
            AutoStartOutcome::Unchanged
        }
    })
}

/// Apply the login-startup preference once per value change. The settings
/// window writes the config; this process owns the Startup-folder side effect.
pub(crate) fn apply_auto_start(cfg: &Config) {
    AUTO_START.with(|controller| controller.borrow_mut().apply(cfg, set_auto_start_outcome));
}

/// Retry a failed auto-start apply on its cooldown, even when the config file
/// has not changed. Only a file-backed, successfully parsed config may drive
/// this side effect.
fn retry_auto_start_if_due() {
    let cfg = config();
    AUTO_START.with(|controller| {
        controller.borrow_mut().retry_if_due(
            CONFIG_FROM_FILE.load(Ordering::Relaxed),
            &cfg,
            set_auto_start_outcome,
        );
    });
}

// ------------------------------------------------- config file watching

/// Called on each TIMER_CONFIG tick. Compares the config file's mtime
/// against the cached value; on change, reloads the config, re-seeds the
/// sample timer (interval may have changed), and re-lays out every overlay.
pub(crate) fn poll_config_change() {
    let poll = CONFIG_WATCHER
        .with(|watcher| sysvitals_support::poll_config(&mut watcher.borrow_mut(), "config reload"));
    let (loaded, source) = match poll {
        ConfigEvent::Unchanged => {
            retry_auto_start_if_due();
            return;
        }
        ConfigEvent::Failed => return,
        ConfigEvent::Loaded {
            config,
            issues,
            source,
        } => {
            log_sanitize_issues("config repair", &issues);
            (config, source)
        }
    };
    CONFIG_FROM_FILE.store(
        source == sysvitals_core::config::ConfigSource::File,
        Ordering::Relaxed,
    );
    CONFIG_LOAD_FAILED.store(false, Ordering::Relaxed);
    store_config(loaded.clone());
    // A missing file (for example after saving a corrupt config aside)
    // is not an explicit "auto-start off" choice. Only an existing,
    // parsed file may apply the platform side effect.
    if source == sysvitals_core::config::ConfigSource::File {
        apply_auto_start(&loaded);
    } else {
        // Let a later re-created config apply its value again, even if it
        // matches the last one we applied.
        AUTO_START.with(|controller| controller.borrow_mut().reset());
    }
    log_event(
        "INFO",
        &format!("config file change detected: {}", loaded.summary()),
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
