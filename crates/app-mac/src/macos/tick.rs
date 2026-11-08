//! tick: split out of `macos` (see `macos/mod.rs` for the module map).

use super::*;

/// Tick driven by the repeating timer. `IN_RUN_LOOP` marks the point where
/// ticks are dispatched from inside `NSApplication::run`; the startup tick
/// runs earlier, so accessibility/tooltip updates are applied from the
/// timer ticks instead of from that first tick.
pub(crate) fn timer_tick() {
    IN_RUN_LOOP.set(true);
    tick();
    IN_RUN_LOOP.set(false);
}

/// Own and (re-)arm the repeating sample timer. The first tick primes it;
/// later ticks re-schedule only when a config hot-reload changed
/// `sample.interval_ms`, so a new interval takes effect within one old tick
/// — the macOS counterpart of the Windows overlay re-seeding its sample
/// timer on config change (previously the interval was read once at startup
/// and never updated live).
pub(crate) fn reschedule_if_interval_changed() {
    let interval_ms = CONFIG.with(|c| c.borrow().sample.interval_ms);
    if APPLIED_INTERVAL_MS.get() == interval_ms {
        return;
    }
    let block = block2::RcBlock::new(|_timer: std::ptr::NonNull<NSTimer>| {
        timer_tick();
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

/// Enqueue the current CPU/MEM sample for the shared background history
/// writer. The main RunLoop never opens or writes SQLite; a slow disk, an
/// old-database rollup backfill, or a prune/VACUUM cannot freeze the menu
/// bar.
pub(crate) fn record_history(cfg: &Config, now: i64, snapshot: &sysvitals_core::Snapshot) {
    if !cfg.history.enabled {
        return;
    }
    let log: sysvitals_core::history_writer::LogFn =
        Arc::new(|level: &str, message: &str| log_event(level, message));
    HISTORY_WRITER.with(|slot| {
        slot.borrow_mut().record(
            now,
            snapshot.cpu_pct,
            snapshot.mem_pct,
            cfg.history.retention_days,
            log,
        );
    });
}

/// Sample metrics, record the history row, and update the indicator.
pub(crate) fn tick() {
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
        let sampler =
            guard.get_or_insert_with(|| Sampler::new(NetSelection::Auto, cfg.sample.cpu_smoothing));
        sampler.sample(now)
    });

    record_history(&cfg, now, &snapshot);

    let row = format_frame(&snapshot, &cfg);
    render_indicator(&row, &cfg);
    // The startup tick runs before the run loop; deferred accessibility
    // updates from that tick land on the first timer-driven tick.
    if IN_RUN_LOOP.get() {
        apply_pending_accessibility();
    }
}
