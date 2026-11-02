//! Network sampling helper.
//!
//! The tricky, bug-prone part of a taskbar monitor lives here, so it is split
//! into two pieces:
//!
//! 1. [`compute_rates`] is pure: it takes the previous baseline plus a vector
//!    of `(name, rx_total, tx_total)` cumulative counters and an elapsed time,
//!    and returns bytes/second. It encodes wrap-around handling, adapter
//!    add/remove reconciliation and the divide-by-zero/first-frame rules. It is
//!    unit-tested against synthetic counter feeds — no real NIC required.
//! 2. [`NetSampler::poll`] adapts `sysinfo::Networks` (real cumulative
//!    counters) into that `(name, rx, tx)` vector after applying interface
//!    selection, then calls `compute_rates`.
//!
//! We use `total_received`/`total_transmitted` (monotonic counters) rather than
//! sysinfo's own per-refresh diff so the differential policy is entirely ours
//! and is testable in isolation: sysinfo exposes cumulative counters, so
//! the differential policy lives entirely here.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use sysinfo::Networks;

/// NIC selection policy.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum NetSelection {
    /// Sum physical adapters only. Virtual switches and VPN tunnels mirror
    /// traffic that the physical NIC already counts, so including them would
    /// double-count it. On Windows the NDIS `HardwareInterface` flag decides;
    /// macOS/Linux and the Windows fallback use a name heuristic.
    #[default]
    Auto,
    /// Aggregate exactly these interface names (user override in settings).
    Interfaces(Vec<String>),
}

impl NetSelection {
    /// Whether an interface name is a candidate under this policy. Auto mode
    /// keeps physical adapters via [`is_physical_name`] (the sampler replaces
    /// that with the native Windows hardware set when it is readable);
    /// explicit mode requires an exact name match.
    pub fn matches(&self, name: &str) -> bool {
        match self {
            NetSelection::Auto => auto_matches(name, None),
            NetSelection::Interfaces(list) => list.iter().any(|n| n == name),
        }
    }
}

/// Auto-mode decision: the NDIS hardware set is authoritative when available;
/// otherwise the name heuristic decides.
fn auto_matches(name: &str, hardware: Option<&HashSet<String>>) -> bool {
    match hardware {
        Some(names) => names.contains(name),
        None => is_physical_name(name),
    }
}

/// Whether an interface name is a loopback alias. Loopback traffic never
/// leaves the machine. The `lo0` prefix covers the macOS slice variants.
pub fn is_loopback(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.is_empty() || lower == "lo" || lower.starts_with("lo0") || lower.starts_with("loopback")
}

/// Name-based physical-adapter heuristic. Used on macOS/Linux, and on Windows
/// until the NDIS hardware table is readable.
///
/// macOS/Linux physical NICs use well-known prefixes (`en*`, `eth*`, `wl*`,
/// `wwan*`), so virtual interfaces (`utun*`, `awdl*`, `bridge*`, `docker*`, ...)
/// never match. Windows adapters can be renamed to arbitrary localized names,
/// so there the heuristic excludes known virtual prefixes instead.
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

/// Virtual adapter name prefixes on Windows (fallback only — the NDIS
/// `HardwareInterface` flag is the primary source of truth).
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
    "zt",   // zerotier
    "wg",   // wireguard
    "awdl", // macOS
    "llnw", // macOS
    "anpi", // macOS
    "stun", // macOS
    "gif",  // BSD tunnel
    "stf",
    "isatap",
    "teredo",
    "vethernet", // Hyper-V virtual
    "hapas",     // Wi-Fi Direct
    "wsl",
    "vpn",
    "vmnet", // VMware
    "vmware",
    "vbox", // VirtualBox
    "virtual",
    "tailscale",
    "zerotier",
    "wintun",
    "nordlynx",
    "mullvad",
    "proton",
];

/// Physical adapter name prefixes on macOS/Linux.
#[cfg(not(windows))]
const PHYSICAL_PREFIXES: &[&str] = &["en", "eth", "wl", "wwan", "usb"];

/// One interface's cumulative counters in a frame: `(name, rx_total, tx_total)`.
pub type Counters = (String, u64, u64);

/// An owned counter frame (what `collect_frame` produces from real sysinfo).
/// Kept as plain data so tests can build it directly.
pub type CounterFrame = Vec<Counters>;

/// Diff a new counter frame against `baseline`, updating `baseline` in place.
///
/// Rules:
/// - `elapsed_secs <= 0` → `(NaN, NaN)` (no time to measure), but the baseline
///   is still refreshed so the next tick is correct.
/// - A gap larger than [`STALE_GAP_SECS`] (sleep / stalled process) → same
///   `(NaN, NaN)` drop: averaging bytes accumulated across a multi-minute
///   freeze into one instantaneous rate would be a lie; the baseline re-seeds
///   so the next normal tick is accurate.
/// - First frame (empty baseline) → `(NaN, NaN)`: seed the baseline only.
/// - Per interface: a counter that went *down* (wrap or driver reset) makes
///   that interface contribute nothing this frame and re-baselines it, instead
///   of producing a negative or absurd spike.
/// - Interfaces newly present contribute nothing (no prior to diff) but enter
///   the baseline for next time.
/// - Interfaces that disappeared (unplug / VPN off) are dropped from baseline.
/// - Final rate = summed byte deltas / `elapsed_secs`, never negative.
pub fn compute_rates(
    baseline: &mut HashMap<String, (u64, u64)>,
    frame: &[Counters],
    elapsed_secs: f64,
) -> (f64, f64) {
    let had_baseline = !baseline.is_empty();
    // Rebuild the baseline map from the current frame each call; first we
    // compute deltas against the *previous* baseline.
    let mut rx_delta: u128 = 0;
    let mut tx_delta: u128 = 0;
    let mut valid_any = false;

    for (name, rx, tx) in frame {
        match baseline.get(name) {
            // Wrap/reset protection: only accumulate if monotonic.
            Some((prev_rx, prev_tx)) if *rx >= *prev_rx && *tx >= *prev_tx => {
                rx_delta += (*rx - *prev_rx) as u128;
                tx_delta += (*tx - *prev_tx) as u128;
                valid_any = true;
            }
            // Newly seen interface (no prior to diff) OR one whose counter
            // wrapped/reset this frame: contributes nothing, but is re-baselined
            // below.
            Some(_) | None => {}
        }
    }

    // Refresh baseline to the current frame.
    baseline.clear();
    for (name, rx, tx) in frame {
        baseline.insert(name.clone(), (*rx, *tx));
    }

    if elapsed_secs <= 0.0 || elapsed_secs > super::cpu::STALE_GAP_SECS {
        // No usable interval, or a stale sleep-spanning gap → "no data".
        return (f64::NAN, f64::NAN);
    }
    if !valid_any {
        // No selected interface had a prior counter. With a previous baseline
        // this means every adapter disappeared (unplug / VPN off) and must
        // read as 0 rather than a stale value or a placeholder. A first frame
        // with no baseline, or a frame containing only new/wrapped
        // interfaces, still has no measurable rate.
        if frame.is_empty() && had_baseline {
            return (0.0, 0.0);
        }
        return (f64::NAN, f64::NAN);
    }

    let up = tx_delta as f64 / elapsed_secs;
    let down = rx_delta as f64 / elapsed_secs;
    (up.max(0.0), down.max(0.0))
}

/// Stateful NIC sampler wrapping [`compute_rates`] plus selection.
pub struct NetSampler {
    selection: NetSelection,
    baseline: HashMap<String, (u64, u64)>,
    /// Alias set of NDIS hardware interfaces (Windows only). Refreshed every
    /// poll so plug/unplug is picked up; `None` until the first successful
    /// table read.
    #[cfg(windows)]
    hardware: crate::sampler::net_win::HardwareInterfaces,
}

impl NetSampler {
    pub fn new(selection: NetSelection) -> Self {
        NetSampler {
            selection,
            baseline: HashMap::new(),
            #[cfg(windows)]
            hardware: crate::sampler::net_win::HardwareInterfaces::new(),
        }
    }

    /// Read `sysinfo` cumulative counters, apply selection, and diff against
    /// the internal baseline using the measured `elapsed`. Returns `(up_bps,
    /// down_bps)` (NaN on the first frame / after a reset).
    pub fn poll(&mut self, networks: &Networks, elapsed: Option<Duration>) -> (f64, f64) {
        let elapsed_secs = match elapsed {
            Some(d) => d.as_secs_f64(),
            None => 0.0,
        };

        #[cfg(windows)]
        let hardware = {
            self.hardware.refresh();
            self.hardware.names()
        };
        #[cfg(not(windows))]
        let hardware = None;

        let frame = collect_frame(networks, &self.selection, hardware);
        compute_rates(&mut self.baseline, &frame, elapsed_secs)
    }
}

/// Extract a filtered counter frame from a `sysinfo::Networks` handle.
/// Auto mode keeps physical adapters only; `hardware` is the native Windows
/// hardware set when readable. Split out so a future native backend can reuse
/// the shape.
fn collect_frame(
    networks: &Networks,
    selection: &NetSelection,
    hardware: Option<&HashSet<String>>,
) -> CounterFrame {
    networks
        .iter()
        .filter_map(|(name, data)| {
            // Interface names are `OsString` since sysinfo 0.40; the release
            // platforms (Windows/macOS) provide Unicode names, so the lossy
            // form is exact in practice and keeps selection matched against
            // the configured text.
            let name = name.to_string_lossy();
            let selected = match selection {
                NetSelection::Auto => auto_matches(name.as_ref(), hardware),
                NetSelection::Interfaces(list) => list.iter().any(|n| n == name.as_ref()),
            };
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
        // baseline now seeded
        assert_eq!(base.get("eth0"), Some(&(1000, 500)));
    }

    #[test]
    fn steady_increment_produces_expected_bps() {
        let mut base = HashMap::new();
        // seed
        compute_rates(&mut base, &[("eth0".to_string(), 0, 0)], secs(2000));
        // +2048 down, +1024 up over 2s => 1024 B/s down, 512 B/s up
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
        // eth0 +1000 down, wlan0 +500 down => 1500 total over 1s
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
        // counters dropped to 100 (reset): contribution discarded, NaN because
        // no interface had a valid diff this frame.
        let (up, down) = compute_rates(&mut base, &[("eth0".to_string(), 100, 100)], secs(1000));
        assert!(up.is_nan() && down.is_nan());
        // baseline re-primed to the new lower value; next increment is valid.
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
        // eth0 wraps (1000 -> 10): skipped; wlan0 +700 valid. Only wlan0 counts.
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
        // vpn0 unplugged: frame has only eth0.
        compute_rates(&mut base, &[("eth0".to_string(), 500, 500)], secs(1000));
        assert_eq!(base.len(), 1);
        assert!(base.contains_key("eth0"));
        assert!(!base.contains_key("vpn0"));
    }

    #[test]
    fn new_interface_contributes_zero_first_frame_it_appears() {
        let mut base = HashMap::new();
        compute_rates(&mut base, &[("eth0".to_string(), 100, 100)], secs(1000));
        // wlan0 just appeared with nonzero counters: no prior => not counted.
        let (up, down) = compute_rates(
            &mut base,
            &[
                ("eth0".to_string(), 200, 200),
                ("wlan0".to_string(), 99999, 99999),
            ],
            secs(1000),
        );
        // only eth0's +100 counts.
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
        // A gap beyond STALE_GAP_SECS (the machine slept) must not divide the
        // bytes accumulated across the freeze into a bogus instantaneous rate.
        let mut base = HashMap::new();
        compute_rates(&mut base, &[("eth0".to_string(), 0, 0)], secs(1000));
        let stale = super::super::cpu::STALE_GAP_SECS + 30.0;
        let (up, down) = compute_rates(&mut base, &[("eth0".to_string(), 100000, 100000)], stale);
        assert!(up.is_nan() && down.is_nan(), "stale gap → no data");
        // Baseline still re-seeded so the next normal tick is correct.
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
        // A frame with no interfaces after a seeded baseline (all adapters
        // vanished, e.g. unplugged) must read as 0, not as the first-frame
        // "--". This state is now reached only when the platform stops
        // enumerating an interface entirely.
        let mut base = HashMap::new();
        compute_rates(&mut base, &[("eth0".to_string(), 1000, 1000)], secs(1000));
        let (up, down) = compute_rates(&mut base, &[], secs(2000));
        assert_eq!(up, 0.0);
        assert_eq!(down, 0.0);
        assert!(base.is_empty());
        // A first frame with no interfaces still has no rate to report.
        let mut fresh = HashMap::new();
        let (up, down) = compute_rates(&mut fresh, &[], secs(1000));
        assert!(up.is_nan() && down.is_nan());
    }

    #[test]
    fn physical_name_heuristic() {
        // Physical adapters pass on every platform's heuristic.
        assert!(is_physical_name("eth0"));
        assert!(is_physical_name("en0"));
        assert!(is_physical_name("wlan0"));
        assert!(is_physical_name("wwan0"));
        // Loopback and virtual adapters never count toward the link rate.
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
        // The native Windows table decides even when the name would fool the
        // heuristic (renamed physical NIC / vendor virtual adapter).
        let mut names = HashSet::new();
        names.insert("Corp NIC".to_string());
        names.insert("eth0".to_string());
        let hardware = Some(&names);
        assert!(auto_matches("Corp NIC", hardware));
        assert!(auto_matches("eth0", hardware));
        assert!(!auto_matches("vEthernet (WSL)", hardware));
        // Without the native set the heuristic decides.
        assert!(auto_matches("eth0", None));
        assert!(!auto_matches("docker0", None));
    }

    #[test]
    fn selection_only_mode_filters_names() {
        let sel = NetSelection::Interfaces(vec!["en0".to_string()]);
        assert!(sel.matches("en0"));
        assert!(!sel.matches("en1"));
        // An explicit choice matches even a loopback-looking name (user intent).
        let sel2 = NetSelection::Interfaces(vec!["lo".to_string()]);
        assert!(sel2.matches("lo"));
        // An explicit list skips the loopback guard for unlisted names too.
        let sel3 = NetSelection::Interfaces(vec!["vEthernet (WSL)".to_string()]);
        assert!(sel3.matches("vEthernet (WSL)"));
        assert!(!sel3.matches("WLAN"));
    }
}
