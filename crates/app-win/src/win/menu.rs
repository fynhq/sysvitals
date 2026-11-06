//! menu: split out of `win` (see `win` for the module map).

use super::*;

// -------------------------------------------------------------------- menu

/// Launch the system resource viewer (Task Manager). This action used to
/// live on a double-click of the indicator; putting it in the context
/// menu is what lets the left click open the trend chart immediately,
/// without waiting for a possible second press.
pub unsafe fn launch_taskmgr() {
    let verb = utf16("open");
    let target = utf16("taskmgr.exe");
    let _ = ShellExecuteW(
        None,
        windows::core::PCWSTR(verb.as_ptr()),
        windows::core::PCWSTR(target.as_ptr()),
        windows::core::PCWSTR::null(),
        windows::core::PCWSTR::null(),
        SW_SHOWNORMAL,
    );
}

/// One row of the context menu. Separators use `id == 0` and an empty
/// label.
#[derive(Clone)]
pub struct MenuEntry {
    pub id: usize,
    pub label: String,
    pub separator: bool,
}

pub fn menu_entries(lang: Language) -> Vec<MenuEntry> {
    // Order: system resource viewer first (most actionable when the
    // indicator is in the way), then Settings, then the lifecycle group.
    vec![
        MenuEntry {
            id: IDM_TASKMGR,
            label: t(lang, "menu.task_manager").to_string(),
            separator: false,
        },
        MenuEntry {
            id: IDM_SETTINGS,
            label: t(lang, "menu.settings").to_string(),
            separator: false,
        },
        MenuEntry {
            id: 0,
            label: String::new(),
            separator: true,
        },
        MenuEntry {
            id: IDM_RESTART,
            label: t(lang, "menu.restart").to_string(),
            separator: false,
        },
        MenuEntry {
            id: IDM_QUIT,
            label: t(lang, "menu.quit").to_string(),
            separator: false,
        },
    ]
}

/// The context menu is the system popup menu. It is the only menu
/// surface the shell renders natively, so it keeps system theming,
/// hit-testing and dismissal semantics without a second message loop.
pub unsafe fn show_native_menu(owner: HWND, entries: &[MenuEntry]) -> Option<usize> {
    let menu = CreatePopupMenu().ok()?;
    for entry in entries {
        if entry.separator {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, windows::core::PCWSTR::null());
        } else {
            let label = utf16(&entry.label);
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                entry.id,
                windows::core::PCWSTR(label.as_ptr()),
            );
        }
    }
    let mut point = POINT::default();
    let _ = GetCursorPos(&mut point);
    // The overlay is WS_EX_NOACTIVATE, so the classic menu needs the
    // documented tray pattern to dismiss on a click-away: foreground the
    // owner first, then post WM_NULL after it returns.
    let _ = SetForegroundWindow(owner);
    let choice = TrackPopupMenu(
        menu,
        TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD,
        point.x,
        point.y,
        None,
        owner,
        None,
    )
    .0 as usize;
    let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
    Some(choice)
}

pub unsafe fn perform_menu_choice(hwnd: HWND, choice: usize) {
    match choice {
        id if id == IDM_TASKMGR => launch_taskmgr(),
        id if id == IDM_SETTINGS => launch_settings(hwnd),
        id if id == IDM_RESTART => restart_self(),
        id if id == IDM_QUIT => {
            close_children();
            shutdown_and_quit();
        }
        _ => {}
    }
}

/// Destroy every overlay, stop the sentinel timers and exit the message
/// loop. Shared by Quit and Restart (which schedules a relaunch first).
/// `QUITTING` is set before the windows are torn down so their
/// `WM_DESTROY` handlers treat it as an intentional exit.
pub unsafe fn shutdown_and_quit() {
    QUITTING.store(true, Ordering::Relaxed);
    let hwnds: Vec<HWND> = OVERLAYS.with(|slot| slot.borrow().iter().map(|o| o.hwnd).collect());
    for hwnd in hwnds {
        let _ = DestroyWindow(hwnd);
    }
    OVERLAYS.with(|slot| slot.borrow_mut().clear());
    let sentinel = sentinel_hwnd();
    let _ = KillTimer(Some(sentinel), TIMER_SAMPLE);
    let _ = KillTimer(Some(sentinel), TIMER_MOVE);
    let _ = KillTimer(Some(sentinel), TIMER_CONFIG);
    let _ = KillTimer(Some(sentinel), TIMER_HISTORY);
    let _ = KillTimer(Some(sentinel), TIMER_WATCH);
    PostQuitMessage(0);
}

pub unsafe fn show_context_menu(hwnd: HWND) {
    let entries = menu_entries(config().display.language);
    // Keep maintain_topmost from lifting the overlay above the menu while
    // it is open (the modal loop still dispatches our WM_TIMER).
    MENU_TRACKING.store(true, Ordering::Relaxed);
    let choice = match show_native_menu(hwnd, &entries) {
        Some(choice) => choice,
        None => {
            log_event("WARN", "menu: the system menu could not be created");
            0
        }
    };
    MENU_TRACKING.store(false, Ordering::Relaxed);
    perform_menu_choice(hwnd, choice);
}
