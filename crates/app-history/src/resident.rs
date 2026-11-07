//! resident: split out of the crate root (see `main.rs` for the module map).

use super::*;

// ------------------------------------------------------- resident mode (Windows)

/// The command/status block created by the indicator. The resident process
/// keeps a read/write view of it for its whole lifetime; the one-shot macOS
/// path never maps it.
#[cfg(windows)]
pub(crate) struct HistoryShared {
    pub(crate) state: *mut sysvitals_core::HistoryState,
}

#[cfg(windows)]
impl HistoryShared {
    pub(crate) fn snapshot(&self) -> sysvitals_core::HistoryState {
        unsafe { sysvitals_core::HistoryState::read_consistent(self.state) }
    }

    pub(crate) fn set_visible(&self, visible: bool) {
        unsafe { sysvitals_core::HistoryState::write_visible(self.state, visible) }
    }

    /// Called by the popup's own dismissal paths (blur/Esc) and by the
    /// resident loop after it takes the window down. Only the child-owned
    /// `visible` flag is cleared: `desired_open` belongs to the indicator,
    /// and writing it here used to race a fresh click and swallow it. The
    /// resident loop records the dismissed generation locally instead.
    pub(crate) fn mark_hidden(&self) {
        self.set_visible(false);
    }
}

#[cfg(not(windows))]
pub(crate) struct HistoryShared;

#[cfg(not(windows))]
impl HistoryShared {
    pub(crate) fn mark_hidden(&self) {}
}

/// Open the indicator-owned state block. A missing block — or one that fails
/// the magic/version check, i.e. was not created by this project's overlay —
/// means this process was not started by a resident-capable overlay, so it
/// exits instead of lingering invisibly.
#[cfg(windows)]
pub(crate) fn open_history_state() -> Option<*mut sysvitals_core::HistoryState> {
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
        if view.Value.is_null() {
            return None;
        }
        let ptr = view.Value as *mut sysvitals_core::HistoryState;
        // Validate the header before trusting any flag: a block without the
        // expected magic/version is not ours (or is corrupt), and consuming
        // its `desired_open` could open or close popups spuriously.
        let header = sysvitals_core::HistoryState::read_consistent(ptr);
        if header.magic != sysvitals_core::HISTORY_STATE_MAGIC
            || header.version != sysvitals_core::HISTORY_STATE_VERSION
        {
            let _ = UnmapViewOfFile(view);
            return None;
        }
        Some(ptr)
    }
}

/// `--parent-pid <pid>`: the process exits when the indicator does, even if
/// the launcher never gets a chance to terminate it. The pure parsing lives
/// in `core::cli` (shared with the settings panel).
pub(crate) fn parse_parent_pid() -> Option<u32> {
    let args: Vec<String> = env::args().skip(1).collect();
    sysvitals_core::cli::parse_parent_pid(&args)
}

/// macOS one-shot popup: quit when the menu-bar indicator goes away instead
/// of lingering as an orphan window. Windows resident mode has the stronger
/// process-handle check in `run_resident`. Shared one-second poll in
/// `sysvitals_panel::watch_parent`.
#[cfg(target_os = "macos")]
pub(crate) fn watch_parent(cx: &mut App) {
    sysvitals_panel::watch_parent(cx, "history");
}

#[cfg(windows)]
pub(crate) fn trim_working_set() {
    unsafe {
        // SIZE_T(-1) asks Windows to trim the process to its minimum working
        // set. Private commit stays allocated; what the user sees in Task
        // Manager (and what competes for physical RAM) drops to a few MB.
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

/// Resident loop used on Windows: keep the process (and GPUI/DirectX) warm
/// with no window, open a window only while the indicator asks for one, and
/// trim the working set while idle. The window is created hidden and shown
/// after its first chart load so the first visible frame already has data.
#[cfg(windows)]
pub(crate) fn run_resident(cx: &mut App) {
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
        let mut popup_size: Option<gpui_kit::Size<Pixels>> = None;
        let mut activated = false;
        let mut opened_at: Option<Instant> = None;
        let mut last_activation_attempt: Option<Instant> = None;
        let mut activation_attempts: u32 = 0;
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

            let snapshot = cx.update(|_app| shared.snapshot());
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
                    popup_size = None;
                    activated = false;
                    opened_at = None;
                    last_activation_attempt = None;
                    activation_attempts = 0;
                    idle_since = Some(Instant::now());
                    trimmed = false;
                    shared.mark_hidden();
                    trim_working_set();
                    continue;
                }
                if snapshot.desired_open == 0 {
                    // `visible == 0` after activation means the view already
                    // dismissed itself (blur/Esc) and its removal is in
                    // flight; calling remove_window again could double-remove
                    // it. A visible window, or one still waiting for its first
                    // activation, was asked to close by the indicator.
                    dismissed_generation = Some(snapshot.generation);
                    if snapshot.visible != 0 || !activated {
                        log_event("INFO", "history: window closed by indicator");
                        let removed = cx.update(|app| {
                            handle
                                .update(app, |_, window, _| window.remove_window())
                                .is_ok()
                        });
                        if !removed {
                            log_event(
                                "WARN",
                                "history: remove_window failed; the hidden window may linger",
                            );
                        }
                    }
                    shared.mark_hidden();
                    window = None;
                    first_load = None;
                    popup_size = None;
                    activated = false;
                    opened_at = None;
                    last_activation_attempt = None;
                    activation_attempts = 0;
                    idle_since = Some(Instant::now());
                    trimmed = false;
                    trim_working_set();
                    continue;
                }
                // A new open request arrived while the window is open: the
                // indicator was clicked on a different taskbar/monitor. GPUI
                // windows cannot be repositioned after creation, so reopen the
                // popup anchored at the new indicator. While the window is
                // still hidden and waiting for its first activation, a second
                // click only re-arms the request; reopening would restart the
                // first-load wait for no benefit.
                if snapshot.generation != open_generation && snapshot.anchor_is_valid() {
                    if !activated {
                        open_generation = snapshot.generation;
                        dismissed_generation = None;
                        continue;
                    }
                    log_event(
                        "INFO",
                        "history: indicator moved; reopening at the new anchor",
                    );
                    let removed = cx.update(|app| {
                        handle
                            .update(app, |_, window, _| window.remove_window())
                            .is_ok()
                    });
                    if !removed {
                        log_event(
                            "WARN",
                            "history: remove_window failed before the move; continuing",
                        );
                    }
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
                        Some((new_handle, ready, new_size)) => {
                            window = Some(new_handle);
                            first_load = Some(ready);
                            popup_size = new_size;
                            activated = false;
                            opened_at = Some(Instant::now());
                            last_activation_attempt = None;
                            activation_attempts = 0;
                            open_generation = snapshot.generation;
                            dismissed_generation = None;
                            idle_since = None;
                            trimmed = false;
                            // `visible` is published only once the window is
                            // actually activated (or the 3 s fallback fires);
                            // until then it is a pending request, not an open
                            // popup.
                        }
                        None => {
                            dismissed_generation = Some(snapshot.generation);
                            shared.mark_hidden();
                            window = None;
                            first_load = None;
                            popup_size = None;
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
                    // GPUI's Windows activate() injects a synthetic Alt press
                    // to satisfy the foreground lock. Retrying every ~100ms
                    // could send dozens of Alt events into the foreground
                    // app, so cap the attempts and space them out. The 3s
                    // fallback below still shows the window without
                    // activation when the lock refuses us.
                    let retry_due = activation_attempts < 3
                        && last_activation_attempt
                            .is_none_or(|last| last.elapsed() > Duration::from_millis(500));
                    if (ready || timed_out) && retry_due {
                        activation_attempts += 1;
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
                            shared.set_visible(true);
                            log_event(
                                "INFO",
                                &format!(
                                    "history: popup shown in {}ms (activation attempts {activation_attempts})",
                                    elapsed_ms()
                                ),
                            );
                        }
                    }
                    if !activated
                        && opened_at
                            .is_some_and(|opened| opened.elapsed() > Duration::from_secs(3))
                    {
                        // The foreground lock kept refusing activation. Show
                        // anyway so the chart is usable; Esc or an indicator
                        // click still closes it, but click-away cannot be
                        // armed. Log once for diagnosis.
                        activated = true;
                        shared.set_visible(true);
                        // No activation edge means the view's observer never
                        // ran, so restore the pre-move size here.
                        if let Some(size) = popup_size {
                            let _ = cx.update(|app| {
                                handle.update(app, |_view, window, _| window.resize(size))
                            });
                        }
                        log_event(
                            "WARN",
                            &format!(
                                "history: popup shown without activation after {}ms (attempts {activation_attempts})",
                                elapsed_ms()
                            ),
                        );
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
                    Some((handle, ready, size)) => {
                        window = Some(handle);
                        first_load = Some(ready);
                        popup_size = size;
                        activated = false;
                        opened_at = Some(Instant::now());
                        last_activation_attempt = None;
                        activation_attempts = 0;
                        open_generation = snapshot.generation;
                        dismissed_generation = None;
                        idle_since = None;
                        trimmed = false;
                        // Visibility is published on the activation edge (or
                        // the 3 s fallback), not at creation: the window is
                        // still hidden here.
                    }
                    None => {
                        // Do not retry this request every poll: the next click
                        // advances the generation and re-arms the open path.
                        dismissed_generation = Some(snapshot.generation);
                        shared.mark_hidden();
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
