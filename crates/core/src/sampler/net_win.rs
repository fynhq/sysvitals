//! Windows physical-adapter enumeration.
//!
//! `sysinfo` reports every NDIS interface that has a MAC and a link speed,
//! which includes virtual switches (Hyper-V `vEthernet`) whose byte counters
//! mirror the physical link. Summing both would double-count the same traffic.
//! The NDIS row carries a `HardwareInterface` bit that is set for real
//! hardware and cleared for virtual adapters, so the sampler uses it as the
//! authoritative "physical NIC" set in Auto mode.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
use windows::Win32::NetworkManagement::Ndis::MediaConnectStateDisconnected;

use super::net::{CounterFrame, NetSelection};

/// The hardware-interface set is only needed on the sysinfo fallback path.
/// It changes on plug/unplug or adapter enable/disable, so a few seconds of
/// staleness only means a newly appeared interface starts contributing on the
/// next refresh.
const HARDWARE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// One `GetIfTable2` read for the steady counter path: alias, cumulative
/// octets and the `HardwareInterface` bit all come from the same row, so this
/// avoids sysinfo's per-refresh adapter/address enumeration entirely.
/// `None` means the table could not be read and the caller must fall back.
pub(crate) fn native_counters(selection: &NetSelection) -> Option<CounterFrame> {
    unsafe {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut table).0 != 0 {
            return None;
        }
        let rows =
            std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize);
        let auto = matches!(selection, NetSelection::Auto);
        let mut frame = Vec::new();
        for row in rows {
            // Match sysinfo's active-link filter: a disconnected or addressless
            // adapter must not enter an explicit selection with stale counters.
            if (row.TransmitLinkSpeed == 0 && row.ReceiveLinkSpeed == 0)
                || row.MediaConnectState == MediaConnectStateDisconnected
                || row.PhysicalAddressLength == 0
            {
                continue;
            }
            if auto && row.InterfaceAndOperStatusFlags._bitfield & 0x01 == 0 {
                continue;
            }
            let Some(name) = alias_string(&row.Alias) else {
                continue;
            };
            if let NetSelection::Interfaces(_) = selection {
                if !selection.matches(&name) {
                    continue;
                }
            }
            frame.push((name, row.InOctets, row.OutOctets));
        }
        FreeMibTable(table as *const core::ffi::c_void);
        Some(frame)
    }
}

/// Decode a NUL-terminated NDIS alias; empty aliases are not usable as
/// selection keys.
fn alias_string(alias: &[u16]) -> Option<String> {
    let len = alias.iter().position(|&c| c == 0).unwrap_or(alias.len());
    if len > 0 {
        Some(String::from_utf16_lossy(&alias[..len]))
    } else {
        None
    }
}

/// Cached alias set of NDIS hardware interfaces.
pub(crate) struct HardwareInterfaces {
    names: Option<HashSet<String>>,
    last_refresh: Option<Instant>,
}

impl HardwareInterfaces {
    pub(crate) fn new() -> Self {
        Self {
            names: None,
            last_refresh: None,
        }
    }

    /// Re-read the interface table, at most once per
    /// [`HARDWARE_REFRESH_INTERVAL`]. On failure the previous set is kept so
    /// a transient error cannot blank the network readout.
    pub(crate) fn refresh(&mut self) {
        if self
            .last_refresh
            .is_some_and(|last| last.elapsed() < HARDWARE_REFRESH_INTERVAL)
        {
            return;
        }
        self.last_refresh = Some(Instant::now());
        if let Some(names) = hardware_names() {
            self.names = Some(names);
        }
    }

    /// `None` means the table has never been read; callers fall back to the
    /// name heuristic.
    pub(crate) fn names(&self) -> Option<&HashSet<String>> {
        self.names.as_ref()
    }
}

fn hardware_names() -> Option<HashSet<String>> {
    unsafe {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut table).0 != 0 {
            return None;
        }
        let rows =
            std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize);
        let mut names = HashSet::with_capacity(rows.len());
        for row in rows {
            // Bit 0 of `MIB_IF_ROW2.InterfaceAndOperStatusFlags` is
            // `HardwareInterface`; virtual switches and tunnels report 0.
            if row.InterfaceAndOperStatusFlags._bitfield & 0x01 != 0 {
                if let Some(name) = alias_string(&row.Alias) {
                    names.insert(name);
                }
            }
        }
        FreeMibTable(table as *const core::ffi::c_void);
        Some(names)
    }
}
