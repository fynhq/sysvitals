//! Windows resident runtime for the history popup.

use super::*;

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

    pub(crate) fn mark_hidden(&self) {
        self.set_visible(false);
    }

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

#[cfg(target_os = "macos")]
pub(crate) fn watch_parent(cx: &mut App) {
    sysvitals_panel::watch_parent(cx, "history");
}

#[cfg(windows)]
pub(crate) fn trim_working_set() {
    unsafe {
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

// Release the GPU as soon as the popup closes; reattach before the next open to avoid a background VSync thread.
#[cfg(windows)]
async fn set_gpu_windowed(cx: &mut AsyncApp, windowed: bool) -> bool {
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
            current
        }
    }
}

#[cfg(windows)]
fn anchor_rect(rect: sysvitals_core::layout::Rect) -> AnchorRect {
    AnchorRect {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

#[cfg(windows)]
pub(crate) fn run_resident(cx: &mut App) {
    use sysvitals_core::resident::{
        Action as ResidentAction, Observation as ResidentObservation, Request as ResidentRequest,
        Resident,
    };

    let Some(state) = open_history_state() else {
        log_event(
            "WARN",
            "history: resident state mapping unavailable; exiting",
        );
        cx.quit();
        return;
    };
    let parent_pid = sysvitals_panel::parent_pid_arg();
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
        let started = Instant::now();
        let now = || started.elapsed();
        let mut machine = Resident::new(now());
        let mut window: Option<AnyWindowHandle> = None;
        let mut first_load: Option<Arc<AtomicBool>> = None;
        let mut popup_size: Option<gpui_kit::Size<Pixels>> = None;

        let open_shared = shared.clone();
        let open_at = move |cx: &mut App, anchor: Option<AnchorRect>, monitor: Option<DisplayId>| {
            apply_theme_and_accent(cx);
            open_history_window(cx, anchor, monitor, true, Some(open_shared.clone()))
        };

        let mut first_pass = true;
        loop {
            if first_pass {
                first_pass = false;
            } else {
                let poll_ms = if window.is_some() {
                    ACTIVE_POLL_MS
                } else {
                    IDLE_POLL_MS
                };
                cx.background_executor()
                    .timer(Duration::from_millis(poll_ms))
                    .await;
            }

            let snapshot = cx.update(|_app| shared.snapshot());
            if parent.exited() {
                log_event("INFO", "history: parent process gone; exiting");
                cx.update(|app| app.quit());
                break;
            }

            let request = ResidentRequest {
                desired_open: snapshot.desired_open != 0,
                generation: snapshot.generation,
                visible: snapshot.visible != 0,
                anchor: snapshot.anchor_is_valid().then_some(
                    sysvitals_core::layout::Rect::new(
                        snapshot.anchor[0],
                        snapshot.anchor[1],
                        snapshot.anchor[2],
                        snapshot.anchor[3],
                    ),
                ),
                monitor: (snapshot.monitor != 0).then_some(snapshot.monitor as u64),
            };

            'pump: loop {
                let windows = cx.update(|app| app.windows());
                if window.is_some_and(|handle| !windows.contains(&handle)) {
                    log_event("INFO", "history: window dismissed");
                    window = None;
                    first_load = None;
                    popup_size = None;
                }
                let observation = ResidentObservation {
                    window_present: window.is_some_and(|handle| windows.contains(&handle)),
                    any_window: !windows.is_empty(),
                    window_active: window.is_some_and(|handle| {
                        cx.update(|app| {
                            handle
                                .update(app, |_view, window, _| window.is_window_active())
                                .unwrap_or(false)
                        })
                    }),
                    first_load_ready: first_load
                        .as_ref()
                        .is_none_or(|flag| flag.load(Ordering::Acquire)),
                };
                let mut actions = Vec::new();
                machine.tick(now(), request, observation, &mut actions);
                if actions.is_empty() {
                    break 'pump;
                }
                let mut pump_again = false;
                for action in actions {
                    match action {
                        ResidentAction::Open {
                            anchor,
                            monitor,
                            generation,
                        } => {
                            let opened = cx.update(|app| {
                                open_at(app, anchor.map(anchor_rect), monitor.map(DisplayId::new))
                            });
                            match opened {
                                Some((handle, ready, size)) => {
                                    let size_px =
                                        size.map(|s| (f32::from(s.width), f32::from(s.height)));
                                    window = Some(handle);
                                    first_load = Some(ready);
                                    popup_size = size;
                                    machine.opened(now(), generation, anchor, monitor, size_px);
                                }
                                None => {
                                    window = None;
                                    first_load = None;
                                    popup_size = None;
                                    shared.mark_hidden();
                                    machine.open_failed(now(), generation);
                                    log_event("WARN", "history: resident window open failed");
                                }
                            }
                        }
                        ResidentAction::Reopen {
                            anchor,
                            monitor,
                            generation,
                        } => {
                            let activated = machine.is_activated();
                            if let Some(old) = window {
                                let removed = cx.update(|app| {
                                    old.update(app, |_, window, _| window.remove_window())
                                        .is_ok()
                                });
                                if !removed {
                                    log_event(
                                        "WARN",
                                        "history: remove_window failed before the move; continuing",
                                    );
                                }
                            }
                            window = None;
                            first_load = None;
                            popup_size = None;
                            log_event(
                                "INFO",
                                if activated {
                                    "history: indicator moved; reopening at the new anchor"
                                } else {
                                    "history: request changed the anchor while the popup was hidden; reopening"
                                },
                            );
                            let opened = cx.update(|app| {
                                open_at(app, anchor.map(anchor_rect), monitor.map(DisplayId::new))
                            });
                            match opened {
                                Some((handle, ready, size)) => {
                                    let size_px =
                                        size.map(|s| (f32::from(s.width), f32::from(s.height)));
                                    window = Some(handle);
                                    first_load = Some(ready);
                                    popup_size = size;
                                    machine.opened(now(), generation, anchor, monitor, size_px);
                                }
                                None => {
                                    shared.mark_hidden();
                                    machine.open_failed(now(), generation);
                                    log_event("WARN", "history: reopen at the new anchor failed");
                                }
                            }
                        }
                        ResidentAction::RemoveWindow => {
                            if let Some(handle) = window {
                                let removed = cx.update(|app| {
                                    handle
                                        .update(app, |_, window, _| window.remove_window())
                                        .is_ok()
                                });
                                if removed {
                                    log_event("INFO", "history: window closed by indicator");
                                    window = None;
                                    first_load = None;
                                    popup_size = None;
                                } else {
                                    log_event(
                                        "WARN",
                                        "history: remove_window failed; the hidden window may linger",
                                    );
                                }
                            }
                        }
                        ResidentAction::ActivateWindow => {
                            if let Some(handle) = window {
                                let _ = cx.update(|app| {
                                    handle
                                        .update(app, |_view, window, _| window.activate_window())
                                });
                            }
                        }
                        ResidentAction::Show => {
                            shared.set_visible(true);
                            log_event(
                                "INFO",
                                &format!(
                                    "history: popup shown in {}ms (activation attempts {})",
                                    elapsed_ms(),
                                    machine.activation_attempts()
                                ),
                            );
                        }
                        ResidentAction::ShowWithoutActivation => {
                            shared.set_visible(true);
                            log_event(
                                "WARN",
                                &format!(
                                    "history: popup shown without activation after {}ms (attempts {})",
                                    elapsed_ms(),
                                    machine.activation_attempts()
                                ),
                            );
                        }
                        ResidentAction::ResizeToStored => {
                            if let (Some(handle), Some(size)) = (window, popup_size) {
                                let _ = cx.update(|app| {
                                    handle
                                        .update(app, |_view, window, _| window.resize(size))
                                });
                            }
                        }
                        ResidentAction::Hide => shared.mark_hidden(),
                        ResidentAction::AttachGpu => {
                            let attached = set_gpu_windowed(cx, true).await;
                            machine.gpu_result(attached);
                            if attached {
                                pump_again = true;
                            } else {
                                machine.gpu_failed(now(), request.generation);
                                shared.mark_hidden();
                                log_event(
                                    "WARN",
                                    "history: DirectX device unavailable; not opening the popup",
                                );
                                break 'pump;
                            }
                        }
                        ResidentAction::ReleaseGpu => {
                            let attached = set_gpu_windowed(cx, false).await;
                            machine.gpu_result(attached);
                        }
                        ResidentAction::TrimWorkingSet => trim_working_set(),
                        ResidentAction::IdleExit { generation } => {
                            log_event(
                                "INFO",
                                &format!(
                                    "history: resident idle for {}s; exiting to release its heap",
                                    machine.idle_for(now()).as_secs()
                                ),
                            );
                            shared.mark_idle_exit(generation);
                            cx.update(|app| app.quit());
                            return;
                        }
                    }
                }
                if !pump_again {
                    break 'pump;
                }
            }
        }
    })
    .detach();
}
