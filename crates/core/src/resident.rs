//! Pure resident-popup lifecycle state machine.

use std::time::Duration;

use crate::layout::Rect;

// Idle exit at 10 min; first trim 2 s after close, then every 30 s.
pub const IDLE_EXIT: Duration = Duration::from_secs(10 * 60);

pub const FIRST_LOAD_TIMEOUT: Duration = Duration::from_millis(1_500);

// Activation: 3 attempts at 500 ms, then show unactivated after 3 s; a dismissed generation is never reopened.
pub const ACTIVATION_RETRY: Duration = Duration::from_millis(500);

pub const ACTIVATION_FALLBACK: Duration = Duration::from_secs(3);

pub const MAX_ACTIVATION_ATTEMPTS: u32 = 3;

pub const FIRST_TRIM: Duration = Duration::from_secs(2);

pub const TRIM_EVERY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Request {
    pub desired_open: bool,
    pub generation: u32,
    pub visible: bool,
    pub anchor: Option<Rect>,
    pub monitor: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    pub window_present: bool,
    pub any_window: bool,
    pub window_active: bool,
    pub first_load_ready: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Open {
        anchor: Option<Rect>,
        monitor: Option<u64>,
        generation: u32,
    },
    Reopen {
        anchor: Option<Rect>,
        monitor: Option<u64>,
        generation: u32,
    },
    RemoveWindow,
    ActivateWindow,
    Show,
    ShowWithoutActivation,
    ResizeToStored,
    Hide,
    AttachGpu,
    ReleaseGpu,
    TrimWorkingSet,
    IdleExit {
        generation: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct WindowMeta {
    generation: u32,
    anchor: Option<Rect>,
    monitor: Option<u64>,
    size: Option<(f32, f32)>,
    opened_at: Duration,
    activated: bool,
    closing: bool,
}

#[derive(Debug)]
pub struct Resident {
    window: Option<WindowMeta>,
    gpu_attached: bool,
    gpu_pending: bool,
    dismissed_generation: Option<u32>,
    pending_close_generation: Option<u32>,
    idle_since: Duration,
    next_trim: Option<Duration>,
    startup_release_attempted: bool,
    activation_attempts: u32,
    last_activation_attempt: Option<Duration>,
}

impl Resident {
    pub fn new(now: Duration) -> Self {
        Self {
            window: None,
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

    pub fn window_generation(&self) -> Option<u32> {
        self.window.map(|w| w.generation)
    }

    pub fn is_activated(&self) -> bool {
        self.window.is_some_and(|w| w.activated)
    }

    pub fn gpu_attached(&self) -> bool {
        self.gpu_attached
    }

    pub fn activation_attempts(&self) -> u32 {
        self.activation_attempts
    }

    pub fn idle_for(&self, now: Duration) -> Duration {
        now.saturating_sub(self.idle_since)
    }

    pub fn tick(&mut self, now: Duration, req: Request, obs: Observation, out: &mut Vec<Action>) {
        if let Some(meta) = self.window {
            if !obs.window_present {
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
                self.gpu_attached = false;
                out.push(Action::ReleaseGpu);
                return;
            }

            let mut meta = meta;
            if !req.desired_open {
                if req.visible || !meta.activated {
                    out.push(Action::RemoveWindow);
                }
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

            if Some(req.generation) != Some(meta.generation) {
                if let Some(anchor) = req.anchor {
                    let anchor_changed = Some(anchor) != meta.anchor || req.monitor != meta.monitor;
                    if !meta.activated && !anchor_changed {
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

        if !self.startup_release_attempted {
            self.startup_release_attempted = true;
            if self.gpu_attached {
                self.gpu_attached = false;
                out.push(Action::ReleaseGpu);
            }
        }
        let idle_for = now.saturating_sub(self.idle_since);
        if idle_for > IDLE_EXIT {
            // A view that is removing itself may still own a window for a frame; never tear the process down underneath it.
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

    pub fn open_failed(&mut self, now: Duration, generation: u32) {
        self.window = None;
        self.dismissed_generation = Some(generation);
        self.pending_close_generation = None;
        self.idle_since = now;
        self.next_trim = Some(now + TRIM_EVERY);
        self.activation_attempts = 0;
        self.last_activation_attempt = None;
    }

    pub fn gpu_result(&mut self, attached: bool) {
        self.gpu_pending = false;
        self.gpu_attached = attached;
    }

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
        run(&mut m, at(0), req(false, 1, None), obs(false));
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

        let actions = run(&mut m, at(200), req(true, 1, None), loaded);
        assert!(actions.is_empty());
        let actions = run(&mut m, at(520), req(true, 1, None), loaded);
        assert_eq!(actions, vec![Action::ActivateWindow]);
        let actions = run(&mut m, at(1_030), req(true, 1, None), loaded);
        assert_eq!(actions, vec![Action::ActivateWindow]);
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
        let actions = run(&mut m, at(400), req(true, 6, None), obs(false));
        assert!(matches!(actions.as_slice(), [Action::AttachGpu]));
    }

    #[test]
    fn indicator_close_removes_the_window_and_records_the_generation() {
        let mut m = Resident::new(at(0));
        run(&mut m, at(0), req(true, 9, None), obs(false));
        m.opened(at(0), 9, None, None, None);
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
        let actions = run(&mut m, at(250), req(true, 5, None), obs(false));
        assert_eq!(actions, vec![Action::ReleaseGpu]);
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
