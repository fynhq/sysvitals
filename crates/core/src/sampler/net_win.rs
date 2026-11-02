//! Windows physical-adapter enumeration.
//!
//! `sysinfo` reports every NDIS interface that has a MAC and a link speed,
//! which includes virtual switches (Hyper-V `vEthernet`) whose byte counters
//! mirror the physical link. Summing both would double-count the same traffic.
//! The NDIS row carries a `HardwareInterface` bit that is set for real
//! hardware and cleared for virtual adapters, so the sampler uses it as the
//! authoritative "physical NIC" set in Auto mode.

use std::collections::HashSet;

use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};

/// Cached alias set of NDIS hardware interfaces.
pub(crate) struct HardwareInterfaces {
    names: Option<HashSet<String>>,
}

impl HardwareInterfaces {
    pub(crate) fn new() -> Self {
        Self { names: None }
    }

    /// Re-read the interface table. On failure the previous set is kept so a
    /// transient error cannot blank the network readout.
    pub(crate) fn refresh(&mut self) {
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
                let alias = &row.Alias;
                let len = alias.iter().position(|&c| c == 0).unwrap_or(alias.len());
                if len > 0 {
                    names.insert(String::from_utf16_lossy(&alias[..len]));
                }
            }
        }
        FreeMibTable(table as *const core::ffi::c_void);
        Some(names)
    }
}
