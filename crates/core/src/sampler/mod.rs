//! Sampling layer facade.

mod cpu;
mod mem;
mod net;

#[cfg(windows)]
mod cpu_win;
#[cfg(windows)]
mod net_win;

pub use cpu::CpuSampler;
pub use net::{adapter_counts, AdapterInfo, NetSampler};

use std::time::Instant;

use sysinfo::{CpuRefreshKind, MemoryRefreshKind, Networks, RefreshKind, System};

use crate::model::Snapshot;

pub struct Sampler {
    system: Option<System>,
    networks: Option<Networks>,
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
    // A resident indicator degrades to placeholder readings instead of aborting on a sysinfo failure.
    pub fn new() -> Self {
        match Self::try_new() {
            Ok(sampler) => sampler,
            Err(error) => {
                log_degraded(&error);
                Self {
                    system: None,
                    networks: None,
                    net: NetSampler::new(),
                    cpu: CpuSampler::default(),
                    #[cfg(windows)]
                    cpu_counter: None,
                    last_tick: None,
                }
            }
        }
    }

    fn try_new() -> Result<Self, String> {
        let system = System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::everything())
                .with_memory(MemoryRefreshKind::nothing().with_ram()),
        )
        .map_err(|error| format!("system information unavailable: {error}"))?;
        let networks =
            Networks::new().map_err(|error| format!("network enumeration unavailable: {error}"))?;
        Ok(Sampler {
            system: Some(system),
            networks: Some(networks),
            net: NetSampler::new(),
            cpu: CpuSampler::default(),
            #[cfg(windows)]
            cpu_counter: cpu_win::CpuCounter::new(),
            last_tick: None,
        })
    }

    // First sample seeds baselines and reports NaN for CPU/network; memory is valid immediately.
    pub fn sample(&mut self, now_unix: i64) -> Snapshot {
        #[cfg(windows)]
        let pdh_cpu = self.cpu_counter.as_mut().and_then(|c| c.read());

        let now = Instant::now();
        let elapsed = self.last_tick.map(|prev| now.duration_since(prev));
        self.last_tick = Some(now);

        // The sysinfo baseline is refreshed even when PDH answered, so a fallback after an
        // intermittent PDH failure reads one tick instead of averaging every skipped tick.
        let (sysinfo_cpu, mem_pct) = match self.system.as_mut() {
            Some(system) => {
                system.refresh_cpu_usage();
                system.refresh_memory();
                (
                    f64::from(system.global_cpu_usage()),
                    mem::percent_used(system.used_memory(), system.total_memory()),
                )
            }
            None => (f64::NAN, f64::NAN),
        };

        #[cfg(windows)]
        let cpu_raw = pdh_cpu.unwrap_or(sysinfo_cpu);
        #[cfg(not(windows))]
        let cpu_raw = sysinfo_cpu;

        let cpu_pct = self.cpu.update(cpu_raw, elapsed);
        let (up_bps, down_bps) = match self.networks.as_mut() {
            Some(networks) => self.net.poll(networks, elapsed),
            None => (f64::NAN, f64::NAN),
        };

        Snapshot {
            ts: now_unix,
            cpu_pct,
            mem_pct,
            up_bps,
            down_bps,
        }
    }
}

fn log_degraded(error: &str) {
    let Ok(dir) = crate::config::data_dir() else {
        return;
    };
    let now = chrono::Local::now();
    crate::logger::log_line(
        &dir,
        &now.format("%Y-%m-%d").to_string(),
        &now.format("%H:%M:%S%.3f").to_string(),
        "WARN",
        &format!("sampler degraded to placeholder readings: {error}"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_degraded_sampler_reports_placeholders_instead_of_panicking() {
        let mut sampler = Sampler {
            system: None,
            networks: None,
            net: NetSampler::new(),
            cpu: CpuSampler::default(),
            #[cfg(windows)]
            cpu_counter: None,
            last_tick: None,
        };
        for tick in 0..3 {
            let snapshot = sampler.sample(1_700_000_000 + tick);
            assert!(snapshot.cpu_pct.is_nan(), "cpu: {}", snapshot.cpu_pct);
            assert!(snapshot.mem_pct.is_nan(), "mem: {}", snapshot.mem_pct);
            assert!(snapshot.up_bps.is_nan(), "up: {}", snapshot.up_bps);
            assert!(snapshot.down_bps.is_nan(), "down: {}", snapshot.down_bps);
        }
        let row = crate::format::format_frame(
            &sampler.sample(1_700_000_000),
            &crate::config::Config::default(),
        );
        assert_eq!(row.lines[0].left, "CPU: --%");
        assert_eq!(row.lines[1].left, "MEM: --%");
    }

    #[test]
    fn a_real_sampler_samples_without_panicking() {
        let mut sampler = Sampler::new();
        sampler.sample(0);
        let snapshot = sampler.sample(1);
        assert_eq!(snapshot.ts, 1);
        assert!(
            snapshot.mem_pct.is_nan() || (0.0..=100.0).contains(&snapshot.mem_pct),
            "memory stays in range: {}",
            snapshot.mem_pct
        );
    }
}
