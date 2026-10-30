//! Sampling layer.
//!
//! All OS-specific calls live in this module tree (sysinfo plus two
//! Windows-only helpers: the PDH CPU counter and the NDIS interface table);
//! everything else consumes the pure [`Snapshot`] it produces, so each metric
//! can later be swapped for a native call (`GetSystemTimes`, `GetIfTable2`,
//! `host_statistics`, ...) without changing any caller.
//!
//! Correctness rules baked in here, each unit-testable via a
//! fake counter feed (see `net` tests — they do not need a real NIC):
//! - Rates are differentials of monotonically increasing cumulative byte
//!   counters. A counter below its baseline (wrap-around or driver reset)
//!   yields "no data" (NaN) for that frame rather than a negative rate.
//! - Interface set changes (add/remove/unplug) rebuild the baseline instead
//!   of panicking.
//! - A tick gap far larger than any configured interval means the machine
//!   slept or the process was stalled: the frame is dropped as stale (CPU
//!   re-primes from raw, network reports NaN) so no sleep-spanning rate
//!   reaches the display or the history ([`STALE_GAP_SECS`]).
//! - Only physical NICs are aggregated: virtual switches and VPN tunnels
//!   mirror traffic the physical link already counts, so they are excluded
//!   (Windows uses the NDIS `HardwareInterface` flag, elsewhere a name
//!   heuristic); a user override can pin any interface list.
//! - CPU percent follows Task Manager on Windows (`% Processor Utility`,
//!   frequency-weighted) and the OS monitor on macOS (time-based), then is
//!   exponentially smoothed with the `sample.cpu_smoothing` history weight
//!   (0 disables) and clamped.

mod cpu;
mod mem;
mod net;

#[cfg(windows)]
mod cpu_win;
#[cfg(windows)]
mod net_win;

pub use cpu::CpuSampler;
pub use net::{NetSampler, NetSelection};

pub use cpu::STALE_GAP_SECS;

use std::time::Instant;

use sysinfo::{CpuRefreshKind, MemoryRefreshKind, Networks, RefreshKind, System};

use crate::model::Snapshot;

/// Owns the `sysinfo` handles and the inter-frame state needed to turn raw
/// cumulative counters into instantaneous rates. Construct once, call
/// [`Sampler::sample`] on every tick.
pub struct Sampler {
    system: System,
    networks: Networks,
    net: NetSampler,
    cpu: CpuSampler,
    /// Task Manager-equivalent CPU utilization on Windows
    /// (`% Processor Utility`). `None` when the counter is unavailable, in
    /// which case the time-based sysinfo value is used instead.
    #[cfg(windows)]
    cpu_utility: Option<cpu_win::ProcessorUtility>,
    /// Monotonic clock for real elapsed-time deltas, immune to wall-clock
    /// jumps (DST/NTP/sleep). The divisor for every differential is this, not
    /// the nominal interval, so a late tick still yields an accurate rate.
    last_tick: Option<Instant>,
}

impl Sampler {
    /// Build a sampler and seed the first cumulative-counter / memory
    /// baselines. `cpu_smoothing` is the `sample.cpu_smoothing` history
    /// weight (0 disables smoothing); it is fixed for the process lifetime —
    /// the config poller treats it as a startup-only debug setting.
    pub fn new(interfaces: NetSelection, cpu_smoothing: f64) -> Self {
        // sysinfo 0.40 made both constructors fallible for platforms without
        // the required backends. They are infallible on every platform this
        // workspace builds (Windows, macOS, and the Linux test/example host);
        // a failure would leave the indicator with nothing to show, so
        // surface it at startup.
        let system = System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::everything())
                .with_memory(MemoryRefreshKind::nothing().with_ram()),
        )
        .expect("sysinfo system information is supported on this platform");
        let networks = Networks::new_with_refreshed_list()
            .expect("sysinfo network enumeration is supported on this platform");
        Sampler {
            system,
            networks,
            net: NetSampler::new(interfaces),
            cpu: CpuSampler::new(cpu_smoothing),
            #[cfg(windows)]
            cpu_utility: cpu_win::ProcessorUtility::new(),
            last_tick: None,
        }
    }

    /// Take one reading. The first call returns CPU/MEM with NaN network rates
    /// (it only seeds baselines); from the second call on, network rates are
    /// real. `now_unix` is the local wall-clock timestamp to stamp the sample.
    pub fn sample(&mut self, now_unix: i64) -> Snapshot {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        // `false` keeps interfaces that momentarily disappear from the list so
        // a transient unplug doesn't destroy the baseline; reconciliation lives
        // in NetSampler.
        self.networks.refresh(false);

        let now = Instant::now();
        // On the very first tick there is no prior tick; elapsed is 0, and the
        // sub-samplers treat a 0 gap as "no rate yet".
        let elapsed = self.last_tick.map(|prev| now.duration_since(prev));
        self.last_tick = Some(now);

        // Whole-machine CPU percent. On Windows this is Task Manager's
        // `% Processor Utility` (frequency-weighted); sysinfo's time-based
        // `global_cpu_usage()` only serves as the fallback and as the source
        // on macOS, where Activity Monitor is time-based as well.
        #[cfg(windows)]
        let cpu_raw = match self.cpu_utility.as_mut().and_then(|c| c.read()) {
            Some(value) => value,
            None => self.system.global_cpu_usage() as f64,
        };
        #[cfg(not(windows))]
        let cpu_raw = self.system.global_cpu_usage() as f64;
        let cpu_pct = self.cpu.update(cpu_raw, elapsed);
        let mem_pct = mem::percent_used(self.system.used_memory(), self.system.total_memory());
        let (up, down) = self.net.poll(&self.networks, elapsed);

        Snapshot {
            ts: now_unix,
            cpu_pct,
            mem_pct,
            up_bps: up,
            down_bps: down,
        }
    }
}
