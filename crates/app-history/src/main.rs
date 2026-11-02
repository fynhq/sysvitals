//! SysVitals History — a GPUI trend chart popup.
//!
//! Launched by the platform overlay when the user clicks the indicator.
//! Shows CPU and memory usage over time as one merged chart, with a time
//! range selector and a Copy CSV button. When the launcher passes the
//! indicator rectangle via `--anchor`, the window opens as a topmost
//! borderless popup beside the indicator; otherwise it opens centered.
//! Closes on Esc or window blur.

// A GUI child process must not allocate a console window on Windows: without
// this attribute the binary is built as a console subsystem executable and a
// terminal flashes up beside the history window every time it launches.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::collections::HashSet;
use std::env;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
#[cfg(windows)]
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE,
};
#[cfg(windows)]
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, SetProcessWorkingSetSize, WaitForSingleObject,
    PROCESS_SYNCHRONIZE,
};
#[cfg(windows)]
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

use chrono::{DateTime, Local};
use gpui_kit::base::plot::label::{PlotLabel, Text as PlotText};
use gpui_kit::component::button::*;
use gpui_kit::component::chart::AreaChart;
use gpui_kit::component::theme::Theme as UiTheme;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use sysvitals_core::config::{config_path, history_path, Config, CsvScope};
use sysvitals_core::i18n::{t, Language};
use sysvitals_core::store::{floor_to_grid, spread_label_indices, Bucket, Store};
#[cfg(windows)]
use sysvitals_panel::panel_font_family;
use sysvitals_panel::{
    backdrop_for, detect_material, system_accent_hex, PanelSurface, CONTROL_RADIUS,
};

/// Screen rectangle of the indicator, passed by the overlay in GPUI's
/// global logical pixel space (top-left origin, y growing downward).
/// Both platform overlays convert their native coordinates to that space
/// before launching.
struct AnchorRect {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

/// Parse `--anchor L T R B` from the process arguments. Absent, malformed,
/// or degenerate rectangles yield `None`, which keeps the old centered
/// behavior — a bad argument must never prevent the window from opening.
fn parse_anchor() -> Option<AnchorRect> {
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg != "--anchor" {
            continue;
        }
        let mut nums = [0f32; 4];
        for slot in nums.iter_mut() {
            *slot = args.next()?.trim().parse().ok()?;
        }
        if !nums.iter().all(|n| n.is_finite()) {
            return None;
        }
        let [left, top, right, bottom] = nums;
        return (right > left && bottom > top).then_some(AnchorRect {
            left,
            top,
            right,
            bottom,
        });
    }
    None
}

/// Parse `--anchor-monitor <handle>` from the process arguments: the native
/// display handle (HMONITOR on Windows) that hosted the indicator. `None`
/// when absent, zero, or malformed.
fn parse_anchor_monitor() -> Option<DisplayId> {
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg != "--anchor-monitor" {
            continue;
        }
        let raw: u64 = args.next()?.trim().parse().ok()?;
        return (raw != 0).then(|| DisplayId::new(raw));
    }
    None
}

/// The usable area of the display an anchor sits on (taskbar/dock excluded),
/// used to flip and clamp the popup inside a screen the indicator is on.
///
/// When the indicator passed its native display handle, that display is
/// selected directly: GPUI scales each monitor's logical space by its own
/// factor, so a rectangle alone can fall inside two displays' bounds on a
/// mixed-DPI setup. The rectangle probe remains as the fallback for launches
/// without the handle (older companion process, macOS).
fn anchor_work_area(anchor: &AnchorRect, monitor: Option<DisplayId>, cx: &App) -> Bounds<Pixels> {
    if let Some(monitor) = monitor {
        if let Some(display) = cx.displays().into_iter().find(|d| d.id() == monitor) {
            return display.visible_bounds();
        }
    }
    let probe = point(
        px((anchor.left + anchor.right) * 0.5),
        px((anchor.top + anchor.bottom) * 0.5),
    );
    cx.displays()
        .into_iter()
        // The indicator lies inside the taskbar/menu bar, i.e. outside the
        // *work* area. Match against the full monitor bounds first, then use
        // that monitor's work area for placement; otherwise a secondary
        // taskbar would fall back to the primary display.
        .find(|display| display.bounds().contains(&probe))
        .or_else(|| {
            cx.displays()
                .into_iter()
                .find(|display| display.visible_bounds().contains(&probe))
        })
        .or_else(|| cx.primary_display())
        .map(|display| display.visible_bounds())
        .unwrap_or_else(|| Bounds::new(point(px(0.), px(0.)), size(px(1280.), px(720.))))
}

/// Logical breathing room between the popup and the edges it is placed
/// against: the taskbar/menu-bar side and, when the anchor sits close to a
/// monitor edge, the left/right work-area edges too.
const POPUP_GAP: f32 = 8.0;

/// Origin for the popup. The indicator sits inside the taskbar/menu bar,
/// which is *outside* the work area, so anchoring to `anchor.top - gap`
/// leaves almost no visual gap to the bar. Place the popup relative to the
/// work-area edge instead, with the same breathing room the shell's own
/// tray flyouts use. The bar side is inferred from the anchor being outside
/// the work area on that edge.
fn anchor_origin(
    anchor: &AnchorRect,
    size_w: f32,
    size_h: f32,
    work: &Bounds<Pixels>,
) -> Point<Pixels> {
    let work_left = f32::from(work.origin.x);
    let work_right = work_left + f32::from(work.size.width);
    let work_top = f32::from(work.origin.y);
    let work_bottom = work_top + f32::from(work.size.height);
    // Keep the same gap on the left/right screen edges: with the indicator
    // anchored at the taskbar's far left the centered popup would otherwise
    // be clamped flush against the monitor edge (x = 0). The width passed in
    // is already shrunk to leave room for both gaps on a narrow work area.
    let clamp_x = |x: f32| {
        let min = work_left + POPUP_GAP;
        let max = (work_right - size_w - POPUP_GAP).max(min);
        x.max(min).min(max)
    };
    let clamp_y = |y: f32| y.max(work_top).min((work_bottom - size_h).max(work_top));
    let center_x = (anchor.left + anchor.right) * 0.5;
    let center_y = (anchor.top + anchor.bottom) * 0.5;

    // The bar is below/above/left/right of the work area when the indicator
    // rect lies beyond that edge.
    let bar_below = anchor.top >= work_bottom - 1.0 && anchor.bottom > work_bottom;
    let bar_above = anchor.bottom <= work_top + 1.0 && anchor.top < work_top;
    let bar_left = anchor.right <= work_left + 1.0;
    let bar_right = anchor.left >= work_right - 1.0;

    if bar_below {
        return point(
            px(clamp_x(center_x - size_w * 0.5)),
            px((work_bottom - POPUP_GAP - size_h).max(work_top)),
        );
    }
    if bar_above {
        return point(
            px(clamp_x(center_x - size_w * 0.5)),
            px((work_top + POPUP_GAP).min((work_bottom - size_h).max(work_top))),
        );
    }
    if bar_left {
        return point(
            px((work_left + POPUP_GAP).min(work_right - size_w)),
            px(clamp_y(center_y - size_h * 0.5)),
        );
    }
    if bar_right {
        return point(
            px((work_right - POPUP_GAP - size_w).max(work_left)),
            px(clamp_y(center_y - size_h * 0.5)),
        );
    }

    // Fallback for a free-floating indicator: prefer below, then above.
    let below = anchor.bottom + POPUP_GAP;
    let above = anchor.top - POPUP_GAP - size_h;
    let y = if below + size_h <= work_bottom {
        below
    } else if above >= work_top {
        above
    } else {
        (work_bottom - size_h).max(work_top)
    };
    point(px(clamp_x(center_x - size_w * 0.5)), px(y))
}

/// Time range options for the chart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeRange {
    Min5,
    Hour1,
    Day1,
    Month1,
}

impl TimeRange {
    const ALL: [TimeRange; 4] = [
        TimeRange::Min5,
        TimeRange::Hour1,
        TimeRange::Day1,
        TimeRange::Month1,
    ];

    fn seconds(self) -> i64 {
        match self {
            TimeRange::Min5 => 300,
            TimeRange::Hour1 => 3600,
            TimeRange::Day1 => 86400,
            TimeRange::Month1 => 2_592_000,
        }
    }

    /// Fine bucket width shared by the chart curve and the CSV export:
    /// detailed enough to preserve resolution at each range while staying
    /// bounded (a month is ~720 rows, not 1.3M). The curve draws one point
    /// per fine bucket; only the sparse axis *labels* use the coarser clean
    /// step from [`Self::label_step_secs`].
    fn bucket_secs(self) -> i64 {
        match self {
            TimeRange::Min5 => 2,
            TimeRange::Hour1 => 10,
            TimeRange::Day1 => 120,
            TimeRange::Month1 => 3600,
        }
    }

    /// The clean wall-clock step the x-axis labels are floored to — the
    /// range picks the step so every label reads as a round local time:
    ///   5 min    = 60 s       (every local minute)
    ///   1 hour   = 600 s      (every :10 / :20 / ... local ten-minute mark)
    ///   1 day    = 7 200 s    (every 2 h at 00/02/.../22 local time)
    ///   1 month  = 432 000 s  (every 5 days at local midnight)
    /// Every grid is offset-aligned to the machine's local time zone; steps
    /// like 7200 s do not divide every real UTC offset (e.g. +5:30), so a
    /// zero-offset grid would read :30 instead of the intended marks.
    /// The curve is NOT drawn at this step: it uses [`Self::bucket_secs`] so
    /// the line keeps its detail. Each label point floors its own timestamp
    /// to this grid, so the six spread ticks read as clean times while the
    /// line stays fine-grained. The query window start is aligned down to
    /// this grid (see [`floor_to_grid`]), so the first label dates the window
    /// start.
    fn label_step_secs(self) -> i64 {
        match self {
            TimeRange::Min5 => 60,
            TimeRange::Hour1 => 600,
            TimeRange::Day1 => 7_200,
            TimeRange::Month1 => 432_000,
        }
    }

    fn label(self, lang: Language) -> &'static str {
        match self {
            TimeRange::Min5 => t(lang, "chart.range.5m"),
            TimeRange::Hour1 => t(lang, "chart.range.1h"),
            TimeRange::Day1 => t(lang, "chart.range.1d"),
            TimeRange::Month1 => t(lang, "chart.range.1mo"),
        }
    }

    /// Whether the open popup should reload this range every few seconds.
    /// 5 m and 1 h read a small tail of the raw table and stay live; 1 d and
    /// 1 mo scan the whole retention window (the month aggregate is seconds
    /// at the 30-day cap), so they load once on open/range switch and stay
    /// static while open. Clicking the active range button reloads it.
    fn auto_refresh(self) -> bool {
        matches!(self, TimeRange::Min5 | TimeRange::Hour1)
    }
}

/// A single timestamped sample pair for the merged chart: one fine bucket
/// renders as one x position carrying both series values, which is why CPU
/// and memory live in one struct instead of two point lists. `label` is the
/// sparse x-axis tick text — only clean wall-clock points (per
/// [`TimeRange::label_step_secs`]) carry a non-empty label, the rest stay
/// empty so the curve stays detailed while the axis stays readable. `ts` is
/// kept so the hover tooltip can show the full date and time for any point.
#[derive(Debug, Clone)]
struct ChartPoint {
    ts: i64,
    label: String,
    cpu: f64,
    mem: f64,
}

/// How often a visible history window reloads the stored samples. The
/// indicator writes samples every sample interval, so redrawing every few
/// seconds makes the chart follow the indicator in near real time.
const REFRESH_INTERVAL: Duration = Duration::from_secs(3);

/// Shared-state poll cadence. A visible popup needs tight polling so the
/// indicator's close request and blur dismissal feel immediate; the resident
/// process without a window is idle and can wake 10 times, not 25 times, per
/// second. Only Windows has the resident loop.
#[cfg(windows)]
const ACTIVE_POLL_MS: u64 = 40;
#[cfg(windows)]
const IDLE_POLL_MS: u64 = 100;

/// How many x-axis tick labels the panel draws, spread evenly across the fine
/// data points (mirrored by [`spread_label_indices`] when deciding which
/// points carry label text).
const X_TICK_COUNT: usize = 6;

/// Axis label size. The chart library paints its tick labels at a fixed
/// 10 px (`gpui_base::plot::label::TEXT_SIZE`) with no public size knob, so
/// this panel asks the chart for no labels (`x_tick_count(0)` and empty
/// `y_tick_format` keep both axis lines and their space) and paints them
/// itself at this size.
const AXIS_LABEL_SIZE: f32 = 12.0;

/// The strip the chart reserves under its axis line for x labels: the
/// library's 10 px label size plus four 2 px gaps (`AXIS_GAP`).
const X_AXIS_STRIP_H: f32 = 18.0;

/// The value-axis gutter the chart keeps left of the plot (`VALUE_AXIS_GAP`,
/// never narrower than 32 px for the 0..=100 labels at its 10 px font). The
/// x scale starts this far inside the chart and runs to its right edge, so
/// the custom labels use the same origin to stay under their data points.
const X_AXIS_GUTTER: f32 = 32.0;

/// Y-axis domain and tick count. The library's y labels are suppressed the
/// same way as the x labels (empty tick text keeps the gutter and axis
/// line), so the overlay below reproduces these ticks at the same size as
/// the x labels.
const Y_AXIS_MIN: f64 = 0.0;
const Y_AXIS_MAX: f64 = 100.0;
const Y_TICK_COUNT: usize = 6;

/// Chart stroke colors shared by the series, their fills, and the legend.
const CPU_HUE: f32 = 210.0;
const MEM_HUE: f32 = 150.0;

fn cpu_color() -> Hsla {
    hsla(CPU_HUE / 360.0, 0.8, 0.55, 1.0)
}

fn mem_color() -> Hsla {
    hsla(MEM_HUE / 360.0, 0.7, 0.45, 1.0)
}

/// A fill that is invisible: the merged chart draws two zero-alpha areas,
/// which is exactly two lines with no shading.
fn transparent_color() -> Hsla {
    hsla(0.0, 0.0, 0.0, 0.0)
}

// ------------------------------------------------------- resident mode (Windows)

#[cfg(windows)]
fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The command/status block created by the indicator. The resident process
/// keeps a read/write view of it for its whole lifetime; the one-shot macOS
/// path never maps it.
#[cfg(windows)]
struct HistoryShared {
    state: *mut sysvitals_core::HistoryState,
}

#[cfg(windows)]
impl HistoryShared {
    fn snapshot(&self) -> sysvitals_core::HistoryState {
        unsafe { sysvitals_core::HistoryState::read_consistent(self.state) }
    }

    fn set_visible(&self, visible: bool) {
        unsafe { sysvitals_core::HistoryState::write_visible(self.state, visible) }
    }

    /// Called by the popup's own dismissal paths (blur/Esc) and by the
    /// resident loop after it takes the window down. Only the child-owned
    /// `visible` flag is cleared: `desired_open` belongs to the indicator,
    /// and writing it here used to race a fresh click and swallow it. The
    /// resident loop records the dismissed generation locally instead.
    fn mark_hidden(&self) {
        self.set_visible(false);
    }
}

#[cfg(not(windows))]
struct HistoryShared;

#[cfg(not(windows))]
impl HistoryShared {
    fn mark_hidden(&self) {}
}

/// Open the indicator-owned state block. A missing block means this process
/// was not started by a resident-capable overlay, so it exits instead of
/// lingering invisibly.
#[cfg(windows)]
fn open_history_state() -> Option<*mut sysvitals_core::HistoryState> {
    let name = utf16(sysvitals_core::HISTORY_STATE_MAP);
    unsafe {
        let mapping = OpenFileMappingW(
            (FILE_MAP_READ | FILE_MAP_WRITE).0,
            false,
            windows::core::PCWSTR(name.as_ptr()),
        )
        .ok()?;
        let view = MapViewOfFile(
            mapping,
            FILE_MAP_READ | FILE_MAP_WRITE,
            0,
            0,
            std::mem::size_of::<sysvitals_core::HistoryState>(),
        );
        let _ = CloseHandle(mapping);
        (!view.Value.is_null()).then_some(view.Value as *mut sysvitals_core::HistoryState)
    }
}

/// `--parent-pid <pid>`: the process exits when the indicator does, even if
/// the launcher never gets a chance to terminate it.
fn parse_parent_pid() -> Option<u32> {
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--parent-pid" {
            return args.next()?.parse().ok();
        }
    }
    None
}

/// macOS one-shot popup: quit when the menu-bar indicator goes away instead
/// of lingering as an orphan window. Windows resident mode has the stronger
/// process-handle check in `run_resident`.
#[cfg(target_os = "macos")]
fn watch_parent(cx: &mut App) {
    let Some(pid) = parse_parent_pid() else {
        return;
    };
    log_event("INFO", &format!("history: watching parent pid {pid}"));
    cx.spawn(async move |cx| loop {
        cx.background_executor().timer(Duration::from_secs(1)).await;
        if unsafe { libc::kill(pid as i32, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            log_event("INFO", "history: parent process gone; exiting");
            cx.update(|app| app.quit());
            break;
        }
    })
    .detach();
}

#[cfg(windows)]
fn trim_working_set() {
    unsafe {
        // SIZE_T(-1) asks Windows to trim the process to its minimum working
        // set. Private commit stays allocated; what the user sees in Task
        // Manager (and what competes for physical RAM) drops to a few MB.
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

struct HistoryView {
    lang: Language,
    /// Created hidden by the resident launcher; do not claim activation on
    /// the first (offscreen) frame. The loop activates it after the first
    /// data load.
    hidden_start: bool,
    range: TimeRange,
    buckets: Vec<Bucket>,
    /// One connection kept for the window's lifetime. Reopening SQLite every
    /// three seconds re-ran WAL/schema setup and contended with the
    /// indicator's writer for no benefit.
    store: Option<Store>,
    /// One alert per failure streak: a corrupt database would otherwise log
    /// every refresh tick (3 s), drowning the useful history.
    data_error_logged: bool,
    csv_scope: CsvScope,
    copied: bool,
    copy_failed: bool,
    /// Monotonic ticket for the copy confirmation timer. An older timer must
    /// not clear a newer copy's message.
    copy_feedback_seq: u64,
    /// Last config mtime applied, in nanoseconds; the popup keeps language and
    /// CSV-scope settings live instead of freezing them at launch.
    config_mtime: u64,
    /// Guards blur-close: only close after the window has been active at
    /// least once, so a transient "inactive" signal at open time (a macOS
    /// nonactivating panel that has not become key yet) cannot close the
    /// popup the instant it appears.
    ever_active: bool,
    /// Kept alive so the activation observer is not cancelled; dropped when
    /// the view is released.
    _activation: Option<Subscription>,
    /// Kept alive so the appearance observer is not cancelled. The panel
    /// always mirrors the OS light/dark theme (independent of the user's
    /// `display.theme` setting, which now only governs the indicator), so
    /// a live OS flip repaints this window too — see `observe_appearance`.
    _appearance: Option<Subscription>,
    /// Kept alive so the Esc observer is not cancelled. Esc closes the popup.
    /// This is an app-level keystroke observer, not a window paint-phase
    /// listener: registering from `render` with `Window::on_key_event` would
    /// touch the dispatch tree's active node, which does not exist yet for
    /// the root view on the first paint frame — GPUI unwraps it and panics,
    /// killing the process the moment the popup opens. This process owns a
    /// single window with no text fields, so a global keystroke observer is
    /// both safe and simpler.
    _escape: Option<Subscription>,
    /// Last accent value seen by the live-refresh poll; the override only
    /// re-applies when this moves (see `start_accent_watch`). Seeded with
    /// `None` so the first tick always re-applies once (idempotent when the
    /// startup override already succeeded).
    last_accent: Option<u32>,
    /// Signals the resident launcher that the first database read finished.
    /// The window stays hidden until this flips, so the first visible frame
    /// already carries chart data.
    first_load: Arc<AtomicBool>,
    /// Shared command/status block in resident mode; `None` for the one-shot
    /// launch used on macOS and for the fallback path.
    shared: Option<Arc<HistoryShared>>,
}

impl HistoryView {
    fn new(
        lang: Language,
        csv_scope: CsvScope,
        hidden_start: bool,
        first_load: Arc<AtomicBool>,
        shared: Option<Arc<HistoryShared>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self {
            lang,
            hidden_start,
            range: TimeRange::Hour1,
            buckets: Vec::new(),
            store: None,
            data_error_logged: false,
            csv_scope,
            copied: false,
            copy_failed: false,
            copy_feedback_seq: 0,
            config_mtime: 0,
            ever_active: false,
            _activation: None,
            _appearance: None,
            _escape: None,
            last_accent: None,
            first_load,
            shared,
        };
        view.start_refresh_loop(cx);
        view.start_escape_observer(cx);
        view.start_accent_watch(cx);
        view
    }

    /// Tell the resident indicator that this popup is gone. Called before
    /// every `remove_window`; a no-op for the one-shot launch.
    fn mark_hidden(&self) {
        if let Some(shared) = &self.shared {
            shared.mark_hidden();
        }
    }

    /// Poll the OS accent every 500 ms and re-apply the override when it
    /// moved. The appearance observer only fires on a light/dark MODE flip
    /// (the Windows platform layer filters `WM_SETTINGCHANGE` down to
    /// `ImmersiveColorSet` with an appearance change), so an accent-only
    /// change never re-runs `apply_accent` on its own. The probe runs inside
    /// `update`: on macOS `system_accent_hex` reads AppKit's
    /// `controlAccentColor`, which belongs to the main thread. The loop stops
    /// with the view entity, i.e. when the window closes. Same shape as the
    /// settings window's accent poll.
    fn start_accent_watch(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            let alive = this
                .update(cx, |view: &mut HistoryView, cx| {
                    let current = system_accent_hex();
                    if current == view.last_accent {
                        return true;
                    }
                    view.last_accent = current;
                    if current.is_some() {
                        sysvitals_panel::apply_accent(cx);
                    } else {
                        // The OS moved back to "multicolor": reload the palette
                        // so the old accent override is removed.
                        apply_theme_and_accent(cx);
                    }
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !alive {
                break;
            }
        })
        .detach();
    }

    /// Dismiss the popup when Esc is pressed.
    fn start_escape_observer(&mut self, cx: &mut Context<Self>) {
        if self._escape.is_some() {
            return;
        }
        let subscription = cx.observe_keystrokes(|view, event, window, _cx| {
            if event.keystroke.key == "escape" {
                view.mark_hidden();
                window.remove_window();
            }
        });
        self._escape = Some(subscription);
    }

    /// Watch the window's OS-appearance changes and re-apply the system
    /// palette (the popup always follows the system; `display.theme` is
    /// indicator-only). Idempotent, and registered from the first render
    /// pass like `observe_activation` — never from a paint-phase callback,
    /// which would hit the empty active-node stack on the root view's first
    /// frame (see the crash note on `_escape`).
    fn observe_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self._appearance.is_some() {
            return;
        }
        let subscription = cx.observe_window_appearance(window, |_view: &mut Self, _w, cx| {
            // The popup always mirrors the OS light/dark appearance; a
            // live system flip repaints it with the same palette that the
            // rest of the desktop is now using. Accent override must be
            // re-applied because a palette reload replaces every field.
            apply_theme_and_accent(cx);
            cx.notify();
        });
        self._appearance = Some(subscription);
    }

    /// Watch the window's activation and dismiss the popup when it loses
    /// focus after having been focused once. Idempotent.
    fn observe_activation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self._activation.is_some() {
            return;
        }
        // The observer only sees activation *changes*: a window created with
        // `focus: true` is already active when the first render registers it,
        // and that edge is not replayed. Seed the guard from the current
        // state, otherwise the first click-away would not close the popup.
        if window.is_window_active() {
            self.ever_active = true;
        }
        // On Windows the popup can open *without* ever being activated: the
        // indicator overlay was the foreground process when it launched this
        // child, and the foreground-lock rules can reject the startup
        // activation attempt — then `ever_active` never flips and a click away
        // never closes the window (the reported bug: a fast double click opens
        // the popup but clicking elsewhere leaves it stuck; a single click
        // usually activates fine). When the first-frame seed found the window
        // inactive, claim activation through GPUI's own `activate_window` path
        // (reassert placement + inject an Alt press so `SetForegroundWindow`
        // is permitted). The resulting WM_ACTIVATE edge flows through the
        // observer below and arms the blur-close. macOS is deliberately
        // untouched: its panel is an accessory app's window with its own
        // key-window lifecycle, unverified at runtime, and the bug was only
        // ever reproduced on Windows.
        #[cfg(windows)]
        if !self.ever_active && !self.hidden_start {
            window.activate_window();
            // If activation completed synchronously, the observer below will
            // not see an edge; seed the guard from the post-activation state.
            if window.is_window_active() {
                self.ever_active = true;
            }
        }
        let subscription = cx.observe_window_activation(window, |view, window, _| {
            if window.is_window_active() {
                view.ever_active = true;
            } else if view.ever_active {
                // Clicked away: dismiss the popup like a native menu.
                view.mark_hidden();
                window.remove_window();
            }
        });
        self._activation = Some(subscription);
    }

    /// Reload the stored samples every [`REFRESH_INTERVAL`] while the window
    /// lives. The loop stops as soon as the view entity is released, i.e.
    /// when the window closes.
    /// Reload language and CSV scope when the config file changes while the
    /// popup is open. The theme is system-owned; only these two settings are
    /// consumed by this panel. Returns whether anything visible changed so
    /// the static 1 d / 1 mo views can skip their three-second repaint.
    fn reload_config(&mut self) -> bool {
        let Ok(path) = config_path() else {
            return false;
        };
        let mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        if mtime == self.config_mtime {
            return false;
        }
        match Config::load(&path) {
            Ok((cfg, _)) => {
                self.config_mtime = mtime;
                let changed =
                    self.lang != cfg.display.language || self.csv_scope != cfg.history.csv_scope;
                self.lang = cfg.display.language;
                self.csv_scope = cfg.history.csv_scope;
                changed
            }
            Err(e) => {
                // Retry only when the file changes again, and keep the last
                // good values in the meantime.
                self.config_mtime = mtime;
                log_event("WARN", &format!("history: config reload failed: {e}"));
                false
            }
        }
    }

    fn start_refresh_loop(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            // Let the window paint its first frame before touching SQLite:
            // opening the database and aggregating buckets used to run inside
            // `HistoryView::new`, delaying the first visible paint.
            cx.background_executor()
                .timer(Duration::from_millis(16))
                .await;
            if this
                .update(cx, |view: &mut HistoryView, cx| {
                    view.reload_config();
                    view.load_data();
                    view.first_load.store(true, Ordering::Release);
                    log_event(
                        "INFO",
                        &format!(
                            "history: window created at {}ms, first data {}ms later ({} points, hidden_start={})",
                            WINDOW_CREATED_MS.load(Ordering::Relaxed),
                            elapsed_ms().saturating_sub(WINDOW_CREATED_MS.load(Ordering::Relaxed) as u128),
                            view.buckets.len(),
                            view.hidden_start,
                        ),
                    );
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                if this
                    .update(cx, |view: &mut HistoryView, cx| {
                        let config_changed = view.reload_config();
                        // 1 d / 1 mo scan the whole retention window; keep
                        // them static while open. The range button loads them
                        // on switch (and re-clicking the active range reloads).
                        // Their static view also skips the repaint unless the
                        // config changed something visible.
                        let live = view.range.auto_refresh();
                        if live {
                            view.load_data();
                        }
                        if live || config_changed {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// The exact query window the chart is showing: a start floored to the
    /// range's label grid, the current time, the fine curve bucket width, and
    /// the local-time offset used for both bucketing and label flooring.
    /// CSV export and the chart both use this so "copy what you see" is true
    /// at the window boundary too.
    fn visible_query(&self, now: i64) -> (i64, i64, i64, i64) {
        let step = self.range.label_step_secs();
        let offset = Local::now().offset().local_minus_utc() as i64;
        let from = floor_to_grid(now - self.range.seconds(), step, offset);
        (from, now, self.range.bucket_secs(), offset)
    }

    /// Keep one SQLite connection for the window. Reopening every refresh
    /// re-ran WAL/schema setup and could fail with `SQLITE_BUSY` while the
    /// indicator was writing, leaving the chart empty.
    fn ensure_store(&mut self) -> bool {
        if self.store.is_some() {
            return true;
        }
        let path = match history_path() {
            Ok(path) => path,
            Err(e) => {
                self.log_data_error(format!("history: cannot resolve the store path: {e}"));
                return false;
            }
        };
        match Store::open(&path) {
            Ok(store) => {
                self.store = Some(store);
                true
            }
            Err(e) => {
                self.log_data_error(format!(
                    "history: store open failed: {}: {e}",
                    path.display()
                ));
                false
            }
        }
    }

    fn log_data_error(&mut self, message: String) {
        if !self.data_error_logged {
            log_event("WARN", &message);
            self.data_error_logged = true;
        }
    }

    fn load_data(&mut self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let (from, to, bucket, offset) = self.visible_query(now);
        // Clear on failure: otherwise a range switch would keep showing the
        // previous range's points under the new axis.
        let mut buckets = Vec::new();
        if self.ensure_store() {
            // The curve draws at the fine CSV bucket width; axis labels
            // floor each point's timestamp to the clean grid (see
            // `points`). One query serves both — no coarse pass anymore.
            let result = self
                .store
                .as_ref()
                .map(|store| store.buckets_offset(from, to, bucket, offset));
            match result {
                Some(Ok(loaded)) => {
                    buckets = loaded;
                    self.data_error_logged = false;
                }
                Some(Err(e)) => {
                    self.log_data_error(format!("history: query failed: {e}"));
                    // A broken connection should be retried on the next
                    // refresh rather than kept forever.
                    self.store = None;
                }
                None => {}
            }
        }
        self.buckets = buckets;
    }

    fn points(&self) -> Vec<ChartPoint> {
        // One point per fine bucket: the curve keeps its full resolution.
        // Only the indices the panel paints labels at (`spread_label_indices`,
        // mirroring the library's selection rule) carry label text, floored to
        // the range's clean grid (60/600/7200/432000 s) so every visible tick
        // reads as a round time. A labeled point's pixel position may sit up
        // to one fine bucket off that clean boundary — under 1% of the window
        // — which is the cost of keeping the line detailed; the label text,
        // not the geometry, carries the roundness.
        //
        // Day-crossing: the first labeled point, and any whose floored local
        // day differs from the previous labeled point's, carries "MM-DD HH:MM"
        // so a midnight crossing is visible; within one day the label is just
        // HH:MM. The month view labels with MM-DD only. A freshly started
        // indicator can floor two labeled points to the same wall-clock string
        // — painting it twice reads as broken, so only the first keeps it.
        let month_view = self.range == TimeRange::Month1;
        let step = self.range.label_step_secs();
        let offset = Local::now().offset().local_minus_utc() as i64;
        let labeled: HashSet<usize> = spread_label_indices(self.buckets.len(), X_TICK_COUNT)
            .into_iter()
            .collect();
        let mut points = Vec::with_capacity(self.buckets.len());
        let mut prev_day: Option<String> = None;
        let mut last_shown: Option<String> = None;
        for (i, b) in self.buckets.iter().enumerate() {
            let mut label = String::new();
            if labeled.contains(&i) {
                let clean = floor_to_grid(b.ts, step, offset);
                let day = day_of(clean);
                let crosses = prev_day.as_ref().is_none_or(|p| *p != day);
                prev_day = Some(day);
                label = if month_view {
                    fmt_date(clean)
                } else if crosses {
                    fmt_date_time(clean)
                } else {
                    fmt_time(clean)
                };
                if last_shown.as_deref() == Some(label.as_str()) {
                    label = String::new();
                } else {
                    last_shown = Some(label.clone());
                }
            }
            points.push(ChartPoint {
                ts: b.ts,
                label,
                cpu: b.cpu,
                mem: b.mem,
            });
        }
        points
    }

    fn copy_csv(&mut self, cx: &mut Context<Self>) {
        self.copy_failed = false;
        let lang = self.lang;

        if self.csv_scope == CsvScope::Visible {
            // Exactly the points currently drawn, including the grid-aligned
            // window start and the local-time bucket phase.
            self.finish_copy(self.buckets.clone(), lang, cx);
            return;
        }

        // The longer scopes scan the whole retention window; a month of raw
        // samples on the UI thread visibly froze the popup. Query in the
        // background and only build the clipboard text back on the UI thread.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let (seconds, bucket_secs) = match self.csv_scope {
            CsvScope::Visible => unreachable!(),
            CsvScope::Last24h => (86_400, 120),
            CsvScope::Last7d => (604_800, 600),
            CsvScope::Last30d => (2_592_000, 3600),
        };
        let offset = Local::now().offset().local_minus_utc() as i64;
        let from = floor_to_grid(now - seconds, bucket_secs, offset);
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let loaded = executor
                .spawn(async move {
                    let path = history_path().map_err(|e| e.to_string())?;
                    let store = Store::open(&path).map_err(|e| e.to_string())?;
                    store
                        .buckets_offset(from, now, bucket_secs, offset)
                        .map_err(|e| e.to_string())
                })
                .await;
            let _ = this.update(cx, |view: &mut HistoryView, cx| match loaded {
                Ok(buckets) => view.finish_copy(buckets, lang, cx),
                Err(e) => {
                    log_event("WARN", &format!("history: CSV query failed: {e}"));
                    view.copied = false;
                    view.copy_failed = true;
                    view.schedule_copied_reset(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Build the clipboard text, write it, and arm the confirmation reset.
    fn finish_copy(&mut self, buckets: Vec<Bucket>, lang: Language, cx: &mut Context<Self>) {
        // Tab-separated rows paste straight into spreadsheet cells (WPS /
        // Excel split on tabs on paste), and unlike a space separator they
        // do not collide with the internal space of the "YYYY-MM-DD HH:MM:SS"
        // timestamp column. The UTF-8 BOM keeps CJK headers readable in Excel.
        let mut csv = String::from("\u{FEFF}");
        // Header labels come from the core i18n table (single source of truth
        // for the CSV columns), not a local copy, so a header edit lives in
        // exactly one place.
        let header = format!(
            "{}\t{}\t{}",
            t(lang, "csv.header.timestamp"),
            t(lang, "csv.header.cpu"),
            t(lang, "csv.header.mem")
        );
        csv.push_str(&header);
        csv.push('\n');
        for b in &buckets {
            let ts_str = format_ts_full(b.ts);
            csv.push_str(&format!("{ts_str}\t{:.1}\t{:.1}\n", b.cpu, b.mem));
        }

        match arboard::Clipboard::new() {
            Ok(mut clipboard) => match clipboard.set_text(csv) {
                Ok(()) => {
                    self.copied = true;
                    self.copy_failed = false;
                    log_event(
                        "INFO",
                        &format!("history: CSV copied ({} rows)", buckets.len()),
                    );
                }
                Err(e) => {
                    self.copied = false;
                    self.copy_failed = true;
                    log_event("WARN", &format!("history: clipboard write failed: {e}"));
                }
            },
            Err(e) => {
                self.copied = false;
                self.copy_failed = true;
                log_event("WARN", &format!("history: clipboard unavailable: {e}"));
            }
        }
        self.schedule_copied_reset(cx);
        cx.notify();
    }

    /// Clear the "copied" confirmation once the history window has had time
    /// to show it. Each copy takes a new ticket; an older timer that fires
    /// later must not clear a newer confirmation.
    fn schedule_copied_reset(&mut self, cx: &mut Context<Self>) {
        self.copy_feedback_seq = self.copy_feedback_seq.wrapping_add(1);
        let seq = self.copy_feedback_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1200))
                .await;
            let _ = this.update(cx, |view: &mut HistoryView, cx| {
                if view.copy_feedback_seq == seq {
                    view.copied = false;
                    view.copy_failed = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

/// Format a unix timestamp as HH:MM in the machine's local time zone.
fn fmt_time(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&Local).format("%H:%M").to_string())
        .unwrap_or_default()
}

/// Format a unix timestamp as MM-DD, the terse date label for the month
/// view's x axis (year is redundant inside a 30-day window).
fn fmt_date(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&Local).format("%m-%d").to_string())
        .unwrap_or_default()
}

/// Format a unix timestamp as `MM-DD HH:MM`, used when a shorter view's x
/// axis crosses midnight and the tick that opens a new day must show both.
fn fmt_date_time(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&Local).format("%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// Local calendar date as a stable comparison key (`YYYY-MM-DD`). Two
/// timestamps match only if their wall-clock date in the user's zone
/// matches, which is the exact "day crossed" test the x-axis labels need.
/// A string key is used rather than an integer day count because it relies
/// on only the `%Y-%m-%d` format directive, avoiding a version-dependent
/// helper on `NaiveDate`.
fn day_of(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&Local).format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// Format a unix timestamp as YYYY-MM-DD HH:MM:SS in the machine's local
/// time zone. Used by both the CSV export and the hover tooltip so exported
/// rows and the point readout agree.
fn format_ts_full(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}

// --------------------------------------------------------------- diagnostics

/// Process-start instant for the popup's startup milestones. The launcher
/// already logs how long `CreateProcessW` took; these milestones cover the
/// other half of the wait — from process start to window creation, first
/// chart data, and (for a pre-warmed window) the reveal.
static START: OnceLock<Instant> = OnceLock::new();

/// Elapsed time at the moment the window was created, stored so the first
/// data-load log (which runs after the first frame, off the startup path)
/// can report it without writing to disk during startup.
static WINDOW_CREATED_MS: AtomicU64 = AtomicU64::new(0);

fn elapsed_ms() -> u128 {
    START
        .get()
        .map(|start| start.elapsed().as_millis())
        .unwrap_or(0)
}

/// Append one milestone line to the shared daily log. The log path never
/// fails the popup: errors are swallowed inside `logger::log_line`.
fn log_event(level: &str, message: &str) {
    sysvitals_panel::log_event(level, message);
}

impl Render for HistoryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = self.lang;
        let points = self.points();
        let point_count = points.len();
        // The chart itself paints no x labels (`x_tick_count(0)` below); this
        // panel draws them at a readable size instead. Each entry is a data
        // index plus its clean-time label text; only the six indices the
        // chart would label carry one.
        let axis_labels: Vec<(usize, String)> = points
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.label.is_empty())
            .map(|(i, p)| (i, p.label.clone()))
            .collect();
        let copied = self.copied;
        let copy_failed = self.copy_failed;
        let current_range = self.range;

        // Register the blur-close observer and the live appearance observer
        // exactly once. Both are registered here (not from `new`) because
        // neither is available before the window exists; and neither uses a
        // paint-phase API, so the empty-dispatch-tree root-view trap that
        // killed the original Esc handler does not apply.
        self.observe_activation(window, cx);
        self.observe_appearance(window, cx);

        // Build range buttons individually to avoid borrow issues with cx.listener.
        let mut range_buttons = h_flex().gap_1();
        for r in TimeRange::ALL {
            let is_active = r == current_range;
            let label = r.label(lang).to_string();
            let btn = Button::new(format!("range-{:?}", r))
                .small()
                .rounded(px(CONTROL_RADIUS))
                .when(is_active, |b| b.primary())
                .label(label)
                .on_click(cx.listener(move |this: &mut Self, _: &ClickEvent, _, cx| {
                    this.range = r;
                    this.load_data();
                    cx.notify();
                }));
            range_buttons = range_buttons.child(btn);
        }

        // Icon-only copy button: a green check flashes for ~1.2s after a
        // successful copy, then the button returns to the copy icon. The
        // tooltip keeps the textual hint in the current language.
        let copy_tip = if copied {
            t(lang, "chart.copied").to_string()
        } else if copy_failed {
            t(lang, "chart.copy_failed").to_string()
        } else {
            t(lang, "chart.copy_csv").to_string()
        };

        let copy_btn = Button::new("copy-csv")
            .small()
            .rounded(px(CONTROL_RADIUS))
            .tooltip(copy_tip)
            .when(copied, |b| {
                b.icon(IconName::CircleCheck)
                    .text_color(hsla(150.0 / 360.0, 0.7, 0.45, 1.0))
            })
            .when(!copied, |b| b.icon(IconName::Copy))
            .when(copy_failed, |b| b.text_color(hsla(0.0, 0.7, 0.5, 1.0)))
            .on_click(cx.listener(|this: &mut Self, _: &ClickEvent, _, cx| {
                this.copy_csv(cx);
            }));

        let header = h_flex()
            .justify_between()
            .items_center()
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .child(t(lang, "chart.title").to_string()),
            )
            .child(h_flex().gap_2().child(range_buttons).child(copy_btn));
        // `theme.border` resolves to a near-black neutral in dark mode, so
        // on the dark tinted popup the hairline read as a black line. Derive
        // it from the foreground instead: light on dark, dark on light, like
        // the system separators.
        let header_divider = div()
            .h(px(1.))
            .w_full()
            .bg(cx.theme().foreground.opacity(0.12));

        // One merged chart for both series. AreaChart accumulates one entry
        // per `y`/`stroke`/`fill`/`name` call in order, so two of each give
        // two lines; the zero-alpha fills keep the look of plain lines.
        // Percentages share one 0..=100 axis, which is what makes merging
        // sane.
        //
        // `appear(false)`: the library's default 1s left-to-right draw-in
        // is what the user experiences as "slow to show" — the popup's
        // job is to be readable the instant it opens, so the sweep is off.
        //
        // Y axis: `y_domain(0, 100)` + `y_tick_count(6)` + `y_padding(0, 0)`.
        // The library spaces the ticks evenly over the *full* plot height but
        // maps the pinned domain only into the height shrunk by the top
        // padding (10 px default), so the labels would read the padded
        // positions (e.g. 20.8 / 41.6 / ... instead of 20 / 40 / ...). With
        // zero padding the tick grid and the domain grid coincide and the
        // six labels land exactly on 0 / 20 / 40 / 60 / 80 / 100; the series
        // clips at the domain edges as before (pinned-plot mask). Its tick
        // labels are also painted by the overlay: `y_tick_format` returns
        // empty text, which keeps the axis line and the 32 px value-axis
        // gutter (the minimum gutter is fixed) while hiding the library's
        // 10 px labels.
        //
        // X axis: `x_tick_count(0)` keeps the axis line and the space the
        // library reserves for tick labels, but suppresses the labels: it
        // paints them at a fixed 10 px with no public size knob, so the
        // labels are drawn at `AXIS_LABEL_SIZE` by the overlay added below.
        // `points()` still labels the same six evenly spread indices
        // (mirroring the library's selection rule) with clean-time text, so
        // the overlay reads exactly like the axis the chart would have drawn,
        // only larger.
        //
        // `tooltip_title` overrides the axis label with the full
        // `YYYY-MM-DD HH:MM:SS` (with seconds), because the axis labels are
        // deliberately terse and a hover should give exact timing.
        //
        // Both label sets are painted by one canvas over the chart. The
        // canvas receives the chart's final bounds at paint time, so the
        // labels reproduce the library's plot geometry (the 32 px value-axis
        // gutter and the 18 px strip under the x axis) without re-deriving
        // the layout. The canvas paints before the chart, keeping a hover
        // tooltip above the labels.
        let y_labels: Vec<String> = (0..Y_TICK_COUNT)
            .map(|k| {
                let value =
                    Y_AXIS_MAX - (Y_AXIS_MAX - Y_AXIS_MIN) * k as f64 / (Y_TICK_COUNT - 1) as f64;
                format!("{value:.0}")
            })
            .collect();
        let label_color = cx.theme().muted_foreground;
        let axis_overlay = canvas(
            |_bounds, _window, _cx| {},
            move |bounds, _, window, cx| {
                let plot_left = X_AXIS_GUTTER;
                let plot_h = (f32::from(bounds.size.height) - X_AXIS_STRIP_H).max(0.0);
                let plot_w = (f32::from(bounds.size.width) - plot_left).max(0.0);
                let mut texts = Vec::with_capacity(axis_labels.len() + y_labels.len());
                for (i, text) in &axis_labels {
                    let frac = if point_count > 1 {
                        *i as f32 / (point_count - 1) as f32
                    } else {
                        0.5
                    };
                    let first = point_count > 1 && *i == 0;
                    let last = point_count > 1 && *i == point_count - 1;
                    let align = if first {
                        TextAlign::Left
                    } else if last {
                        TextAlign::Right
                    } else {
                        TextAlign::Center
                    };
                    texts.push(
                        PlotText::new(
                            text.clone(),
                            point(px(plot_left + plot_w * frac), px(plot_h + 3.0)),
                            label_color,
                        )
                        .font_size(px(AXIS_LABEL_SIZE))
                        .align(align),
                    );
                }
                let last_tick = (y_labels.len() - 1).max(1) as f32;
                for (k, text) in y_labels.iter().enumerate() {
                    let tick_y = plot_h * k as f32 / last_tick;
                    let top = (tick_y - AXIS_LABEL_SIZE * 0.5)
                        .clamp(0.0, (plot_h - AXIS_LABEL_SIZE).max(0.0));
                    texts.push(
                        PlotText::new(
                            text.clone(),
                            point(px(plot_left - 4.0), px(top)),
                            label_color,
                        )
                        .font_size(px(AXIS_LABEL_SIZE))
                        .align(TextAlign::Right),
                    );
                }
                PlotLabel::new(texts).paint(&bounds, window, cx);
            },
        )
        .absolute()
        .size_full();
        let merged_chart = div().flex_1().relative().child(axis_overlay).child(
            AreaChart::new(points)
                .x(|p: &ChartPoint| p.label.clone())
                .y(|p: &ChartPoint| p.cpu)
                .stroke(cpu_color())
                .fill(transparent_color())
                .name(t(lang, "chart.cpu").to_string())
                .y(|p: &ChartPoint| p.mem)
                .stroke(mem_color())
                .fill(transparent_color())
                .name(t(lang, "chart.mem").to_string())
                .y_domain(Y_AXIS_MIN, Y_AXIS_MAX)
                .y_padding(0.0, 0.0)
                .x_axis(true)
                .x_tick_count(0)
                .y_axis(true)
                .y_tick_format(|_| "")
                .y_tick_count(Y_TICK_COUNT)
                .grid(true)
                .appear(false)
                .tooltip_title(|p: &ChartPoint| format_ts_full(p.ts).into())
                .tooltip_value(|_p: &ChartPoint, _series: usize, v: f64| format!("{v:.2}%").into()),
        );

        // Shared legend: dot + label per series, aligned with the chart's
        // own colors.
        let legend = h_flex()
            .gap_4()
            .items_center()
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(div().w(px(8.)).h(px(8.)).rounded(px(4.)).bg(cpu_color()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().foreground.opacity(0.7))
                            .child(t(lang, "chart.cpu").to_string()),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(div().w(px(8.)).h(px(8.)).rounded(px(4.)).bg(mem_color()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().foreground.opacity(0.7))
                            .child(t(lang, "chart.mem").to_string()),
                    ),
            );

        v_flex()
            .size_full()
            .p_4()
            .gap_2()
            .child(header)
            .child(header_divider)
            .child(legend)
            .child(merged_chart)
    }
}

// ---------------------------------------------------------------------------
// Shared theme, accent and backdrop helpers
// ---------------------------------------------------------------------------

/// Re-apply the shared theme/accent/material stack, then the history-only
/// chart-grid, button-state and font overrides.
fn apply_theme_and_accent(cx: &mut App) {
    sysvitals_panel::apply_theme_and_accent(cx, PanelSurface::History);
    // The library's `chart_grid` falls back to `border.opacity(0.6)`, and
    // the dark palette's border is a near-black neutral, so the grid read as
    // black lines on the dark panel. Derive it from the foreground instead:
    // light hairlines on dark, dark hairlines on light.
    UiTheme::update(cx, |theme| {
        theme.colors.chart_grid = theme.foreground.opacity(0.12);
    });
    // The button hover/active fills are translucent neutral layers. Windows'
    // renderer accumulates alpha across nested translucent layers, so over
    // this popup's material-tinted page the hover surface renders as a mid
    // gray. Use the opaque equivalents of what the settings window shows
    // over its opaque cards, so hovering a range or copy button reads the
    // same in both panels. macOS composites alpha correctly and keeps the
    // library colors.
    #[cfg(windows)]
    UiTheme::update(cx, |theme| {
        let (hover, active) = if theme.mode.is_dark() {
            (hsla(0., 0., 0.176, 1.), hsla(0., 0., 0.173, 1.))
        } else {
            (hsla(0., 0., 0.941, 1.), hsla(0., 0., 0.957, 1.))
        };
        theme.colors.button_hover = hover;
        theme.colors.button_active = active;
    });
    // Win11's Segoe UI Variable has an optical-size axis tuned for UI text;
    // the palette reload above may reset the family, so re-apply it here.
    #[cfg(windows)]
    UiTheme::update(cx, |theme| {
        if let Some(family) = panel_font_family() {
            theme.font_family = family.into();
        }
    });
}

/// Open one history window. The caller owns its lifetime: the macOS path
/// opens it visible and lets the process exit with it, while the resident
/// Windows loop opens it hidden, waits for the first data load, then reveals
/// it.
///
/// `hidden_start` is passed to the view so its activation observer does not
/// claim activation for the offscreen first frame: the resident launcher
/// activates the window itself once the chart data is ready.
fn open_history_window(
    cx: &mut App,
    anchor: Option<AnchorRect>,
    monitor: Option<DisplayId>,
    hidden_start: bool,
    shared: Option<Arc<HistoryShared>>,
) -> Option<(AnyWindowHandle, Arc<AtomicBool>)> {
    let cfg = match config_path().and_then(|p| Config::load(&p).map(|(c, _)| c)) {
        Ok(cfg) => cfg,
        Err(e) => {
            log_event(
                "WARN",
                &format!("history: config load failed: {e}; using defaults"),
            );
            Config::default()
        }
    };
    let lang = cfg.display.language;
    let csv_scope = cfg.history.csv_scope;
    let window_title = format!("SysVitals {}", t(lang, "chart.title"));

    // Popup geometry in GPUI logical pixels. Height cut 25% (520 -> 390):
    // a 3x shorter vertical span is still readable for a two-line trend,
    // and the popup sits closer to the indicator without dominating the
    // desktop. Anchor origin logic keeps `POPUP_GAP` on every edge it is
    // placed against, so a work area narrower than the default width
    // shrinks the popup instead of pinning it flush to a screen edge.
    let popup_size = size(px(680.), px(390.));
    let window_bounds = match anchor.as_ref() {
        Some(a) => {
            let work = anchor_work_area(a, monitor, cx);
            let work_w = f32::from(work.size.width);
            let width = f32::from(popup_size.width).min((work_w - 2.0 * POPUP_GAP).max(1.0));
            let popup_size = size(px(width), popup_size.height);
            Bounds::new(
                anchor_origin(a, width, f32::from(popup_size.height), &work),
                popup_size,
            )
        }
        None => Bounds::centered(None, popup_size, cx),
    };
    let anchored = anchor.is_some();

    // Anchored popups behave like a menu: no title bar, no shadow of
    // a normal window, cannot be dragged or resized. When no anchor
    // was passed, fall back to a plain titled window so the process
    // is still usable if the indicator failed to compute a rect.
    //
    // macOS needs the anchored case to pass `titlebar: None`. The
    // transparent-titlebar path keeps `NSClosableWindowMask` while adding
    // `NSFullSizeContentViewWindowMask`, so AppKit still renders the
    // traffic-light buttons on top of content that now spans the whole
    // window — they collided with the chart's top-left. Dropping the
    // titlebar options removes the closable bit from the style mask, so no
    // window buttons are created. The library's only other lever,
    // `traffic_light_position`, merely relocates the buttons rather than
    // removing them. Windows ignores this field for its borderless popup,
    // so that platform keeps the previous options unchanged.
    #[cfg(target_os = "macos")]
    let titlebar = if anchored {
        None
    } else {
        Some(TitlebarOptions {
            title: Some(window_title.clone().into()),
            ..Default::default()
        })
    };
    #[cfg(not(target_os = "macos"))]
    let titlebar = Some(TitlebarOptions {
        title: Some(window_title.clone().into()),
        appears_transparent: anchored,
        ..Default::default()
    });

    let window_options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(window_bounds)),
        // Select the indicator's display explicitly. Without this GPUI
        // converts the logical bounds with the primary display's scale
        // factor, which places a popup on the wrong screen (or falls back to
        // the primary display) whenever the two monitors use different DPIs.
        display_id: monitor,
        titlebar,
        kind: if anchored {
            WindowKind::PopUp
        } else {
            WindowKind::Normal
        },
        is_movable: !anchored,
        is_resizable: !anchored,
        // System backdrop material (Win11 Mica / Win10-era acrylic blur /
        // macOS window blur, opaque fallback on anything older or when the
        // user disabled transparency effects). The palette tint applied in
        // `apply_theme_and_accent` is what lets it show through. This is
        // purely a compositor attribute — it does not change the window's
        // activation edges, so the blur-close contract is untouched.
        window_background: backdrop_for(detect_material()),
        show: !hidden_start,
        focus: !hidden_start,
        ..Default::default()
    };

    let first_load = Arc::new(AtomicBool::new(false));
    // Blur-close and appearance observation are registered from the first
    // render pass (both idempotent); Esc is handled by an app-level keystroke
    // observer set up in `HistoryView::new`. The build closure only needs to
    // create the view.
    let opened = gpui_kit::open_window(window_options, cx, {
        let first_load = first_load.clone();
        let shared = shared.clone();
        move |_window, cx| {
            cx.new(|cx| HistoryView::new(lang, csv_scope, hidden_start, first_load, shared, cx))
        }
    });
    let (window_handle, _view) = match opened {
        Ok(opened) => opened,
        Err(e) => {
            log_event("WARN", &format!("history: open window failed: {e}"));
            return None;
        }
    };
    WINDOW_CREATED_MS.store(elapsed_ms() as u64, Ordering::Relaxed);
    Some((window_handle, first_load))
}

/// Resident loop used on Windows: keep the process (and GPUI/DirectX) warm
/// with no window, open a window only while the indicator asks for one, and
/// trim the working set while idle. The window is created hidden and shown
/// after its first chart load so the first visible frame already has data.
#[cfg(windows)]
fn run_resident(cx: &mut App) {
    let Some(state) = open_history_state() else {
        log_event(
            "WARN",
            "history: resident state mapping unavailable; exiting",
        );
        cx.quit();
        return;
    };
    let parent_pid = parse_parent_pid();
    let parent =
        parent_pid.and_then(|pid| unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid).ok() });
    if parent.is_none() {
        log_event(
            "WARN",
            "history: resident parent process unavailable; exiting",
        );
        cx.quit();
        return;
    }
    let parent = parent.expect("checked above");
    let shared = Arc::new(HistoryShared { state });
    log_event(
        "INFO",
        &format!(
            "history: resident started (version={}, parent pid {parent_pid:?})",
            env!("CARGO_PKG_VERSION")
        ),
    );

    cx.spawn(async move |cx| {
        let mut window: Option<AnyWindowHandle> = None;
        let mut first_load: Option<Arc<AtomicBool>> = None;
        let mut activated = false;
        let mut opened_at: Option<Instant> = None;
        let mut last_activation_attempt: Option<Instant> = None;
        let mut idle_since: Option<Instant> = Some(Instant::now());
        let mut trimmed = false;
        // Request generation that opened the current window. A blur/Esc
        // dismissal records it as dismissed so the same request is not
        // reopened while a newer click still gets through.
        let mut open_generation = 0u32;
        let mut dismissed_generation: Option<u32> = None;

        loop {
            let poll_ms = if window.is_some() {
                ACTIVE_POLL_MS
            } else {
                IDLE_POLL_MS
            };
            cx.background_executor()
                .timer(Duration::from_millis(poll_ms))
                .await;

            let snapshot = cx.update(|_app| unsafe { shared.snapshot() });
            let parent_dead = unsafe { WaitForSingleObject(parent, 0) != WAIT_TIMEOUT };
            if parent_dead {
                log_event("INFO", "history: parent process gone; exiting");
                cx.update(|app| app.quit());
                break;
            }

            let windows = cx.update(|app| app.windows());
            if let Some(handle) = window {
                // Blur/Esc and external closes remove the window behind the
                // loop's back; the request flag normally arrives first.
                if !windows.contains(&handle) {
                    log_event("INFO", "history: window dismissed");
                    dismissed_generation = Some(open_generation);
                    window = None;
                    first_load = None;
                    activated = false;
                    opened_at = None;
                    last_activation_attempt = None;
                    idle_since = Some(Instant::now());
                    trimmed = false;
                    unsafe { shared.mark_hidden() };
                    trim_working_set();
                    continue;
                }
                if snapshot.desired_open == 0 {
                    // `visible == 0` means the view already dismissed itself
                    // (blur/Esc) and its removal is in flight; calling
                    // `remove_window` again could double-remove it. A still
                    // visible window was asked to close by the indicator.
                    dismissed_generation = Some(snapshot.generation);
                    if snapshot.visible != 0 {
                        log_event("INFO", "history: window closed by indicator");
                        let _ = cx.update(|app| {
                            let _ = handle.update(app, |_, window, _| window.remove_window());
                        });
                    }
                    unsafe { shared.mark_hidden() };
                    window = None;
                    first_load = None;
                    activated = false;
                    opened_at = None;
                    last_activation_attempt = None;
                    idle_since = Some(Instant::now());
                    trimmed = false;
                    trim_working_set();
                    continue;
                }
                // A new open request arrived while the window is open: the
                // indicator was clicked on a different taskbar/monitor. GPUI
                // windows cannot be repositioned after creation, so reopen the
                // popup anchored at the new indicator.
                if snapshot.generation != open_generation && snapshot.anchor_is_valid() {
                    log_event(
                        "INFO",
                        "history: indicator moved; reopening at the new anchor",
                    );
                    let _ = cx.update(|app| {
                        let _ = handle.update(app, |_, window, _| window.remove_window());
                    });
                    let anchor = AnchorRect {
                        left: snapshot.anchor[0],
                        top: snapshot.anchor[1],
                        right: snapshot.anchor[2],
                        bottom: snapshot.anchor[3],
                    };
                    let monitor =
                        (snapshot.monitor != 0).then(|| DisplayId::new(snapshot.monitor as u64));
                    let reopened = cx.update(|app| {
                        apply_theme_and_accent(app);
                        open_history_window(app, Some(anchor), monitor, true, Some(shared.clone()))
                    });
                    match reopened {
                        Some((new_handle, ready)) => {
                            window = Some(new_handle);
                            first_load = Some(ready);
                            activated = false;
                            opened_at = Some(Instant::now());
                            last_activation_attempt = None;
                            open_generation = snapshot.generation;
                            dismissed_generation = None;
                            idle_since = None;
                            trimmed = false;
                            unsafe { shared.set_visible(true) };
                        }
                        None => {
                            dismissed_generation = Some(snapshot.generation);
                            unsafe { shared.mark_hidden() };
                            window = None;
                            first_load = None;
                            log_event("WARN", "history: reopen at the new anchor failed");
                        }
                    }
                    continue;
                }
                if !activated {
                    let ready = first_load
                        .as_ref()
                        .map_or(true, |flag| flag.load(Ordering::Acquire));
                    let timed_out = opened_at.map_or(true, |opened| {
                        opened.elapsed() > Duration::from_millis(1500)
                    });
                    let retry_due = last_activation_attempt
                        .is_none_or(|last| last.elapsed() > Duration::from_millis(100));
                    if (ready || timed_out) && retry_due {
                        let active = cx.update(|app| {
                            handle
                                .update(app, |_view, window, _| {
                                    window.activate_window();
                                    window.is_window_active()
                                })
                                .unwrap_or(false)
                        });
                        last_activation_attempt = Some(Instant::now());
                        if active {
                            activated = true;
                            unsafe { shared.set_visible(true) };
                            log_event(
                                "INFO",
                                &format!("history: popup shown in {}ms", elapsed_ms()),
                            );
                        } else if opened_at
                            .is_some_and(|opened| opened.elapsed() > Duration::from_secs(3))
                        {
                            // The foreground lock kept refusing activation.
                            // Show anyway so the chart is usable; Esc or an
                            // indicator click still closes it, but click-away
                            // cannot be armed. Log once for diagnosis.
                            activated = true;
                            unsafe { shared.set_visible(true) };
                            log_event(
                                "WARN",
                                &format!(
                                    "history: popup shown without activation after {}ms",
                                    elapsed_ms()
                                ),
                            );
                        }
                    }
                }
                continue;
            }

            if snapshot.desired_open != 0
                && windows.is_empty()
                && Some(snapshot.generation) != dismissed_generation
            {
                let anchor = snapshot.anchor_is_valid().then(|| AnchorRect {
                    left: snapshot.anchor[0],
                    top: snapshot.anchor[1],
                    right: snapshot.anchor[2],
                    bottom: snapshot.anchor[3],
                });
                let monitor =
                    (snapshot.monitor != 0).then(|| DisplayId::new(snapshot.monitor as u64));
                let opened = cx.update(|app| {
                    // The OS accent may have changed while the popup was
                    // closed; re-apply before the first paint.
                    apply_theme_and_accent(app);
                    open_history_window(app, anchor, monitor, true, Some(shared.clone()))
                });
                match opened {
                    Some((handle, ready)) => {
                        window = Some(handle);
                        first_load = Some(ready);
                        activated = false;
                        opened_at = Some(Instant::now());
                        last_activation_attempt = None;
                        open_generation = snapshot.generation;
                        dismissed_generation = None;
                        idle_since = None;
                        trimmed = false;
                        unsafe { shared.set_visible(true) };
                    }
                    None => {
                        // Do not retry this request every poll: the next click
                        // advances the generation and re-arms the open path.
                        dismissed_generation = Some(snapshot.generation);
                        unsafe { shared.mark_hidden() };
                        log_event("WARN", "history: resident window open failed");
                    }
                }
            } else {
                let now = Instant::now();
                if idle_since.is_none() {
                    idle_since = Some(now);
                    trimmed = false;
                }
                let idle_for = idle_since.map_or(Duration::ZERO, |since| since.elapsed());
                if !trimmed && idle_for > Duration::from_secs(2) {
                    trim_working_set();
                    trimmed = true;
                } else if idle_for > Duration::from_secs(30) {
                    trim_working_set();
                    idle_since = Some(now);
                }
            }
        }

        let _ = unsafe { CloseHandle(parent) };
    })
    .detach();
}

fn main() {
    sysvitals_panel::install_panic_hook();
    let _ = START.set(Instant::now());

    // GPUI does not declare DPI awareness itself. Without this the process
    // is treated as DPI-unaware and Windows bitmap-stretches the whole window
    // on a scaled display, which is what makes the text look soft.
    #[cfg(windows)]
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // Windows resident mode: the overlay starts this process once and keeps
    // it alive with no window. macOS keeps the one-shot launch used by its
    // menu-bar overlay.
    #[cfg(windows)]
    let resident = env::args().any(|arg| arg == "--resident");

    // Register the bundled icon asset source before the run loop starts.
    // Without this the GPUI default asset source (`()`) returns `Ok(None)`
    // for every path, so every `IconName` renders as blank — this is exactly
    // what made the Copy / CircleCheck glyphs disappear from the copy button.
    //
    // macOS: this process is a transient panel of a menu-bar utility, so it
    // must not own foreground application UI (a Dock icon and a menu bar named
    // after the process). Requesting the headless windowing mode maps to the
    // AppKit accessory activation policy before the run loop starts, which
    // still opens windows normally. The default quit mode on macOS is explicit
    // (the process survives its last window), which would leave an invisible
    // accessory process running after the user closes this window, so the
    // process is asked to quit with its last window instead.
    //
    // Windows has no equivalent: a headless windowing request there refuses to
    // open windows, so the default windowed startup is left untouched there.
    #[cfg(target_os = "macos")]
    let app = gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .with_windowing(WindowingRequest::Headless)
        .with_quit_mode(QuitMode::LastWindowClosed);
    #[cfg(windows)]
    let app = if resident {
        // The resident process outlives every window; the indicator owns its
        // lifetime through the parent handle and explicit termination.
        gpui_kit::application()
            .with_assets(gpui_kit::assets::Assets)
            .with_quit_mode(QuitMode::Explicit)
    } else {
        gpui_kit::application().with_assets(gpui_kit::assets::Assets)
    };
    #[cfg(not(any(windows, target_os = "macos")))]
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);

    app.run(move |cx| {
        gpui_kit::init(cx);
        // gpui-component's `theme::init` forces the light palette; sync
        // to the OS appearance before the window opens so the first paint
        // already has the right colors (the popup always follows the
        // system, independent of `display.theme`, which is indicator-only
        // now). The Windows accent override comes after, because a palette
        // reload wipes it.
        apply_theme_and_accent(cx);

        #[cfg(windows)]
        if resident {
            run_resident(cx);
            return;
        }

        #[cfg(target_os = "macos")]
        watch_parent(cx);

        let anchor = parse_anchor();
        let monitor = parse_anchor_monitor();
        log_event(
            "INFO",
            &format!(
                "history: popup starting (version={}, anchor={:?}, monitor={:?})",
                env!("CARGO_PKG_VERSION"),
                anchor.as_ref().map(|a| (a.left, a.top, a.right, a.bottom)),
                monitor
            ),
        );
        if open_history_window(cx, anchor, monitor, false, None).is_none() {
            cx.quit();
        }
    });
    log_event("INFO", "history: exited");
}
