//! macOS menu-bar layer implementation.

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
use std::os::fd::IntoRawFd;
use std::process::Child;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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

#[link(name = "ServiceManagement", kind = "framework")]
extern "C" {}

const DUAL_ROW_FONT_FACTOR: f64 = 0.8;

const DUAL_ROW_MIN_STEP_FACTOR: f64 = 0.95;

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

fn install_exception_logging() {
    // SAFETY: registering a process-wide handler that only reads the
    // exception and writes a log line; the exception keeps propagating
    // afterwards, so termination behaviour is unchanged.
    unsafe { NSSetUncaughtExceptionHandler(Some(log_uncaught_exception)) };
}

// Opened once at startup, so a crash after a date roll lands in the startup day's file.
static CRASH_LOG_FD: AtomicI32 = AtomicI32::new(-1);

const CRASH_SIGNALS: [std::ffi::c_int; 3] = [libc::SIGSEGV, libc::SIGBUS, libc::SIGABRT];

const CRASH_LINE_DIGITS: &[u8; 16] = b"0123456789abcdef";

fn signal_name(sig: std::ffi::c_int) -> &'static str {
    match sig {
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGBUS => "SIGBUS",
        libc::SIGABRT => "SIGABRT",
        _ => "unknown",
    }
}

struct CrashLine<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl CrashLine<'_> {
    fn text(&mut self, bytes: &[u8]) {
        for byte in bytes {
            if self.len < self.buf.len() {
                self.buf[self.len] = *byte;
            }
            self.len += 1;
        }
    }

    fn number(&mut self, mut value: u64, radix: u64) {
        let mut digits = [0u8; 20];
        let mut used = digits.len();
        loop {
            used -= 1;
            digits[used] = CRASH_LINE_DIGITS[(value % radix) as usize];
            value /= radix;
            if value == 0 {
                break;
            }
        }
        self.text(&digits[used..]);
    }
}

fn crash_signal_line(buf: &mut [u8], sig: std::ffi::c_int, addr: usize) -> usize {
    let mut line = CrashLine { buf, len: 0 };
    line.text(b"[FATAL] signal ");
    line.number(sig.max(0) as u64, 10);
    line.text(b" (");
    line.text(signal_name(sig).as_bytes());
    line.text(b") at 0x");
    line.number(addr as u64, 16);
    line.text(b" - process will terminate\n");
    line.len.min(line.buf.len())
}

fn open_crash_log_fd() -> std::ffi::c_int {
    let Ok(dir) = sysvitals_core::config::data_dir() else {
        return -1;
    };
    let (date, _) = sysvitals_support::local_now();
    let logs = sysvitals_core::logger::log_dir(&dir);
    let _ = std::fs::create_dir_all(&logs);
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs.join(format!("sysvitals-{date}.log")))
    else {
        return -1;
    };
    file.into_raw_fd()
}

extern "C" fn crash_signal_handler(
    sig: std::ffi::c_int,
    info: *mut libc::siginfo_t,
    _context: *mut libc::c_void,
) {
    // SAFETY: runs in signal context, so only the kernel-provided `siginfo`
    // is read; the line is formatted on the stack and written with
    // async-signal-safe `write`, never through the locking logger.
    let addr = if info.is_null() {
        0
    } else {
        unsafe { (*info).si_addr as usize }
    };
    let mut buf = [0u8; 160];
    let len = crash_signal_line(&mut buf, sig, addr);
    let fd = CRASH_LOG_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        // SAFETY: `fd` was opened by this process and kept open for its
        // lifetime; `buf` holds `len` initialized bytes.
        unsafe {
            let _ = libc::write(fd, buf.as_ptr().cast::<libc::c_void>(), len);
        }
    }
    // SAFETY: restoring SIG_DFL and re-raising makes the kernel deliver the
    // signal with its default action, so the OS crash report is unchanged.
    unsafe {
        let default = libc::sigaction {
            sa_sigaction: libc::SIG_DFL,
            sa_mask: 0,
            sa_flags: 0,
        };
        let _ = libc::sigaction(sig, &default, std::ptr::null_mut());
        let _ = libc::raise(sig);
    }
}

fn install_signal_logging() {
    CRASH_LOG_FD.store(open_crash_log_fd(), Ordering::Relaxed);
    for sig in CRASH_SIGNALS {
        let act = libc::sigaction {
            sa_sigaction: crash_signal_handler as *const () as libc::sighandler_t,
            sa_mask: 0,
            sa_flags: libc::SA_SIGINFO,
        };
        // SAFETY: `act` names a live `extern "C"` handler and an empty mask;
        // the handler re-raises under SIG_DFL, so termination behaviour and
        // the OS crash report are unchanged.
        unsafe {
            let _ = libc::sigaction(sig, &act, std::ptr::null_mut());
        }
    }
}

thread_local! {
    static SAMPLER: RefCell<Option<Sampler>> = const { RefCell::new(None) };
    static CONFIG: RefCell<Config> = RefCell::new(Config::default());
    static STATUS_ITEM: RefCell<Option<Retained<NSStatusItem>>> = const { RefCell::new(None) };
    static CONFIG_WATCHER: RefCell<ConfigWatcher> = RefCell::new(ConfigWatcher::new());
    static MTM: RefCell<Option<MainThreadMarker>> = const { RefCell::new(None) };
    static HISTORY_WRITER: RefCell<HistoryWriterSlot> =
        const { RefCell::new(HistoryWriterSlot::new()) };
    static SAMPLE_TIMER: RefCell<Option<Retained<NSTimer>>> = const { RefCell::new(None) };
    static APPLIED_INTERVAL_MS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static AUTO_START: RefCell<AutoStartController> = RefCell::new(AutoStartController::new());
    static SETTINGS_CHILD: RefCell<Option<Child>> = const { RefCell::new(None) };
    static HISTORY_CHILD: RefCell<Option<Child>> = const { RefCell::new(None) };
    static ACTION_HANDLER: RefCell<Option<Retained<ActionHandler>>> = const { RefCell::new(None) };


    static DRAW_SIGNATURE: RefCell<String> = const { RefCell::new(String::new()) };
    static RENDER_INPUT_SIGNATURE: RefCell<String> = const { RefCell::new(String::new()) };
    static DUAL_SHRUNK_FONT: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
    static DUAL_OVERFLOW_LOGGED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static STATUS_IMAGE: RefCell<Option<StatusImage>> = const { RefCell::new(None) };
    static ACCESSIBILITY_APPLIED: RefCell<String> = const { RefCell::new(String::new()) };
    static FIRST_FRAME_LOGGED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[derive(Clone, Copy)]
struct CocoaRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

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

// Convert the anchor to the hosting screen's local top-left space; a global rect misplaces secondary-display panels.
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

fn mtm() -> MainThreadMarker {
    MTM.with(|m| m.borrow().expect("MainThreadMarker not set"))
}

// The callback runs while the RefCell guard is alive; returning the button would drop it.
pub(crate) fn with_status_button(mtm: MainThreadMarker, f: impl FnOnce(&NSStatusBarButton)) {
    STATUS_ITEM.with(|si| {
        let si_borrow = si.borrow();
        let Some(item) = si_borrow.as_ref() else {
            return;
        };
        let Some(button) = item.button(mtm) else {
            return;
        };
        f(&button);
    });
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SysVitalsActionHandler"]
    struct ActionHandler;

    impl ActionHandler {
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
                toggle_history();
            }
        }

        #[unsafe(method(showSettings:))]
        fn show_settings(&self, _sender: Option<&AnyObject>) {
            launch_settings();
        }

        #[unsafe(method(showActivityMonitor:))]
        fn show_activity_monitor(&self, _sender: Option<&AnyObject>) {
            open_activity_monitor();
        }

        #[unsafe(method(restartApp:))]
        fn restart_app(&self, _sender: Option<&AnyObject>) {
            restart_self();
        }

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
        unsafe { msg_send![this, init] }
    }
}

pub(crate) fn run() {
    sysvitals_support::install_panic_hook();
    let _instance = match acquire_instance_lock("sysvitals.lock") {
        InstanceLock::Acquired(guard) => Some(guard),
        InstanceLock::Busy => {
            log_event("INFO", "another instance is already running; exiting");
            return;
        }
        InstanceLock::Unavailable(error) => {
            log_event(
                "WARN",
                &format!("instance guard unavailable: {error}; continuing"),
            );
            None
        }
    };
    install_exception_logging();
    install_signal_logging();

    let app = objc2::rc::autoreleasepool(|_pool| {
        let mtm = MainThreadMarker::new().expect("must run on main thread");
        MTM.with(|m| *m.borrow_mut() = Some(mtm));

        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

        let status_bar = NSStatusBar::systemStatusBar();
        let status_item = status_bar.statusItemWithLength(NSVariableStatusItemLength);

        if let Some(button) = status_item.button(mtm) {
            let title = NSString::from_str("SysVitals");
            button.setTitle(&title);
        }

        let handler = ActionHandler::new(mtm);

        if let Some(button) = status_item.button(mtm) {
            // SAFETY: setting a valid target and selector on the button.
            unsafe {
                button.setTarget(Some(&handler));
                button.setAction(Some(sel!(buttonClicked:)));
            }
            let mask = NSEventMask::LeftMouseUp | NSEventMask::RightMouseUp;
            let _ = button.sendActionOn(mask);
            disable_button_highlight(&button);
        }

        // Keep the action handler alive for the process lifetime; it is the context-menu action target.
        app.setDelegate(Some(ProtocolObject::from_ref(&*handler)));

        ACTION_HANDLER.with(|h| *h.borrow_mut() = Some(handler));

        STATUS_ITEM.with(|si| *si.borrow_mut() = Some(status_item));

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
        if initial_config_ok {
            apply_auto_start(&cfg);
            log_event("INFO", "startup: login-item state applied");
        }

        tick();
        app
    });

    app.run();
    log_event("WARN", "run loop returned unexpectedly");
}

#[cfg(test)]
mod tests {
    use super::{crash_signal_line, local_anchor_rect, signal_name, CocoaRect};

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

    #[test]
    fn crash_line_names_the_signal_and_the_fault_address() {
        let mut buf = [0u8; 160];
        let len = crash_signal_line(&mut buf, libc::SIGSEGV, 0x1234);

        assert_eq!(
            std::str::from_utf8(&buf[..len]).unwrap(),
            "[FATAL] signal 11 (SIGSEGV) at 0x1234 - process will terminate\n"
        );
    }

    #[test]
    fn crash_line_truncates_instead_of_overrunning_the_buffer() {
        let mut buf = [0u8; 12];
        let len = crash_signal_line(&mut buf, libc::SIGABRT, 0);

        assert_eq!(len, 12);
        assert_eq!(std::str::from_utf8(&buf[..len]).unwrap(), "[FATAL] sign");
    }

    #[test]
    fn signal_name_falls_back_for_unhandled_signals() {
        assert_eq!(signal_name(libc::SIGBUS), "SIGBUS");
        assert_eq!(signal_name(libc::SIGINT), "unknown");
    }
}
