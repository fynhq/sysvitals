//! children: split out of `macos` (see `macos/mod.rs` for the module map).

use super::*;
use std::time::{Duration, Instant};

/// How long a freshly spawned History process is treated as "starting" for
/// click-toggle purposes. Killing a process that has not created its window
/// yet makes a fast second click/double-click look like "nothing happened";
/// ignoring the toggle during this window keeps the first launch alive.
const HISTORY_LAUNCH_GRACE: Duration = Duration::from_secs(3);

thread_local! {
    /// When the current History child was spawned. `None` when no child was
    /// spawned in this process (or it has been reaped).
    static HISTORY_LAUNCHED_AT: std::cell::Cell<Option<Instant>> =
        const { std::cell::Cell::new(None) };
}

/// Whether a live History child is still inside its launch grace period.
fn history_starting(elapsed: Option<Duration>) -> bool {
    elapsed.is_some_and(|elapsed| elapsed < HISTORY_LAUNCH_GRACE)
}

/// Directory containing the running executable.
pub(crate) fn exe_dir() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|p| p.to_path_buf())
}

/// Path to relaunch this program as: the enclosing `.app` bundle when
/// running from one, otherwise the executable itself.
///
/// `open` resolves a bundle path through LaunchServices, which applies the
/// bundle's Info.plist (notably `LSUIElement`) and reuses the registered
/// application identity. Handing it the raw binary inside
/// `Contents/MacOS/` instead bypasses that registration, so the relaunched
/// process may not be treated as the same app.
pub(crate) fn relaunch_target() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let bundle = exe
        .ancestors()
        .find(|p| p.extension() == Some(std::ffi::OsStr::new("app")))?;
    Some(bundle.to_path_buf())
}

/// Whether a previously spawned child (held in `slot`) is still running.
/// Reaps it once it has exited.
pub(crate) fn child_alive(slot: &RefCell<Option<Child>>) -> bool {
    let mut borrow = slot.borrow_mut();
    match borrow.as_mut() {
        Some(child) => match child.try_wait() {
            Ok(Some(_status)) => {
                *borrow = None;
                false
            }
            Ok(None) => true,
            // A waitpid error means the child can no longer be managed
            // (typically already reaped). Treating it as alive used to
            // permanently block the next launch for that panel.
            Err(e) => {
                log_event("WARN", &format!("child wait failed: {e}"));
                *borrow = None;
                false
            }
        },
        None => false,
    }
}

/// Reap children that exited on their own so a long-running indicator does
/// not accumulate zombies between launches.
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
                HISTORY_LAUNCHED_AT.with(|launched| launched.set(None));
            }
        }
    });
}

/// Terminate a child panel and reap it so no zombie remains.
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
            // The kill guarantees a prompt exit; the wait only reaps.
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

/// Close both child panels on restart/quit. Leaving them alive makes a
/// restarted instance open a second History popup and leaves Settings
/// orphaned after Quit.
pub(crate) fn close_children() {
    let _ = close_history();
    let _ = close_settings();
}

/// Pass the indicator's anchor and hosting display to a child panel. The
/// rectangle is in that display's local GPUI space (see `anchor_rect`), and
/// the native display id lets the panel select the screen explicitly on
/// mixed-DPI/multi-display setups instead of probing the rectangle against
/// every screen's local bounds.
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

/// Launch the external GPUI settings process (`SysVitalsSettings`), one
/// live instance at a time.
pub(crate) fn launch_settings() {
    // `thread_local!` statics expose a `LocalKey`; reach the cell through
    // `with`, and re-entrancy (`child_alive` borrows it) must not nest, so
    // query liveness first, then spawn, then store.
    if SETTINGS_CHILD.with(child_alive) {
        return;
    }
    let Some(dir) = exe_dir() else {
        log_event("WARN", "launch_settings: cannot determine exe directory");
        return;
    };
    let settings = dir.join("SysVitalsSettings");
    if !settings.exists() {
        log_event(
            "WARN",
            &format!("launch_settings: not found: {}", settings.display()),
        );
        return;
    }
    let parent_pid = std::process::id().to_string();
    let mut cmd = std::process::Command::new(settings);
    cmd.args(["--parent-pid", parent_pid.as_str()]);
    // Let the settings window open on the display that hosts the clicked
    // status item instead of always centering on the primary screen.
    append_anchor_args(&mut cmd);
    match cmd.spawn() {
        Ok(child) => {
            SETTINGS_CHILD.with(|slot| *slot.borrow_mut() = Some(child));
        }
        Err(e) => log_event("WARN", &format!("launch_settings: spawn failed: {e}")),
    }
}

/// Launch the external history chart process (`SysVitalsHistory`), passing
/// the indicator's screen rect so the popup can anchor to it. One live
/// instance at a time.
pub(crate) fn launch_history() {
    if HISTORY_CHILD.with(child_alive) {
        return;
    }
    let Some(dir) = exe_dir() else {
        log_event("WARN", "launch_history: cannot determine exe directory");
        return;
    };
    let history = dir.join("SysVitalsHistory");
    if !history.exists() {
        log_event(
            "WARN",
            &format!("launch_history: not found: {}", history.display()),
        );
        return;
    }
    let parent_pid = std::process::id().to_string();
    let mut cmd = std::process::Command::new(history);
    cmd.args(["--parent-pid", parent_pid.as_str()]);
    append_anchor_args(&mut cmd);
    match cmd.spawn() {
        Ok(child) => {
            HISTORY_LAUNCHED_AT.with(|launched| launched.set(Some(Instant::now())));
            HISTORY_CHILD.with(|slot| *slot.borrow_mut() = Some(child));
        }
        Err(e) => log_event("WARN", &format!("launch_history: spawn failed: {e}")),
    }
}

/// Close the trend popup if one is running: the click-to-toggle half of
/// `button_clicked:`. Kills the child and reaps it (`wait`) so no zombie
/// lingers under the long-running indicator. Returns `true` when a live
/// popup was closed; `false` when nothing was running (or it had already
/// exited, which is reaped here so the slot is free for the next open).
pub(crate) fn close_history() -> bool {
    let closed = HISTORY_CHILD.with(|slot| terminate_child(slot, "history popup"));
    HISTORY_LAUNCHED_AT.with(|launched| launched.set(None));
    closed
}

/// Toggle the history popup for a status-item click. A child that is still
/// inside its launch grace period is left alone: GPUI has not created its
/// window yet, so treating a second click as "close" would kill the first
/// launch and leave the user with no popup at all (double-click / impatient
/// second click).
pub(crate) fn toggle_history() {
    if HISTORY_CHILD.with(child_alive) {
        let elapsed = HISTORY_LAUNCHED_AT.with(|launched| launched.get().map(|at| at.elapsed()));
        if history_starting(elapsed) {
            log_event(
                "INFO",
                "history: popup is still starting; ignoring toggle until its first window appears",
            );
            return;
        }
        if close_history() {
            return;
        }
        // The child exited between the liveness check and the close; fall
        // through and launch a fresh popup.
    }
    launch_history();
}

/// Open Activity Monitor (macOS equivalent of Task Manager). The `open`
/// helper is short-lived, but `status()` would still block the menu-bar main
/// thread while LaunchServices resolves the bundle. Spawn it and reap it on a
/// detached thread so the click returns immediately and no zombie remains.
pub(crate) fn open_activity_monitor() {
    // Resolve the system app by bundle id rather than its localized
    // display name; `open -b` works on non-English systems too.
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

/// Restart: schedule a delayed relaunch, then exit.
pub(crate) fn restart_self() {
    // Prefer the enclosing .app bundle; fall back to the bare executable
    // when running unpackaged (e.g. straight from target/release).
    let Some(target) = relaunch_target().or_else(|| std::env::current_exe().ok()) else {
        return;
    };
    let target_str = target.to_string_lossy().to_string();
    // Pass the path as a positional parameter instead of interpolating it
    // into the shell program: a quote in the install path must not break
    // the relaunch command.
    let _ = std::process::Command::new("/bin/sh")
        .args(["-c", "sleep 2; open \"$1\"", "sh", target_str.as_str()])
        .spawn();
    // Close child panels before exiting: a restarted instance starts with
    // empty slots and would otherwise open a second History popup.
    close_children();
    // Exit current instance.
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
}
