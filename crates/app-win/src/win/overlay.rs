//! overlay: split out of `win` (see `win` for the module map).

use super::*;

/// Per-taskbar overlay state. One entry per visible taskbar: the main bar
/// plus one per extra monitor. Everything that used to be a process-wide
/// global (window size, orientation) lives here so mixed-DPI multi-monitor
/// setups measure and render each indicator independently. All access is on
/// the message thread.
#[derive(Clone, Copy)]
pub(crate) struct Overlay {
    pub(crate) hwnd: HWND,
    /// The taskbar window that owns this overlay (its z-band follows it).
    pub(crate) taskbar: HWND,
    /// Monitor hosting the taskbar (HMONITOR as isize). Identifies the
    /// indicator for the history popup's move-to-screen decision.
    pub(crate) monitor: isize,
    pub(crate) vertical: bool,
    pub(crate) left_edge: bool,
    /// Measured content size in physical pixels at this overlay's DPI.
    /// Width only grows; a config/orientation/DPI change resets it.
    pub(crate) win_w: i32,
    pub(crate) win_h: i32,
    /// Last rectangle handed to `MoveWindow`. Placement runs every second
    /// and after every sample; an unchanged rect needs no window-manager
    /// work. `None` until the first successful placement.
    pub(crate) last_rect: Option<(i32, i32, i32, i32)>,
    /// A render was skipped because the overlay was hidden (auto-hide,
    /// fullscreen taskbar, restored window). The next visible tick must
    /// repaint instead of showing the pre-hide frame.
    pub(crate) needs_render: bool,
    // Per-overlay diagnostic latches. Keeping these on the overlay (instead
    // of process globals) means one monitor's z-order/cloak churn cannot
    // suppress or misattribute another monitor's log line.
    pub(crate) topmost_lost_count: u32,
    pub(crate) last_occluder: isize,
    pub(crate) visibility_anomaly: bool,
    pub(crate) shown_once: bool,
    pub(crate) cloak_logged: bool,
    pub(crate) cloak_move_fail_logged: bool,
    pub(crate) layered_fail_logged: bool,
}

pub(crate) fn overlay_by_hwnd(hwnd: HWND) -> Option<Overlay> {
    OVERLAYS.with(|slot| slot.borrow().iter().find(|o| o.hwnd == hwnd).copied())
}

/// Run `f` on the overlay owning `hwnd`. Returns `None` when the overlay is
/// gone (taskbar disappeared between ticks).
pub(crate) fn with_overlay_mut<R>(hwnd: HWND, f: impl FnOnce(&mut Overlay) -> R) -> Option<R> {
    OVERLAYS.with(|slot| slot.borrow_mut().iter_mut().find(|o| o.hwnd == hwnd).map(f))
}

pub(crate) fn set_overlay_needs_render(hwnd: HWND, needs_render: bool) {
    OVERLAYS.with(|slot| {
        if let Some(ov) = slot.borrow_mut().iter_mut().find(|o| o.hwnd == hwnd) {
            ov.needs_render = needs_render;
        }
    });
}

pub(crate) fn overlay_needs_render(hwnd: HWND) -> bool {
    overlay_by_hwnd(hwnd).is_some_and(|o| o.needs_render)
}

/// Reset one overlay's grow-only width so a fresh measurement takes effect
/// (config change, DPI change, orientation flip).
pub(crate) fn reset_width(hwnd: HWND) {
    OVERLAYS.with(|slot| {
        if let Some(ov) = slot.borrow_mut().iter_mut().find(|o| o.hwnd == hwnd) {
            ov.win_w = 0;
        }
    });
}

// ------------------------------------------------------- taskbar geometry

/// Taskbar geometry: rect in screen coordinates plus orientation.
pub(crate) struct TaskbarInfo {
    pub(crate) rect: RECT,
    pub(crate) vertical: bool,
    /// True when a vertical bar is docked to the *left* screen edge. The
    /// placement code uses it to pick which edge the indicator must never
    /// run past when its column is wider than the bar's thickness.
    pub(crate) left_edge: bool,
    /// Monitor hosting this taskbar (HMONITOR as isize).
    pub(crate) monitor: isize,
}

/// Whether the shell's auto-hide setting is enabled. The state is global
/// (per shell), not per bar; `ABM_GETSTATE` returns it without reporting
/// the current visibility, which the geometry check in
/// [`taskbar_info_for`] supplies.
pub(crate) unsafe fn taskbar_auto_hide_enabled() -> bool {
    let cached = AUTO_HIDE.load(Ordering::Relaxed);
    if cached != u32::MAX {
        return cached == 1;
    }
    let mut data = APPBARDATA {
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        ..Default::default()
    };
    let enabled = (SHAppBarMessage(ABM_GETSTATE, &mut data) as u32 & ABS_AUTOHIDE) != 0;
    AUTO_HIDE.store(u32::from(enabled), Ordering::Relaxed);
    enabled
}

/// Geometry for one specific taskbar window, or `None` when that bar is
/// not currently on screen (auto-hidden, or shell not ready).
///
/// The main bar's rect used to come from `ABM_GETTASKBARPOS`, but that call
/// only reports the main taskbar; secondary bars (`Shell_SecondaryTrayWnd`)
/// must be measured with `GetWindowRect`, so every bar uses the same path.
/// Orientation is inferred from the rect: a taskbar is always far longer
/// along its docked edge than it is thick.
pub(crate) unsafe fn taskbar_info_for(bar: HWND) -> Option<TaskbarInfo> {
    let mut r = RECT::default();
    if GetWindowRect(bar, &mut r).is_err() {
        return None;
    }
    if r.right <= r.left || r.bottom <= r.top {
        return None;
    }
    // A taskbar window that is not visible has no indicator. This covers
    // auto-hide (the shell slides the bar away and hides it) and a
    // secondary bar that still exists as a window after the user turns
    // off "show taskbar on all displays". A visible bar parked off its
    // monitor is rejected by the intersection checks below.
    if !IsWindowVisible(bar).as_bool() {
        return None;
    }
    // The bar can live on any monitor, including negative virtual
    // coordinates. Validate against the virtual screen, never the primary.
    let virtual_rect = virtual_screen_rect();
    let on_screen = r.bottom > virtual_rect.top
        && r.top < virtual_rect.bottom
        && r.right > virtual_rect.left
        && r.left < virtual_rect.right;
    if !on_screen {
        return None;
    }
    let monitor = MonitorFromWindow(bar, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let mon_rect = if GetMonitorInfoW(monitor, &mut mi).as_bool() {
        mi.rcMonitor
    } else {
        virtual_rect
    };
    // A hidden bar is parked just off its own monitor edge; require the
    // rect to still intersect the monitor that hosts it.
    let intersects = r.bottom > mon_rect.top
        && r.top < mon_rect.bottom
        && r.right > mon_rect.left
        && r.left < mon_rect.right;
    if !intersects {
        return None;
    }
    let vertical = (r.bottom - r.top) > (r.right - r.left);
    // Auto-hide leaves only a thin strip of the bar on screen. Geometry
    // alone would still place the overlay partly off-screen; make the
    // "hidden" state explicit and keep the overlay hidden while the user
    // is not revealing the bar.
    if taskbar_auto_hide_enabled() {
        let visible_w = r.right.min(mon_rect.right) - r.left.max(mon_rect.left);
        let visible_h = r.bottom.min(mon_rect.bottom) - r.top.max(mon_rect.top);
        let thickness = if vertical { visible_w } else { visible_h };
        if thickness <= 4 {
            return None;
        }
    }
    let left_edge = vertical && r.left <= mon_rect.left;
    Some(TaskbarInfo {
        rect: r,
        vertical,
        left_edge,
        monitor: monitor.0 as isize,
    })
}

/// Bounding rectangle of the whole virtual desktop (all monitors), in
/// physical pixels. Used for on-screen validation and for monitor-local
/// clamping that must work with negative coordinates.
pub(crate) unsafe fn virtual_screen_rect() -> RECT {
    let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
    let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
    let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
    let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
    let (fallback_w, fallback_h) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
    let width = if width > 0 { width } else { fallback_w };
    let height = if height > 0 { height } else { fallback_h };
    RECT {
        left,
        top,
        right: left + width.max(1),
        bottom: top + height.max(1),
    }
}

/// Monitor that hosts a given taskbar, or the virtual desktop when the
/// monitor cannot be resolved. The indicator must be clamped to this
/// monitor, not the primary one, for secondary/negative-coordinate bars.
pub(crate) unsafe fn taskbar_monitor_rect_for(bar: HWND) -> RECT {
    let fallback = virtual_screen_rect();
    let monitor = MonitorFromWindow(bar, MONITOR_DEFAULTTONEAREST);
    if monitor.is_invalid() {
        return fallback;
    }
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(monitor, &mut info).as_bool() {
        info.rcMonitor
    } else {
        fallback
    }
}

/// Left edge of one visible direct child of a taskbar, if present.
pub(crate) unsafe fn visible_child_left(bar: HWND, class: &str) -> Option<i32> {
    let cls = utf16(class);
    let child = FindWindowExW(Some(bar), None, windows::core::PCWSTR(cls.as_ptr()), None).ok()?;
    if child.is_invalid() || !IsWindowVisible(child).as_bool() {
        return None;
    }
    let mut rect = RECT::default();
    if GetWindowRect(child, &mut rect).is_err() || rect.right <= rect.left {
        return None;
    }
    Some(rect.left)
}

/// Horizontal anchor for an indicator inside one taskbar: the left edge of
/// its notification area when the bar shows tray icons, otherwise the left
/// edge of the show-desktop button, otherwise the clock's left edge, and
/// finally the bar's right edge.
///
/// Secondary bars have no `TrayNotifyWnd` until "show tray icons on all
/// displays" is enabled. Anchoring such a bar to its raw right edge would
/// put the indicator on top of the date/time — the only thing on that end
/// of the bar — so the clock is resolved through UI Automation first.
pub(crate) unsafe fn taskbar_anchor_left(bar: HWND) -> i32 {
    if let Some(left) = visible_child_left(bar, "TrayNotifyWnd") {
        return left;
    }
    if let Some(left) = visible_child_left(bar, "TrayShowDesktopButtonWClass") {
        return left;
    }
    let mut rect = RECT::default();
    if GetWindowRect(bar, &mut rect).is_err() {
        return 0;
    }
    if let Some(left) = clock_anchor_left(bar, rect) {
        return left;
    }
    rect.right
}

/// Cached clock-button anchor for one taskbar. UIA crosses into Explorer
/// and costs tens of milliseconds, so the value is re-read at most once
/// every two minutes (or immediately when the bar's rect changes); a
/// failed lookup is retried after ten seconds.
#[derive(Clone, Copy)]
pub(crate) struct ClockAnchorCache {
    pub(crate) bar: isize,
    pub(crate) bar_rect: RECT,
    pub(crate) at: Instant,
    pub(crate) left: Option<i32>,
}

pub(crate) static CLOCK_ANCHOR: Mutex<Vec<ClockAnchorCache>> = Mutex::new(Vec::new());

/// Drop every cached UIA clock anchor. Called when a system setting that can
/// move the clock arrives: the cache key is only the taskbar handle and rect,
/// so a text-size or time-format change would otherwise stay stale until the
/// two-minute TTL expires.
pub(crate) fn invalidate_clock_anchor() {
    if let Ok(mut guard) = CLOCK_ANCHOR.lock() {
        guard.clear();
    }
}

/// One-shot latch for the off-screen placement warning; a bad margin/font
/// combination would otherwise log on every move tick.
static PLACEMENT_SKIPPED_LOGGED: AtomicBool = AtomicBool::new(false);

/// Report (once) that the measured indicator would leave its monitor, so a
/// user with an extreme margin/font combination gets a diagnosable line
/// instead of a silently frozen overlay.
fn warn_placement_skipped(x: i32, y: i32, w: i32, h: i32) {
    if !PLACEMENT_SKIPPED_LOGGED.swap(true, Ordering::Relaxed) {
        log_event(
            "WARN",
            &format!(
                "overlay placement skipped: rect {w}x{h} at ({x},{y}) leaves the taskbar monitor; keeping the last position"
            ),
        );
    }
}

pub(crate) fn rect_eq(a: RECT, b: RECT) -> bool {
    a.left == b.left && a.top == b.top && a.right == b.right && a.bottom == b.bottom
}

/// Left edge of the clock button on a taskbar, resolved through UI
/// Automation. On secondary bars without mirrored tray icons the clock is
/// drawn by the XAML taskbar and has no child window, so the automation
/// tree is the only way to know where it starts. The right-most visible
/// `SystemTray.*` element (ignoring the show-desktop button) is the clock.
pub(crate) fn uia_clock_left(bar: HWND) -> Option<i32> {
    use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation, TreeScope_Descendants};
    let automation: IUIAutomation = unsafe {
        CoCreateInstance(
            &CUIAutomation,
            None::<&windows::core::IUnknown>,
            CLSCTX_INPROC_SERVER,
        )
    }
    .ok()?;
    let root = unsafe { automation.ElementFromHandle(bar) }.ok()?;
    let condition = unsafe { automation.CreateTrueCondition() }.ok()?;
    let elements = unsafe { root.FindAll(TreeScope_Descendants, &condition) }.ok()?;
    let count = unsafe { elements.Length() }.ok()?;
    let mut best: Option<i32> = None;
    for index in 0..count {
        let Ok(element) = (unsafe { elements.GetElement(index) }) else {
            continue;
        };
        let Ok(class) = (unsafe { element.CurrentClassName() }) else {
            continue;
        };
        let class = class.to_string();
        if !class.starts_with("SystemTray.") || class.contains("ShowDesktop") {
            continue;
        }
        let Ok(offscreen) = (unsafe { element.CurrentIsOffscreen() }) else {
            continue;
        };
        if offscreen.as_bool() {
            continue;
        }
        let Ok(rect) = (unsafe { element.CurrentBoundingRectangle() }) else {
            continue;
        };
        if rect.right <= rect.left || rect.bottom <= rect.top {
            continue;
        }
        best = Some(best.map_or(rect.left, |best| best.max(rect.left)));
    }
    best
}

pub(crate) fn clock_anchor_left(bar: HWND, bar_rect: RECT) -> Option<i32> {
    let now = Instant::now();
    if let Ok(guard) = CLOCK_ANCHOR.lock() {
        if let Some(cached) = guard
            .iter()
            .find(|c| c.bar == bar.0 as isize && rect_eq(c.bar_rect, bar_rect))
        {
            let ttl = if cached.left.is_some() {
                Duration::from_secs(120)
            } else {
                Duration::from_secs(10)
            };
            if now.duration_since(cached.at) < ttl {
                return cached.left;
            }
        }
    }
    let left = uia_clock_left(bar);
    if let Ok(mut guard) = CLOCK_ANCHOR.lock() {
        let changed = match guard.iter_mut().find(|c| c.bar == bar.0 as isize) {
            Some(cached) => {
                let changed = cached.left != left || !rect_eq(cached.bar_rect, bar_rect);
                *cached = ClockAnchorCache {
                    bar: bar.0 as isize,
                    bar_rect,
                    at: now,
                    left,
                };
                changed
            }
            None => {
                // Bars come and go with monitor changes; keep the cache
                // bounded instead of accumulating stale handles.
                if guard.len() >= 8 {
                    guard.clear();
                }
                guard.push(ClockAnchorCache {
                    bar: bar.0 as isize,
                    bar_rect,
                    at: now,
                    left,
                });
                true
            }
        };
        drop(guard);
        if changed {
            match left {
                    Some(left) => log_event(
                        "INFO",
                        &format!(
                            "taskbar 0x{:x}: clock anchor at x={left}",
                            bar.0 as usize
                        ),
                    ),
                    None => log_event(
                        "WARN",
                        &format!(
                            "taskbar 0x{:x}: no tray or clock anchor; falling back to the bar's right edge",
                            bar.0 as usize
                        ),
                    ),
                }
        }
    }
    left
}

/// Every taskbar window: the main bar plus one per extra monitor. All
/// secondary bars share the class name `Shell_SecondaryTrayWnd`, so they
/// are enumerated with `FindWindowExW`.
pub(crate) unsafe fn taskbar_windows() -> Vec<HWND> {
    let mut out = Vec::new();
    if let Some(main) = find_taskbar() {
        out.push(main);
    }
    let cls = utf16("Shell_SecondaryTrayWnd");
    let mut prev: Option<HWND> = None;
    loop {
        match FindWindowExW(None, prev, windows::core::PCWSTR(cls.as_ptr()), None) {
            Ok(h) if !h.is_invalid() => {
                out.push(h);
                prev = Some(h);
            }
            _ => break,
        }
    }
    out
}

/// Set while a context menu is being tracked. `TrackPopupMenu` runs a
/// modal loop that still dispatches our `WM_TIMER`, and the menu lives
/// in the topmost band — re-asserting the indicator's band during tracking
/// would lift the tiny overlay back above the open menu. Skip those ticks.
pub(crate) static MENU_TRACKING: AtomicBool = AtomicBool::new(false);
/// De-duplicates the "overlay creation failed" warning: reconcile retries
/// every second, so a persistent failure must not log on every attempt.
/// Creation failures happen before an overlay exists, so this one stays
/// process-global.
pub(crate) static OVERLAY_CREATE_FAIL_LOGGED: AtomicBool = AtomicBool::new(false);

/// The Windows virtual-desktop manager, created on demand. The overlay is
/// a top-level window, so Windows cloaks it when the user switches to
/// another virtual desktop; the taskbar itself appears on every desktop,
/// so the indicator should follow the user too.
pub(crate) unsafe fn vd_manager() -> Option<IVirtualDesktopManager> {
    VD_MANAGER.with(|slot| {
        if slot.borrow().is_none() {
            let created: windows::core::Result<IVirtualDesktopManager> = CoCreateInstance(
                &VirtualDesktopManager,
                None::<&windows::core::IUnknown>,
                CLSCTX_INPROC_SERVER,
            );
            match created {
                Ok(manager) => *slot.borrow_mut() = Some(manager),
                Err(e) => {
                    log_event("WARN", &format!("virtual desktop manager unavailable: {e}"));
                    return None;
                }
            }
        }
        slot.borrow().clone()
    })
}

/// Move the overlay to the desktop currently hosting the foreground
/// window (or the shell window on an empty desktop). Used only when DWM
/// reports the overlay as cloaked.
pub(crate) unsafe fn move_to_current_desktop(hwnd: HWND) {
    let Some(manager) = vd_manager() else {
        return;
    };
    let mut target = GetForegroundWindow();
    if target.is_invalid() {
        target = GetShellWindow();
    }
    if target.is_invalid() {
        log_event("WARN", "cloaked overlay: no foreground window to follow");
        return;
    }
    let Ok(desktop) = manager.GetWindowDesktopId(target) else {
        if with_overlay_mut(hwnd, |ov| {
            !std::mem::replace(&mut ov.cloak_move_fail_logged, true)
        })
        .unwrap_or(false)
        {
            log_event("WARN", "cloaked overlay: cannot read the target desktop id");
        }
        return;
    };
    match manager.MoveWindowToDesktop(hwnd, &desktop) {
        Ok(()) => {
            with_overlay_mut(hwnd, |ov| ov.cloak_move_fail_logged = false);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        Err(e) => {
            if with_overlay_mut(hwnd, |ov| {
                !std::mem::replace(&mut ov.cloak_move_fail_logged, true)
            })
            .unwrap_or(false)
            {
                log_event("WARN", &format!("overlay virtual-desktop move failed: {e}"));
            }
        }
    }
}

/// Restore a hidden or minimized overlay, and bring it back from another
/// virtual desktop. `IsWindowVisible` alone cannot see either state: a
/// minimized window still has `WS_VISIBLE`, and a cloaked window still
/// reports itself visible, so both used to leave the indicator invisible
/// with no log line until the process was restarted.
pub(crate) unsafe fn maintain_visibility(hwnd: HWND) {
    let hidden = !IsWindowVisible(hwnd).as_bool();
    let minimized = IsIconic(hwnd).as_bool();
    let anomaly = hidden || minimized;
    let (was_anomalous, first_show) = with_overlay_mut(hwnd, |ov| {
        let was = std::mem::replace(&mut ov.visibility_anomaly, anomaly);
        let first = if anomaly {
            !std::mem::replace(&mut ov.shown_once, true)
        } else {
            ov.shown_once = true;
            false
        };
        (was, first)
    })
    .unwrap_or((false, false));
    if anomaly {
        if !was_anomalous && !first_show {
            log_event(
                "INFO",
                &format!("overlay not visible (hidden={hidden} minimized={minimized}); restoring"),
            );
        }
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        // A restored window may have skipped renders while it was hidden.
        set_overlay_needs_render(hwnd, true);
    }

    let mut cloaked = 0u32;
    let is_cloaked = DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut u32 as *mut std::ffi::c_void,
        std::mem::size_of::<u32>() as u32,
    )
    .is_ok()
        && cloaked != 0;
    if is_cloaked {
        let first_cloak =
            with_overlay_mut(hwnd, |ov| !std::mem::replace(&mut ov.cloak_logged, true))
                .unwrap_or(false);
        if first_cloak {
            log_event(
                "INFO",
                "overlay cloaked on another virtual desktop; moving it here",
            );
        }
        move_to_current_desktop(hwnd);
    } else {
        with_overlay_mut(hwnd, |ov| {
            ov.cloak_logged = false;
            ov.cloak_move_fail_logged = false;
        });
    }
}

/// Log once per new covering window when another topmost window sits on
/// the indicator's center after the band assertion. This is the
/// diagnostic that distinguishes "our window vanished" from "a shell
/// flyout or third-party topmost window is parked over us".
pub(crate) unsafe fn log_occlusion(hwnd: HWND) {
    if MENU_TRACKING.load(Ordering::Relaxed) {
        return;
    }
    let mut rc = RECT::default();
    if GetWindowRect(hwnd, &mut rc).is_err() {
        return;
    }
    let point = POINT {
        x: (rc.left + rc.right) / 2,
        y: (rc.top + rc.bottom) / 2,
    };
    let hit = WindowFromPoint(point);
    if hit.is_invalid() || hit == hwnd {
        with_overlay_mut(hwnd, |ov| ov.last_occluder = 0);
        return;
    }
    let hit_raw = hit.0 as isize;
    let repeated = with_overlay_mut(hwnd, |ov| {
        std::mem::replace(&mut ov.last_occluder, hit_raw) == hit_raw
    })
    .unwrap_or(true);
    if repeated {
        return;
    }
    let mut class_buf = [0u16; 128];
    let class_len = GetClassNameW(hit, &mut class_buf).max(0) as usize;
    let mut pid = 0u32;
    GetWindowThreadProcessId(hit, Some(&mut pid));
    // Do not call GetWindowTextW here: it sends WM_GETTEXT to another
    // process and can block this message thread if that window is hung.
    // Class + PID is enough to identify a diagnostic occluder and avoids
    // writing the user's window title into the shared log.
    log_event(
        "WARN",
        &format!(
            "overlay center covered by class='{}' pid={pid}",
            String::from_utf16_lossy(&class_buf[..class_len]),
        ),
    );
}

/// Re-assert the topmost style only when Windows has actually dropped it.
///
/// The overlay is owned by `Shell_TrayWnd`; owned windows stay above their
/// owner and follow the taskbar's z-band, which is what keeps the
/// indicator visible while Start / Task View push the taskbar into its
/// higher band. Re-inserting the window at the top of the topmost band
/// every tick cannot cross bands and only fights the shell's animations,
/// so the unconditional assertion was removed. A lost style bit (seen on
/// live installs after some z-order reshuffles) is still repaired once.
///
/// Skip while the context menu is tracking: the menu lives in the
/// topmost band — re-raising now would lift the indicator above the menu
/// the user is reading. This branch runs only on the rare lost-bit path.
pub(crate) unsafe fn maintain_topmost(hwnd: HWND) {
    if MENU_TRACKING.load(Ordering::Relaxed) {
        return;
    }
    let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
    if (ex & WS_EX_TOPMOST.0 as isize) != 0 {
        return;
    }
    let n = with_overlay_mut(hwnd, |ov| {
        ov.topmost_lost_count = ov.topmost_lost_count.wrapping_add(1);
        ov.topmost_lost_count
    })
    .unwrap_or(0);
    if n != 0 && (n == 1 || n.is_multiple_of(60)) {
        log_event(
            "INFO",
            &format!("topmost band lost; re-asserting (count={n})"),
        );
    }
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
}

/// Move-only placement for one overlay. Visibility follows that overlay's
/// taskbar alone: as long as the taskbar is on screen the indicator stays
/// visible. It hides only when the taskbar itself is gone (auto-hide or a
/// true fullscreen app pushing it off screen). No foreground-window
/// heuristics involved.
pub(crate) unsafe fn reposition_overlay(hwnd: HWND) {
    let Some(ov) = overlay_by_hwnd(hwnd) else {
        return;
    };
    let Some(info) = taskbar_info_for(ov.taskbar) else {
        if IsWindowVisible(hwnd).as_bool() {
            log_event(
                "INFO",
                "overlay hidden: taskbar not on screen (auto-hide or fullscreen app)",
            );
        }
        // The hide is intentional; the next visible tick should restore
        // without reporting it as an unexpected disappearance.
        with_overlay_mut(hwnd, |ov| ov.visibility_anomaly = true);
        set_overlay_needs_render(hwnd, true);
        let _ = ShowWindow(hwnd, SW_HIDE);
        return;
    };
    maintain_visibility(hwnd);
    // Keep the cached monitor/edge fresh (a Windows 10 bar can be dragged
    // to another monitor or docked edge between ticks) and detect an
    // orientation flip, which resets the grow-only width and re-layouts.
    let orientation_changed = ov.vertical != info.vertical;
    // The taskbar can move to another monitor (Windows 10 drag, or a
    // display reconfiguration). Re-measure instead of keeping the width
    // grown at the old monitor's DPI; the hidden-overlay case does not
    // always deliver WM_DPICHANGED.
    let monitor_changed = ov.monitor != info.monitor;
    OVERLAYS.with(|slot| {
        if let Some(o) = slot.borrow_mut().iter_mut().find(|o| o.hwnd == hwnd) {
            o.monitor = info.monitor;
            o.left_edge = info.left_edge;
            if orientation_changed {
                o.vertical = info.vertical;
            }
            if orientation_changed || monitor_changed {
                o.win_w = 0;
            }
        }
    });
    if orientation_changed || monitor_changed {
        if orientation_changed {
            log_event(
                "INFO",
                &format!("taskbar orientation changed: vertical={}", info.vertical),
            );
        }
        if monitor_changed {
            log_event("INFO", "taskbar moved to another monitor; re-measuring");
        }
        relayout_overlay(hwnd);
        return;
    }
    // `maintain_visibility` may have just restored a hidden/minimized
    // overlay, and a sample tick may have skipped its render meanwhile.
    // Repaint with the current snapshot before the normal move-only path.
    if overlay_needs_render(hwnd) {
        relayout_overlay(hwnd);
        return;
    }
    apply_position_with_info(hwnd, info);
}

/// Place one overlay at its anchor using its measured size. Called both by
/// the once-per-second geometry tick and immediately after a measured
/// resize: growing the width while keeping the old left edge would push a
/// right-anchored overlay into the tray until the next tick.
pub(crate) unsafe fn apply_position(hwnd: HWND) {
    let Some(ov) = overlay_by_hwnd(hwnd) else {
        return;
    };
    let Some(info) = taskbar_info_for(ov.taskbar) else {
        if IsWindowVisible(hwnd).as_bool() {
            log_event(
                "INFO",
                "overlay hidden: taskbar not on screen (auto-hide or fullscreen app)",
            );
        }
        // The hide is intentional; the next visible tick should restore
        // without reporting it as an unexpected disappearance.
        with_overlay_mut(hwnd, |ov| ov.visibility_anomaly = true);
        set_overlay_needs_render(hwnd, true);
        let _ = ShowWindow(hwnd, SW_HIDE);
        return;
    };
    maintain_visibility(hwnd);
    apply_position_with_info(hwnd, info);
}

pub(crate) unsafe fn apply_position_with_info(hwnd: HWND, info: TaskbarInfo) {
    // Taskbar is on screen: keep the indicator in the topmost band (it can
    // be re-banded below shell flyouts between ticks; see maintain_topmost).
    maintain_topmost(hwnd);
    let Some(ov) = overlay_by_hwnd(hwnd) else {
        return;
    };
    let bar = info.rect;
    let cfg = config();
    let w = ov.win_w;
    let h = ov.win_h;
    if w <= 0 || h <= 0 {
        return; // not measured yet
    }

    let (x, y) = if info.vertical {
        // Vertical taskbar: horizontally centered within the bar thickness.
        // When the stacked column is wider than the bar's thickness,
        // centering would push the window past the screen edge it is
        // docked to and clip it. Clamp so the overflow goes into the
        // desktop instead: keep the left edge on-screen for a left-docked
        // bar, the right edge on-screen for a right-docked bar.
        let center_x = bar.left + (bar.right - bar.left - w) / 2;
        let monitor = taskbar_monitor_rect_for(ov.taskbar);
        let x = if info.left_edge {
            center_x.max(monitor.left)
        } else {
            center_x.min(monitor.right - w).max(monitor.left)
        };
        // On a vertical bar `left_margin_px` measures from the bar's TOP
        // edge. The far-left anchor concept does not exist here, so the
        // value is honored unconditionally.
        let y = bar.top + cfg.windows.left_margin_px as i32;
        (x, y)
    } else {
        // Horizontal taskbar (bottom or top).
        let left_anchor =
            cfg.windows.anchor == sysvitals_core::Anchor::Left && win11_centered_taskbar();
        let x = if left_anchor {
            bar.left + cfg.windows.left_margin_px as i32
        } else {
            // Anchor to this bar's own notification area (or its
            // show-desktop button when the bar has no tray icons).
            taskbar_anchor_left(ov.taskbar) - w
        };
        let y = bar.top + (bar.bottom - bar.top - h) / 2;
        (x, y)
    };

    if info.vertical {
        if y < bar.top || y + h > bar.bottom {
            warn_placement_skipped(x, y, w, h);
            return;
        }
    } else {
        // Negative virtual coordinates are legitimate on a monitor placed
        // left of/above the primary one; clamp to that monitor instead of
        // treating every negative number as off-screen.
        let monitor = taskbar_monitor_rect_for(ov.taskbar);
        if x < monitor.left || y < monitor.top || x + w > monitor.right || y + h > monitor.bottom {
            warn_placement_skipped(x, y, w, h);
            return; // would leave the monitor; keep last position
        }
    }
    let rect = (x, y, w, h);
    let already_placed = OVERLAYS.with(|slot| {
        slot.borrow()
            .iter()
            .find(|o| o.hwnd == hwnd)
            .is_some_and(|o| o.last_rect == Some(rect))
    });
    let moved = if already_placed {
        false
    } else if MoveWindow(hwnd, x, y, w, h, true).is_ok() {
        OVERLAYS.with(|slot| {
            if let Some(ov) = slot.borrow_mut().iter_mut().find(|o| o.hwnd == hwnd) {
                ov.last_rect = Some(rect);
            }
        });
        true
    } else {
        false
    };
    // Only re-show when actually hidden (e.g. returning from auto-hide).
    if !IsWindowVisible(hwnd).as_bool() {
        log_event("INFO", "overlay re-shown after being hidden");
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    // Occlusion is a diagnostic: probing every second buys nothing, but a
    // fresh placement is worth checking immediately.
    let probe = LAST_OCCLUSION_PROBE.with(|cell| {
        let now = Instant::now();
        let due = moved
            || cell
                .get()
                .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(5));
        if due {
            cell.set(Some(now));
        }
        due
    });
    if probe {
        log_occlusion(hwnd);
    }
}

/// Primary taskbar window. Re-queried after every explorer restart; the
/// overlay uses it as its owner so the two share a z-band.
pub(crate) unsafe fn find_taskbar() -> Option<HWND> {
    let cls = utf16("Shell_TrayWnd");
    FindWindowW(windows::core::PCWSTR(cls.as_ptr()), None).ok()
}

/// Create the layered overlay, optionally owned by the taskbar.
///
/// Owner (not parent) is what makes Start menu / Task View safe: those
/// shell surfaces promote `Shell_TrayWnd` from z-band 1 to band 6, and an
/// owned window follows its owner's band automatically. `HWND_TOPMOST`
/// alone cannot cross bands, which is why the previous per-tick assertion
/// only produced a one-second flicker.
pub(crate) unsafe fn create_overlay(owner: Option<HWND>) -> Option<HWND> {
    let hmodule = GetModuleHandleW(None).ok()?;
    let class_name = utf16("SysVitalsOverlay");
    let title = utf16("SysVitals");
    let hwnd = CreateWindowExW(
        // `WS_EX_NOACTIVATE` keeps a click from pulling foreground away
        // from the History popup; the popup activates itself on reveal.
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        windows::core::PCWSTR(class_name.as_ptr()),
        windows::core::PCWSTR(title.as_ptr()),
        WS_POPUP,
        -32_000,
        -32_000,
        200,
        24,
        owner,
        None,
        Some(hmodule.into()),
        None,
    )
    .ok()?;
    // Topmost so the overlay sits above the taskbar's own content.
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
    Some(hwnd)
}

/// Bring the set of overlays in line with the set of taskbars: destroy
/// overlays whose taskbar is gone and create overlays for taskbars that do
/// not have one yet. Called at startup, on `TaskbarCreated`, on display
/// changes, and by the move tick (which also catches secondary bars that
/// appear a moment after the main one, and hot-plugged monitors).
pub(crate) unsafe fn reconcile_overlays() {
    if QUITTING.load(Ordering::Relaxed) {
        return;
    }
    let bars = taskbar_windows();
    // Drop overlays whose taskbar no longer exists. Collect first and
    // destroy after releasing the registry borrow: `DestroyWindow` sends
    // `WM_DESTROY` synchronously, and that handler re-borrows `OVERLAYS`.
    let mut to_destroy = Vec::new();
    OVERLAYS.with(|slot| {
        let mut overlays = slot.borrow_mut();
        let mut i = 0;
        while i < overlays.len() {
            if IsWindow(Some(overlays[i].taskbar)).as_bool() {
                i += 1;
                continue;
            }
            let ov = overlays.remove(i);
            to_destroy.push(ov.hwnd);
            log_event("INFO", "overlay removed: its taskbar disappeared");
        }
    });
    for hwnd in to_destroy {
        if IsWindow(Some(hwnd)).as_bool() {
            let _ = DestroyWindow(hwnd);
        }
    }
    // Create overlays for taskbars that lack one.
    for &bar in &bars {
        let exists = OVERLAYS.with(|slot| slot.borrow().iter().any(|o| o.taskbar == bar));
        if exists {
            continue;
        }
        let Some(hwnd) = create_overlay(Some(bar)) else {
            if !OVERLAY_CREATE_FAIL_LOGGED.swap(true, Ordering::Relaxed) {
                log_event("WARN", "overlay creation failed for a taskbar");
            }
            continue;
        };
        OVERLAY_CREATE_FAIL_LOGGED.store(false, Ordering::Relaxed);
        let info = taskbar_info_for(bar);
        let (vertical, left_edge, monitor) = match info {
            Some(i) => (i.vertical, i.left_edge, i.monitor),
            None => (
                false,
                false,
                MonitorFromWindow(bar, MONITOR_DEFAULTTONEAREST).0 as isize,
            ),
        };
        OVERLAYS.with(|slot| {
            slot.borrow_mut().push(Overlay {
                hwnd,
                taskbar: bar,
                monitor,
                vertical,
                left_edge,
                win_w: 0,
                win_h: 0,
                last_rect: None,
                needs_render: false,
                topmost_lost_count: 0,
                last_occluder: 0,
                visibility_anomaly: false,
                shown_once: false,
                cloak_logged: false,
                cloak_move_fail_logged: false,
                layered_fail_logged: false,
            })
        });
        log_event(
            "INFO",
            &format!("overlay created for a taskbar (vertical={vertical})"),
        );
        // Show before the first layout so the placement pass does not log
        // a spurious "re-shown"; the window starts off-screen anyway.
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        relayout_overlay(hwnd);
    }
}

/// Re-place every overlay against its own taskbar. Per-overlay show/hide
/// follows each taskbar's visibility (auto-hide, fullscreen on that
/// monitor), so one hidden bar does not affect the others.
pub(crate) fn reposition_all() {
    let hwnds: Vec<HWND> = OVERLAYS.with(|slot| slot.borrow().iter().map(|o| o.hwnd).collect());
    for hwnd in hwnds {
        unsafe { reposition_overlay(hwnd) };
    }
}
