//! config: split out of `macos` (see `macos/mod.rs` for the module map).

use super::*;

/// Retry a failed login-item apply on its cooldown, even when the config file
/// has not changed. The retry timer is only armed by a file-backed apply, so
/// a missing/never-loaded config cannot start the side effect on its own.
fn retry_auto_start_if_due() {
    let cfg = CONFIG.with(|c| c.borrow().clone());
    AUTO_START.with(|controller| {
        controller
            .borrow_mut()
            .retry_if_due(true, &cfg, set_launch_at_login_outcome);
    });
}

/// Load or reload the config file. The watcher only advances its cache on a
/// successful parse, so a corrupt file keeps the last good config and is
/// retried when it changes again.
pub(crate) fn reload_config_if_changed() {
    let poll = CONFIG_WATCHER
        .with(|watcher| sysvitals_support::poll_config(&mut watcher.borrow_mut(), "config reload"));
    let (cfg, issues, source) = match poll {
        ConfigEvent::Unchanged => {
            retry_auto_start_if_due();
            return;
        }
        ConfigEvent::Failed => return,
        ConfigEvent::Loaded {
            config,
            issues,
            source,
        } => (config, issues, source),
    };
    log_sanitize_issues("config repair", &issues);
    CONFIG.with(|c| *c.borrow_mut() = cfg.clone());
    if source == sysvitals_core::config::ConfigSource::File {
        apply_auto_start(&cfg);
    } else {
        // A missing file is not an explicit "off" choice; let a later
        // re-created config apply its value again.
        AUTO_START.with(|controller| controller.borrow_mut().reset());
    }
    log_event("INFO", &format!("config reloaded: {}", cfg.summary()));
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

/// Map the `SMAppService` result onto the shared auto-start outcome.
fn set_launch_at_login_outcome(desired: bool) -> Result<AutoStartOutcome, String> {
    match set_launch_at_login(desired)? {
        None => Ok(AutoStartOutcome::Unchanged),
        Some(2) if desired => Ok(AutoStartOutcome::PendingApproval),
        Some(status) => Ok(AutoStartOutcome::Changed {
            detail: Some(format!("status={status}")),
        }),
    }
}

/// Apply the login-startup preference once per value change. The settings
/// window writes the config; this process owns the ServiceManagement call.
pub(crate) fn apply_auto_start(cfg: &Config) {
    AUTO_START.with(|controller| {
        controller
            .borrow_mut()
            .apply(cfg, set_launch_at_login_outcome);
    });
}
