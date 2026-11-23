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

// The subset of a platform adapter row that decides whether it contributes to the link rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdapterInfo {
    pub transmit_link_speed: u64,
    pub receive_link_speed: u64,
    pub media_connected: bool,
    pub physical_address_length: u32,
    pub hardware_interface: bool,
}

// Virtual switches, VPN tunnels and unplugged ports mirror or stall the physical link rate, so a
// counted adapter must be hardware, connected, addressed and report a speed in some direction.
pub fn adapter_counts(info: &AdapterInfo) -> bool {
    info.hardware_interface
        && info.media_connected
        && info.physical_address_length != 0
        && (info.transmit_link_speed != 0 || info.receive_link_speed != 0)
}

// Diff cumulative counters against the baseline, drop vanished interfaces, and skip virtual/VPN adapters.
pub fn compute_rates(
    baseline: &mut HashMap<String, (u64, u64)>,
    frame: CounterFrame,
    elapsed_secs: f64,
) -> (f64, f64) {
    let had_baseline = !baseline.is_empty();
    let frame_empty = frame.is_empty();
    let mut rx_delta: u128 = 0;
    let mut tx_delta: u128 = 0;
    let mut rx_valid = false;
    let mut tx_valid = false;

    for (name, rx, tx) in &frame {
        if let Some((prev_rx, prev_tx)) = baseline.get(name) {
            // Each direction is judged on its own: a wrap on one must not discard the other's delta.
            if *rx >= *prev_rx {
                rx_delta += (*rx - *prev_rx) as u128;
                rx_valid = true;
            }
            if *tx >= *prev_tx {
                tx_delta += (*tx - *prev_tx) as u128;
                tx_valid = true;
            }
        }
    }

    baseline.clear();
    for (name, rx, tx) in frame {
        baseline.insert(name, (rx, tx));
    }

    if elapsed_secs <= 0.0 || elapsed_secs > super::cpu::STALE_GAP_SECS {
        return (f64::NAN, f64::NAN);
    }
    if !rx_valid && !tx_valid {
        // No selected interface had a prior counter: with a previous baseline this reads 0 (unplug/VPN off), not a stale value.
        if frame_empty && had_baseline {
            return (0.0, 0.0);
        }
        return (f64::NAN, f64::NAN);
    }

    let up = if tx_valid {
        (tx_delta as f64 / elapsed_secs).max(0.0)
    } else {
        f64::NAN
    };
    let down = if rx_valid {
        (rx_delta as f64 / elapsed_secs).max(0.0)
    } else {
        f64::NAN
    };
    (up, down)
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
                return compute_rates(&mut self.baseline, frame, elapsed_secs);
            }
            networks.refresh(false);
            self.hardware.refresh();
            let frame = collect_frame(networks, self.hardware.names());
            compute_rates(&mut self.baseline, frame, elapsed_secs)
        }

        #[cfg(not(windows))]
        {
            networks.refresh(false);
            let frame = collect_frame(networks, None);
            compute_rates(&mut self.baseline, frame, elapsed_secs)
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
        let (up, down) = compute_rates(&mut base, frame, secs(2000));
        assert!(up.is_nan() && down.is_nan());
        assert_eq!(base.get("eth0"), Some(&(1000, 500)));
    }

    #[test]
    fn steady_increment_produces_expected_bps() {
        let mut base = HashMap::new();
        compute_rates(&mut base, vec![("eth0".to_string(), 0, 0)], secs(2000));
        let (up, down) = compute_rates(
            &mut base,
            vec![("eth0".to_string(), 2048, 1024)],
            secs(2000),
        );
        assert_eq!(up, 512.0);
        assert_eq!(down, 1024.0);
    }

    #[test]
    fn aggregates_across_multiple_interfaces() {
        let mut base = HashMap::new();
        compute_rates(
            &mut base,
            vec![("eth0".to_string(), 0, 0), ("wlan0".to_string(), 0, 0)],
            secs(1000),
        );
        let (_, down) = compute_rates(
            &mut base,
            vec![("eth0".to_string(), 1000, 0), ("wlan0".to_string(), 500, 0)],
            secs(1000),
        );
        assert_eq!(down, 1500.0);
    }

    #[test]
    fn counter_wrap_produces_no_negative_and_rebaselines() {
        let mut base = HashMap::new();
        compute_rates(
            &mut base,
            vec![("eth0".to_string(), 5000, 5000)],
            secs(1000),
        );
        let (up, down) = compute_rates(&mut base, vec![("eth0".to_string(), 100, 100)], secs(1000));
        assert!(up.is_nan() && down.is_nan());
        assert_eq!(base.get("eth0"), Some(&(100, 100)));
        let (up, down) = compute_rates(
            &mut base,
            vec![("eth0".to_string(), 100 + 200, 100)],
            secs(1000),
        );
        assert_eq!(down, 200.0);
        assert_eq!(up, 0.0);
    }

    #[test]
    fn a_transmit_wrap_keeps_the_valid_receive_delta() {
        let mut base = HashMap::new();
        compute_rates(
            &mut base,
            vec![("eth0".to_string(), 1000, 1000)],
            secs(1000),
        );
        let (up, down) =
            compute_rates(&mut base, vec![("eth0".to_string(), 3000, 500)], secs(1000));
        assert_eq!(down, 2000.0, "the receive delta is still usable");
        assert!(up.is_nan(), "the regressed direction reports no reading");
        assert_eq!(
            base.get("eth0"),
            Some(&(3000, 500)),
            "both directions rebase so the next tick is a single interval"
        );
        let (up, down) =
            compute_rates(&mut base, vec![("eth0".to_string(), 3000, 900)], secs(1000));
        assert_eq!(up, 400.0, "transmit recovers on the next tick");
        assert_eq!(down, 0.0);
    }

    #[test]
    fn a_receive_wrap_keeps_the_valid_transmit_delta() {
        let mut base = HashMap::new();
        compute_rates(
            &mut base,
            vec![("eth0".to_string(), 1000, 1000)],
            secs(1000),
        );
        let (up, down) =
            compute_rates(&mut base, vec![("eth0".to_string(), 200, 2000)], secs(1000));
        assert_eq!(up, 1000.0);
        assert!(down.is_nan());
    }

    #[test]
    fn wrap_on_one_interface_does_not_poison_others() {
        let mut base = HashMap::new();
        compute_rates(
            &mut base,
            vec![
                ("eth0".to_string(), 1000, 1000),
                ("wlan0".to_string(), 0, 0),
            ],
            secs(1000),
        );
        let (_, down) = compute_rates(
            &mut base,
            vec![("eth0".to_string(), 10, 5), ("wlan0".to_string(), 700, 0)],
            secs(1000),
        );
        assert_eq!(down, 700.0);
    }

    #[test]
    fn removed_interface_drops_from_baseline() {
        let mut base = HashMap::new();
        compute_rates(
            &mut base,
            vec![("eth0".to_string(), 0, 0), ("vpn0".to_string(), 0, 0)],
            secs(1000),
        );
        assert_eq!(base.len(), 2);
        compute_rates(&mut base, vec![("eth0".to_string(), 500, 500)], secs(1000));
        assert_eq!(base.len(), 1);
        assert!(base.contains_key("eth0"));
        assert!(!base.contains_key("vpn0"));
    }

    #[test]
    fn a_replaced_interface_set_reads_zero_not_stale() {
        let mut base = HashMap::new();
        compute_rates(&mut base, vec![("vpn0".to_string(), 100, 100)], secs(1000));
        let (up, down) = compute_rates(
            &mut base,
            vec![("eth0".to_string(), 9_000, 9_000)],
            secs(1000),
        );
        assert!(
            up.is_nan() && down.is_nan(),
            "a fresh interface must not report the vanished one's counters"
        );
    }

    #[test]
    fn new_interface_contributes_zero_first_frame_it_appears() {
        let mut base = HashMap::new();
        compute_rates(&mut base, vec![("eth0".to_string(), 100, 100)], secs(1000));
        let (up, down) = compute_rates(
            &mut base,
            vec![
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
        compute_rates(&mut base, vec![("eth0".to_string(), 0, 0)], secs(1000));
        let (up, down) = compute_rates(&mut base, vec![("eth0".to_string(), 1000, 1000)], 0.0);
        assert!(up.is_nan() && down.is_nan());
    }

    #[test]
    fn stale_gap_is_dropped_as_no_data() {
        let mut base = HashMap::new();
        compute_rates(&mut base, vec![("eth0".to_string(), 0, 0)], secs(1000));
        let stale = super::super::cpu::STALE_GAP_SECS + 30.0;
        let (up, down) =
            compute_rates(&mut base, vec![("eth0".to_string(), 100000, 100000)], stale);
        assert!(up.is_nan() && down.is_nan(), "stale gap → no data");
        assert_eq!(base.get("eth0"), Some(&(100000, 100000)));
        let (up, down) = compute_rates(
            &mut base,
            vec![("eth0".to_string(), 102000, 100000)],
            secs(2000),
        );
        assert_eq!(down, 1000.0);
        assert!(up.is_nan() || up == 0.0);
    }

    #[test]
    fn link_down_after_baseline_reports_zero_not_nan() {
        let mut base = HashMap::new();
        compute_rates(
            &mut base,
            vec![("eth0".to_string(), 1000, 1000)],
            secs(1000),
        );
        let (up, down) = compute_rates(&mut base, Vec::new(), secs(2000));
        assert_eq!(up, 0.0);
        assert_eq!(down, 0.0);
        assert!(base.is_empty());
        let mut fresh = HashMap::new();
        let (up, down) = compute_rates(&mut fresh, Vec::new(), secs(1000));
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

    fn adapter() -> AdapterInfo {
        AdapterInfo {
            transmit_link_speed: 1_000_000_000,
            receive_link_speed: 1_000_000_000,
            media_connected: true,
            physical_address_length: 6,
            hardware_interface: true,
        }
    }

    #[test]
    fn only_a_connected_physical_adapter_with_a_link_speed_counts() {
        assert!(adapter_counts(&adapter()), "the reference adapter counts");

        let mut virtual_adapter = adapter();
        virtual_adapter.hardware_interface = false;
        assert!(
            !adapter_counts(&virtual_adapter),
            "a virtual switch does not"
        );

        let mut disconnected = adapter();
        disconnected.media_connected = false;
        assert!(!adapter_counts(&disconnected), "an unplugged port does not");

        let mut addressed = adapter();
        addressed.physical_address_length = 0;
        assert!(
            !adapter_counts(&addressed),
            "an adapter without a physical address does not"
        );

        let mut no_speed = adapter();
        no_speed.transmit_link_speed = 0;
        no_speed.receive_link_speed = 0;
        assert!(!adapter_counts(&no_speed), "a zero link speed does not");

        let mut half_duplex_report = adapter();
        half_duplex_report.transmit_link_speed = 0;
        assert!(
            adapter_counts(&half_duplex_report),
            "one reported direction is enough"
        );
        let mut other_direction = adapter();
        other_direction.receive_link_speed = 0;
        assert!(adapter_counts(&other_direction));
    }
}
