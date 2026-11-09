//! CPU sampling helper.
//!
//! `sysinfo`'s `System::cpu_usage()` already returns a time-normalized whole
//! machine percentage (it diffs busy/idle internally across refreshes), so this
//! layer does not recompute a rate — it only dampens frame-to-frame jitter with
//! exponential smoothing and clamps to a sane range. The first real value is
//! passed through un-smoothed to avoid the classic EMA pull toward zero.
//!
//! Smoothing semantics: `alpha` is the weight of history. `alpha = 0`
//! therefore tracks the raw signal exactly; larger values follow the raw
//! signal *less* closely. [`crate::Sampler`] uses the default weight (0.3);
//! the configurable knob was removed to keep the exposed settings minimal.

use std::time::Duration;

/// A tick gap larger than this many seconds means the machine slept or the
/// process was stalled (the fastest sample interval is 3 s, so a real tick
/// never approaches this). The frame spanning such a gap is stale: CPU
/// re-primes from the raw signal instead of blending across the gap, and
/// network reports NaN rather than a sleep-spanning rate.
pub const STALE_GAP_SECS: f64 = 60.0;

/// First Windows build whose Task Manager reports time-based CPU utilization.
///
/// Windows 11 24H2 (KB5058411, May 2025) switched Task Manager from the
/// frequency-weighted `% Processor Utility` to time-based `% Processor Time`
/// and moved the old weighted value to a separate "CPU Utility" display; the
/// change was not backported to 23H2 or earlier. To stay aligned with Task
/// Manager on every supported OS, the Windows sampler picks its PDH counter
/// from the build number: time-based at or above this build, utility below.
// Compiled on Windows (where the sampler consumes it) and in test builds on
// every platform (so the boundary tests run in the Linux quality gate);
// without this the lib target on non-Windows hosts sees no user and the
// dead-code lint fires.
#[cfg(any(windows, test))]
pub(crate) const TM_TIME_BASED_BUILD: u32 = 26100;

/// Whether Task Manager on a Windows machine with this build number shows
/// time-based CPU utilization (as opposed to frequency-weighted utility).
#[cfg(any(windows, test))]
pub(crate) fn task_manager_is_time_based(build: u32) -> bool {
    build >= TM_TIME_BASED_BUILD
}

/// Exponential moving average over CPU percent, with warm-up handling.
#[derive(Debug, Clone)]
pub struct CpuSampler {
    /// History weight in `[0, 1)`. `0` disables smoothing (pass-through).
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
            // `clamp` keeps NaN; a non-finite factor falls back to the default.
            alpha: if alpha.is_finite() {
                alpha.clamp(0.0, 1.0)
            } else {
                0.3
            },
            last: None,
        }
    }

    /// Fold a raw sysinfo percentage into the smoothed value.
    ///
    /// `elapsed` is the gap since the previous tick, or `None` on the very
    /// first tick. A gap far larger than any configured interval means the
    /// machine slept (or the process was stalled): the previous smoothed value
    /// is stale, so we re-prime on the raw signal instead of blending across
    /// the sleep. Otherwise `None`/first tick passes the raw value through to
    /// avoid the classic EMA pull toward zero.
    pub fn update(&mut self, raw: f64, elapsed: Option<Duration>) -> f64 {
        // The first sysinfo CPU refresh is not an interval average: sysinfo
        // itself documents the first call as inaccurate and recommends two
        // refreshes with ~200 ms between them. The sampler is constructed
        // immediately before its first sample, so that reading must not prime
        // the EMA or reach the display. Network already reports NaN for this
        // frame; CPU follows the same "no usable interval" rule.
        if elapsed.is_none() {
            return self.last.unwrap_or(f64::NAN);
        }
        // A non-finite reading is "no data", not zero: clamping would keep
        // NaN and poison every later EMA step. Leave the previous value (or
        // NaN on the very first tick) and wait for a finite sample.
        if !raw.is_finite() {
            return self.last.unwrap_or(f64::NAN);
        }
        let raw = raw.clamp(0.0, 100.0);
        // Wake/stall guard: a gap beyond the stale threshold invalidates the
        // whole smoothing history, not just one frame.
        if elapsed.map(|d| d.as_secs_f64() > STALE_GAP_SECS) == Some(true) {
            self.last = Some(raw);
            return raw;
        }
        match self.last {
            // No previous sample: pass through and prime.
            None => {
                self.last = Some(raw);
                raw
            }
            Some(prev) => {
                // Standard EMA with `alpha` as the history weight. The explicit
                // zero branch keeps a pass-through exact (no float drift) and
                // documents the "smoothing disabled" contract.
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
        // sysinfo's first refresh is not a valid interval average.
        assert!(s.update(50.0, None).is_nan());
        // The first sample with a measured interval primes the EMA.
        assert_eq!(s.update(100.0, Some(Duration::from_millis(2000))), 100.0);
        let v = s.update(50.0, Some(Duration::from_millis(2000)));
        assert!((v - 65.0).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn smoothing_weights_history_by_alpha() {
        let mut s = CpuSampler::new(0.3);
        s.update(50.0, Some(Duration::from_millis(2000))); // prime at 50
                                                           // second: 0.7*raw + 0.3*prev = 70 + 15 = 85
        let v = s.update(100.0, Some(Duration::from_millis(2000)));
        assert!((v - 85.0).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn alpha_zero_disables_smoothing_and_tracks_raw() {
        // Zero history weight means no smoothing, so the displayed value is
        // the raw reading (not frozen at the previous one).
        let mut s = CpuSampler::new(0.0);
        s.update(50.0, Some(Duration::from_millis(2000)));
        let v = s.update(100.0, Some(Duration::from_millis(2000)));
        assert_eq!(v, 100.0);
    }

    #[test]
    fn alpha_one_freezes_at_previous() {
        // alpha = 1 is full history weight (maximum smoothing): the value never
        // moves off the primed sample. The sampler uses 0.3, so this is only
        // reachable by direct construction.
        let mut s = CpuSampler::new(1.0);
        s.update(50.0, Some(Duration::from_millis(2000)));
        let v = s.update(100.0, Some(Duration::from_millis(2000)));
        assert_eq!(v, 50.0);
    }

    #[test]
    fn raw_is_clamped_to_percent_range() {
        // alpha 0 = pass-through, so the clamp on the raw value is directly
        // observable.
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
        // A bad frame returns the last good value and leaves the EMA intact.
        let bad = s.update(f64::INFINITY, Some(Duration::from_millis(1000)));
        assert_eq!(bad, 50.0);
        let v = s.update(100.0, Some(Duration::from_millis(2000)));
        assert!((v - 85.0).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn stale_gap_reprimes_instead_of_blending() {
        // A gap past STALE_GAP_SECS (sleep/stall) must drop the stale history:
        // the raw value passes through untouched rather than blending toward
        // the pre-sleep sample.
        let mut s = CpuSampler::new(0.3);
        s.update(50.0, Some(Duration::from_millis(2000)));
        let big = Duration::from_secs_f64(STALE_GAP_SECS + 10.0);
        let v = s.update(100.0, Some(big));
        assert_eq!(v, 100.0, "stale gap must re-prime to raw, not blend");
    }

    #[test]
    fn task_manager_formula_switches_at_24h2_build() {
        // Windows 10 and Windows 11 23H2-: Task Manager shows the
        // frequency-weighted utility.
        assert!(!task_manager_is_time_based(19045)); // Win10 22H2
        assert!(!task_manager_is_time_based(22631)); // Win11 23H2

        // Windows 11 24H2 (KB5058411) and later: time-based.
        assert!(task_manager_is_time_based(26100)); // Win11 24H2
        assert!(task_manager_is_time_based(26200)); // Win11 25H2
    }
}
