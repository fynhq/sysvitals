//! Pure lifecycle state machine for the Windows resident history process.
//!
//! The resident process is a GPUI child that lives without a window, opens a
//! popup only while the indicator's shared-memory block asks for one, and
//! exits after ten idle minutes. The interesting part is not the GPUI calls
//! but *when* to make them: open/close/reopen decisions, the activation retry
//! schedule, the 3 s fallback that shows a window the foreground lock keeps
//! refusing, the idle-exit handshake, and the GPU detach/re-attach cycle.
//!
//! Those decisions used to live inline in the resident loop, mixed with
//! `App`/window calls, so none of them could run in the Linux unit-test gate.
//! This module keeps the same policy as plain data: [`Resident::tick`] takes
//! a [`Request`] (the indicator-owned block), an [`Observation`] (what the
//! platform reports), and appends [`Action`] values for the caller to execute.
//! The caller owns the window handle, the GPU switch and the shared-memory
//! writes; everything here is deterministic and time-injected.
//!
//! Behavior contract (kept identical to the previous inline loop):
//! - A request generation that already dismissed itself is not reopened.
//! - A changed anchor while the window is still hidden rebuilds the window;
//!   the same anchor only re-arms the generation.
//! - Activation is attempted at most three times, one every 500 ms, once the
//!   first chart load is ready or 1.5 s have passed; after 3 s the popup is
//!   shown without activation (click-away stays unarmed).
//! - A closed popup releases the GPU device immediately; the idle process
//!   exits after ten quiet minutes and records the consumed generation.
//! - The working set is trimmed 2 s after going idle and then every 30 s.

use std::time::Duration;

use crate::layout::Rect;

/// How long the resident may stay without a window before it exits entirely.
/// The indicator relaunches it on the next click; staying warm across a short
/// gap keeps repeated opens instant.
pub const IDLE_EXIT: Duration = Duration::from_secs(10 * 60);

/// How long the hidden popup waits for its first chart load before activation
/// is attempted anyway.
pub const FIRST_LOAD_TIMEOUT: Duration = Duration::from_millis(1_500);

/// Spacing between activation attempts. GPUI's Windows `activate()` injects a
/// synthetic Alt press; retrying every poll would spam the foreground app.
pub const ACTIVATION_RETRY: Duration = Duration::from_millis(500);

/// Give up on activation after this long and show the window anyway.
pub const ACTIVATION_FALLBACK: Duration = Duration::from_secs(3);

/// Maximum activation attempts before the fallback takes over.
pub const MAX_ACTIVATION_ATTEMPTS: u32 = 3;

/// First working-set trim after the window goes away.
pub const FIRST_TRIM: Duration = Duration::from_secs(2);

/// Working-set trim cadence while idle.
pub const TRIM_EVERY: Duration = Duration::from_secs(30);

/// One snapshot of the command block, already decoded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Request {
    /// The indicator wants the popup open.
    pub desired_open: bool,
    /// Request sequence. Doubles as the anchor seqlock; a newer value is a
    /// new click, a moved indicator or a toggle.
    pub generation: u32,
    /// The history process's own status flag: nonzero while a window exists.
    pub visible: bool,
    /// Non-degenerate indicator rectangle, if the block carried one.
    pub anchor: Option<Rect>,
    /// Native display handle hosting the indicator, if known.
    pub monitor: Option<u64>,
}

/// What the platform reports for this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    /// The window this process opened is still in the app's window list.
    pub window_present: bool,
    /// Any window is still alive. During teardown a dismissed view can own a
    /// window for a frame after our handle is gone; opening a new one then
    /// would race the removal.
    pub any_window: bool,
    /// The popup is the foreground window.
    pub window_active: bool,
    /// The first chart load has finished (or no load flag was installed).
    pub first_load_ready: bool,
}

/// One side effect the caller must perform, in order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    /// Create the hidden popup, then report back through
    /// [`Resident::opened`] or [`Resident::open_failed`].
    Open {
        anchor: Option<Rect>,
        monitor: Option<u64>,
        generation: u32,
    },
    /// Remove the current window and create a new hidden one at the request's
    /// anchor. Used when the indicator moved.
    Reopen {
        anchor: Option<Rect>,
        monitor: Option<u64>,
        generation: u32,
    },
    /// Remove the window because the indicator asked to close it.
    RemoveWindow,
    /// Ask the platform to bring the popup to the foreground.
    ActivateWindow,
    /// Publish `visible = 1`.
    Show,
    /// Publish `visible = 1` without an activation edge; the caller must also
    /// restore the stored popup size because the view's activation observer
    /// never ran.
    ShowWithoutActivation,
    /// Restore the pre-move popup size (only after a fallback show).
    ResizeToStored,
    /// Publish `visible = 0`.
    Hide,
    /// Re-attach the GPU (`request_windowing(Windowed)`).
    AttachGpu,
    /// Release the GPU (`request_windowing(Headless)`).
    ReleaseGpu,
    /// Trim the process working set.
    TrimWorkingSet,
    /// Record the consumed generation and quit; the indicator relaunches a
    /// newer request.
    IdleExit { generation: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct WindowMeta {
    generation: u32,
    anchor: Option<Rect>,
    monitor: Option<u64>,
    size: Option<(f32, f32)>,
    opened_at: Duration,
    activated: bool,
    /// The close request has already emitted its one-time observations
    /// (hide/trim); later ticks only retry the removal until the window goes.
    closing: bool,
}

/// Deterministic resident policy. Construct once per process, call
/// [`Resident::tick`] on every loop pass, and feed the platform results back
/// through the `on_*` methods.
#[derive(Debug)]
pub struct Resident {
    window: Option<WindowMeta>,
    gpu_attached: bool,
    /// An `AttachGpu` action is in flight; do not open or re-issue it.
    gpu_pending: bool,
    /// Request generation that dismissed a popup. The same request must not
    /// reopen it; a newer click advances the generation and gets through.
    dismissed_generation: Option<u32>,
    /// Generation of an indicator-requested close, used when the window is
    /// actually gone (the indicator may have moved on by then).
    pending_close_generation: Option<u32>,
    idle_since: Duration,
    next_trim: Option<Duration>,
    /// The startup release has been requested. A failed release is retried by
    /// the trim cadence, not on every idle poll.
    startup_release_attempted: bool,
    activation_attempts: u32,
    last_activation_attempt: Option<Duration>,
}

impl Resident {
    /// `now` is the process' monotonic clock at construction.
    pub fn new(now: Duration) -> Self {
        Self {
            window: None,
            // The app starts windowed, so GPUI has already created the
            // DirectX device. The first idle tick releases it.
            gpu_attached: true,
            gpu_pending: false,
            dismissed_generation: None,
            pending_close_generation: None,
            idle_since: now,
            next_trim: None,
            startup_release_attempted: false,
            activation_attempts: 0,
            last_activation_attempt: None,
        }
    }

    /// Generation of the currently open window, if any.
    pub fn window_generation(&self) -> Option<u32> {
        self.window.map(|w| w.generation)
    }

    /// Whether the current window has been activated (and `visible` published).
    pub fn is_activated(&self) -> bool {
        self.window.is_some_and(|w| w.activated)
    }

    /// Whether the GPU device is believed to be attached.
    pub fn gpu_attached(&self) -> bool {
        self.gpu_attached
    }

    /// Activation attempts spent on the current window (diagnostics).
    pub fn activation_attempts(&self) -> u32 {
        self.activation_attempts
    }

    /// How long the process has been idle at `now` (diagnostics).
    pub fn idle_for(&self, now: Duration) -> Duration {
        now.saturating_sub(self.idle_since)
    }

    /// Consume one loop pass. Appends zero or more actions to `out`.
    pub fn tick(&mut self, now: Duration, req: Request, obs: Observation, out: &mut Vec<Action>) {
        if let Some(meta) = self.window {
            if !obs.window_present {
                // The window went away behind our back (blur/Esc, a failed
                // activation, or a removal we asked for).
                self.window = None;
                let generation = self
                    .pending_close_generation
                    .take()
                    .unwrap_or(meta.generation);
                self.dismissed_generation = Some(generation);
                self.idle_since = now;
                self.next_trim = Some(now + TRIM_EVERY);
                self.activation_attempts = 0;
                self.last_activation_attempt = None;
                if !meta.closing {
                    out.push(Action::Hide);
                    out.push(Action::TrimWorkingSet);
                }
                // The device is only released once the window is really gone;
                // a lingering hidden window still renders through it.
                self.gpu_attached = false;
                out.push(Action::ReleaseGpu);
                return;
            }

            let mut meta = meta;
            if !req.desired_open {
                if req.visible || !meta.activated {
                    // A visible window, or one still waiting for its first
                    // activation, was asked to close by the indicator.
                    out.push(Action::RemoveWindow);
                }
                // A view that dismissed itself already published `visible=0`
                // and is removing its own window; calling remove again could
                // double-remove it. The window-gone branch finishes cleanup.
                if !meta.closing {
                    meta.closing = true;
                    self.pending_close_generation = Some(req.generation);
                    self.dismissed_generation = Some(req.generation);
                    self.idle_since = now;
                    self.next_trim = Some(now + TRIM_EVERY);
                    out.push(Action::Hide);
                    out.push(Action::TrimWorkingSet);
                }
                self.window = Some(meta);
                return;
            }

            // A new request arrived while the window exists: the indicator
            // was clicked on another taskbar/monitor, or a second click
            // arrived while the hidden window was still loading.
            if Some(req.generation) != Some(meta.generation) {
                if let Some(anchor) = req.anchor {
                    let anchor_changed = Some(anchor) != meta.anchor || req.monitor != meta.monitor;
                    if !meta.activated && !anchor_changed {
                        // Same anchor while still hidden: only re-arm the
                        // generation; the pending activation will show it.
                        meta.generation = req.generation;
                        self.window = Some(meta);
                        self.dismissed_generation = None;
                        return;
                    }
                    out.push(Action::Reopen {
                        anchor: Some(anchor),
                        monitor: req.monitor,
                        generation: req.generation,
                    });
                    return;
                }
                // No valid anchor: ignore, like the previous loop did.
            }

            if !meta.activated {
                if self.activation_attempts > 0 && obs.window_active {
                    meta.activated = true;
                    self.window = Some(meta);
                    out.push(Action::Show);
                    return;
                }
                let ready = obs.first_load_ready;
                let timed_out = now.saturating_sub(meta.opened_at) > FIRST_LOAD_TIMEOUT;
                let retry_due = self.activation_attempts < MAX_ACTIVATION_ATTEMPTS
                    && self
                        .last_activation_attempt
                        .is_none_or(|last| now.saturating_sub(last) > ACTIVATION_RETRY);
                if (ready || timed_out) && retry_due {
                    self.activation_attempts += 1;
                    self.last_activation_attempt = Some(now);
                    self.window = Some(meta);
                    out.push(Action::ActivateWindow);
                    return;
                }
                if now.saturating_sub(meta.opened_at) > ACTIVATION_FALLBACK {
                    // The foreground lock kept refusing activation. Show
                    // anyway so the chart is usable; Esc or an indicator
                    // click still closes it, but click-away cannot be armed.
                    meta.activated = true;
                    let has_size = meta.size.is_some();
                    self.window = Some(meta);
                    out.push(Action::ShowWithoutActivation);
                    if has_size {
                        out.push(Action::ResizeToStored);
                    }
                    return;
                }
            }
            self.window = Some(meta);
            return;
        }

        // No window from here on.
        if req.desired_open && Some(req.generation) != self.dismissed_generation {
            if !obs.any_window {
                if !self.gpu_attached {
                    if self.gpu_pending {
                        return;
                    }
                    self.gpu_pending = true;
                    out.push(Action::AttachGpu);
                    return;
                }
                out.push(Action::Open {
                    anchor: req.anchor,
                    monitor: req.monitor,
                    generation: req.generation,
                });
            }
            return;
        }

        // Idle: release the startup device, exit after ten quiet minutes, and
        // trim the working set on a slowing cadence.
        if !self.startup_release_attempted {
            self.startup_release_attempted = true;
            if self.gpu_attached {
                // The app starts windowed; a resident launched without a
                // pending click releases the device right away.
                self.gpu_attached = false;
                out.push(Action::ReleaseGpu);
            }
        }
        let idle_for = now.saturating_sub(self.idle_since);
        if idle_for > IDLE_EXIT {
            // A view that is removing itself may still own a window for a
            // frame; never tear the process down underneath it.
            if !obs.any_window {
                out.push(Action::TrimWorkingSet);
                out.push(Action::IdleExit {
                    generation: req.generation,
                });
            }
            return;
        }
        let trim_due = self.next_trim.map_or(idle_for > FIRST_TRIM, |at| now >= at);
        if trim_due && !obs.any_window {
            if self.gpu_attached {
                self.gpu_attached = false;
                out.push(Action::ReleaseGpu);
            }
            out.push(Action::TrimWorkingSet);
            self.next_trim = Some(now + TRIM_EVERY);
        }
    }

    /// Report a successful window creation.
    pub fn opened(
        &mut self,
        now: Duration,
        generation: u32,
        anchor: Option<Rect>,
        monitor: Option<u64>,
        size: Option<(f32, f32)>,
    ) {
        self.window = Some(WindowMeta {
            generation,
            anchor,
            monitor,
            size,
            opened_at: now,
            activated: false,
            closing: false,
        });
        self.dismissed_generation = None;
        self.pending_close_generation = None;
        self.activation_attempts = 0;
        self.last_activation_attempt = None;
        self.next_trim = None;
    }

    /// Report a failed window creation. The same request must not be retried
    /// on every poll; the next click advances the generation.
    pub fn open_failed(&mut self, now: Duration, generation: u32) {
        self.window = None;
        self.dismissed_generation = Some(generation);
        self.pending_close_generation = None;
        self.idle_since = now;
        self.next_trim = Some(now + TRIM_EVERY);
        self.activation_attempts = 0;
        self.last_activation_attempt = None;
    }

    /// Report the result of an `AttachGpu` action. A failure dismisses the
    /// request that needed the device; the caller also publishes `Hide`.
    pub fn gpu_result(&mut self, attached: bool) {
        self.gpu_pending = false;
        self.gpu_attached = attached;
    }

    /// Record an attach failure for `generation` so the request is not
    /// retried every poll.
    pub fn gpu_failed(&mut self, now: Duration, generation: u32) {
        self.gpu_result(false);
        self.dismissed_generation = Some(generation);
        self.idle_since = now;
        self.next_trim = Some(now + TRIM_EVERY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    fn rect(left: f32, top: f32) -> Rect {
        Rect::new(left, top, left + 120.0, top + 24.0)
    }

    fn req(desired_open: bool, generation: u32, anchor: Option<Rect>) -> Request {
        Request {
            desired_open,
            generation,
            visible: false,
            anchor,
            monitor: None,
        }
    }

    fn obs(window_present: bool) -> Observation {
        Observation {
            window_present,
            any_window: window_present,
            window_active: false,
            first_load_ready: true,
        }
    }

    fn run(m: &mut Resident, now: Duration, r: Request, o: Observation) -> Vec<Action> {
        let mut out = Vec::new();
        m.tick(now, r, o, &mut out);
        out
    }

    #[test]
    fn startup_without_a_click_releases_the_device() {
        let mut m = Resident::new(at(0));
        let actions = run(&mut m, at(0), req(false, 0, None), obs(false));
        assert_eq!(actions, vec![Action::ReleaseGpu]);
        assert!(!m.gpu_attached());
    }

    #[test]
    fn startup_with_a_pending_click_opens_without_a_device_cycle() {
        let mut m = Resident::new(at(0));
        let actions = run(
            &mut m,
            at(0),
            req(true, 7, Some(rect(0.0, 0.0))),
            obs(false),
        );
        assert_eq!(
            actions,
            vec![Action::Open {
                anchor: Some(rect(0.0, 0.0)),
                monitor: None,
                generation: 7,
            }]
        );
    }

    #[test]
    fn idle_startup_attaches_the_device_before_opening() {
        let mut m = Resident::new(at(0));
        // The idle tick released the device first.
        run(&mut m, at(0), req(false, 1, None), obs(false));
        // A click arrives while idle.
        let actions = run(
            &mut m,
            at(250),
            req(true, 2, Some(rect(5.0, 5.0))),
            obs(false),
        );
        assert_eq!(actions, vec![Action::AttachGpu]);
        m.gpu_result(true);
        let actions = run(
            &mut m,
            at(260),
            req(true, 2, Some(rect(5.0, 5.0))),
            obs(false),
        );
        assert!(matches!(
            actions.as_slice(),
            [Action::Open { generation: 2, .. }]
        ));
    }

    #[test]
    fn a_failed_attach_dismisses_the_request() {
        let mut m = Resident::new(at(0));
        run(&mut m, at(0), req(false, 1, None), obs(false));
        let actions = run(&mut m, at(100), req(true, 2, None), obs(false));
        assert_eq!(actions, vec![Action::AttachGpu]);
        m.gpu_failed(at(120), 2);
        let actions = run(&mut m, at(250), req(true, 2, None), obs(false));
        assert!(actions.is_empty(), "the failed request must not retry");
        // A newer click gets through.
        let actions = run(&mut m, at(500), req(true, 3, None), obs(false));
        assert!(matches!(actions.as_slice(), [Action::AttachGpu]));
    }

    #[test]
    fn activation_waits_for_the_first_load_then_retries_every_500ms() {
        let mut m = Resident::new(at(0));
        run(&mut m, at(0), req(true, 1, None), obs(false));
        m.opened(at(0), 1, None, None, Some((680.0, 390.0)));
        let loaded = Observation {
            first_load_ready: true,
            ..obs(true)
        };
        let actions = run(&mut m, at(10), req(true, 1, None), loaded);
        assert_eq!(actions, vec![Action::ActivateWindow]);

        // Too early for the next attempt.
        let actions = run(&mut m, at(200), req(true, 1, None), loaded);
        assert!(actions.is_empty());
        // 500 ms after the first attempt.
        let actions = run(&mut m, at(520), req(true, 1, None), loaded);
        assert_eq!(actions, vec![Action::ActivateWindow]);
        // Third attempt.
        let actions = run(&mut m, at(1_030), req(true, 1, None), loaded);
        assert_eq!(actions, vec![Action::ActivateWindow]);
        // Cap reached; the 3 s fallback shows it without activation.
        let actions = run(&mut m, at(3_100), req(true, 1, None), loaded);
        assert_eq!(
            actions,
            vec![Action::ShowWithoutActivation, Action::ResizeToStored]
        );
        assert!(m.is_activated());
    }

    #[test]
    fn an_activation_edge_publishes_visible() {
        let mut m = Resident::new(at(0));
        run(&mut m, at(0), req(true, 1, None), obs(false));
        m.opened(at(0), 1, None, None, None);
        run(
            &mut m,
            at(10),
            req(true, 1, None),
            Observation {
                first_load_ready: true,
                ..obs(true)
            },
        );
        let actions = run(
            &mut m,
            at(110),
            req(true, 1, None),
            Observation {
                window_active: true,
                ..obs(true)
            },
        );
        assert_eq!(actions, vec![Action::Show]);
        assert!(m.is_activated());
    }

    #[test]
    fn load_timeout_does_not_wait_forever_for_the_first_frame() {
        let mut m = Resident::new(at(0));
        run(&mut m, at(0), req(true, 1, None), obs(false));
        m.opened(at(0), 1, None, None, None);
        let not_ready = Observation {
            first_load_ready: false,
            ..obs(true)
        };
        let actions = run(&mut m, at(1_400), req(true, 1, None), not_ready);
        assert!(actions.is_empty(), "1.4 s is still inside the timeout");
        let actions = run(&mut m, at(1_600), req(true, 1, None), not_ready);
        assert_eq!(actions, vec![Action::ActivateWindow]);
    }

    #[test]
    fn a_self_dismissed_window_is_not_reopened_for_the_same_request() {
        let mut m = Resident::new(at(0));
        run(&mut m, at(0), req(true, 5, None), obs(false));
        m.opened(at(0), 5, None, None, None);
        // Blur/Esc removed the window without an indicator close.
        let actions = run(&mut m, at(100), req(true, 5, None), obs(false));
        assert_eq!(
            actions,
            vec![Action::Hide, Action::TrimWorkingSet, Action::ReleaseGpu]
        );
        let actions = run(&mut m, at(250), req(true, 5, None), obs(false));
        assert!(
            actions.is_empty(),
            "the consumed generation must stay closed"
        );
        // The next click opens again.
        let actions = run(&mut m, at(400), req(true, 6, None), obs(false));
        assert!(matches!(actions.as_slice(), [Action::AttachGpu]));
    }

    #[test]
    fn indicator_close_removes_the_window_and_records_the_generation() {
        let mut m = Resident::new(at(0));
        run(&mut m, at(0), req(true, 9, None), obs(false));
        m.opened(at(0), 9, None, None, None);
        // Make it visible first.
        run(
            &mut m,
            at(10),
            req(true, 9, None),
            Observation {
                first_load_ready: true,
                ..obs(true)
            },
        );
        run(
            &mut m,
            at(100),
            req(true, 9, None),
            Observation {
                window_active: true,
                ..obs(true)
            },
        );
        let close = Request {
            visible: true,
            ..req(false, 10, None)
        };
        let actions = run(&mut m, at(200), close, obs(true));
        assert_eq!(
            actions,
            vec![Action::RemoveWindow, Action::Hide, Action::TrimWorkingSet]
        );
        let actions = run(&mut m, at(300), req(false, 10, None), obs(false));
        assert_eq!(actions, vec![Action::ReleaseGpu]);
        // The indicator's request generation is dismissed even though the
        // window was opened by generation 9.
        let actions = run(&mut m, at(400), req(true, 10, None), obs(false));
        assert!(actions.is_empty());
    }

    #[test]
    fn a_self_closing_view_is_not_removed_twice() {
        let mut m = Resident::new(at(0));
        run(&mut m, at(0), req(true, 3, None), obs(false));
        m.opened(at(0), 3, None, None, None);
        run(
            &mut m,
            at(10),
            req(true, 3, None),
            Observation {
                first_load_ready: true,
                ..obs(true)
            },
        );
        run(
            &mut m,
            at(100),
            req(true, 3, None),
            Observation {
                window_active: true,
                ..obs(true)
            },
        );
        // visible already 0 and activated: the view is removing itself.
        let actions = run(
            &mut m,
            at(200),
            Request {
                desired_open: false,
                visible: false,
                ..req(false, 4, None)
            },
            obs(true),
        );
        assert_eq!(actions, vec![Action::Hide, Action::TrimWorkingSet]);
    }

    #[test]
    fn reopening_at_the_same_anchor_only_rearms_the_generation() {
        let mut m = Resident::new(at(0));
        let anchor = rect(10.0, 10.0);
        run(&mut m, at(0), req(true, 1, Some(anchor)), obs(false));
        m.opened(at(0), 1, Some(anchor), None, None);
        let actions = run(&mut m, at(100), req(true, 2, Some(anchor)), obs(true));
        assert!(actions.is_empty());
        assert_eq!(m.window_generation(), Some(2));
    }

    #[test]
    fn reopening_at_a_new_anchor_rebuilds_the_hidden_window() {
        let mut m = Resident::new(at(0));
        let anchor = rect(10.0, 10.0);
        run(&mut m, at(0), req(true, 1, Some(anchor)), obs(false));
        m.opened(at(0), 1, Some(anchor), Some(11), None);
        let moved = rect(400.0, 10.0);
        let request = Request {
            monitor: Some(11),
            ..req(true, 2, Some(moved))
        };
        let actions = run(&mut m, at(100), request, obs(true));
        assert_eq!(
            actions,
            vec![Action::Reopen {
                anchor: Some(moved),
                monitor: Some(11),
                generation: 2,
            }]
        );
    }

    #[test]
    fn a_moved_indicator_reopens_even_after_activation() {
        let mut m = Resident::new(at(0));
        let anchor = rect(10.0, 10.0);
        run(&mut m, at(0), req(true, 1, Some(anchor)), obs(false));
        m.opened(at(0), 1, Some(anchor), None, None);
        run(
            &mut m,
            at(10),
            req(true, 1, Some(anchor)),
            Observation {
                first_load_ready: true,
                ..obs(true)
            },
        );
        run(
            &mut m,
            at(100),
            req(true, 1, Some(anchor)),
            Observation {
                window_active: true,
                ..obs(true)
            },
        );
        let moved = rect(800.0, 40.0);
        let actions = run(&mut m, at(200), req(true, 2, Some(moved)), obs(true));
        assert!(matches!(actions.as_slice(), [Action::Reopen { .. }]));
    }

    #[test]
    fn a_new_request_does_not_open_while_a_view_is_still_tearing_down() {
        let mut m = Resident::new(at(0));
        run(&mut m, at(0), req(false, 1, None), obs(false));
        let observation = Observation {
            window_present: false,
            any_window: true,
            ..obs(false)
        };
        let actions = run(&mut m, at(100), req(true, 2, None), observation);
        assert!(actions.is_empty(), "wait for the old window to disappear");
    }

    #[test]
    fn idle_exit_records_the_generation_after_ten_minutes() {
        let mut m = Resident::new(at(0));
        run(&mut m, at(0), req(false, 1, None), obs(false));
        // Keep the trim cadence fed so the idle exit check is what fires.
        run(&mut m, at(590_000), req(false, 1, None), obs(false));
        let actions = run(&mut m, at(599_000), req(false, 1, None), obs(false));
        assert!(actions.is_empty());
        let actions = run(&mut m, at(601_000), req(false, 4, None), obs(false));
        assert_eq!(
            actions,
            vec![Action::TrimWorkingSet, Action::IdleExit { generation: 4 }]
        );
    }

    #[test]
    fn idle_exit_waits_for_a_tearing_down_window() {
        let mut m = Resident::new(at(0));
        run(&mut m, at(0), req(false, 1, None), obs(false));
        let observation = Observation {
            window_present: false,
            any_window: true,
            ..obs(false)
        };
        let actions = run(&mut m, at(601_000), req(false, 1, None), observation);
        assert!(actions.is_empty());
    }

    #[test]
    fn trim_cadence_is_two_seconds_then_every_thirty() {
        let mut m = Resident::new(at(0));
        run(&mut m, at(0), req(false, 1, None), obs(false));
        let actions = run(&mut m, at(2_500), req(false, 1, None), obs(false));
        assert_eq!(actions, vec![Action::TrimWorkingSet]);
        let actions = run(&mut m, at(20_000), req(false, 1, None), obs(false));
        assert!(actions.is_empty());
        let actions = run(&mut m, at(32_600), req(false, 1, None), obs(false));
        assert_eq!(actions, vec![Action::TrimWorkingSet]);
    }

    #[test]
    fn a_failed_open_is_not_retried_until_the_next_click() {
        let mut m = Resident::new(at(0));
        run(&mut m, at(0), req(true, 5, None), obs(false));
        m.open_failed(at(50), 5);
        // The first idle pass releases the startup device …
        let actions = run(&mut m, at(250), req(true, 5, None), obs(false));
        assert_eq!(actions, vec![Action::ReleaseGpu]);
        // … and the failed request itself is not retried.
        let actions = run(&mut m, at(500), req(true, 5, None), obs(false));
        assert!(actions.is_empty());
        let actions = run(&mut m, at(750), req(true, 6, None), obs(false));
        assert_eq!(
            actions,
            vec![Action::AttachGpu],
            "the released device must come back first"
        );
        m.gpu_result(true);
        let actions = run(&mut m, at(760), req(true, 6, None), obs(false));
        assert!(matches!(
            actions.as_slice(),
            [Action::Open { generation: 6, .. }]
        ));
    }

    #[test]
    fn the_fallback_restores_the_size_only_when_one_was_stored() {
        let loaded = Observation {
            first_load_ready: true,
            ..obs(true)
        };

        let mut m = Resident::new(at(0));
        run(&mut m, at(0), req(true, 1, None), obs(false));
        m.opened(at(0), 1, None, None, None);
        for t in [10, 520, 1_030] {
            assert_eq!(
                run(&mut m, at(t), req(true, 1, None), loaded),
                vec![Action::ActivateWindow]
            );
        }
        assert_eq!(
            run(&mut m, at(3_100), req(true, 1, None), loaded),
            vec![Action::ShowWithoutActivation],
            "no stored size to restore"
        );

        let mut m = Resident::new(at(0));
        run(&mut m, at(0), req(true, 1, None), obs(false));
        m.opened(at(0), 1, None, None, Some((640.0, 380.0)));
        for t in [10, 520, 1_030] {
            assert_eq!(
                run(&mut m, at(t), req(true, 1, None), loaded),
                vec![Action::ActivateWindow]
            );
        }
        assert_eq!(
            run(&mut m, at(3_100), req(true, 1, None), loaded),
            vec![Action::ShowWithoutActivation, Action::ResizeToStored]
        );
    }
}
