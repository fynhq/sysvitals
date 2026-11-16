//! macOS config watching and launch-at-login state.

use super::*;

// Login-item retry is only armed by a file-backed apply; a missing config cannot start auto-start on its own.
fn retry_auto_start_if_due() {
    let cfg = CONFIG.with(|c| c.borrow().clone());
    AUTO_START.with(|controller| {
        controller
            .borrow_mut()
            .retry_if_due(true, &cfg, set_launch_at_login_outcome);
    });
}

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
        AUTO_START.with(|controller| controller.borrow_mut().reset());
    }
    log_event("INFO", &format!("config reloaded: {}", cfg.summary()));
}

pub(crate) fn set_launch_at_login(enabled: bool) -> Result<Option<isize>, String> {
    let cls = AnyClass::get(c"SMAppService")
        .ok_or_else(|| "SMAppService unavailable (requires macOS 13)".to_string())?;
    if cls.class_method(sel!(mainAppService)).is_none() {
        return Err("SMAppService.mainAppService unavailable".to_string());
    }
    let service: *mut AnyObject = unsafe { msg_send![cls, mainAppService] };
    if service.is_null() {
        return Err("SMAppService.mainAppService unavailable".to_string());
    }
    let status: isize = unsafe { msg_send![service, status] };
    if enabled {
        // Already enabled or awaiting approval in System Settings; registering again would only produce an error.
        if status == 1 {
            return Ok(None);
        }
        if status == 2 {
            return Ok(Some(status));
        }
    } else if status == 0 || status == 3 {
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

fn set_launch_at_login_outcome(desired: bool) -> Result<AutoStartOutcome, String> {
    match set_launch_at_login(desired)? {
        None => Ok(AutoStartOutcome::Unchanged),
        Some(2) if desired => Ok(AutoStartOutcome::PendingApproval),
        Some(status) => Ok(AutoStartOutcome::Changed {
            detail: Some(format!("status={status}")),
        }),
    }
}

pub(crate) fn apply_auto_start(cfg: &Config) {
    AUTO_START.with(|controller| {
        controller
            .borrow_mut()
            .apply(cfg, set_launch_at_login_outcome);
    });
}
