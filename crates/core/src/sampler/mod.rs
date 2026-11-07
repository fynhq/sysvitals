//! Sampling layer.
//!
//! All OS-specific calls live in this module tree (sysinfo plus three
//! Windows-only helpers: the PDH CPU counter, the NDIS interface table-based
//! network counters, and the hardware-interface filter used by the sysinfo
//! fallback); everything else consumes the pure [`Snapshot`] it produces, so
//! each metric can later be swapped for a native call (`GetSystemTimes`,
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
//! - CPU percent follows Task Manager on Windows (the PDH counter is chosen
//!   per OS build: frequency-weighted `% Processor Utility` below Windows 11
//!   24H2, time-based `% Processor Time` from 24H2 on, where Task Manager
//!   switched formulas) and the OS monitor on macOS (time-based), then is
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
    /// Task Manager-equivalent CPU percent on Windows. The concrete PDH
    /// counter follows the OS build (see `cpu_win`); `None` when PDH is
    /// unavailable, in which case the time-based sysinfo value is used.
    #[cfg(windows)]
    cpu_counter: Option<cpu_win::CpuCounter>,
    /// Monotonic clock for real elapsed-time deltas, immune to wall-clock
    /// jumps (DST/NTP/sleep). The divisor for every differential is this, not
    /// the nominal interval, so a late tick still yields an accurate rate.
    last_tick: Option<Instant>,
}

impl Sampler {
    /// Build a sampler and seed the first CPU / memory baseline. The network
    /// baseline is seeded by the first [`Sampler::sample`] call; on Windows a
    /// successful native counter read never needs sysinfo's interface list.
    /// `cpu_smoothing` is the `sample.cpu_smoothing` weight (0 disables
    /// smoothing). The config pollers rebuild the sampler when that value
    /// changes, which re-primes the CPU EMA and re-seeds the network baseline
    /// (one `--` frame, equivalent to a restart).
    pub fn new(interfaces: NetSelection, cpu_smoothing: f64) -> Self {
        // sysinfo 0.40 made both constructors fallible for platforms without
        // the required backends. They only fail on targets this workspace
        // does not build for (Windows and macOS always succeed), and the
        // indicator has nothing to show without them, so surface a failure
        // at startup instead of silently painting `--` forever.
        let system = System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::everything())
                .with_memory(MemoryRefreshKind::nothing().with_ram()),
        )
        .expect("sysinfo system information is supported on this platform");
        // Empty by design: the fallback path refreshes it on first use, and
        // refreshing here would pay sysinfo's Windows interface/address
        // enumeration even when the native counter path succeeds.
        let networks =
            Networks::new().expect("sysinfo network enumeration is supported on this platform");
        Sampler {
            system,
            networks,
            net: NetSampler::new(interfaces),
            cpu: CpuSampler::new(cpu_smoothing),
            #[cfg(windows)]
            cpu_counter: cpu_win::CpuCounter::new(),
            last_tick: None,
        }
    }

    /// Take one reading. The first call returns CPU/MEM with NaN network rates
    /// (it only seeds baselines); from the second call on, network rates are
    /// real. `now_unix` is the local wall-clock timestamp to stamp the sample.
    pub fn sample(&mut self, now_unix: i64) -> Snapshot {
        // Whole-machine CPU percent. On Windows the PDH counter follows
        // Task Manager's formula for the running OS build (frequency-weighted
        // utility below Windows 11 24H2, time-based from 24H2 on); sysinfo's
        // time-based `global_cpu_usage()` is the fallback and the source on
        // macOS, where Activity Monitor is time-based as well.
        //
        // Read PDH before refreshing sysinfo so a successful Windows counter
        // does not pay for an all-core sysinfo refresh every tick. Only the
        // fallback path refreshes sysinfo; the first PDH read primes the
        // counter and therefore takes the fallback once.
        #[cfg(windows)]
        let pdh_cpu = self.cpu_counter.as_mut().and_then(|c| c.read());
        #[cfg(windows)]
        if pdh_cpu.is_none() {
            self.system.refresh_cpu_usage();
        }
        #[cfg(not(windows))]
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();

        let now = Instant::now();
        // On the very first tick there is no prior tick; elapsed is 0, and the
        // sub-samplers treat a 0 gap as "no rate yet".
        let elapsed = self.last_tick.map(|prev| now.duration_since(prev));
        self.last_tick = Some(now);

        #[cfg(windows)]
        let cpu_raw = pdh_cpu.unwrap_or_else(|| self.system.global_cpu_usage() as f64);
        #[cfg(not(windows))]
        let cpu_raw = self.system.global_cpu_usage() as f64;
        let cpu_pct = self.cpu.update(cpu_raw, elapsed);
        let mem_pct = mem::percent_used(self.system.used_memory(), self.system.total_memory());
        // `NetSampler` chooses the native Windows counters when available and
        // only refreshes the sysinfo list on the fallback path. Passing the
        // handle mutably keeps that decision in one place.
        let (up, down) = self.net.poll(&mut self.networks, elapsed);

        Snapshot {
            ts: now_unix,
            cpu_pct,
            mem_pct,
            up_bps: up,
            down_bps: down,
        }
    }
}
