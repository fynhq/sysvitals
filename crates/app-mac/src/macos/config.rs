//! config: split out of `macos` (see `macos/mod.rs` for the module map).

use super::*;

pub(crate) fn log_config_issues(issues: &[sysvitals_core::config::SanitizeIssue]) {
    sysvitals_support::log_sanitize_issues("config repair", issues);
}

/// A failed auto-start apply is retried without waiting for the user to touch
/// the setting again. The cooldown keeps a permanently failing
/// `SMAppService` call from logging on every sample tick.
const AUTO_START_RETRY_COOLDOWN: Duration = Duration::from_secs(300);

thread_local! {
    static AUTO_START_RETRY_AT: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

/// Retry a failed login-item apply on its cooldown, even when the config file
/// has not changed. The retry timer is only armed by a file-backed apply, so
/// a missing/never-loaded config cannot start the side effect on its own.
fn retry_auto_start_if_due() {
    let due = AUTO_START_RETRY_AT.with(|slot| slot.get().is_some_and(|at| Instant::now() >= at));
    if !due {
        return;
    }
    AUTO_START_RETRY_AT.with(|slot| slot.set(None));
    let cfg = CONFIG.with(|c| c.borrow().clone());
    apply_auto_start(&cfg);
}

/// Load or reload the config file. The watcher only advances its cache on a
/// successful parse, so a corrupt file keeps the last good config and is
/// retried when it changes again.
pub(crate) fn reload_config_if_changed() {
    let poll = CONFIG_WATCHER.with(|watcher| watcher.borrow_mut().poll());
    let (cfg, issues, source) = match poll {
        ConfigPoll::Unchanged => {
            retry_auto_start_if_due();
            return;
        }
        ConfigPoll::Failed { error, logged } => {
            if logged {
                log_event("WARN", &format!("config reload failed: {error}"));
            }
            return;
        }
        ConfigPoll::Loaded {
            config,
            issues,
            source,
        } => (config, issues, source),
    };
    log_config_issues(&issues);
    // The sampler fixes `cpu_smoothing` at construction; a changed value
    // drops it so the next tick rebuilds it (re-primes the CPU EMA and
    // re-seeds the network baseline — one "--" frame, the same cost as a
    // restart).
    let smoothing_changed =
        CONFIG.with(|c| c.borrow().sample.cpu_smoothing != cfg.sample.cpu_smoothing);
    CONFIG.with(|c| *c.borrow_mut() = cfg.clone());
    if smoothing_changed {
        SAMPLER.with(|s| *s.borrow_mut() = None);
        log_event(
            "INFO",
            &format!(
                "cpu_smoothing changed to {}: sampler rebuilds on the next tick",
                cfg.sample.cpu_smoothing
            ),
        );
    }
    if source == sysvitals_core::config::ConfigSource::File {
        apply_auto_start(&cfg);
    } else {
        // A missing file is not an explicit "off" choice; let a later
        // re-created config apply its value again.
        APPLIED_AUTO_START.with(|c| c.set(None));
        AUTO_START_RETRY_AT.with(|slot| slot.set(None));
    }
    log_event(
        "INFO",
        &format!(
            "config reloaded: interval={}ms dual_row={} show_resources={} \
                 show_network={} item_spacing={}px auto_start={}",
            cfg.sample.interval_ms,
            cfg.display.dual_row,
            cfg.display.show_resources,
            cfg.display.show_network,
            cfg.display.item_spacing,
            cfg.startup.auto_start,
        ),
    );
}

/// Ask `SMAppService` for the current login-item state. Returns the new
/// status when a call was made, `None` when the desired state already held.
/// Status values: 0 not registered, 1 enabled, 2 requires approval,
/// 3 not found (macOS 13+).
pub(crate) fn set_launch_at_login(enabled: bool) -> Result<Option<isize>, String> {
    let cls = AnyClass::get(c"SMAppService")
        .ok_or_else(|| "SMAppService unavailable (requires macOS 13)".to_string())?;
    // Swift exposes this class property as `mainApp`, but the Objective-C
    // selector is `mainAppService`. Sending `mainApp` raises an
    // unrecognized-selector exception, which unwinds into Rust and aborts
    // the whole process.
    if cls.class_method(sel!(mainAppService)).is_none() {
        return Err("SMAppService.mainAppService unavailable".to_string());
    }
    let service: *mut AnyObject = unsafe { msg_send![cls, mainAppService] };
    if service.is_null() {
        return Err("SMAppService.mainAppService unavailable".to_string());
    }
    let status: isize = unsafe { msg_send![service, status] };
    if enabled {
        // Already enabled, or waiting for the user's approval in System
        // Settings; registering again would only produce an error.
        if status == 1 {
            return Ok(None);
        }
        if status == 2 {
            return Ok(Some(status));
        }
    } else if status == 0 || status == 3 {
        // Nothing registered, or the app is not a bundle (e.g. a dev
        // binary): unregistering would only fail.
        return Ok(None);
    }

    let mut error: *mut AnyObject = std::ptr::null_mut();
    let ok: Bool = unsafe {
        if enabled {
            msg_send![service, registerAndReturnError: &mut error]
        } else {
            msg_send![service, unregisterAndReturnError: &mut error]
        }
    };
    let new_status: isize = unsafe { msg_send![service, status] };
    if ok.as_bool() {
        Ok(Some(new_status))
    } else {
        Err(format!("SMAppService status={new_status}"))
    }
}

/// Apply the login-startup preference once per value change. The settings
/// window writes the config; this process owns the ServiceManagement call.
pub(crate) fn apply_auto_start(cfg: &Config) {
    // See the Windows counterpart: an inherited `false` must not remove
    // a login item; an inherited `true` may still repair/register one.
    if !cfg.startup.auto_start_explicit && !cfg.startup.auto_start {
        // Leave the system state alone, but forget the last applied bit
        // so a later explicit/legacy `true` can still repair the entry.
        APPLIED_AUTO_START.with(|c| c.set(None));
        AUTO_START_RETRY_AT.with(|slot| slot.set(None));
        return;
    }
    let desired = cfg.startup.auto_start;
    if APPLIED_AUTO_START.with(|c| c.get()) == Some(desired) {
        AUTO_START_RETRY_AT.with(|slot| slot.set(None));
        return;
    }
    match set_launch_at_login(desired) {
        Ok(status) => {
            APPLIED_AUTO_START.with(|c| c.set(Some(desired)));
            AUTO_START_RETRY_AT.with(|slot| slot.set(None));
            match status {
                Some(2) if desired => log_event(
                    "INFO",
                    "auto-start requires approval in System Settings > Login Items",
                ),
                Some(status) => log_event(
                    "INFO",
                    &format!(
                        "auto-start {} (status={status})",
                        if desired { "enabled" } else { "disabled" }
                    ),
                ),
                None => {}
            }
        }
        Err(e) => {
            AUTO_START_RETRY_AT.with(|slot| {
                slot.set(Some(Instant::now() + AUTO_START_RETRY_COOLDOWN));
            });
            log_event(
                "WARN",
                &format!(
                    "auto-start {} failed: {e}",
                    if desired { "enable" } else { "disable" }
                ),
            );
        }
    }
}
