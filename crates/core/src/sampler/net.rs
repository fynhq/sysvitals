//! Network sampling, counters, and interface selection.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use sysinfo::Networks;

fn auto_matches(name: &str, hardware: Option<&HashSet<String>>) -> bool {
    match hardware {
        Some(names) => names.contains(name),
        None => is_physical_name(name),
    }
}

pub fn is_loopback(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.is_empty() || lower == "lo" || lower.starts_with("lo0") || lower.starts_with("loopback")
}

// Virtual switches and VPN tunnels mirror physical-NIC traffic, so they never count toward the link rate.
pub fn is_physical_name(name: &str) -> bool {
    if is_loopback(name) {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    #[cfg(windows)]
    {
        !VIRTUAL_PREFIXES.iter().any(|p| lower.starts_with(p))
    }
    #[cfg(not(windows))]
    {
        PHYSICAL_PREFIXES.iter().any(|p| lower.starts_with(p))
    }
}

#[cfg(windows)]
const VIRTUAL_PREFIXES: &[&str] = &[
    "veth",
    "docker",
    "br-",
    "bridge",
    "virbr",
    "tap",
    "tun",
    "utun",
    "zt",
    "wg",
    "gif",
    "stf",
    "isatap",
    "teredo",
    "vethernet",
    "hapas",
    "wsl",
    "vpn",
    "vmnet",
    "vmware",
    "vbox",
    "virtual",
    "tailscale",
    "zerotier",
    "wintun",
    "nordlynx",
    "mullvad",
    "proton",
];

#[cfg(not(windows))]
const PHYSICAL_PREFIXES: &[&str] = &["en", "eth", "wl", "wwan", "usb"];

pub type Counters = (String, u64, u64);

pub type CounterFrame = Vec<Counters>;

// Diff cumulative counters against the baseline, drop vanished interfaces, and skip virtual/VPN adapters.
pub fn compute_rates(
    baseline: &mut HashMap<String, (u64, u64)>,
    frame: &[Counters],
    elapsed_secs: f64,
) -> (f64, f64) {
    let had_baseline = !baseline.is_empty();
    let mut rx_delta: u128 = 0;
    let mut tx_delta: u128 = 0;
    let mut valid_any = false;

    for (name, rx, tx) in frame {
        match baseline.get(name) {
            Some((prev_rx, prev_tx)) if *rx >= *prev_rx && *tx >= *prev_tx => {
                rx_delta += (*rx - *prev_rx) as u128;
                tx_delta += (*tx - *prev_tx) as u128;
                valid_any = true;
            }
            Some(_) | None => {}
        }
    }

    baseline.clear();
    for (name, rx, tx) in frame {
        baseline.insert(name.clone(), (*rx, *tx));
    }

    if elapsed_secs <= 0.0 || elapsed_secs > super::cpu::STALE_GAP_SECS {
        return (f64::NAN, f64::NAN);
    }
    if !valid_any {
        // No selected interface had a prior counter: with a previous baseline this reads 0 (unplug/VPN off), not a stale value.
        if frame.is_empty() && had_baseline {
            return (0.0, 0.0);
        }
        return (f64::NAN, f64::NAN);
    }

    let up = tx_delta as f64 / elapsed_secs;
    let down = rx_delta as f64 / elapsed_secs;
    (up.max(0.0), down.max(0.0))
}

pub struct NetSampler {
    baseline: HashMap<String, (u64, u64)>,
    #[cfg(windows)]
    hardware: crate::sampler::net_win::HardwareInterfaces,
}

impl Default for NetSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl NetSampler {
    pub fn new() -> Self {
        NetSampler {
            baseline: HashMap::new(),
            #[cfg(windows)]
            hardware: crate::sampler::net_win::HardwareInterfaces::new(),
        }
    }

    pub fn poll(&mut self, networks: &mut Networks, elapsed: Option<Duration>) -> (f64, f64) {
        let elapsed_secs = match elapsed {
            Some(d) => d.as_secs_f64(),
            None => 0.0,
        };

        #[cfg(windows)]
        {
            if let Some(frame) = crate::sampler::net_win::native_counters() {
                return compute_rates(&mut self.baseline, &frame, elapsed_secs);
            }
            networks.refresh(false);
            self.hardware.refresh();
            let frame = collect_frame(networks, self.hardware.names());
            compute_rates(&mut self.baseline, &frame, elapsed_secs)
        }

        #[cfg(not(windows))]
        {
            networks.refresh(false);
            let frame = collect_frame(networks, None);
            compute_rates(&mut self.baseline, &frame, elapsed_secs)
        }
    }
}

fn collect_frame(networks: &Networks, hardware: Option<&HashSet<String>>) -> CounterFrame {
    networks
        .iter()
        .filter_map(|(name, data)| {
            let name = name.to_string_lossy();
            let selected = auto_matches(name.as_ref(), hardware);
            if selected {
                Some((
                    name.into_owned(),
                    data.total_received(),
                    data.total_transmitted(),
                ))
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(ms: u64) -> f64 {
        ms as f64 / 1000.0
    }

    #[test]
    fn first_frame_seeds_baseline_and_reports_nan() {
        let mut base = HashMap::new();
        let frame = vec![("eth0".to_string(), 1000u64, 500u64)];
        let (up, down) = compute_rates(&mut base, &frame, secs(2000));
        assert!(up.is_nan() && down.is_nan());
        assert_eq!(base.get("eth0"), Some(&(1000, 500)));
    }

    #[test]
    fn steady_increment_produces_expected_bps() {
        let mut base = HashMap::new();
        compute_rates(&mut base, &[("eth0".to_string(), 0, 0)], secs(2000));
        let (up, down) = compute_rates(&mut base, &[("eth0".to_string(), 2048, 1024)], secs(2000));
        assert_eq!(up, 512.0);
        assert_eq!(down, 1024.0);
    }

    #[test]
    fn aggregates_across_multiple_interfaces() {
        let mut base = HashMap::new();
        compute_rates(
            &mut base,
            &[("eth0".to_string(), 0, 0), ("wlan0".to_string(), 0, 0)],
            secs(1000),
        );
        let (_, down) = compute_rates(
            &mut base,
            &[("eth0".to_string(), 1000, 0), ("wlan0".to_string(), 500, 0)],
            secs(1000),
        );
        assert_eq!(down, 1500.0);
    }

    #[test]
    fn counter_wrap_produces_no_negative_and_rebaselines() {
        let mut base = HashMap::new();
        compute_rates(&mut base, &[("eth0".to_string(), 5000, 5000)], secs(1000));
        let (up, down) = compute_rates(&mut base, &[("eth0".to_string(), 100, 100)], secs(1000));
        assert!(up.is_nan() && down.is_nan());
        assert_eq!(base.get("eth0"), Some(&(100, 100)));
        let (up, down) = compute_rates(
            &mut base,
            &[("eth0".to_string(), 100 + 200, 100)],
            secs(1000),
        );
        assert_eq!(down, 200.0);
        assert_eq!(up, 0.0);
    }

    #[test]
    fn wrap_on_one_interface_does_not_poison_others() {
        let mut base = HashMap::new();
        compute_rates(
            &mut base,
            &[
                ("eth0".to_string(), 1000, 1000),
                ("wlan0".to_string(), 0, 0),
            ],
            secs(1000),
        );
        let (_, down) = compute_rates(
            &mut base,
            &[("eth0".to_string(), 10, 5), ("wlan0".to_string(), 700, 0)],
            secs(1000),
        );
        assert_eq!(down, 700.0);
    }

    #[test]
    fn removed_interface_drops_from_baseline() {
        let mut base = HashMap::new();
        compute_rates(
            &mut base,
            &[("eth0".to_string(), 0, 0), ("vpn0".to_string(), 0, 0)],
            secs(1000),
        );
        assert_eq!(base.len(), 2);
        compute_rates(&mut base, &[("eth0".to_string(), 500, 500)], secs(1000));
        assert_eq!(base.len(), 1);
        assert!(base.contains_key("eth0"));
        assert!(!base.contains_key("vpn0"));
    }

    #[test]
    fn new_interface_contributes_zero_first_frame_it_appears() {
        let mut base = HashMap::new();
        compute_rates(&mut base, &[("eth0".to_string(), 100, 100)], secs(1000));
        let (up, down) = compute_rates(
            &mut base,
            &[
                ("eth0".to_string(), 200, 200),
                ("wlan0".to_string(), 99999, 99999),
            ],
            secs(1000),
        );
        assert_eq!(down, 100.0);
        assert_eq!(up, 100.0);
    }

    #[test]
    fn zero_elapsed_is_nan() {
        let mut base = HashMap::new();
        compute_rates(&mut base, &[("eth0".to_string(), 0, 0)], secs(1000));
        let (up, down) = compute_rates(&mut base, &[("eth0".to_string(), 1000, 1000)], 0.0);
        assert!(up.is_nan() && down.is_nan());
    }

    #[test]
    fn stale_gap_is_dropped_as_no_data() {
        let mut base = HashMap::new();
        compute_rates(&mut base, &[("eth0".to_string(), 0, 0)], secs(1000));
        let stale = super::super::cpu::STALE_GAP_SECS + 30.0;
        let (up, down) = compute_rates(&mut base, &[("eth0".to_string(), 100000, 100000)], stale);
        assert!(up.is_nan() && down.is_nan(), "stale gap → no data");
        assert_eq!(base.get("eth0"), Some(&(100000, 100000)));
        let (up, down) = compute_rates(
            &mut base,
            &[("eth0".to_string(), 102000, 100000)],
            secs(2000),
        );
        assert_eq!(down, 1000.0);
        assert!(up.is_nan() || up == 0.0);
    }

    #[test]
    fn link_down_after_baseline_reports_zero_not_nan() {
        let mut base = HashMap::new();
        compute_rates(&mut base, &[("eth0".to_string(), 1000, 1000)], secs(1000));
        let (up, down) = compute_rates(&mut base, &[], secs(2000));
        assert_eq!(up, 0.0);
        assert_eq!(down, 0.0);
        assert!(base.is_empty());
        let mut fresh = HashMap::new();
        let (up, down) = compute_rates(&mut fresh, &[], secs(1000));
        assert!(up.is_nan() && down.is_nan());
    }

    #[test]
    fn physical_name_heuristic() {
        assert!(is_physical_name("eth0"));
        assert!(is_physical_name("en0"));
        assert!(is_physical_name("wlan0"));
        assert!(is_physical_name("wwan0"));
        assert!(!is_physical_name("lo"));
        assert!(!is_physical_name("lo0"));
        assert!(!is_physical_name("Loopback Pseudo-Interface 1"));
        assert!(!is_physical_name("vEthernet (WSL)"));
        assert!(!is_physical_name("utun4"));
        assert!(!is_physical_name("docker0"));
        assert!(!is_physical_name(""));
    }

    #[test]
    fn hardware_set_is_authoritative_in_auto_mode() {
        let mut names = HashSet::new();
        names.insert("Corp NIC".to_string());
        names.insert("eth0".to_string());
        let hardware = Some(&names);
        assert!(auto_matches("Corp NIC", hardware));
        assert!(auto_matches("eth0", hardware));
        assert!(!auto_matches("vEthernet (WSL)", hardware));
        assert!(auto_matches("eth0", None));
        assert!(!auto_matches("docker0", None));
    }
}
