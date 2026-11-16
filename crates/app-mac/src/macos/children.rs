//! History child launch, readiness markers, and teardown.

use super::*;
use std::time::{Duration, Instant};

// The history child writes a PID ready marker when its window exists; an unreported child gets a 3 s grace.
const HISTORY_LAUNCH_GRACE: Duration = Duration::from_secs(3);

const HISTORY_READY_FILE: &str = "history-window.ready";

thread_local! {
    static HISTORY_LAUNCHED_AT: std::cell::Cell<Option<Instant>> =
        const { std::cell::Cell::new(None) };
}

fn history_starting(elapsed: Option<Duration>) -> bool {
    elapsed.is_some_and(|elapsed| elapsed < HISTORY_LAUNCH_GRACE)
}

fn should_close_popup(elapsed: Option<Duration>, ready: bool) -> bool {
    ready || !history_starting(elapsed)
}

fn history_ready_path() -> Option<std::path::PathBuf> {
    sysvitals_core::config::data_dir()
        .ok()
        .map(|dir| dir.join(HISTORY_READY_FILE))
}

fn clear_history_ready() {
    if let Some(path) = history_ready_path() {
        let _ = std::fs::remove_file(path);
    }
}

fn marker_pid(text: &str) -> Option<u32> {
    text.trim().parse().ok()
}

fn history_reported_ready(child_pid: u32) -> bool {
    history_ready_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| marker_pid(&text))
        == Some(child_pid)
}

pub(crate) fn exe_dir() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|p| p.to_path_buf())
}

pub(crate) fn relaunch_target() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let bundle = exe
        .ancestors()
        .find(|p| p.extension() == Some(std::ffi::OsStr::new("app")))?;
    Some(bundle.to_path_buf())
}

fn live_child_pid(slot: &RefCell<Option<Child>>) -> Option<u32> {
    let mut borrow = slot.borrow_mut();
    match borrow.as_mut() {
        Some(child) => match child.try_wait() {
            Ok(Some(_status)) => {
                *borrow = None;
                None
            }
            Ok(None) => Some(child.id()),
            Err(e) => {
                log_event("WARN", &format!("child wait failed: {e}"));
                *borrow = None;
                None
            }
        },
        None => None,
    }
}

pub(crate) fn child_alive(slot: &RefCell<Option<Child>>) -> bool {
    live_child_pid(slot).is_some()
}

pub(crate) fn reap_children() {
    SETTINGS_CHILD.with(|slot| {
        let mut borrow = slot.borrow_mut();
        if let Some(child) = borrow.as_mut() {
            if matches!(child.try_wait(), Ok(Some(_))) {
                *borrow = None;
            }
        }
    });
    HISTORY_CHILD.with(|slot| {
        let mut borrow = slot.borrow_mut();
        if let Some(child) = borrow.as_mut() {
            if matches!(child.try_wait(), Ok(Some(_))) {
                *borrow = None;
                forget_history_launch();
            }
        }
    });
}

fn forget_history_launch() {
    HISTORY_LAUNCHED_AT.with(|launched| launched.set(None));
    clear_history_ready();
}

pub(crate) fn terminate_child(slot: &RefCell<Option<Child>>, label: &str) -> bool {
    let mut borrow = slot.borrow_mut();
    let Some(child) = borrow.as_mut() else {
        return false;
    };
    match child.try_wait() {
        Ok(Some(_)) => {
            *borrow = None;
            false
        }
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            *borrow = None;
            log_event("INFO", &format!("{label} process closed by indicator"));
            true
        }
    }
}

pub(crate) fn close_settings() -> bool {
    SETTINGS_CHILD.with(|slot| terminate_child(slot, "settings window"))
}

pub(crate) fn close_children() {
    let _ = close_history();
    let _ = close_settings();
}

fn append_anchor_args(cmd: &mut std::process::Command) {
    let Some((l, t, r, b, display)) = anchor_rect() else {
        return;
    };
    let l = l.to_string();
    let t = t.to_string();
    let r = r.to_string();
    let b = b.to_string();
    cmd.args(["--anchor", l.as_str(), t.as_str(), r.as_str(), b.as_str()]);
    if display != 0 {
        let display = display.to_string();
        cmd.args(["--anchor-monitor", display.as_str()]);
    }
}

fn launch_panel(
    exe_name: &str,
    label: &str,
    prepare: impl FnOnce(&mut std::process::Command),
    record: impl FnOnce(Child),
) {
    let Some(dir) = exe_dir() else {
        log_event("WARN", &format!("{label}: cannot determine exe directory"));
        return;
    };
    let path = dir.join(exe_name);
    if !path.exists() {
        log_event("WARN", &format!("{label}: not found: {}", path.display()));
        return;
    }
    let parent_pid = std::process::id().to_string();
    let mut cmd = std::process::Command::new(path);
    cmd.args(["--parent-pid", parent_pid.as_str()]);
    append_anchor_args(&mut cmd);
    prepare(&mut cmd);
    match cmd.spawn() {
        Ok(child) => record(child),
        Err(e) => log_event("WARN", &format!("{label}: spawn failed: {e}")),
    }
}

pub(crate) fn launch_settings() {
    if SETTINGS_CHILD.with(child_alive) {
        return;
    }
    launch_panel(
        "SysVitalsSettings",
        "launch_settings",
        |_| {},
        |child| {
            SETTINGS_CHILD.with(|slot| *slot.borrow_mut() = Some(child));
        },
    );
}

pub(crate) fn launch_history() {
    if HISTORY_CHILD.with(child_alive) {
        return;
    }
    clear_history_ready();
    let ready_path = history_ready_path();
    launch_panel(
        "SysVitalsHistory",
        "launch_history",
        |cmd| {
            if let Some(path) = ready_path.as_ref() {
                cmd.arg("--ready-file").arg(path);
            }
        },
        |child| {
            HISTORY_LAUNCHED_AT.with(|launched| launched.set(Some(Instant::now())));
            HISTORY_CHILD.with(|slot| *slot.borrow_mut() = Some(child));
        },
    );
}

pub(crate) fn close_history() -> bool {
    let closed = HISTORY_CHILD.with(|slot| terminate_child(slot, "history popup"));
    forget_history_launch();
    closed
}

pub(crate) fn toggle_history() {
    if let Some(pid) = HISTORY_CHILD.with(live_child_pid) {
        let elapsed = HISTORY_LAUNCHED_AT.with(|launched| launched.get().map(|at| at.elapsed()));
        let ready = history_reported_ready(pid);
        if !should_close_popup(elapsed, ready) {
            log_event(
                "INFO",
                "history: popup window not created yet; ignoring toggle until it reports ready",
            );
            return;
        }
        if close_history() {
            return;
        }
    }
    launch_history();
}

pub(crate) fn open_activity_monitor() {
    match std::process::Command::new("open")
        .args(["-b", "com.apple.ActivityMonitor"])
        .spawn()
    {
        Ok(mut child) => {
            let _ = std::thread::Builder::new()
                .name("sysvitals-open-activity".to_string())
                .spawn(move || {
                    let _ = child.wait();
                });
        }
        Err(e) => log_event("WARN", &format!("open Activity Monitor failed: {e}")),
    }
}

pub(crate) fn restart_self() {
    let Some(target) = relaunch_target().or_else(|| std::env::current_exe().ok()) else {
        return;
    };
    let target_str = target.to_string_lossy().to_string();
    let _ = std::process::Command::new("/bin/sh")
        .args(["-c", "sleep 2; open \"$1\"", "sh", target_str.as_str()])
        .spawn();
    close_children();
    let mtm = mtm();
    let app = NSApplication::sharedApplication(mtm);
    app.terminate(None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_fresh_child_is_treated_as_starting() {
        assert!(!history_starting(None));
        assert!(history_starting(Some(Duration::ZERO)));
        assert!(history_starting(Some(
            HISTORY_LAUNCH_GRACE - Duration::from_millis(1)
        )));
        assert!(!history_starting(Some(HISTORY_LAUNCH_GRACE)));
        assert!(!history_starting(Some(
            HISTORY_LAUNCH_GRACE + Duration::from_secs(1)
        )));
    }

    #[test]
    fn a_reported_window_is_closable_immediately() {
        assert!(should_close_popup(Some(Duration::ZERO), true));
        assert!(should_close_popup(Some(Duration::from_millis(200)), true));
    }

    #[test]
    fn an_unreported_child_stays_protected_until_the_fallback() {
        assert!(!should_close_popup(Some(Duration::from_millis(500)), false));
        assert!(!should_close_popup(
            Some(HISTORY_LAUNCH_GRACE - Duration::from_millis(1)),
            false
        ));
        assert!(should_close_popup(Some(HISTORY_LAUNCH_GRACE), false));
        assert!(should_close_popup(None, false));
    }

    #[test]
    fn ready_marker_parses_a_trailing_newline() {
        assert_eq!(marker_pid("4242\n"), Some(4242));
        assert_eq!(marker_pid(" 4242 "), Some(4242));
        assert_eq!(marker_pid(""), None);
        assert_eq!(marker_pid("not-a-pid"), None);
    }
}
