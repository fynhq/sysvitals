//! macOS display layer: NSStatusItem menu-bar indicator.
//!
//! Shows CPU / memory / network stats in the macOS menu bar using a
//! self-drawn template `NSImage` on an `NSStatusItem`. Samples periodically via
//! sysvitals-core and formats with the shared format engine; AppKit tints the
//! template from the menu-bar appearance.
//!
//! Interaction model (matches Windows):
//! - Single-click: toggle the history trend chart popup
//! - Right-click: context menu (Activity Monitor / Settings / Restart / Quit)
//!
//! Architecture: pure objc2 + AppKit for the indicator itself. The GPUI
//! settings window (app-settings) and history chart (app-history) are
//! launched as separate processes, same as on Windows.

#[cfg(target_os = "macos")]
mod macos {
    use std::cell::RefCell;
    use std::os::fd::AsRawFd;
    use std::process::Child;
    use std::time::{SystemTime, UNIX_EPOCH};

    use chrono::Local;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Bool};
    use objc2::{define_class, msg_send, sel, ClassType, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSAppearanceCustomization, NSApplication, NSApplicationActivationPolicy,
        NSAttributedStringNSStringDrawing, NSBezierPath, NSCellImagePosition, NSColor, NSEventMask,
        NSEventType, NSFont, NSFontAttributeName, NSFontWeightSemibold, NSImage, NSImageScaling,
        NSMenu, NSMenuItem, NSScreen, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
        NSView,
    };
    use objc2_foundation::{
        NSAttributedString, NSDictionary, NSObject, NSObjectProtocol, NSPoint, NSRect, NSRunLoop,
        NSRunLoopCommonModes, NSSize, NSString, NSTimer,
    };

    use sysvitals_core::config::{config_path, history_path};
    use sysvitals_core::format::{format_frame, join_prefix};
    use sysvitals_core::{t, Config, FormattedRow, Metric, NetSelection, Sampler};

    /// Font-size factor for the dual-row layout: the menu bar has a fixed
    /// height, so two lines only fit with the system menu-bar font scaled
    /// down. 0.8 is the default tuning; the fit loop in `render_indicator`
    /// reduces it further only if the two line boxes still exceed the bar.
    const DUAL_ROW_FONT_FACTOR: f64 = 0.8;

    /// Alpha of the active-status rounded backdrop inside the template image.
    /// The template tint supplies the light/dark color; only the mask opacity
    /// is fixed here. Shown only while a panel or menu is open, with a 1px
    /// inset and no extra side padding.
    const STATUS_HIGHLIGHT_ALPHA: f64 = 0.16;

    /// Conservative upper bound for one rate cell. `format_rate` keeps the
    /// numeric field at most four characters per unit tier (including the
    /// 1024 rounding), and "M" is the widest tier letter, so measuring this
    /// string reserves every possible rate value for the current font.
    const RATE_RESERVE: &str = "8888 M/s";

    // ------------------------------------------------------------- diagnostics

    /// Local date (`YYYY-MM-DD`) and time (`HH:MM:SS`) for the log stamp, from
    /// chrono's local clock (the macOS counterpart of the Windows overlay's
    /// `GetLocalTime`-based `local_now`).
    fn local_now() -> (String, String) {
        let now = Local::now();
        (
            now.format("%Y-%m-%d").to_string(),
            now.format("%H:%M:%S").to_string(),
        )
    }

    /// Append one line to today's log under the data dir (mirrors the Windows
    /// overlay: `core::logger` is platform-neutral, only the clock differs).
    /// Logging must never break the indicator, so failures are ignored.
    fn log_event(level: &str, message: &str) {
        let Ok(dir) = sysvitals_core::config::data_dir() else {
            return;
        };
        let (date, time) = local_now();
        sysvitals_core::logger::log_line(&dir, &date, &time, level, message);
    }

    /// Remove log files older than the retention window. Run once at startup.
    fn prune_old_logs() {
        let Ok(dir) = sysvitals_core::config::data_dir() else {
            return;
        };
        let (date, _) = local_now();
        sysvitals_core::logger::prune_logs(
            &dir,
            &date,
            sysvitals_core::logger::DEFAULT_RETENTION_DAYS,
        );
    }

    /// Rust panic hook writing the panic to the log before the process dies.
    /// The macOS counterpart of the Windows crash diagnostics minus the Win32
    /// unhandled-exception filter (a SEH-only API; a mach-exception port
    /// handler was judged out of scope — panics and normal exits are covered).
    fn install_panic_hook() {
        std::panic::set_hook(Box::new(|info| {
            let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };
            let loc = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_else(|| "unknown location".to_string());
            log_event("FATAL", &format!("panic at {loc}: {msg}"));
        }));
    }

    /// Single-instance guard: an exclusive, non-blocking `flock` on
    /// `<data_dir>/sysvitals.lock`. Returns `true` when this process now holds
    /// the lock; a second launch finds it taken and exits. The file is
    /// deliberately leaked (`mem::forget`) because `flock` locks live on the
    /// open file description — keeping the fd open for the process lifetime is
    /// what keeps the lock held, and process exit releases it (no stale lock).
    /// Every failure to even open the file returns `true`: a broken data dir
    /// must never block the indicator from starting.
    fn acquire_single_instance() -> bool {
        let Ok(dir) = sysvitals_core::config::data_dir() else {
            return true;
        };
        let _ = std::fs::create_dir_all(&dir);
        let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(dir.join("sysvitals.lock"))
        else {
            return true;
        };
        // SAFETY: `flock` on our own freshly opened fd; LOCK_EX|LOCK_NB never
        // blocks and only fails when another holder exists.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return false;
        }
        std::mem::forget(file);
        true
    }

    thread_local! {
        static SAMPLER: RefCell<Option<Sampler>> = const { RefCell::new(None) };
        static CONFIG: RefCell<Config> = RefCell::new(Config::default());
        static STATUS_ITEM: RefCell<Option<Retained<NSStatusItem>>> = const { RefCell::new(None) };
        /// Last successfully applied config mtime in nanoseconds. Two saves
        /// inside one second must still be visible to the poller.
        static CONFIG_MTIME: RefCell<u64> = const { RefCell::new(0) };
        static CONFIG_ERROR_MTIME: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
        static MTM: RefCell<Option<MainThreadMarker>> = const { RefCell::new(None) };
        // History store, lazily opened on the first tick so a missing data
        // dir never blocks the indicator (mirrors the Windows overlay).
        static STORE: RefCell<Option<sysvitals_core::Store>> = const { RefCell::new(None) };
        static STORE_WRITE_ERROR_LOGGED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static TICK_COUNT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        // The repeating sample timer, kept so the interval can be re-applied
        // when the config file changes it (parity with the Windows overlay,
        // which re-seeds its sample timer on config hot-reload).
        static SAMPLE_TIMER: RefCell<Option<Retained<NSTimer>>> = const { RefCell::new(None) };
        /// Interval currently in effect on the timer; the first tick primes
        /// it (0) and schedules the real timer from there.
        static APPLIED_INTERVAL_MS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
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
        /// Last effective dual-row size after the height fit loop, so a
        /// requested-vs-effective font shrink is logged once rather than on
        /// every sample.
        static DUAL_SHRUNK_FONT: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
        /// One-shot diagnostic for a dual-row block that still exceeds the
        /// menu-bar thickness after the automatic font shrink.
        static DUAL_OVERFLOW_LOGGED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };

        // --- Panel-open highlight state ------------------------------------
        // AppKit's native highlight lasts only while the pointer is pressed,
        // then clears on mouse-up. A system menu-bar extra stays highlighted
        // while its panel or menu is open, but the history popup and context
        // menu are separate surfaces, so the indicator models that open state
        // itself. There is deliberately no hover state: the backdrop appears
        // only while a panel is open, with no extra side padding.
        static HISTORY_POPUP_OPEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static CONTEXT_MENU_OPEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// Composed highlight request; selects which pre-rendered template
        /// image `apply_status_image` mounts.
        static HIGHLIGHT_ON: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static STATUS_IMAGES: RefCell<Option<StatusImages>> = const { RefCell::new(None) };
        /// Variant currently mounted; `None` forces a remount after the images
        /// are rebuilt. Avoids replacing the image on every watcher tick.
        static APPLIED_HIGHLIGHT: std::cell::Cell<Option<bool>> =
            const { std::cell::Cell::new(None) };
        /// Polls the history child while the popup is open, so the highlight
        /// clears promptly when it closes by blur/Esc rather than waiting for
        /// the next sample tick (which can be seconds away).
        static HISTORY_WATCH_TIMER: RefCell<Option<Retained<NSTimer>>> =
            const { RefCell::new(None) };
    }

    /// Screen rectangle (left, top, right, bottom) in GPUI's global top-left,
    /// y-downward logical coordinate space. Uses the button's own bounds in
    /// screen coordinates (not the whole status-bar window) and converts
    /// Cocoa's bottom-left global space with the *primary* screen height, so a
    /// menu bar extra on a secondary display anchors correctly too.
    fn anchor_rect() -> Option<(i64, i64, i64, i64)> {
        let mtm = mtm();
        STATUS_ITEM.with(|si| {
            let item = si.borrow().clone()?;
            let button = item.button(mtm)?;
            let window = button.window()?;
            let rect = window.convertRectToScreen(button.convertRect_toView(button.bounds(), None));
            let screen = window.screen().or_else(|| NSScreen::mainScreen(mtm))?;
            let primary_height = NSScreen::screens(mtm)
                .firstObject()
                .map(|s| s.frame().size.height)
                .unwrap_or_else(|| screen.frame().size.height);
            // Cocoa origin is bottom-left with y growing upward; GPUI/CGDisplay
            // global space starts at the primary screen's top-left.
            let left = rect.origin.x;
            let top = primary_height - (rect.origin.y + rect.size.height);
            let right = left + rect.size.width;
            let bottom = top + rect.size.height;
            Some((
                left.round() as i64,
                top.round() as i64,
                right.round() as i64,
                bottom.round() as i64,
            ))
        })
    }

    /// Get the MainThreadMarker (must be called from main thread).
    fn mtm() -> MainThreadMarker {
        MTM.with(|m| m.borrow().expect("MainThreadMarker not set"))
    }

    /// Load or reload the config file. The mtime cache only advances after a
    /// successful parse, so a corrupt file keeps the last good config and is
    /// retried when it changes again.
    fn reload_config_if_changed() {
        let Ok(path) = config_path() else {
            return;
        };
        let mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);

        let prev = CONFIG_MTIME.with(|m| *m.borrow());
        if mtime == prev {
            return;
        }
        match Config::load(&path) {
            Ok((cfg, _)) => {
                CONFIG_MTIME.with(|m| *m.borrow_mut() = mtime);
                CONFIG_ERROR_MTIME.set(0);
                CONFIG.with(|c| *c.borrow_mut() = cfg.clone());
                log_event(
                    "INFO",
                    &format!(
                        "config reloaded: interval={}ms dual_row={} show_resources={} \
                         show_network={} item_spacing={}px",
                        cfg.sample.interval_ms,
                        cfg.display.dual_row,
                        cfg.display.show_resources,
                        cfg.display.show_network,
                        cfg.display.item_spacing,
                    ),
                );
            }
            Err(e) => {
                // Keep the last good config and retry on the next mtime
                // change; log once per broken mtime.
                if CONFIG_ERROR_MTIME.get() != mtime {
                    CONFIG_ERROR_MTIME.set(mtime);
                    log_event("WARN", &format!("config reload failed: {e}"));
                }
            }
        }
    }

    /// Own and (re-)arm the repeating sample timer. The first tick primes it;
    /// later ticks re-schedule only when a config hot-reload changed
    /// `sample.interval_ms`, so a new interval takes effect within one old tick
    /// — the macOS counterpart of the Windows overlay re-seeding its sample
    /// timer on config change (previously the interval was read once at startup
    /// and never updated live).
    fn reschedule_if_interval_changed() {
        let interval_ms = CONFIG.with(|c| c.borrow().sample.interval_ms);
        if APPLIED_INTERVAL_MS.get() == interval_ms {
            return;
        }
        let block = block2::RcBlock::new(|_timer: std::ptr::NonNull<NSTimer>| {
            tick();
        });
        // SAFETY: the block captures nothing non-Send and fires on the main run
        // loop, same contract as the other timers here.
        let timer = unsafe {
            NSTimer::timerWithTimeInterval_repeats_block(interval_ms as f64 / 1000.0, true, &block)
        };
        // Add to common modes, not just the default mode: the context menu's
        // nested tracking loop would otherwise pause sampling/history while
        // the menu is open.
        let run_loop = NSRunLoop::mainRunLoop();
        unsafe { run_loop.addTimer_forMode(&timer, NSRunLoopCommonModes) };
        // Install the new timer and invalidate the one it replaces so exactly
        // one keeps firing.
        let prev = SAMPLE_TIMER.with(|slot| slot.replace(Some(timer)));
        if let Some(old) = prev {
            old.invalidate();
        }
        APPLIED_INTERVAL_MS.set(interval_ms);
    }

    /// Insert the current CPU/MEM sample into the history database and prune
    /// old rows periodically. Mirrors the Windows overlay: a failed open is
    /// retried every 300 ticks; write failures are logged once.
    fn record_history(cfg: &Config, now: i64, snapshot: &sysvitals_core::Snapshot) {
        if !cfg.history.enabled {
            return;
        }
        let tick = TICK_COUNT.with(|c| {
            let v = c.get().wrapping_add(1);
            c.set(v);
            v
        });
        STORE.with(|s| {
            let mut guard = s.borrow_mut();
            if guard.is_none() && (tick == 1 || tick % 300 == 0) {
                if let Ok(path) = history_path() {
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    match sysvitals_core::Store::open(&path) {
                        Ok(store) => {
                            log_event("INFO", &format!("history store opened: {}", path.display()));
                            *guard = Some(store);
                        }
                        Err(e) => {
                            log_event(
                                "WARN",
                                &format!("history store open failed: {} — {e}", path.display()),
                            );
                            *guard = None;
                        }
                    }
                }
            }
            if let Some(store) = guard.as_ref() {
                // Non-finite samples (first/wake ticks) are skipped inside
                // `insert`; the returned bool is not acted on here.
                if let Err(e) = store.insert(now, snapshot.cpu_pct, snapshot.mem_pct) {
                    if !STORE_WRITE_ERROR_LOGGED.replace(true) {
                        log_event("WARN", &format!("history insert failed: {e}"));
                    }
                }
                if tick % 300 == 0 {
                    if let Err(e) = store.prune(now, cfg.history.retention_days) {
                        if !STORE_WRITE_ERROR_LOGGED.replace(true) {
                            log_event("WARN", &format!("history prune failed: {e}"));
                        }
                    }
                }
            }
        });
    }

    /// One text run baked into the current indicator image: an attributed
    /// string (carrying the font) plus the point to draw it at, in the image's
    /// flipped (top-left origin, y downward) space. The list is captured by the
    /// image's drawing handler, so AppKit can rasterize it without touching
    /// thread-local state from a possibly non-main thread.
    #[derive(Clone)]
    struct DrawItem {
        attr: Retained<NSAttributedString>,
        x: f64,
        y: f64,
    }

    /// The two template variants for the current layout: a transparent
    /// background for idle state and a rounded translucent background for the
    /// open panel/menu state. Template rendering tints the alpha mask with the
    /// menu-bar foreground color, so the backdrop follows light/dark
    /// automatically (black wash in light mode, white wash in dark mode).
    struct StatusImages {
        normal: Retained<NSImage>,
        highlighted: Retained<NSImage>,
        width: f64,
    }

    /// One laid-out dual-row line: left run, optional right run, with measured
    /// widths/heights kept for the column math.
    struct RowRun {
        la: Retained<NSAttributedString>,
        lw: f64,
        lh: f64,
        ra: Option<Retained<NSAttributedString>>,
        rw: f64,
        rh: f64,
    }

    /// Build one attributed run (font baked in) and measure it. `size()` reads
    /// the attributed string's own attributes, so measurement and drawing can
    /// never disagree on the font.
    fn make_run(text: &str, font: &Retained<NSFont>) -> (Retained<NSAttributedString>, f64, f64) {
        let s = NSString::from_str(text);
        // NSFont's super chain is NSFont -> NSObject -> AnyObject; the
        // attribute-dict value type is AnyObject, so two `as_super` steps land
        // on the right reference (`as_super` comes from `objc2::ClassType`).
        let obj: &AnyObject = font.as_super().as_super();
        // `NSFontAttributeName` is an extern static (the pointer itself lives
        // in AppKit), so reading it needs unsafe; dereferencing it to the
        // `&NSString` it always holds is the only operation, which the
        // framework guarantees to be valid.
        let key: &NSString = unsafe { NSFontAttributeName };
        let dict = NSDictionary::from_slices(&[key], &[obj]);
        // SAFETY: the attribute dictionary holds a valid `NSFont` under the
        // documented `NSFontAttributeName` key.
        let attr = unsafe { NSAttributedString::new_with_attributes(&s, &dict) };
        let sz = attr.size();
        (attr, sz.width, sz.height)
    }

    /// Build the measured runs for the 2x2 dual-row layout. Kept separate so
    /// the caller can shrink the font and retry if the block exceeds the menu
    /// bar height.
    fn layout_dual(row: &FormattedRow, font: &Retained<NSFont>) -> (Vec<RowRun>, f64, f64, f64) {
        let mut runs: Vec<RowRun> = Vec::with_capacity(row.lines.len());
        for line in &row.lines {
            let (la, lw, lh) = make_run(line.left.trim_end(), font);
            let (ra, rw, rh) = if line.right.is_empty() {
                (None, 0.0, 0.0)
            } else {
                let (a, w, h) = make_run(line.right.trim_end(), font);
                (Some(a), w, h)
            };
            runs.push(RowRun {
                la,
                lw,
                lh,
                ra,
                rw,
                rh,
            });
        }
        let left_w = runs.iter().map(|r| r.lw).fold(0f64, f64::max);
        let right_w = runs.iter().map(|r| r.rw).fold(0f64, f64::max);
        let line_h = runs.iter().map(|r| r.lh.max(r.rh)).fold(0f64, f64::max);
        (runs, left_w, right_w, line_h)
    }

    /// The metrics the current config shows, in the same order
    /// `format_frame` uses.
    fn visible_metrics(cfg: &Config) -> Vec<Metric> {
        Metric::ALL
            .into_iter()
            .filter(|m| cfg.display.metric_visible(*m))
            .collect()
    }

    /// Worst-case width of one metric cell for the font currently in use.
    /// This is the fixed slot width: the value may gain or lose a digit
    /// inside the slot, but the slot itself never moves. Percentages can only
    /// reach "100%"; network rates reserve the widest four-character numeric
    /// field across all unit tiers (see `RATE_RESERVE`).
    fn metric_reserve(metric: Metric, cfg: &Config, font: &Retained<NSFont>) -> f64 {
        let value = match metric {
            Metric::Cpu | Metric::Mem => "100%",
            Metric::Up | Metric::Down => RATE_RESERVE,
        };
        make_run(&join_prefix(metric_prefix(metric, cfg), value), font).1
    }

    /// Configured label prefix for one metric.
    fn metric_prefix(metric: Metric, cfg: &Config) -> &str {
        match metric {
            Metric::Cpu => &cfg.display.prefix.cpu,
            Metric::Mem => &cfg.display.prefix.mem,
            Metric::Up => &cfg.display.prefix.up,
            Metric::Down => &cfg.display.prefix.down,
        }
    }

    /// Split a formatted cell into its configured label and value. The
    /// formatter joins them with one space; drawing the two runs separately
    /// lets the label stay at the slot start while the value is pinned to the
    /// slot end, so the indicator's outer edges hug its content. A cell that
    /// does not start with the configured prefix (unexpected formatting) is
    /// returned whole as the value.
    fn split_cell(text: &str, prefix: &str) -> (String, String) {
        let text = text.trim_end();
        if !prefix.is_empty() {
            if let Some(rest) = text.strip_prefix(prefix) {
                return (prefix.to_string(), rest.trim_start().to_string());
            }
        }
        (String::new(), text.to_string())
    }

    /// Widest reserved cell in a dual-row column, measured with the final
    /// (possibly height-fitted) font.
    fn column_reserve(metrics: &[Metric], cfg: &Config, font: &Retained<NSFont>) -> f64 {
        metrics
            .iter()
            .map(|m| metric_reserve(*m, cfg, font))
            .fold(0f64, f64::max)
    }

    /// Font used for the two-line layout: the regular menu-bar face scaled
    /// down to fit the fixed bar height would lose too much stroke weight, so
    /// dual-row uses a semibold system face at the same reduced size. The
    /// single-row layout keeps `menuBarFontOfSize(0.0)` untouched.
    fn dual_row_font(size: f64) -> Retained<NSFont> {
        // SAFETY: `NSFontWeightSemibold` is an AppKit extern static; reading
        // any extern static is unsafe, matching the `NSFontAttributeName` use
        // below. The value itself is a plain `CGFloat` constant.
        let weight = unsafe { NSFontWeightSemibold };
        NSFont::systemFontOfSize_weight(size, weight)
    }

    /// Render the formatted frame into the status item as a self-drawn
    /// template image — the macOS counterpart of the Windows overlay's GDI
    /// painting, giving pixel control over dual-row alignment, item spacing
    /// and vertical centering that AppKit's string title does not offer.
    ///
    /// Slot widths come from the format's upper bounds rather than the
    /// current sample: a value gaining or losing a digit only moves inside
    /// its own slot, so a dual-row column origin and the status item's length
    /// stay fixed between samples. There is no baked outer inset and no
    /// custom hover/press background: content meets the item's outer edges so
    /// NSStatusBar can apply its normal inter-item spacing, as native extras
    /// do.
    ///
    /// Skips every action when the render signature (layout parameters plus
    /// all texts) is unchanged: the mounted image simply stays, so an
    /// idle-second tick costs nothing (no rebuild, no bitmap churn). On change
    /// a fresh block-image is built and mounted; the previous image is
    /// reference-counted away.
    fn render_indicator(row: &FormattedRow, cfg: &Config) {
        let dual = cfg.display.dual_row && row.lines.len() > 1;
        let spacing = f64::from(cfg.display.item_spacing.max(1));
        if !cfg.display.show_resources && !cfg.display.show_network {
            return;
        }
        // The menu-bar height (logical points) is the canvas height; drawing
        // inside it keeps the glyphs at the system's own vertical band.
        let img_h = NSStatusBar::systemStatusBar().thickness();

        // Single-row strictly follows the system menu-bar font (size 0 =
        // standard); dual-row starts from the 0.8x variant and the fit loop
        // below shrinks it further only when the two line boxes exceed the
        // fixed bar height.
        let base = NSFont::menuBarFontOfSize(0.0);
        // Never shrink below the previous 0.75x baseline: if 0.8x cannot be
        // made to fit by tightening the line step, fall back to the known-good
        // size instead of ending up smaller than before.
        let font_floor = (base.pointSize() * 0.75).max(1.0);
        let mut font = if dual {
            let scaled = (base.pointSize() * DUAL_ROW_FONT_FACTOR).max(1.0);
            dual_row_font(scaled)
        } else {
            base
        };
        let requested_size = font.pointSize();

        // --- lay out the runs (flipped space: y grows downward) -----------
        let mut items: Vec<DrawItem> = Vec::new();
        let mut texts: Vec<String> = Vec::new();
        let width;
        let mut line_step = 0.0_f64;
        // `display.theme` is deliberately not consulted: the indicator is a
        // template image, which AppKit tints from the menu-bar appearance —
        // light/dark inversion is automatic and per-item color is not a thing
        // on the menu bar (see the theme-picker removal in app-settings).
        if dual {
            // 2x2 quadrant mirroring the Windows overlay's `measure_frame`:
            // left column resources, right column network. When only one
            // group is enabled, `format_frame` puts those metrics in the left
            // column; mirror that split here so the reserved slots match.
            let (left_metrics, right_metrics) =
                if cfg.display.show_resources && cfg.display.show_network {
                    (
                        vec![Metric::Cpu, Metric::Mem],
                        vec![Metric::Up, Metric::Down],
                    )
                } else if cfg.display.show_resources {
                    (vec![Metric::Cpu, Metric::Mem], Vec::new())
                } else {
                    (vec![Metric::Up, Metric::Down], Vec::new())
                };
            for line in &row.lines {
                texts.push(line.left.clone());
                texts.push(line.right.clone());
            }
            let (mut runs, mut left_w, mut right_w, mut line_h) = layout_dual(row, &font);
            let n = runs.len() as f64;
            line_step = line_h;
            let mut block_h = line_h + line_step * (n - 1.0).max(0.0);
            // The system menu-bar thickness is the hard canvas height. The
            // attributed line box includes leading, so first try tightening
            // the inter-line step (two short rows of labels can sit closer
            // without touching); only if even the minimum step does not fit
            // do we shrink the font and re-measure. This is what lets the
            // requested 0.8x size survive on a standard bar instead of being
            // silently pulled back to the old 0.75x by the height guard.
            let mut attempts = 0;
            while block_h > img_h && attempts < 3 {
                // The two rows are short labels without descenders in the
                // default configuration, so a step equal to the font size is
                // still readable; this keeps the requested 0.8x size instead
                // of letting the old height guard pull it back to 0.75x.
                let min_step = font.pointSize().max(1.0);
                let candidate = ((img_h - 0.5).max(1.0) - line_h) / (n - 1.0).max(1.0);
                if candidate >= min_step {
                    line_step = candidate.min(line_h);
                    block_h = line_h + line_step * (n - 1.0).max(0.0);
                    break;
                }
                let factor = (img_h / block_h * 0.98).clamp(0.5, 1.0);
                let target = (font.pointSize() * factor).max(font_floor);
                if target >= font.pointSize() - 0.01 {
                    break;
                }
                font = dual_row_font(target);
                let laid = layout_dual(row, &font);
                runs = laid.0;
                left_w = laid.1;
                right_w = laid.2;
                line_h = laid.3;
                line_step = line_h;
                block_h = line_h + line_step * (n - 1.0).max(0.0);
                attempts += 1;
            }
            if attempts > 0 {
                let effective = font.pointSize();
                if (DUAL_SHRUNK_FONT.with(|slot| slot.get()) - effective).abs() > 0.01 {
                    DUAL_SHRUNK_FONT.with(|slot| slot.set(effective));
                    log_event(
                        "INFO",
                        &format!(
                            "dual-row font fitted: requested={requested_size:.2} \
                             effective={effective:.2} bar={img_h:.1}"
                        ),
                    );
                }
            }
            if block_h > img_h && !DUAL_OVERFLOW_LOGGED.replace(true) {
                log_event(
                    "WARN",
                    &format!(
                        "dual-row block exceeds menu bar: block={block_h:.1} bar={img_h:.1} font={:.2}",
                        font.pointSize()
                    ),
                );
            }
            let y0 = ((img_h - block_h) / 2.0).max(0.0);
            // Fixed slots keep every origin stable, but the image has no
            // baked outer padding: the left cell starts at x = 0 and the
            // right value ends at the image's right edge. Inter-item spacing
            // is left to NSStatusBar, matching native menu-bar extras. Within
            // the right column the label stays at the column's fixed x while
            // the value is right-aligned, so labels remain aligned and only
            // the numerals' left edge moves inside their slot.
            let left_slot = column_reserve(&left_metrics, cfg, &font).max(left_w);
            let right_slot = column_reserve(&right_metrics, cfg, &font).max(right_w);
            let has_right = runs.iter().any(|r| r.ra.is_some());
            width = left_slot + (if has_right { spacing + right_slot } else { 0.0 });
            let right_x = left_slot + spacing;
            for (i, r) in runs.iter().enumerate() {
                let y = y0 + line_step * i as f64;
                items.push(DrawItem {
                    attr: r.la.clone(),
                    x: 0.0,
                    y,
                });
                if r.ra.is_some() {
                    let line = &row.lines[i];
                    let prefix = right_metrics
                        .get(i)
                        .map(|m| metric_prefix(*m, cfg))
                        .unwrap_or("");
                    let (label, value) = split_cell(&line.right, prefix);
                    if !label.is_empty() {
                        let (attr, _, _) = make_run(&label, &font);
                        items.push(DrawItem {
                            attr,
                            x: right_x,
                            y,
                        });
                    }
                    if !value.is_empty() {
                        let (attr, value_w, _) = make_run(&value, &font);
                        items.push(DrawItem {
                            attr,
                            x: width - value_w,
                            y,
                        });
                    }
                }
            }
        } else {
            // Single row: each metric gets its own fixed slot, so a digit
            // change in one metric cannot push the following metrics around;
            // each slot is vertically centered on its own measured height.
            let parts: Vec<String> = if !row.items.is_empty() {
                row.items.clone()
            } else {
                let mut v = Vec::new();
                for line in &row.lines {
                    v.push(line.left.clone());
                    if !line.right.is_empty() {
                        v.push(line.right.clone());
                    }
                }
                v
            };
            if parts.is_empty() {
                return;
            }
            let metrics = visible_metrics(cfg);
            let mut slots: Vec<f64> = metrics
                .iter()
                .map(|m| metric_reserve(*m, cfg, &font))
                .collect();
            if slots.len() != parts.len() {
                // Defensive fallback: the frame and config disagree on the
                // visible metric set, so measure the actual strings instead
                // of risking a clipped slot.
                slots = parts
                    .iter()
                    .map(|p| make_run(p.trim_end(), &font).1)
                    .collect();
            }
            let count = parts.len();
            let mut slot_start = 0.0;
            for (i, part) in parts.into_iter().enumerate() {
                let (attr, w, h) = make_run(part.trim_end(), &font);
                texts.push(part);
                let slot = slots.get(i).copied().unwrap_or(w).max(w);
                // First cell hugs the left edge; the last cell hugs the right
                // edge so the item's visible bounds match its content and the
                // system can apply its normal inter-item spacing. Middle
                // slots stay left-aligned on their fixed origins.
                let x = if count > 1 && i + 1 == count {
                    slot_start + slot - w
                } else {
                    slot_start
                };
                let y = ((img_h - h) / 2.0).max(0.0);
                items.push(DrawItem { attr, x, y });
                slot_start += slot + spacing;
            }
            width = slot_start - spacing;
        }
        if items.is_empty() || width <= 0.0 {
            return;
        }

        let sig = format!(
            "{dual}|{spacing}|{img_h:.1}|{:.2}|{width:.1}|{line_step:.2}|{}",
            font.pointSize(),
            texts.join("\u{1}")
        );
        if DRAW_SIGNATURE.with(|s| *s.borrow() == sig) {
            return;
        }

        // --- build the template images -------------------------------------
        // Flipped drawing context: origin top-left, y downward — the space the
        // layout above was computed in. The handlers capture the run list, so
        // rasterization never touches thread-local state. Two variants are
        // pre-rendered: a transparent idle image and an open-panel image with
        // a rounded translucent backdrop. Template rendering tints the alpha
        // mask with the menu-bar foreground, so the backdrop is a black wash
        // in light mode and a white wash in dark mode. The backdrop has no
        // extra side padding: it follows the item's current content edges.
        let normal_items = items.clone();
        let normal_block = block2::RcBlock::new(move |_rect: NSRect| -> Bool {
            for it in &normal_items {
                it.attr.drawAtPoint(NSPoint::new(it.x, it.y));
            }
            Bool::YES
        });
        let normal = NSImage::imageWithSize_flipped_drawingHandler(
            NSSize::new(width, img_h),
            true,
            &normal_block,
        );
        normal.setTemplate(true);

        let highlight_block = block2::RcBlock::new(move |_rect: NSRect| -> Bool {
            let backdrop = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                NSRect::new(
                    NSPoint::new(1.0, 1.0),
                    NSSize::new((width - 2.0).max(1.0), (img_h - 2.0).max(1.0)),
                ),
                4.0,
                4.0,
            );
            // Template images use the alpha channel as the mask; the RGB
            // value is irrelevant, only the opacity matters.
            NSColor::colorWithWhite_alpha(0.0, STATUS_HIGHLIGHT_ALPHA).set();
            backdrop.fill();
            for it in &items {
                it.attr.drawAtPoint(NSPoint::new(it.x, it.y));
            }
            Bool::YES
        });
        let highlighted = NSImage::imageWithSize_flipped_drawingHandler(
            NSSize::new(width, img_h),
            true,
            &highlight_block,
        );
        highlighted.setTemplate(true);

        STATUS_IMAGES.with(|slot| {
            *slot.borrow_mut() = Some(StatusImages {
                normal,
                highlighted,
                width,
            });
        });
        APPLIED_HIGHLIGHT.set(None);
        DRAW_SIGNATURE.with(|s| *s.borrow_mut() = sig);
        refresh_status_highlight();
    }

    /// Sample metrics, record the history row, and update the indicator.
    fn tick() {
        reload_config_if_changed();
        reschedule_if_interval_changed();
        reap_children();

        let cfg = CONFIG.with(|c| c.borrow().clone());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let snapshot = SAMPLER.with(|s| {
            let mut guard = s.borrow_mut();
            let sampler = guard
                .get_or_insert_with(|| Sampler::new(NetSelection::Auto, cfg.sample.cpu_smoothing));
            sampler.sample(now)
        });

        record_history(&cfg, now, &snapshot);

        let row = format_frame(&snapshot, &cfg);
        render_indicator(&row, &cfg);
    }

    /// Directory containing the running executable.
    fn exe_dir() -> Option<std::path::PathBuf> {
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
    fn relaunch_target() -> Option<std::path::PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let bundle = exe
            .ancestors()
            .find(|p| p.extension() == Some(std::ffi::OsStr::new("app")))?;
        Some(bundle.to_path_buf())
    }

    /// Whether a previously spawned child (held in `slot`) is still running.
    /// Reaps it once it has exited.
    fn child_alive(slot: &RefCell<Option<Child>>) -> bool {
        let mut borrow = slot.borrow_mut();
        match borrow.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(_status)) => {
                    *borrow = None;
                    false
                }
                _ => true,
            },
            None => false,
        }
    }

    /// Reap children that exited on their own so a long-running indicator does
    /// not accumulate zombies between launches.
    fn reap_children() {
        SETTINGS_CHILD.with(|slot| {
            let mut borrow = slot.borrow_mut();
            if let Some(child) = borrow.as_mut() {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    *borrow = None;
                }
            }
        });
        let history_exited = HISTORY_CHILD.with(|slot| {
            let mut borrow = slot.borrow_mut();
            if let Some(child) = borrow.as_mut() {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    *borrow = None;
                    return true;
                }
            }
            false
        });
        if history_exited {
            HISTORY_POPUP_OPEN.set(false);
            stop_history_watch();
            refresh_status_highlight();
        }
    }

    /// Terminate a child panel and reap it so no zombie remains.
    fn terminate_child(slot: &RefCell<Option<Child>>, label: &str) -> bool {
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

    fn close_settings() -> bool {
        SETTINGS_CHILD.with(|slot| terminate_child(slot, "settings window"))
    }

    /// Close both child panels on restart/quit. Leaving them alive makes a
    /// restarted instance open a second History popup and leaves Settings
    /// orphaned after Quit.
    fn close_children() {
        let _ = close_history();
        let _ = close_settings();
    }

    /// Launch the external GPUI settings process (`SysVitalsSettings`), one
    /// live instance at a time.
    fn launch_settings() {
        // `thread_local!` statics expose a `LocalKey`; reach the cell through
        // `with`, and re-entrancy (`child_alive` borrows it) must not nest, so
        // query liveness first, then spawn, then store.
        if SETTINGS_CHILD.with(|slot| child_alive(slot)) {
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
        match std::process::Command::new(settings).spawn() {
            Ok(child) => {
                SETTINGS_CHILD.with(|slot| *slot.borrow_mut() = Some(child));
            }
            Err(e) => log_event("WARN", &format!("launch_settings: spawn failed: {e}")),
        }
    }

    /// Launch the external history chart process (`SysVitalsHistory`), passing
    /// the indicator's screen rect so the popup can anchor to it. One live
    /// instance at a time.
    fn launch_history() {
        if HISTORY_CHILD.with(|slot| child_alive(slot)) {
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
        let mut cmd = std::process::Command::new(history);
        if let Some((l, t, r, b)) = anchor_rect() {
            cmd.args([
                "--anchor",
                &l.to_string(),
                &t.to_string(),
                &r.to_string(),
                &b.to_string(),
            ]);
        }
        match cmd.spawn() {
            Ok(child) => {
                HISTORY_CHILD.with(|slot| *slot.borrow_mut() = Some(child));
                // Keep the status item highlighted while the popup is open,
                // like a system menu-bar extra with its panel showing.
                HISTORY_POPUP_OPEN.set(true);
                refresh_status_highlight();
                start_history_watch();
            }
            Err(e) => log_event("WARN", &format!("launch_history: spawn failed: {e}")),
        }
    }

    /// Close the trend popup if one is running: the click-to-toggle half of
    /// `button_clicked:`. Kills the child and reaps it (`wait`) so no zombie
    /// lingers under the long-running indicator. Returns `true` when a live
    /// popup was closed; `false` when nothing was running (or it had already
    /// exited, which is reaped here so the slot is free for the next open).
    fn close_history() -> bool {
        let closed = HISTORY_CHILD.with(|slot| terminate_child(slot, "history popup"));
        // Clear even when no live child was found: it may have just exited on
        // its own while the watcher was between ticks.
        HISTORY_POPUP_OPEN.set(false);
        stop_history_watch();
        refresh_status_highlight();
        closed
    }

    /// Poll the history child while its popup is open. The child exits on
    /// blur/Esc on its own, and the sample tick may be seconds away; this
    /// clears the open-panel highlight within ~150 ms.
    fn start_history_watch() {
        stop_history_watch();
        let block = block2::RcBlock::new(|_timer: std::ptr::NonNull<NSTimer>| {
            if HISTORY_CHILD.with(|slot| child_alive(slot)) {
                // Reassert while the popup is alive: AppKit clears the native
                // pressed state when the click tracking loop ends, so the
                // initial highlight from the action alone lasts one frame.
                refresh_status_highlight();
                return;
            }
            HISTORY_POPUP_OPEN.set(false);
            refresh_status_highlight();
            stop_history_watch();
        });
        // SAFETY: the block captures no non-Send state and runs on the main
        // run loop, same contract as the sample timer above.
        let timer = unsafe { NSTimer::timerWithTimeInterval_repeats_block(0.15, true, &block) };
        let run_loop = NSRunLoop::mainRunLoop();
        // SAFETY: adding a live timer to the current main run loop; common
        // modes keep it alive during menu/popup tracking loops.
        unsafe { run_loop.addTimer_forMode(&timer, NSRunLoopCommonModes) };
        HISTORY_WATCH_TIMER.with(|slot| {
            if let Some(prev) = slot.borrow_mut().replace(timer) {
                prev.invalidate();
            }
        });
    }

    fn stop_history_watch() {
        HISTORY_WATCH_TIMER.with(|slot| {
            if let Some(timer) = slot.borrow_mut().take() {
                timer.invalidate();
            }
        });
    }

    /// Recompute the status button's highlight from the open surfaces. There
    /// is no hover source: the backdrop only marks an open panel or menu.
    fn refresh_status_highlight() {
        set_button_highlight(HISTORY_POPUP_OPEN.get() || CONTEXT_MENU_OPEN.get());
    }

    /// Open Activity Monitor (macOS equivalent of Task Manager). `status()`
    /// waits for the short-lived `open` helper, so it cannot leave a zombie.
    fn open_activity_monitor() {
        if let Err(e) = std::process::Command::new("open")
            .args(["-a", "Activity Monitor"])
            .status()
        {
            log_event("WARN", &format!("open Activity Monitor failed: {e}"));
        }
    }

    /// Restart: schedule a delayed relaunch, then exit.
    fn restart_self() {
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

    /// Show the context menu at the status item position.
    fn show_context_menu() {
        let mtm = mtm();
        let target = ACTION_HANDLER.with(|h| h.borrow().clone());
        let Some(target) = target.as_ref() else {
            return;
        };
        STATUS_ITEM.with(|si| {
            let si_borrow = si.borrow();
            let Some(item) = si_borrow.as_ref() else {
                return;
            };
            let Some(button) = item.button(mtm) else {
                return;
            };

            let menu = NSMenu::new(mtm);
            // A menu popped from a status-item button can inherit the button's
            // vibrant menu-bar appearance, which renders dark in light mode.
            // Follow the app's effective appearance so the menu matches the
            // system like native menu-extra menus.
            let app = NSApplication::sharedApplication(mtm);
            let appearance = app.effectiveAppearance();
            menu.setAppearance(Some(&appearance));
            // Menu labels follow the configured language, like the Windows
            // overlay's tray menu (previously hardcoded English here).
            let lang = CONFIG.with(|c| c.borrow().display.language);

            // Activity Monitor first: it used to be the double-click action,
            // and moving it to the top of the context menu is what freed the
            // left click to open the trend chart instantly (mirrors the
            // Windows overlay's Task Manager entry).
            let activity_title = NSString::from_str(t(lang, "menu.activity_monitor"));
            let activity_item = NSMenuItem::new(mtm);
            activity_item.setTitle(&activity_title);
            // SAFETY: setting a valid selector on a menu item.
            unsafe { activity_item.setAction(Some(sel!(showActivityMonitor:))) };
            // SAFETY: `target` is the live ActionHandler kept in ACTION_HANDLER.
            unsafe { activity_item.setTarget(Some(target)) };
            menu.addItem(&activity_item);

            let settings_title = NSString::from_str(t(lang, "menu.settings"));
            let settings_item = NSMenuItem::new(mtm);
            settings_item.setTitle(&settings_title);
            unsafe { settings_item.setAction(Some(sel!(showSettings:))) };
            // SAFETY: `target` is the live ActionHandler kept in ACTION_HANDLER.
            unsafe { settings_item.setTarget(Some(target)) };
            menu.addItem(&settings_item);

            let restart_title = NSString::from_str(t(lang, "menu.restart"));
            let restart_item = NSMenuItem::new(mtm);
            restart_item.setTitle(&restart_title);
            unsafe { restart_item.setAction(Some(sel!(restartApp:))) };
            // SAFETY: `target` is the live ActionHandler kept in ACTION_HANDLER.
            unsafe { restart_item.setTarget(Some(target)) };

            // Separator, then Restart above Quit: Restart is a lifecycle
            // action like Quit, so both sit below the divider, Restart first.
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            menu.addItem(&restart_item);

            let quit_title = NSString::from_str(t(lang, "menu.quit"));
            let quit_item = NSMenuItem::new(mtm);
            quit_item.setTitle(&quit_title);
            unsafe { quit_item.setAction(Some(sel!(quitApp:))) };
            unsafe { quit_item.setTarget(Some(target)) };
            menu.addItem(&quit_item);

            // Pop the menu directly at the button's lower-left corner. The
            // previous approach (attach with `setMenu`, then `performClick`,
            // then detach) is unreliable once the item lives in the notch
            // overflow panel: `performClick` there highlights the button but
            // does not pop the attached menu, so a right-click looked like it
            // did nothing. `popUpMenuPositioningItem` shows the menu against
            // the button view regardless of how the item is positioned, and it
            // runs a nested tracking loop that returns once the menu closes.
            let view: &NSView = &button;
            let location = NSPoint::new(0.0, button.bounds().size.height);
            // The nested menu tracking loop returns when the menu closes, so
            // the button can hold the open-panel backdrop for the whole
            // session like a system status item.
            CONTEXT_MENU_OPEN.set(true);
            refresh_status_highlight();
            let _ = menu.popUpMenuPositioningItem_atLocation_inView(None, location, Some(view));
            CONTEXT_MENU_OPEN.set(false);
            refresh_status_highlight();
        });
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
                match event.as_ref().map(|e| e.r#type()) {
                    Some(NSEventType::RightMouseUp) | Some(NSEventType::RightMouseDown) => {
                        show_context_menu();
                    }
                    // Any non-right click toggles the trend chart: a live
                    // popup is closed, otherwise a new one opens (matching the
                    // Windows overlay). There is no double-click branch and no
                    // deferral — Activity Monitor lives in the context menu's
                    // first item, so a left click never waits for a possible
                    // second press.
                    _ => {
                        if !close_history() {
                            launch_history();
                        }
                    }
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
    );

    /// Select the idle or open-panel template variant. AppKit's native
    /// `highlight:` is not used: it is tied to a pointer tracking loop and
    /// does not render reliably from programmatic state.
    fn set_button_highlight(on: bool) {
        HIGHLIGHT_ON.set(on);
        apply_status_image();
    }

    /// Mount the pre-rendered template variant and its width. Both variants
    /// are built together by `render_indicator`, so switching never allocates
    /// or re-lays out text.
    fn apply_status_image() {
        let on = HIGHLIGHT_ON.get();
        if APPLIED_HIGHLIGHT.get() == Some(on) {
            return;
        }
        let mtm = mtm();
        let applied = STATUS_IMAGES.with(|images| {
            let images = images.borrow();
            let Some(images) = images.as_ref() else {
                return false;
            };
            let image = if on {
                &images.highlighted
            } else {
                &images.normal
            };
            STATUS_ITEM.with(|si| {
                let si_borrow = si.borrow();
                let Some(item) = si_borrow.as_ref() else {
                    return false;
                };
                let Some(button) = item.button(mtm) else {
                    return false;
                };
                // Title cleared so only the image renders; `ImageOnly` +
                // `ScaleNone` keeps AppKit from rescaling the point layout.
                button.setTitle(&NSString::from_str(""));
                button.setImage(Some(image));
                button.setImagePosition(NSCellImagePosition::ImageOnly);
                button.setImageScaling(NSImageScaling::ScaleNone);
                item.setLength(images.width);
                // Call through `NSView` explicitly: `NSControl` also declares a
                // deprecated zero-argument `setNeedsDisplay()`, which wins
                // method resolution on the button and shadows the view method.
                let view: &NSView = &button;
                view.setNeedsDisplay(true);
                view.displayIfNeeded();
                true
            })
        });
        if applied {
            APPLIED_HIGHLIGHT.set(Some(on));
        }
    }

    impl ActionHandler {
        fn new(mtm: MainThreadMarker) -> Retained<Self> {
            let this = Self::alloc(mtm);
            // ActionHandler does not override init, so calling it directly
            // dispatches to NSObject's init. Avoids the super() wrapper which
            // triggers RetainSemantics issues with Allocated<T> in objc2 0.6.5.
            unsafe { msg_send![this, init] }
        }
    }

    pub fn run() {
        // Crash diagnostics and the single-instance guard run before anything
        // else, mirroring the Windows overlay's startup sequence. A second
        // launch exits quietly; the lock is held via an fd that is deliberately
        // leaked (see `acquire_single_instance`).
        install_panic_hook();
        if !acquire_single_instance() {
            eprintln!("app-mac: another instance is already running; exiting");
            return;
        }

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
        }

        // Keep the handler alive for the process lifetime, addressable so the
        // context menu can name it as the action target (see ACTION_HANDLER).
        // The stored clone is sufficient; leaking the original as well would
        // just retain one extra reference forever.
        ACTION_HANDLER.with(|h| *h.borrow_mut() = Some(handler));

        // Store the status item for the tick callback.
        STATUS_ITEM.with(|si| *si.borrow_mut() = Some(status_item));

        // Load initial config.
        if let Ok(path) = config_path() {
            if let Ok((cfg, _)) = Config::load(&path) {
                CONFIG.with(|c| *c.borrow_mut() = cfg);
            }
        }

        // Daily-file logging: prune stale logs once, then stamp the start and
        // the effective config (same shape as the Windows overlay's startup
        // log, so a user can just send this file when something looks wrong).
        prune_old_logs();
        let cfg = CONFIG.with(|c| c.borrow().clone());
        log_event("INFO", "SysVitals (mac) starting");
        log_event(
            "INFO",
            &format!(
                "config loaded: interval={}ms language={:?} theme={:?} dual_row={} \
                 show_resources={} show_network={} item_spacing={}px",
                cfg.sample.interval_ms,
                cfg.display.language,
                cfg.display.theme,
                cfg.display.dual_row,
                cfg.display.show_resources,
                cfg.display.show_network,
                cfg.display.item_spacing,
            ),
        );

        // Initial tick — also primes the repeating sample timer (see
        // `reschedule_if_interval_changed`).
        tick();

        // Enter the application event loop (blocks forever).
        app.run();
    }
}

#[cfg(not(target_os = "macos"))]
mod macos {
    pub fn run() {
        eprintln!("app-mac: this binary only runs on macOS.");
    }
}

fn main() {
    macos::run();
}
