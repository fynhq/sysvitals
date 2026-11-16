//! Overlay window procedure and message routing.

use super::*;

pub(crate) fn handle_history_click(hwnd: HWND) {
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
            history_wait_cursor_begin();
        }
    } else if HISTORY_ANCHOR_MONITOR.load(Ordering::Relaxed) == this_monitor {
        history_close_request();
        HISTORY_ANCHOR_MONITOR.store(0, Ordering::Relaxed);
    } else {
        log_event(
            "INFO",
            "history: moving popup to the clicked indicator's screen",
        );
        if history_open_request(hwnd) {
            HISTORY_ANCHOR_MONITOR.store(this_monitor, Ordering::Relaxed);
        }
    }
}

fn setting_change_name(lparam: LPARAM) -> Option<String> {
    use std::ffi::c_void;
    use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows::Win32::System::Threading::GetCurrentProcess;

    if lparam.0 == 0 {
        return None;
    }
    let mut buf = [0u16; 256];
    let mut read = 0usize;
    let ok = unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            lparam.0 as *const c_void,
            buf.as_mut_ptr() as *mut c_void,
            std::mem::size_of_val(&buf),
            Some(&mut read as *mut usize),
        )
    }
    .is_ok();
    if !ok || read == 0 {
        return None;
    }
    let units = (read / std::mem::size_of::<u16>()).min(buf.len());
    let len = buf[..units].iter().position(|&c| c == 0).unwrap_or(units);
    Some(String::from_utf16_lossy(&buf[..len]))
}

fn relevant_setting_change(lparam: LPARAM) -> bool {
    if lparam.0 == 0 {
        return true;
    }
    match setting_change_name(lparam) {
        Some(name) => matches!(
            name.as_str(),
            "ImmersiveColorSet" | "WindowMetrics" | "intl" | "HighContrast"
        ),
        None => true,
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_SETTINGCHANGE => {
            if !relevant_setting_change(lparam) {
                return LRESULT(0);
            }
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
                log_event(
                    "INFO",
                    "overlay destroyed with its taskbar; reconcile will recreate",
                );
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            if (lparam.0 as usize & 0xFFFF) as u32 == HTCLIENT && wait_cursor_active() {
                if let Ok(cursor) = unsafe { LoadCursorW(None, IDC_WAIT) } {
                    unsafe { SetCursor(Some(cursor)) };
                    return LRESULT(1);
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub(crate) fn run() {
    unsafe {
        install_crash_hooks();

        let _instance = match acquire_instance_lock("Local\\SysVitals_SingleInstance") {
            InstanceLock::Acquired(guard) => guard,
            InstanceLock::Busy => {
                log_event("INFO", "another instance is already running; exiting");
                return;
            }
            InstanceLock::Unavailable(error) => {
                log_event(
                    "WARN",
                    &format!("single-instance mutex unavailable: {error}; exiting"),
                );
                return;
            }
        };

        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        init_child_job();
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        refresh_theme();
        let cfg = config();

        log_event(
            "INFO",
            &format!(
                "SysVitals starting (version={}, build={SV_BUILD_ID})",
                env!("CARGO_PKG_VERSION")
            ),
        );
        log_event("INFO", &format!("config loaded: {}", cfg.summary()));
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

        reconcile_overlays();
        if LAST_SNAPSHOT.with(|s| s.borrow().is_none()) {
            sample_tick();
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn sentinel_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
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
        let _ = settings_child_alive();
        wait_cursor_reconcile();
        return LRESULT(0);
    }
    if msg == WM_DISPLAYCHANGE {
        log_event("INFO", "WM_DISPLAYCHANGE: display configuration changed");
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
