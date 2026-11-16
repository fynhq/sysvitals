//! CPU sampling helper and smoothing.

use std::time::Duration;

// A >60 s tick gap means sleep/stall: re-prime CPU instead of smoothing across it, and report network as NaN.
pub const STALE_GAP_SECS: f64 = 60.0;

// Windows 11 24H2 (build 26100) switched Task Manager to time-based CPU; earlier builds use weighted utility.
#[cfg(any(windows, test))]
pub(crate) const TM_TIME_BASED_BUILD: u32 = 26100;

#[cfg(any(windows, test))]
pub(crate) fn task_manager_is_time_based(build: u32) -> bool {
    build >= TM_TIME_BASED_BUILD
}

#[derive(Debug, Clone)]
pub struct CpuSampler {
    alpha: f64,
    last: Option<f64>,
}

impl Default for CpuSampler {
    fn default() -> Self {
        CpuSampler {
            alpha: 0.3,
            last: None,
        }
    }
}

impl CpuSampler {
    pub fn new(alpha: f64) -> Self {
        CpuSampler {
            alpha: if alpha.is_finite() {
                alpha.clamp(0.0, 1.0)
            } else {
                0.3
            },
            last: None,
        }
    }

    pub fn update(&mut self, raw: f64, elapsed: Option<Duration>) -> f64 {
        if elapsed.is_none() {
            return self.last.unwrap_or(f64::NAN);
        }
        if !raw.is_finite() {
            return self.last.unwrap_or(f64::NAN);
        }
        let raw = raw.clamp(0.0, 100.0);
        if elapsed.map(|d| d.as_secs_f64() > STALE_GAP_SECS) == Some(true) {
            self.last = Some(raw);
            return raw;
        }
        match self.last {
            None => {
                self.last = Some(raw);
                raw
            }
            Some(prev) => {
                let smoothed = if self.alpha <= 0.0 {
                    raw
                } else {
                    (1.0 - self.alpha) * raw + self.alpha * prev
                };
                self.last = Some(smoothed);
                smoothed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_tick_has_no_rate_and_does_not_prime_the_ema() {
        let mut s = CpuSampler::new(0.3);
        assert!(s.update(50.0, None).is_nan());
        assert_eq!(s.update(100.0, Some(Duration::from_millis(2000))), 100.0);
        let v = s.update(50.0, Some(Duration::from_millis(2000)));
        assert!((v - 65.0).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn smoothing_weights_history_by_alpha() {
        let mut s = CpuSampler::new(0.3);
        s.update(50.0, Some(Duration::from_millis(2000)));
        let v = s.update(100.0, Some(Duration::from_millis(2000)));
        assert!((v - 85.0).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn alpha_zero_disables_smoothing_and_tracks_raw() {
        let mut s = CpuSampler::new(0.0);
        s.update(50.0, Some(Duration::from_millis(2000)));
        let v = s.update(100.0, Some(Duration::from_millis(2000)));
        assert_eq!(v, 100.0);
    }

    #[test]
    fn alpha_one_freezes_at_previous() {
        let mut s = CpuSampler::new(1.0);
        s.update(50.0, Some(Duration::from_millis(2000)));
        let v = s.update(100.0, Some(Duration::from_millis(2000)));
        assert_eq!(v, 50.0);
    }

    #[test]
    fn raw_is_clamped_to_percent_range() {
        let mut s = CpuSampler::new(0.0);
        assert_eq!(s.update(140.0, Some(Duration::from_millis(1000))), 100.0);
        assert_eq!(s.update(-10.0, Some(Duration::from_millis(1000))), 0.0);
    }

    #[test]
    fn non_finite_alpha_falls_back_to_default() {
        let mut s = CpuSampler::new(f64::NAN);
        s.update(50.0, Some(Duration::from_millis(2000)));
        let v = s.update(100.0, Some(Duration::from_millis(2000)));
        assert!((v - 85.0).abs() < 1e-9, "NaN alpha poisoned the EMA: {v}");
    }

    #[test]
    fn non_finite_raw_does_not_poison_the_ema() {
        let mut s = CpuSampler::new(0.3);
        assert!(s
            .update(f64::NAN, Some(Duration::from_millis(1000)))
            .is_nan());
        s.update(50.0, Some(Duration::from_millis(2000)));
        let bad = s.update(f64::INFINITY, Some(Duration::from_millis(1000)));
        assert_eq!(bad, 50.0);
        let v = s.update(100.0, Some(Duration::from_millis(2000)));
        assert!((v - 85.0).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn stale_gap_reprimes_instead_of_blending() {
        let mut s = CpuSampler::new(0.3);
        s.update(50.0, Some(Duration::from_millis(2000)));
        let big = Duration::from_secs_f64(STALE_GAP_SECS + 10.0);
        let v = s.update(100.0, Some(big));
        assert_eq!(v, 100.0, "stale gap must re-prime to raw, not blend");
    }

    #[test]
    fn task_manager_formula_switches_at_24h2_build() {
        assert!(!task_manager_is_time_based(19045));
        assert!(!task_manager_is_time_based(22631));

        assert!(task_manager_is_time_based(26100));
        assert!(task_manager_is_time_based(26200));
    }
}
