//! wndproc: split out of `win` (see `win` for the module map).

use super::*;

// ----------------------------------------------------------------- wndproc

/// Left-click toggle. Opens the popup anchored at this indicator when it
/// is closed. When it is open, a click on the same monitor's indicator
/// closes it (toggle); a click on a different monitor's indicator moves it
/// there. `visible` (owned by the child) is the actual open state;
/// `desired_open` alone is not (it stays 1 after a blur/Esc dismissal).
pub fn handle_history_click(hwnd: HWND) {
    if !history_child_alive() {
        history_mark_stopped();
        HISTORY_ANCHOR_MONITOR.store(0, Ordering::Relaxed);
    }
    let visible = history_snapshot().is_some_and(|state| state.visible != 0);
    let this_monitor = overlay_by_hwnd(hwnd).map(|o| o.monitor).unwrap_or(0);
    if !visible {
        ensure_history_resident();
        if history_open_request(hwnd) {
            HISTORY_ANCHOR_MONITOR.store(this_monitor, Ordering::Relaxed);
        }
    } else if HISTORY_ANCHOR_MONITOR.load(Ordering::Relaxed) == this_monitor {
        history_close_request();
        HISTORY_ANCHOR_MONITOR.store(0, Ordering::Relaxed);
    } else {
        // A different monitor's indicator was clicked while the popup is
        // open: republish the open request with this indicator's anchor.
        // The resident loop sees the generation change while a window is
        // open and reopens it there.
        log_event(
            "INFO",
            "history: moving popup to the clicked indicator's screen",
        );
        if history_open_request(hwnd) {
            HISTORY_ANCHOR_MONITOR.store(this_monitor, Ordering::Relaxed);
        }
    }
}

/// Whether a `WM_SETTINGCHANGE` broadcast affects the taskbar's appearance,
/// metrics or clock format. `lParam == 0` is the documented "all settings"
/// broadcast and is always honored; other categories (environment, policy,
/// input language, ...) must not trigger registry reads and a full relayout
/// on every taskbar.
fn relevant_setting_change(lparam: LPARAM) -> bool {
    if lparam.0 == 0 {
        return true;
    }
    let ptr = lparam.0 as *const u16;
    let mut len = 0usize;
    // The payload is a NUL-terminated string owned by the sender. Bound the
    // scan so a malformed pointer cannot run away.
    while len < 256 && unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    let name = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(ptr, len) });
    matches!(
        name.as_str(),
        "ImmersiveColorSet" | "WindowMetrics" | "intl" | "HighContrast"
    )
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_SETTINGCHANGE => {
            if !relevant_setting_change(lparam) {
                return LRESULT(0);
            }
            // Theme, text-size and font changes arrive here. Re-read the
            // theme, drop the cached font metrics and re-measure: the
            // previous code repainted with the old width, clipping the
            // new weight/size until the next sample tick. The UIA clock
            // anchor moves when the text size or time format changes, so
            // its cache must be dropped as well (it is keyed by the bar
            // rect, which does not change).
            let old_light = LIGHT_THEME.load(Ordering::Relaxed);
            let old_hc = HIGH_CONTRAST.load(Ordering::Relaxed);
            refresh_theme();
            let new_light = LIGHT_THEME.load(Ordering::Relaxed);
            let new_hc = HIGH_CONTRAST.load(Ordering::Relaxed);
            if old_light != new_light {
                log_event("INFO", &format!("theme changed: light={}", new_light == 1));
            }
            if old_hc != new_hc {
                log_event(
                    "INFO",
                    &format!("high contrast changed: enabled={}", new_hc == 1),
                );
            }
            invalidate_clock_anchor();
            invalidate_ui_font();
            reset_width(hwnd);
            relayout_overlay(hwnd);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            log_event(
                "INFO",
                &format!(
                    "WM_DPICHANGED: dpi={} (system {})",
                    GetDpiForWindow(hwnd),
                    GetDpiForSystem()
                ),
            );
            invalidate_clock_anchor();
            invalidate_ui_font();
            reset_width(hwnd);
            relayout_overlay(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            // Toggle the trend chart. There is no deferral: the class
            // style dropped `CS_DBLCLKS`, so Windows does not synthesize a
            // `WM_LBUTTONDBLCLK`, and Task Manager lives in the
            // right-click menu. Any left click is a left click. A press
            // paints nothing: the indicator has no pressed/hover backdrop.
            handle_history_click(hwnd);
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            show_context_menu(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            OVERLAYS.with(|slot| slot.borrow_mut().retain(|o| o.hwnd != hwnd));
            if !QUITTING.load(Ordering::Relaxed) {
                // The taskbar owns this window; explorer restarted and
                // destroyed it. The sentinel's reconcile recreates it
                // against the new taskbar. The resident History process is
                // independent and keeps running.
                log_event(
                    "INFO",
                    "overlay destroyed with its taskbar; reconcile will recreate",
                );
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// --------------------------------------------------------------------- run

pub fn run() {
    unsafe {
        // Crash diagnostics first.
        install_crash_hooks();

        // Single-instance guard.
        let mutex_name = utf16("Local\\SysVitals_SingleInstance");
        let _mutex = match CreateMutexW(None, false, windows::core::PCWSTR(mutex_name.as_ptr())) {
            Ok(h) => {
                if windows::Win32::Foundation::GetLastError()
                    == windows::Win32::Foundation::ERROR_ALREADY_EXISTS
                {
                    log_event("INFO", "another instance is already running; exiting");
                    return;
                }
                h
            }
            Err(_) => return,
        };

        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // Every panel child is owned by this job; closing the handle on
        // process exit kills the children even after a crash.
        init_child_job();
        // Needed only when the indicator is found cloaked on another
        // virtual desktop; the apartment is never otherwise used.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        refresh_theme();
        let cfg = config();

        // Seed the config mtime so the first poll does not false-trigger.
        CONFIG_MTIME.store(config_mtime(), Ordering::Relaxed);

        // Daily-file logging: the first line also opens the file and prunes
        // stale logs (see `core::logger`), so no separate startup prune.
        log_event(
            "INFO",
            &format!(
                "SysVitals starting (version={}, build={SV_BUILD_ID})",
                env!("CARGO_PKG_VERSION")
            ),
        );
        log_event(
            "INFO",
            &format!(
                "config loaded: interval={}ms language={:?} theme={:?} dual_row={} \
                     show_resources={} show_network={} item_spacing={}px anchor={:?} \
                     left_margin={}px auto_start={}",
                cfg.sample.interval_ms,
                cfg.display.language,
                cfg.display.theme,
                cfg.display.dual_row,
                cfg.display.show_resources,
                cfg.display.show_network,
                cfg.display.item_spacing,
                cfg.windows.anchor,
                cfg.windows.left_margin_px,
                cfg.startup.auto_start,
            ),
        );
        if !CONFIG_LOAD_FAILED.load(Ordering::Relaxed) && CONFIG_FROM_FILE.load(Ordering::Relaxed) {
            apply_auto_start(&cfg);
        }
        let centered = win11_centered_taskbar();
        log_event(
            "INFO",
            &format!(
                "environment: win11_centered_taskbar={} dpi={} build={:?}",
                centered,
                GetDpiForSystem(),
                os_build_number()
            ),
        );

        let hmodule = GetModuleHandleW(None).expect("module handle");
        let class_name = utf16("SysVitalsOverlay");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            // No `CS_DBLCLKS`: the indicator's left click now opens the
            // trend chart immediately, and the system resource viewer
            // (Task Manager) lives in the right-click menu. Keeping
            // `CS_DBLCLKS` would let Windows synthesize a
            // `WM_LBUTTONDBLCLK` after the trailing `WM_LBUTTONUP`,
            // which we no longer handle; dropping it here makes the
            // click pipeline honestly single-click only.
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hmodule.into(),
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH::default(),
            lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            log_event("WARN", "overlay window class registration failed");
            return;
        }

        // The sentinel is a hidden top-level window that is deliberately
        // *not* owned by the taskbar: it must outlive explorer restarts to
        // receive the `TaskbarCreated` broadcast and rebuild the overlay.
        let taskbar_created =
            RegisterWindowMessageW(windows::core::PCWSTR(utf16("TaskbarCreated").as_ptr()));
        if taskbar_created != 0 {
            let _ = TASKBAR_CREATED.set(taskbar_created);
        } else {
            log_event("WARN", "TaskbarCreated registration failed");
        }
        let sentinel_class = utf16("SysVitalsSentinel");
        let sentinel_wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(sentinel_wndproc),
            hInstance: hmodule.into(),
            lpszClassName: windows::core::PCWSTR(sentinel_class.as_ptr()),
            ..Default::default()
        };
        if RegisterClassExW(&sentinel_wc) == 0 {
            log_event("WARN", "sentinel class registration failed");
            return;
        }
        let sentinel_title = utf16("SysVitalsSentinel");
        let sentinel = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            windows::core::PCWSTR(sentinel_class.as_ptr()),
            windows::core::PCWSTR(sentinel_title.as_ptr()),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hmodule.into()),
            None,
        );
        match sentinel {
            Ok(sentinel) => {
                SENTINEL_HWND.store(sentinel.0 as isize, Ordering::Relaxed);
                let interval_ms = cfg.sample.interval_ms.clamp(500, 5000) as u32;
                let sample_ok = SetTimer(Some(sentinel), TIMER_SAMPLE, interval_ms, None) != 0;
                SAMPLE_TIMER_OK.store(sample_ok, Ordering::Relaxed);
                if !sample_ok {
                    log_event(
                        "WARN",
                        "SetTimer(TIMER_SAMPLE) failed; the watchdog will retry",
                    );
                }
                for (id, elapse, label) in [
                    (TIMER_MOVE, MOVE_MS, "move"),
                    (TIMER_CONFIG, CONFIG_POLL_MS, "config"),
                    (TIMER_HISTORY, HISTORY_SYNC_MS, "history"),
                    (TIMER_WATCH, WATCHDOG_MS, "watchdog"),
                ] {
                    if SetTimer(Some(sentinel), id, elapse, None) == 0 {
                        log_event(
                            "WARN",
                            &format!("SetTimer({label}) failed; that subsystem is inactive"),
                        );
                    }
                }
            }
            Err(e) => {
                log_event(
                    "WARN",
                    &format!("sentinel window creation failed: {e}; exiting"),
                );
                return;
            }
        }

        // Create one overlay per existing taskbar, then paint the first
        // sample. `reconcile_overlays` also runs every move tick, on
        // `TaskbarCreated` and on display changes, so taskbars that appear
        // later (secondary bars, hot-plug, explorer restart) get one too.
        reconcile_overlays();
        if LAST_SNAPSHOT.with(|s| s.borrow().is_none()) {
            sample_tick();
        }
        // The resident History framework is independent of any overlay.
        ensure_history_resident();

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Hidden top-level window that survives explorer restarts. It owns every
/// periodic timer and receives the `TaskbarCreated` broadcast, driving all
/// overlays at once (the overlays themselves are owned by their taskbars
/// and die with them, so they cannot be the ones holding the timers).
unsafe extern "system" fn sentinel_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Session end (shutdown, restart, log off). The indicator has no
    // unsaved state, so it never delays the shell: allow the shutdown,
    // then run the normal teardown when the session is really ending.
    // A cancelled shutdown arrives as `WM_ENDSESSION` with a zero
    // `wparam` and must leave the process running untouched.
    if msg == WM_QUERYENDSESSION {
        return LRESULT(1);
    }
    if msg == WM_ENDSESSION {
        if wparam.0 != 0 {
            log_event("INFO", "system session ending; closing panels and exiting");
            close_children();
            shutdown_and_quit();
        }
        return LRESULT(0);
    }
    if TASKBAR_CREATED.get().is_some_and(|created| *created == msg) {
        log_event("INFO", "TaskbarCreated received; reconciling overlays");
        WIN11_CENTERED.store(u32::MAX, Ordering::Relaxed);
        AUTO_HIDE.store(u32::MAX, Ordering::Relaxed);
        let _ = KillTimer(Some(hwnd), TIMER_REBIND);
        if find_taskbar().is_some() {
            reconcile_overlays();
            reposition_all();
        } else {
            let _ = SetTimer(Some(hwnd), TIMER_REBIND, REBIND_RETRY_MS, None);
        }
        return LRESULT(0);
    }
    if msg == WM_APP_HISTORY_READY {
        sync_history_state();
        return LRESULT(0);
    }
    if msg == WM_APP_SETTINGS_READY {
        // Reap a child that exited immediately after launch; the worker
        // has already stored or cleared the handle.
        let _ = settings_child_alive();
        return LRESULT(0);
    }
    if msg == WM_DISPLAYCHANGE {
        log_event("INFO", "WM_DISPLAYCHANGE: display configuration changed");
        // Monitor handles are not stable across a display reconfiguration;
        // forget which screen anchors the popup so the next click opens
        // or moves it again instead of being mistaken for a same-screen
        // toggle. The settings-window monitor cache is equally stale and
        // must not keep claiming the panel is already on "that" screen.
        HISTORY_ANCHOR_MONITOR.store(0, Ordering::Relaxed);
        SETTINGS_MONITOR.store(0, Ordering::Relaxed);
        invalidate_clock_anchor();
        invalidate_ui_font();
        OVERLAYS.with(|slot| {
            for ov in slot.borrow_mut().iter_mut() {
                ov.win_w = 0;
            }
        });
        reconcile_overlays();
        relayout_all();
        return LRESULT(0);
    }
    if msg == WM_TIMER {
        match wparam.0 {
            TIMER_SAMPLE => {
                sample_tick();
                return LRESULT(0);
            }
            TIMER_MOVE => {
                reconcile_overlays();
                reposition_all();
                return LRESULT(0);
            }
            TIMER_CONFIG => {
                poll_config_change();
                return LRESULT(0);
            }
            TIMER_HISTORY => {
                sync_history_state();
                return LRESULT(0);
            }
            TIMER_REBIND => {
                if find_taskbar().is_some() {
                    let _ = KillTimer(Some(hwnd), TIMER_REBIND);
                    reconcile_overlays();
                    reposition_all();
                }
                return LRESULT(0);
            }
            TIMER_WATCH => {
                if !QUITTING.load(Ordering::Relaxed) {
                    if !SAMPLE_TIMER_OK.load(Ordering::Relaxed) {
                        let interval_ms = config().sample.interval_ms.clamp(500, 5000) as u32;
                        if SetTimer(Some(hwnd), TIMER_SAMPLE, interval_ms, None) != 0 {
                            SAMPLE_TIMER_OK.store(true, Ordering::Relaxed);
                            log_event("INFO", "SetTimer(TIMER_SAMPLE) recovered");
                        }
                    }
                    // Re-probe shell/taskbar preferences at a low cadence;
                    // the per-tick geometry code then reads the cache
                    // instead of making a synchronous shell/registry
                    // call on every overlay.
                    AUTO_HIDE.store(u32::MAX, Ordering::Relaxed);
                    WIN11_CENTERED.store(u32::MAX, Ordering::Relaxed);
                    reconcile_overlays();
                }
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
