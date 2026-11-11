//! resident: split out of the crate root (see `main.rs` for the module map).

use super::*;

// ------------------------------------------------------- resident mode (Windows)

/// How long the resident framework may stay without a window before it exits
/// entirely. The indicator relaunches it on the next click; staying warm
/// across a short gap keeps repeated opens instant, exiting after ten quiet
/// minutes releases the GPUI heap and the text system. The DirectX device is
/// released much earlier: every closed popup drops it immediately.
#[cfg(windows)]
const IDLE_EXIT: Duration = Duration::from_secs(10 * 60);

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

    /// Record the request generation this process has consumed, right before
    /// an idle exit. The indicator uses it to tell a click that raced the
    /// exit apart from a popup that dismissed itself earlier (its
    /// `desired_open` stays 1 by design).
    pub(crate) fn mark_idle_exit(&self, generation: u32) {
        self.set_visible(false);
        unsafe {
            sysvitals_core::HistoryState::write_closed_generation(self.state, generation);
        }
    }
}

// SAFETY: `state` points into a process-lifetime file mapping owned by the
// indicator. All access goes through the seqlock/volatile accessors in
// `sysvitals_core::HistoryState`, which are built for concurrent cross-process
// reads and writes, so sharing the handle across threads is sound.
#[cfg(windows)]
unsafe impl Send for HistoryShared {}
#[cfg(windows)]
unsafe impl Sync for HistoryShared {}

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
/// in `core::cli` (shared with the settings panel). Resident mode is
/// Windows-only; the macOS one-shot path uses the panel helper instead.
#[cfg(windows)]
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

/// Switch GPUI's platform between windowed (DirectX device + VSync thread
/// attached) and headless (both released). Returns the state that is believed
/// to be in effect now.
///
/// The resident process spends most of its life with no window. Keeping the
/// D3D11 device and its 60 Hz VSync thread alive for that idle time is pure
/// overhead — the device alone is the largest block of the resident's private
/// commit. Detaching after every closed popup and reattaching just before the
/// next window creates one is exactly what GPUI's own windowing-mode switch
/// does, so the renderer is recreated by `open_window` as usual.
#[cfg(windows)]
async fn set_gpu_windowed(cx: &mut AsyncApp, windowed: bool) -> bool {
    // Query the platform instead of tracking our own bit: a previous request
    // may have failed, and `request_windowing` reports a redundant switch as
    // an error. `graphical_environment()` is the same signal GPUI's own
    // headless example uses.
    let current = cx.update(|app| app.graphical_environment().is_some());
    if current == windowed {
        return current;
    }
    let request = if windowed {
        gpui_kit::WindowingRequest::Windowed(gpui_kit::GraphicalEnvironment::detect())
    } else {
        gpui_kit::WindowingRequest::Headless
    };
    match cx.update(|app| app.request_windowing(request)).await {
        Ok(()) => windowed,
        Err(error) => {
            log_event(
                "WARN",
                &format!(
                    "history: {} the DirectX device failed: {error}",
                    if windowed { "attaching" } else { "releasing" }
                ),
            );
            // The failed request left the previous state in effect.
            current
        }
    }
}

/// Resident loop used on Windows: keep the process warm with no window, open
/// a window only while the indicator asks for one, and trim the working set
/// while idle. The DirectX device is released whenever no window exists and
/// recreated on the next open; after ten idle minutes the process exits
/// entirely. The window is created hidden and shown after its first chart
/// load so the first visible frame already has data.
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
    let parent = match parent_pid.map(sysvitals_panel::ParentProcess::open) {
        Some(Ok(parent)) => parent,
        Some(Err(error)) => {
            log_event(
                "WARN",
                &format!("history: resident parent process unavailable: {error}; exiting"),
            );
            cx.quit();
            return;
        }
        None => {
            log_event(
                "WARN",
                "history: resident parent process unavailable; exiting",
            );
            cx.quit();
            return;
        }
    };
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
        // When the next working-set trim is due. `None` means "the 2 s first
        // trim after going idle". The idle start itself is never reset by a
        // trim, otherwise the ten-minute exit check could never fire.
        let mut next_trim: Option<Instant> = None;
        // Request generation that opened the current window. A blur/Esc
        // dismissal records it as dismissed so the same request is not
        // reopened while a newer click still gets through.
        let mut open_generation = 0u32;
        let mut dismissed_generation: Option<u32> = None;
        // The app starts windowed, so GPUI has already created the DirectX
        // device. When this resident was started without a pending click
        // (manual `--resident`, or a relaunch after an idle exit), release it
        // again right away. When the indicator launched us for a click,
        // `desired_open` is already 1 and the first loop pass opens the window
        // without paying a detach/attach cycle.
        let mut gpu_attached = true;
        if shared.snapshot().desired_open == 0 {
            gpu_attached = set_gpu_windowed(cx, false).await;
        }

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
            if parent.exited() {
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
                    // The device and the working set are released below, so
                    // the next routine trim is one 30 s beat away.
                    next_trim = Some(Instant::now() + Duration::from_secs(30));
                    shared.mark_hidden();
                    // Blur/Esc removed the window behind the loop's back, so
                    // GPUI itself may still hold the device and its VSync
                    // thread. Release both while the process sits idle.
                    gpu_attached = set_gpu_windowed(cx, false).await;
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
                    // Set only when this loop actually destroyed the window.
                    // A view that is already removing itself may still be
                    // alive for a frame; releasing the device then would
                    // break its teardown paint.
                    let mut removed = false;
                    if snapshot.visible != 0 || !activated {
                        log_event("INFO", "history: window closed by indicator");
                        removed = cx.update(|app| {
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
                    next_trim = Some(Instant::now() + Duration::from_secs(30));
                    // Only release the device once the window is really gone;
                    // a lingering hidden window still renders through it.
                    if removed {
                        gpu_attached = set_gpu_windowed(cx, false).await;
                    }
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
                        (snapshot.monitor != 0).then_some(DisplayId::new(snapshot.monitor as u64));
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
                            next_trim = None;
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
                        .is_none_or(|flag| flag.load(Ordering::Acquire));
                    let timed_out = opened_at.is_none_or(|opened| {
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
                // The idle resident has no DirectX device; every window needs
                // one. Recreate it before `open_window` builds its renderer.
                if !gpu_attached {
                    gpu_attached = set_gpu_windowed(cx, true).await;
                }
                if !gpu_attached {
                    log_event(
                        "WARN",
                        "history: DirectX device unavailable; not opening the popup",
                    );
                    dismissed_generation = Some(snapshot.generation);
                    shared.mark_hidden();
                    continue;
                }
                let anchor = snapshot.anchor_is_valid().then_some(AnchorRect {
                    left: snapshot.anchor[0],
                    top: snapshot.anchor[1],
                    right: snapshot.anchor[2],
                    bottom: snapshot.anchor[3],
                });
                let monitor =
                    (snapshot.monitor != 0).then_some(DisplayId::new(snapshot.monitor as u64));
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
                        next_trim = None;
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
                    next_trim = None;
                }
                let idle_for = idle_since.map_or(Duration::ZERO, |since| since.elapsed());
                if idle_for > IDLE_EXIT {
                    // A view that is removing itself may still own a window
                    // for a frame; never tear the device down underneath it.
                    if cx.update(|app| app.windows().is_empty()) {
                        log_event(
                            "INFO",
                            &format!(
                                "history: resident idle for {}s; exiting to release its heap",
                                idle_for.as_secs()
                            ),
                        );
                        // Tell the indicator which request this exit consumed;
                        // a click that raced us carries a newer generation and
                        // is relaunched instead of dropped.
                        shared.mark_idle_exit(snapshot.generation);
                        if gpu_attached {
                            gpu_attached = set_gpu_windowed(cx, false).await;
                        }
                        trim_working_set();
                        cx.update(|app| app.quit());
                        break;
                    }
                }
                // First trim 2 s after going idle, then every 30 s. Blur/Esc
                // may have removed the window without this loop seeing the
                // handle disappear, so also drop the idle DirectX device once
                // no window remains. The idle start is not reset here: the
                // ten-minute exit above depends on it growing monotonically.
                let trim_due = next_trim.map_or(idle_for > Duration::from_secs(2), |at| now >= at);
                if trim_due {
                    if gpu_attached && cx.update(|app| app.windows().is_empty()) {
                        gpu_attached = set_gpu_windowed(cx, false).await;
                    }
                    trim_working_set();
                    next_trim = Some(now + Duration::from_secs(30));
                }
            }
        }

    })
    .detach();
}
