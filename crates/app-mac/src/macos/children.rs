//! children: split out of `macos` (see `macos/mod.rs` for the module map).

use super::*;
use std::time::{Duration, Instant};

/// Fallback for a freshly spawned History process that never reports its
/// window. The child normally writes a ready marker as soon as the popup
/// window exists (see [`HISTORY_READY_FILE`]), so a visible popup is
/// closable on the very next click; this timeout only protects a launch
/// that is still creating its window and lets a wedged child be dismissed.
const HISTORY_LAUNCH_GRACE: Duration = Duration::from_secs(3);

/// Marker file under the data directory that the History child writes its
/// PID into once its popup window has been created. A live child without
/// this marker is still starting: killing it then would leave the user with
/// no popup at all (fast second click / double-click). The PID contents make
/// a leftover file from an earlier or crashed launch harmless.
const HISTORY_READY_FILE: &str = "history-window.ready";

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

/// Whether a click on a live child should close the popup. A reported
/// window is always closable; an unreported one stays protected until the
/// fallback grace expires (covers a failed marker write and a wedged child).
fn should_close_popup(elapsed: Option<Duration>, ready: bool) -> bool {
    ready || !history_starting(elapsed)
}

/// Path of the ready marker under the data directory.
fn history_ready_path() -> Option<std::path::PathBuf> {
    sysvitals_core::config::data_dir()
        .ok()
        .map(|dir| dir.join(HISTORY_READY_FILE))
}

/// Remove any marker left by an earlier launch.
fn clear_history_ready() {
    if let Some(path) = history_ready_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// PID stored in a ready-marker file. Trailing whitespace/newlines are
/// tolerated; anything else counts as "not ready".
fn marker_pid(text: &str) -> Option<u32> {
    text.trim().parse().ok()
}

/// Whether the live child reported its window. Matching the PID keeps a
/// stale marker from making a fresh child look ready before its window
/// exists.
fn history_reported_ready(child_pid: u32) -> bool {
    history_ready_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| marker_pid(&text))
        == Some(child_pid)
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

/// PID of a live child in `slot`, reaping it once it has exited. A waitpid
/// error means the child can no longer be managed (typically already
/// reaped); treating it as alive used to permanently block the next launch
/// for that panel.
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

/// Whether a previously spawned child (held in `slot`) is still running.
/// Reaps it once it has exited.
pub(crate) fn child_alive(slot: &RefCell<Option<Child>>) -> bool {
    live_child_pid(slot).is_some()
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
                forget_history_launch();
            }
        }
    });
}

/// Drop the launch bookkeeping once no History child is running: the spawn
/// instant and the ready marker. Both are per-launch state; a stale marker
/// carries an old PID and is harmless, but leaving it would mislead log
/// readers and hide the next child's "not ready yet" phase.
fn forget_history_launch() {
    HISTORY_LAUNCHED_AT.with(|launched| launched.set(None));
    clear_history_ready();
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

/// Spawn one panel binary with the shared parent/anchor arguments. `prepare`
/// adds panel-specific flags; `record` stores the live child in the
/// panel-specific slot.
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
    // Open the panel on the display that hosts the clicked status item
    // instead of always centering on the primary screen.
    append_anchor_args(&mut cmd);
    prepare(&mut cmd);
    match cmd.spawn() {
        Ok(child) => record(child),
        Err(e) => log_event("WARN", &format!("{label}: spawn failed: {e}")),
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
    launch_panel(
        "SysVitalsSettings",
        "launch_settings",
        |_| {},
        |child| {
            SETTINGS_CHILD.with(|slot| *slot.borrow_mut() = Some(child));
        },
    );
}

/// Launch the external history chart process (`SysVitalsHistory`), passing
/// the indicator's screen rect so the popup can anchor to it. One live
/// instance at a time. The child also gets `--ready-file` so it can tell the
/// indicator when its popup window exists.
pub(crate) fn launch_history() {
    if HISTORY_CHILD.with(child_alive) {
        return;
    }
    // A marker from an earlier launch must not make this child look ready
    // before its own window exists.
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

/// Close the trend popup if one is running: the click-to-toggle half of
/// `button_clicked:`. Kills the child and reaps it (`wait`) so no zombie
/// lingers under the long-running indicator. Returns `true` when a live
/// popup was closed; `false` when nothing was running (or it had already
/// exited, which is reaped here so the slot is free for the next open).
pub(crate) fn close_history() -> bool {
    let closed = HISTORY_CHILD.with(|slot| terminate_child(slot, "history popup"));
    forget_history_launch();
    closed
}

/// Toggle the history popup for a status-item click. A live child that has
/// not reported its window yet is left alone: GPUI has not created the popup
/// window, so treating a second click as "close" would kill the first launch
/// and leave the user with no popup at all (double-click / impatient second
/// click). As soon as the child writes its ready marker — i.e. the popup is
/// on screen — the next click closes it, without waiting out a fixed grace.
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

    #[test]
    fn a_reported_window_is_closable_immediately() {
        // The popup is on screen: no grace period applies.
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
        // A child that never reports (failed marker write, wedged startup)
        // must still be dismissible after the fallback.
        assert!(should_close_popup(Some(HISTORY_LAUNCH_GRACE), false));
        // A child not spawned by this process has no launch instant.
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
