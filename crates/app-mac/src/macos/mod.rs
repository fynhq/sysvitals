//! macOS menu-bar layer implementation.
//!
//! Shared state and the application bootstrap live here; rendering,
//! the tick loop, child processes, the context menu and config handling
//! are split into submodules:
//! - `render`   — template image, status-item mounting, accessibility.
//! - `tick`     — timer scheduling, sampling and history recording.
//! - `children` — Settings/History processes and anchor arguments.
//! - `menu`     — the native context menu.
//! - `config`   — hot reload and login-item application.
//!
//! All items in this binary are crate-internal (`pub(crate)`).

mod children;
mod config;
mod menu;
mod render;
mod tick;

pub(crate) use children::*;
pub(crate) use config::*;
pub(crate) use menu::*;
pub(crate) use render::*;
pub(crate) use tick::*;

use std::cell::RefCell;
use std::process::Child;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// Shared process plumbing (daily-file logging, panic hook, single-instance
// guard, build id) lives in `sysvitals-support` so the indicator and the
// GPUI panels cannot drift.
use sysvitals_support::{
    acquire_instance_lock, log_event, log_sanitize_issues, AutoStartController, AutoStartOutcome,
    ConfigEvent, InstanceLock, SV_BUILD_ID,
};

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ProtocolObject};
use objc2::{define_class, msg_send, sel, ClassType, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAppearanceCustomization, NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate,
    NSAttributedStringNSStringDrawing, NSButtonCell, NSCellImagePosition, NSCellStyleMask,
    NSEventMask, NSEventModifierFlags, NSEventType, NSFont, NSFontAttributeName,
    NSFontWeightSemibold, NSImage, NSImageScaling, NSMenu, NSMenuItem, NSScreen, NSStatusBar,
    NSStatusBarButton, NSStatusItem, NSVariableStatusItemLength, NSView,
};
use objc2_foundation::{
    NSAttributedString, NSDictionary, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect,
    NSRunLoop, NSRunLoopCommonModes, NSSize, NSString, NSTimer,
};

use sysvitals_core::format::{
    column_metrics, format_frame, join_prefix, metric_prefix, visible_metrics, PERCENT_RESERVE,
    RATE_RESERVE,
};
use sysvitals_core::{t, Config, ConfigWatcher, FormattedRow, HistoryWriterSlot, Metric, Sampler};

// `SMAppService` lives in ServiceManagement.framework. Linking it keeps the
// class registered for the runtime lookup below; no typed bindings are
// needed for the three calls we make.
#[link(name = "ServiceManagement", kind = "framework")]
extern "C" {}

/// Font-size factor for the dual-row layout: the menu bar has a fixed
/// height, so two lines only fit with the system menu-bar font scaled
/// down. 0.8 is the default tuning; the fit loop in `render_indicator`
/// reduces it further only if the two line boxes still exceed the bar.
const DUAL_ROW_FONT_FACTOR: f64 = 0.8;

/// Tightest inter-line step used by the height fit, as a fraction of the
/// font size. The two rows are short labels; at 0.95x the previous row's
/// descenders still clear the next row's capitals. This lets the 0.75x
/// fallback use the ~9.5pt available in a 22pt bar instead of leaving the
/// block two points taller than the canvas.
const DUAL_ROW_MIN_STEP_FACTOR: f64 = 0.95;

// Worst-case rate cell: `format_rate` keeps the numeric field at most four
// characters per unit tier and "M" is the widest tier letter. The reserve
// string lives in `core::format` so it cannot drift from the formatter.

// ------------------------------------------------------------- diagnostics

// Logging (`log_event`), the build stamp and the Rust panic hook come from
// `sysvitals-support`, shared with the Windows overlay and the GPUI panels.
// The macOS side additionally logs uncaught Objective-C exceptions below; a
// mach-exception port handler was judged out of scope — panics and normal
// exits are covered.

/// Log uncaught Objective-C exceptions before AppKit turns them into a
/// SIGABRT. Rust's panic hook cannot see these, so a crash inside an
/// AppKit call would otherwise leave no trace in the daily log.
extern "C" fn log_uncaught_exception(exception: *mut AnyObject) {
    let mut detail = String::from("unknown exception");
    if !exception.is_null() {
        // SAFETY: `exception` is the live NSException passed by the
        // runtime; both selectors return valid objects or null.
        unsafe {
            let desc: *mut AnyObject = msg_send![exception, description];
            if !desc.is_null() {
                let utf8: *const std::ffi::c_char = msg_send![desc, UTF8String];
                if !utf8.is_null() {
                    detail = std::ffi::CStr::from_ptr(utf8)
                        .to_string_lossy()
                        .into_owned();
                }
            }
        }
    }
    log_event(
        "FATAL",
        &format!("uncaught Objective-C exception: {detail}"),
    );
}

extern "C" {
    fn NSSetUncaughtExceptionHandler(handler: Option<extern "C" fn(*mut AnyObject)>);
}

/// Install the Objective-C crash logger (complements the Rust panic hook).
fn install_exception_logging() {
    // SAFETY: registering a process-wide handler that only reads the
    // exception and writes a log line; the exception keeps propagating
    // afterwards, so termination behaviour is unchanged.
    unsafe { NSSetUncaughtExceptionHandler(Some(log_uncaught_exception)) };
}

thread_local! {
    static SAMPLER: RefCell<Option<Sampler>> = const { RefCell::new(None) };
    static CONFIG: RefCell<Config> = RefCell::new(Config::default());
    static STATUS_ITEM: RefCell<Option<Retained<NSStatusItem>>> = const { RefCell::new(None) };
    /// Config-file mtime cache and parse-error throttle, main-thread only.
    static CONFIG_WATCHER: RefCell<ConfigWatcher> = RefCell::new(ConfigWatcher::new());
    static MTM: RefCell<Option<MainThreadMarker>> = const { RefCell::new(None) };
    // History writes run on a dedicated worker thread. SQLite open /
    // insert / prune on the main RunLoop (up to the 250 ms busy timeout)
    // could freeze the menu bar and its menus on a slow disk; the slot owns
    // the lazy-spawn retry policy.
    static HISTORY_WRITER: RefCell<HistoryWriterSlot> =
        const { RefCell::new(HistoryWriterSlot::new()) };
    // The repeating sample timer, kept so the interval can be re-applied
    // when the config file changes it (parity with the Windows overlay,
    // which re-seeds its sample timer on config hot-reload).
    static SAMPLE_TIMER: RefCell<Option<Retained<NSTimer>>> = const { RefCell::new(None) };
    /// Interval currently in effect on the timer; the first tick primes
    /// it (0) and schedules the real timer from there.
    static APPLIED_INTERVAL_MS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Login-startup policy shared with the Windows overlay; the platform
    /// side effect is the `SMAppService` call.
    static AUTO_START: RefCell<AutoStartController> = RefCell::new(AutoStartController::new());
    // Child process handles, kept so a still-running instance is not
    // duplicated (a second spawn is a no-op until the first exits).
    static SETTINGS_CHILD: RefCell<Option<Child>> = const { RefCell::new(None) };
    static HISTORY_CHILD: RefCell<Option<Child>> = const { RefCell::new(None) };
    // The button/menu action target. Kept addressable so context-menu items
    // can name it explicitly: the handler lives outside the responder chain
    // (nothing inserts it there), so a nil target would leave AppKit's
    // automatic enabling unable to find a receiver and gray the items out.
    static ACTION_HANDLER: RefCell<Option<Retained<ActionHandler>>> = const { RefCell::new(None) };

    // --- Self-drawn indicator image -----------------------------------
    // The status item shows a template `NSImage` we render ourselves
    // instead of a plain `setTitle`. That is the only way to control the
    // two-column dual-row layout to the pixel: item spacing, left-column
    // alignment and vertical centering are all impossible through AppKit's
    // string-titled status button, whose text placement we cannot reach.
    // Making the image a *template* keeps menu-bar light/dark inversion for
    // free (AppKit tints it from the system appearance), consistent with the
    // "indicator always follows the system theme" conclusion that also let
    // us hide the theme picker on macOS.

    /// Render signature of the image currently mounted (layout parameters +
    /// all texts). `tick` compares it first and skips everything when the
    /// frame is identical — no rebuild, no redraw, no bitmap churn.
    static DRAW_SIGNATURE: RefCell<String> = const { RefCell::new(String::new()) };
    /// Cheap input signature (texts + layout + font size) checked before the
    /// expensive attributed-string measurement. Avoids rebuilding fonts/runs
    /// when the sampled values did not change between ticks.
    static RENDER_INPUT_SIGNATURE: RefCell<String> = const { RefCell::new(String::new()) };
    /// Last effective dual-row size after the height fit loop, so a
    /// requested-vs-effective font shrink is logged once rather than on
    /// every sample.
    static DUAL_SHRUNK_FONT: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
    /// One-shot diagnostic for a dual-row block that still exceeds the
    /// menu-bar thickness after the automatic font shrink.
    static DUAL_OVERFLOW_LOGGED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Currently rendered template image and its reserved width.
    static STATUS_IMAGE: RefCell<Option<StatusImage>> = const { RefCell::new(None) };
    /// Accessibility value for the current frame, kept so it can be applied
    /// from a run-loop tick even when the image itself did not change.
    static ACCESSIBILITY_TEXT: RefCell<String> = const { RefCell::new(String::new()) };
    /// Last value actually pushed to AppKit, so an unchanged frame does not
    /// re-send the same accessibility/tooltip update every tick.
    static ACCESSIBILITY_APPLIED: RefCell<String> = const { RefCell::new(String::new()) };
    /// True while `tick` runs from the repeating timer, i.e. inside
    /// `NSApplication::run`. The startup tick runs before the run loop; the
    /// accessibility and tooltip calls are deferred until this is true.
    static IN_RUN_LOOP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// One-shot startup milestone: the first frame has been mounted. Kept
    /// in the daily log so a launch that dies later is still traceable.
    static FIRST_FRAME_LOGGED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A Cocoa screen rectangle in points (bottom-left origin). Kept as plain
/// numbers so the coordinate conversion runs in unit tests without AppKit.
#[derive(Clone, Copy)]
struct CocoaRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// Convert a Cocoa screen rectangle into the hosting screen's local
/// top-left space. Pure math; the AppKit calls stay in `anchor_rect`.
fn local_anchor_rect(rect: CocoaRect, screen: CocoaRect) -> (i64, i64, i64, i64) {
    let left = rect.x - screen.x;
    let top = screen.height - (rect.y - screen.y) - rect.height;
    (
        left.round() as i64,
        top.round() as i64,
        (left + rect.width).round() as i64,
        (top + rect.height).round() as i64,
    )
}

/// Screen rectangle (left, top, right, bottom) in the hosting display's
/// **local** GPUI logical space, plus that display's native ID. GPUI's macOS
/// backend reports `Display::bounds()` relative to each screen's own origin
/// (0,0) and adds the screen frame back when creating a window, so passing a
/// global Cocoa rectangle would land the panels on the primary display (or
/// clamp them to its edge). The button's screen supplies both the origin and
/// the height used for the bottom-left to top-left flip.
fn anchor_rect() -> Option<(i64, i64, i64, i64, u64)> {
    let mtm = mtm();
    STATUS_ITEM.with(|si| {
        let item = si.borrow().clone()?;
        let button = item.button(mtm)?;
        let window = button.window()?;
        let rect = window.convertRectToScreen(button.convertRect_toView(button.bounds(), None));
        let screen = window.screen().or_else(|| NSScreen::mainScreen(mtm))?;
        let screen_frame = screen.frame();
        let (left, top, right, bottom) = local_anchor_rect(
            CocoaRect {
                x: rect.origin.x,
                y: rect.origin.y,
                width: rect.size.width,
                height: rect.size.height,
            },
            CocoaRect {
                x: screen_frame.origin.x,
                y: screen_frame.origin.y,
                width: screen_frame.size.width,
                height: screen_frame.size.height,
            },
        );
        let display_id = unsafe {
            screen
                .deviceDescription()
                .objectForKey_unchecked(objc2_foundation::ns_string!("NSScreenNumber"))
                .map_or(0u32, |number| {
                    let raw: u32 = msg_send![number, unsignedIntValue];
                    raw
                })
        };
        Some((left, top, right, bottom, u64::from(display_id)))
    })
}

/// Get the MainThreadMarker (must be called from main thread).
fn mtm() -> MainThreadMarker {
    MTM.with(|m| m.borrow().expect("MainThreadMarker not set"))
}

// Action handler for button clicks and menu items.
define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SysVitalsActionHandler"]
    struct ActionHandler;

    impl ActionHandler {
        /// Called when the status item button is clicked. A right click
        /// opens the context menu; any other click opens the trend chart
        /// immediately. There is no double-click handling — Activity
        /// Monitor lives in the context menu, so a left click never waits
        /// for a possible second press.
        #[unsafe(method(buttonClicked:))]
        fn button_clicked(&self, _sender: Option<&AnyObject>) {
            let mtm = mtm();
            let app = NSApplication::sharedApplication(mtm);
            let event = app.currentEvent();
            let right_or_control = event.as_ref().is_some_and(|e| {
                matches!(e.r#type(), NSEventType::RightMouseUp | NSEventType::RightMouseDown)
                    || e.modifierFlags().contains(NSEventModifierFlags::Control)
            });
            if right_or_control {
                show_context_menu();
            } else {
                // Any plain left click toggles the trend chart: a live
                // popup is closed, otherwise a new one opens (matching the
                // Windows overlay). A process that is still starting is
                // ignored by the toggle so a double-click cannot kill the
                // first launch before its window exists.
                toggle_history();
            }
        }

        #[unsafe(method(showSettings:))]
        fn show_settings(&self, _sender: Option<&AnyObject>) {
            launch_settings();
        }

        /// Menu-item action: opens Activity Monitor. Used to be the
        /// double-click target on the status item; moved here so the
        /// left click can open the trend chart instantly without a
        /// deferral window.
        #[unsafe(method(showActivityMonitor:))]
        fn show_activity_monitor(&self, _sender: Option<&AnyObject>) {
            open_activity_monitor();
        }

        #[unsafe(method(restartApp:))]
        fn restart_app(&self, _sender: Option<&AnyObject>) {
            restart_self();
        }

        /// Menu-item action: close child panels, then terminate. Keeping
        /// the default `terminate:` responder would quit without reaping
        /// Settings/History.
        #[unsafe(method(quitApp:))]
        fn quit_app(&self, _sender: Option<&AnyObject>) {
            close_children();
            let mtm = mtm();
            NSApplication::sharedApplication(mtm).terminate(None);
        }
    }

    unsafe impl NSObjectProtocol for ActionHandler {}

    // SAFETY: `NSApplicationDelegate` adds no safety requirements of its
    // own; the only requirement is main-thread confinement, which the
    // `MainThreadOnly` thread kind above provides.
    unsafe impl NSApplicationDelegate for ActionHandler {
        /// macOS delivers a quit Apple Event for log out, restart and
        /// shutdown; `terminate:` runs this hook before the process
        /// exits. Closing the child panels here makes the system-driven
        /// path behave like the menu's Quit item. `close_children` is
        /// idempotent, so the menu path landing here twice is harmless.
        #[unsafe(method(applicationWillTerminate:))]
        fn application_will_terminate(&self, _notification: &NSNotification) {
            close_children();
            log_event("INFO", "application terminating; child panels closed");
        }
    }
);

impl ActionHandler {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm);
        // ActionHandler does not override init, so calling it directly
        // dispatches to NSObject's init. Avoids the super() wrapper which
        // triggers RetainSemantics issues with Allocated<T> in objc2 0.6.5.
        unsafe { msg_send![this, init] }
    }
}

pub(crate) fn run() {
    // Crash diagnostics and the single-instance guard run before anything
    // else, mirroring the Windows overlay's startup sequence. A second
    // launch exits quietly; keeping the guard alive holds the flock.
    sysvitals_support::install_panic_hook();
    let _instance = match acquire_instance_lock("sysvitals.lock") {
        InstanceLock::Acquired(guard) => Some(guard),
        InstanceLock::Busy => {
            log_event("INFO", "another instance is already running; exiting");
            return;
        }
        InstanceLock::Unavailable(error) => {
            // A broken data dir or an OS refusal must never block the
            // indicator from starting.
            log_event(
                "WARN",
                &format!("instance guard unavailable: {error}; continuing"),
            );
            None
        }
    };
    install_exception_logging();

    // Everything from the AppKit setup through the first tick runs
    // before `app.run()` installs an autorelease pool per event-loop
    // pass. Wrap it in an explicit pool so autoreleased objects (the
    // status-bar button lookups, system font lookups, login-item
    // plumbing) drain at the end of startup instead of leaking for
    // the process lifetime.
    let app = objc2::rc::autoreleasepool(|_pool| {
        // Get MainThreadMarker — must be on main thread.
        let mtm = MainThreadMarker::new().expect("must run on main thread");
        MTM.with(|m| *m.borrow_mut() = Some(mtm));

        // NSApplication setup — must happen on the main thread.
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

        // Create the status item with variable width.
        let status_bar = NSStatusBar::systemStatusBar();
        let status_item = status_bar.statusItemWithLength(NSVariableStatusItemLength);

        // Set initial title so something shows immediately.
        if let Some(button) = status_item.button(mtm) {
            let title = NSString::from_str("SysVitals");
            button.setTitle(&title);
        }

        // Action handler (kept alive for the process lifetime).
        let handler = ActionHandler::new(mtm);

        // Wire button action (single click opens the chart, right click opens
        // the context menu).
        if let Some(button) = status_item.button(mtm) {
            // SAFETY: setting a valid target and selector on the button.
            unsafe {
                button.setTarget(Some(&handler));
                button.setAction(Some(sel!(buttonClicked:)));
            }
            // Status-item buttons default to firing on mouse-down only, so a
            // right-click never reaches the action and the context menu could
            // not be raised from `buttonClicked:`. Subscribe explicitly to both
            // mouse-up events (Apple's documented pattern for status items).
            let mask = NSEventMask::LeftMouseUp | NSEventMask::RightMouseUp;
            let _ = button.sendActionOn(mask);
            // No pressed/selected backdrop: clear the cell's highlight masks so
            // AppKit does not paint its own highlight behind the glyphs.
            disable_button_highlight(&button);
        }

        // The application delegate is a weak reference; the handler stored in
        // `ACTION_HANDLER` below is what keeps it alive for the process
        // lifetime. The delegate hook covers the quit Apple Event macOS sends
        // at log out / shutdown, which otherwise bypasses the menu's cleanup.
        app.setDelegate(Some(ProtocolObject::from_ref(&*handler)));

        // Keep the handler alive for the process lifetime, addressable so the
        // context menu can name it as the action target (see ACTION_HANDLER).
        // The stored clone is sufficient; leaking the original as well would
        // just retain one extra reference forever.
        ACTION_HANDLER.with(|h| *h.borrow_mut() = Some(handler));

        // Store the status item for the tick callback.
        STATUS_ITEM.with(|si| *si.borrow_mut() = Some(status_item));

        // Load initial config and seed the mtime cache only on success. A
        // failed initial read leaves the watcher unseeded so the first tick
        // retries (and recovers if the failure was transient); the reload
        // path throttles its own log to one line per broken mtime.
        let mut initial_config_ok = false;
        match CONFIG_WATCHER.with(|w| w.borrow_mut().load_initial()) {
            Ok((cfg, issues, source)) => {
                log_sanitize_issues("config repair", &issues);
                CONFIG.with(|c| *c.borrow_mut() = cfg);
                initial_config_ok = source == sysvitals_core::config::ConfigSource::File;
            }
            Err(e) => {
                log_event("WARN", &format!("config load failed: {e}; using defaults"));
            }
        }

        // Daily-file logging: the first line below also opens the file and
        // prunes stale logs (see `core::logger`), so there is no separate
        // startup prune here. Then stamp the start and the effective config
        // (same shape as the Windows overlay's startup log, so a user can
        // just send this file when something looks wrong).
        let cfg = CONFIG.with(|c| c.borrow().clone());
        log_event(
            "INFO",
            &format!(
                "SysVitals (mac) starting (version={}, build={})",
                env!("CARGO_PKG_VERSION"),
                SV_BUILD_ID
            ),
        );
        log_event("INFO", &format!("config loaded: {}", cfg.summary()));
        // A malformed config must not silently remove a login item the user
        // enabled earlier; only apply after a successful initial read.
        if initial_config_ok {
            apply_auto_start(&cfg);
            log_event("INFO", "startup: login-item state applied");
        }

        // Initial tick: primes the repeating sample timer and lazily creates
        // the background history writer on the first enabled sample.
        tick();
        app
    });

    // Enter the application event loop (blocks forever). Quit goes through
    // `terminate:`, which exits the process; returning here means the run
    // loop ended without a quit, which is otherwise invisible.
    app.run();
    log_event("WARN", "run loop returned unexpectedly");
}

#[cfg(test)]
mod tests {
    use super::{local_anchor_rect, CocoaRect};

    fn rect(x: f64, y: f64, width: f64, height: f64) -> CocoaRect {
        CocoaRect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn anchor_is_local_to_the_hosting_screen() {
        let screen = rect(0.0, 0.0, 2880.0, 1800.0);
        let button = rect(2760.0, 1750.0, 120.0, 24.0);

        assert_eq!(local_anchor_rect(button, screen), (2760, 26, 2880, 50));
    }

    #[test]
    fn anchor_subtracts_a_secondary_screen_origin() {
        let screen = rect(1920.0, 1080.0, 1920.0, 1080.0);
        let button = rect(3700.0, 2120.0, 140.0, 25.0);

        assert_eq!(local_anchor_rect(button, screen), (1780, 15, 1920, 40));
    }

    #[test]
    fn anchor_handles_a_screen_left_of_the_primary() {
        let screen = rect(-1920.0, 0.0, 1920.0, 1080.0);
        let button = rect(-1860.0, 1040.0, 140.0, 25.0);

        assert_eq!(local_anchor_rect(button, screen), (60, 15, 200, 40));
    }

    #[test]
    fn anchor_rounds_fractional_points() {
        let screen = rect(0.0, 0.0, 100.0, 50.0);
        let button = rect(10.4, 20.4, 20.2, 10.2);

        assert_eq!(local_anchor_rect(button, screen), (10, 19, 31, 30));
    }
}
