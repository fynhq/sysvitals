//! Sampling layer facade.

mod cpu;
mod mem;
mod net;

#[cfg(windows)]
mod cpu_win;
#[cfg(windows)]
mod net_win;

pub use cpu::CpuSampler;
pub use net::NetSampler;

use std::time::Instant;

use sysinfo::{CpuRefreshKind, MemoryRefreshKind, Networks, RefreshKind, System};

use crate::model::Snapshot;

pub struct Sampler {
    system: System,
    networks: Networks,
    net: NetSampler,
    cpu: CpuSampler,
    #[cfg(windows)]
    cpu_counter: Option<cpu_win::CpuCounter>,
    last_tick: Option<Instant>,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    pub fn new() -> Self {
        let system = System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::everything())
                .with_memory(MemoryRefreshKind::nothing().with_ram()),
        )
        .expect("sysinfo system information is supported on this platform");
        let networks =
            Networks::new().expect("sysinfo network enumeration is supported on this platform");
        Sampler {
            system,
            networks,
            net: NetSampler::new(),
            cpu: CpuSampler::default(),
            #[cfg(windows)]
            cpu_counter: cpu_win::CpuCounter::new(),
            last_tick: None,
        }
    }

    // First sample seeds baselines and reports NaN for CPU/network; memory is valid immediately.
    pub fn sample(&mut self, now_unix: i64) -> Snapshot {
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
        let elapsed = self.last_tick.map(|prev| now.duration_since(prev));
        self.last_tick = Some(now);

        #[cfg(windows)]
        let cpu_raw = pdh_cpu.unwrap_or_else(|| self.system.global_cpu_usage() as f64);
        #[cfg(not(windows))]
        let cpu_raw = self.system.global_cpu_usage() as f64;
        let cpu_pct = self.cpu.update(cpu_raw, elapsed);
        let mem_pct = mem::percent_used(self.system.used_memory(), self.system.total_memory());
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
