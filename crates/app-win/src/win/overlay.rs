//! Per-taskbar overlay state, geometry, and placement.

use super::*;

use std::sync::Mutex;

// One overlay per visible taskbar; mixed-DPI monitors keep their own geometry and render state.
#[derive(Clone, Copy)]
pub(crate) struct Overlay {
    pub(crate) hwnd: HWND,
    pub(crate) taskbar: HWND,
    pub(crate) monitor: isize,
    pub(crate) vertical: bool,
    pub(crate) left_edge: bool,
    pub(crate) win_w: i32,
    pub(crate) win_h: i32,
    pub(crate) last_rect: Option<(i32, i32, i32, i32)>,
    pub(crate) needs_render: bool,
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

pub(crate) fn reset_width(hwnd: HWND) {
    OVERLAYS.with(|slot| {
        if let Some(ov) = slot.borrow_mut().iter_mut().find(|o| o.hwnd == hwnd) {
            ov.win_w = 0;
        }
    });
}

pub(crate) struct TaskbarInfo {
    pub(crate) rect: RECT,
    pub(crate) vertical: bool,
    pub(crate) left_edge: bool,
    pub(crate) monitor: isize,
}

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

pub(crate) unsafe fn taskbar_info_for(bar: HWND) -> Option<TaskbarInfo> {
    let mut r = RECT::default();
    if GetWindowRect(bar, &mut r).is_err() {
        return None;
    }
    if r.right <= r.left || r.bottom <= r.top {
        return None;
    }
    if !IsWindowVisible(bar).as_bool() {
        return None;
    }
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
    let intersects = r.bottom > mon_rect.top
        && r.top < mon_rect.bottom
        && r.right > mon_rect.left
        && r.left < mon_rect.right;
    if !intersects {
        return None;
    }
    let vertical = (r.bottom - r.top) > (r.right - r.left);
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

#[derive(Clone, Copy)]
pub(crate) struct ClockAnchorCache {
    pub(crate) bar: isize,
    pub(crate) bar_rect: RECT,
    pub(crate) at: Instant,
    pub(crate) left: Option<i32>,
}

pub(crate) static CLOCK_ANCHOR: Mutex<Vec<ClockAnchorCache>> = Mutex::new(Vec::new());

pub(crate) fn invalidate_clock_anchor() {
    if let Ok(mut guard) = CLOCK_ANCHOR.lock() {
        guard.clear();
    }
}

static PLACEMENT_SKIPPED_LOGGED: AtomicBool = AtomicBool::new(false);

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

// Skip topmost re-assertion while the context menu is tracked; lifting the overlay would cover the menu.
pub(crate) static MENU_TRACKING: AtomicBool = AtomicBool::new(false);
pub(crate) static OVERLAY_CREATE_FAIL_LOGGED: AtomicBool = AtomicBool::new(false);

static VD_MANAGER_FAIL_LOGGED: AtomicBool = AtomicBool::new(false);

pub(crate) unsafe fn vd_manager() -> Option<IVirtualDesktopManager> {
    VD_MANAGER.with(|slot| {
        if slot.borrow().is_none() {
            let created: windows::core::Result<IVirtualDesktopManager> = CoCreateInstance(
                &VirtualDesktopManager,
                None::<&windows::core::IUnknown>,
                CLSCTX_INPROC_SERVER,
            );
            match created {
                Ok(manager) => {
                    VD_MANAGER_FAIL_LOGGED.store(false, Ordering::Relaxed);
                    *slot.borrow_mut() = Some(manager);
                }
                Err(e) => {
                    if !VD_MANAGER_FAIL_LOGGED.swap(true, Ordering::Relaxed) {
                        log_event("WARN", &format!("virtual desktop manager unavailable: {e}"));
                    }
                    return None;
                }
            }
        }
        slot.borrow().clone()
    })
}

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
    log_event(
        "WARN",
        &format!(
            "overlay center covered by class='{}' pid={pid}",
            String::from_utf16_lossy(&class_buf[..class_len]),
        ),
    );
}

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

unsafe fn hide_overlay_for_missing_taskbar(hwnd: HWND) {
    if IsWindowVisible(hwnd).as_bool() {
        log_event(
            "INFO",
            "overlay hidden: taskbar not on screen (auto-hide or fullscreen app)",
        );
    }
    with_overlay_mut(hwnd, |ov| ov.visibility_anomaly = true);
    set_overlay_needs_render(hwnd, true);
    let _ = ShowWindow(hwnd, SW_HIDE);
}

pub(crate) unsafe fn reposition_overlay(hwnd: HWND) {
    let Some(ov) = overlay_by_hwnd(hwnd) else {
        return;
    };
    let Some(info) = taskbar_info_for(ov.taskbar) else {
        hide_overlay_for_missing_taskbar(hwnd);
        return;
    };
    maintain_visibility(hwnd);
    let orientation_changed = ov.vertical != info.vertical;
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
    if overlay_needs_render(hwnd) {
        relayout_overlay(hwnd);
        return;
    }
    apply_position_with_info(hwnd, info);
}

pub(crate) unsafe fn apply_position(hwnd: HWND) {
    let Some(ov) = overlay_by_hwnd(hwnd) else {
        return;
    };
    let Some(info) = taskbar_info_for(ov.taskbar) else {
        hide_overlay_for_missing_taskbar(hwnd);
        return;
    };
    maintain_visibility(hwnd);
    apply_position_with_info(hwnd, info);
}

struct PlacementInput {
    bar: RECT,
    monitor: RECT,
    vertical: bool,
    left_edge: bool,
    left_anchored: bool,
    tray_left: i32,
    left_margin_px: u32,
    width: i32,
    height: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Placement {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    on_screen: bool,
}

fn placement(input: PlacementInput) -> Placement {
    let (x, y) = if input.vertical {
        let center_x = input.bar.left + (input.bar.right - input.bar.left - input.width) / 2;
        let x = if input.left_edge {
            center_x.max(input.monitor.left)
        } else {
            center_x
                .min(input.monitor.right - input.width)
                .max(input.monitor.left)
        };
        let y = input.bar.top + input.left_margin_px as i32;
        (x, y)
    } else {
        let x = if input.left_anchored {
            input.bar.left + input.left_margin_px as i32
        } else {
            input.tray_left - input.width
        };
        let y = input.bar.top + (input.bar.bottom - input.bar.top - input.height) / 2;
        (x, y)
    };

    let on_screen = if input.vertical {
        y >= input.bar.top && y + input.height <= input.bar.bottom
    } else {
        x >= input.monitor.left
            && y >= input.monitor.top
            && x + input.width <= input.monitor.right
            && y + input.height <= input.monitor.bottom
    };

    Placement {
        x,
        y,
        width: input.width,
        height: input.height,
        on_screen,
    }
}

pub(crate) unsafe fn apply_position_with_info(hwnd: HWND, info: TaskbarInfo) {
    maintain_topmost(hwnd);
    let Some(ov) = overlay_by_hwnd(hwnd) else {
        return;
    };
    let bar = info.rect;
    let cfg = config();
    let w = ov.win_w;
    let h = ov.win_h;
    if w <= 0 || h <= 0 {
        return;
    }

    let left_anchored = !info.vertical
        && cfg.windows.anchor == sysvitals_core::Anchor::Left
        && win11_centered_taskbar();
    let tray_left = if info.vertical || left_anchored {
        0
    } else {
        taskbar_anchor_left(ov.taskbar)
    };
    let placed = placement(PlacementInput {
        bar,
        monitor: taskbar_monitor_rect_for(ov.taskbar),
        vertical: info.vertical,
        left_edge: info.left_edge,
        left_anchored,
        tray_left,
        left_margin_px: cfg.windows.left_margin_px,
        width: w,
        height: h,
    });
    if !placed.on_screen {
        warn_placement_skipped(placed.x, placed.y, placed.width, placed.height);
        return;
    }
    let (x, y) = (placed.x, placed.y);
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
    if !IsWindowVisible(hwnd).as_bool() {
        log_event("INFO", "overlay re-shown after being hidden");
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
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

pub(crate) unsafe fn find_taskbar() -> Option<HWND> {
    let cls = utf16("Shell_TrayWnd");
    FindWindowW(windows::core::PCWSTR(cls.as_ptr()), None).ok()
}

pub(crate) unsafe fn create_overlay(owner: Option<HWND>) -> Option<HWND> {
    let hmodule = GetModuleHandleW(None).ok()?;
    let class_name = utf16("SysVitalsOverlay");
    let title = utf16("SysVitals");
    let hwnd = CreateWindowExW(
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

pub(crate) unsafe fn reconcile_overlays() {
    if QUITTING.load(Ordering::Relaxed) {
        return;
    }
    let bars = taskbar_windows();
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
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        relayout_overlay(hwnd);
    }
}

pub(crate) fn reposition_all() {
    let hwnds: Vec<HWND> = OVERLAYS.with(|slot| slot.borrow().iter().map(|o| o.hwnd).collect());
    for hwnd in hwnds {
        unsafe { reposition_overlay(hwnd) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    fn input(bar: RECT, monitor: RECT, width: i32, height: i32) -> PlacementInput {
        PlacementInput {
            bar,
            monitor,
            vertical: false,
            left_edge: false,
            left_anchored: false,
            tray_left: 0,
            left_margin_px: 8,
            width,
            height,
        }
    }

    fn placed(x: i32, y: i32, width: i32, height: i32, on_screen: bool) -> Placement {
        Placement {
            x,
            y,
            width,
            height,
            on_screen,
        }
    }

    #[test]
    fn horizontal_bar_anchors_left_of_the_tray() {
        let mut sample = input(rect(0, 1032, 1920, 1080), rect(0, 0, 1920, 1080), 120, 40);
        sample.tray_left = 1800;

        assert_eq!(placement(sample), placed(1680, 1036, 120, 40, true));
    }

    #[test]
    fn horizontal_bar_centers_on_a_top_taskbar() {
        let mut sample = input(rect(0, 0, 1920, 48), rect(0, 0, 1920, 1080), 120, 40);
        sample.tray_left = 1800;

        assert_eq!(placement(sample), placed(1680, 4, 120, 40, true));
    }

    #[test]
    fn horizontal_left_anchor_uses_the_config_margin() {
        let mut sample = input(rect(0, 1032, 1920, 1080), rect(0, 0, 1920, 1080), 120, 40);
        sample.left_anchored = true;
        sample.tray_left = 1800;

        assert_eq!(placement(sample), placed(8, 1036, 120, 40, true));
    }

    #[test]
    fn horizontal_placement_that_leaves_the_monitor_is_rejected() {
        let mut sample = input(rect(0, 1032, 1920, 1080), rect(0, 0, 1920, 1080), 120, 40);
        sample.tray_left = 100;

        assert_eq!(placement(sample), placed(-20, 1036, 120, 40, false));
    }

    #[test]
    fn negative_virtual_coordinates_stay_on_a_left_monitor() {
        let mut sample = input(rect(-1920, 1032, 0, 1080), rect(-1920, 0, 0, 1080), 120, 40);
        sample.tray_left = -100;

        assert_eq!(placement(sample), placed(-220, 1036, 120, 40, true));
    }

    #[test]
    fn vertical_left_bar_keeps_the_left_edge_on_screen() {
        let mut sample = input(rect(0, 0, 48, 1080), rect(0, 0, 1920, 1080), 100, 40);
        sample.vertical = true;
        sample.left_edge = true;
        sample.left_margin_px = 20;

        assert_eq!(placement(sample), placed(0, 20, 100, 40, true));
    }

    #[test]
    fn vertical_right_bar_keeps_the_right_edge_on_screen() {
        let mut sample = input(rect(1872, 0, 1920, 1080), rect(0, 0, 1920, 1080), 100, 40);
        sample.vertical = true;
        sample.left_margin_px = 20;

        assert_eq!(placement(sample), placed(1820, 20, 100, 40, true));
    }

    #[test]
    fn vertical_placement_below_the_bar_is_rejected() {
        let mut sample = input(rect(0, 100, 48, 500), rect(0, 0, 1920, 1080), 100, 40);
        sample.vertical = true;
        sample.left_edge = true;
        sample.left_margin_px = 500;

        assert_eq!(placement(sample), placed(0, 600, 100, 40, false));
    }
}
