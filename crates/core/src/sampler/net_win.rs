//! Windows physical-adapter enumeration.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, MIB_IF_ROW2, MIB_IF_TABLE2,
};
use windows::Win32::NetworkManagement::Ndis::MediaConnectStateDisconnected;

use super::net::{adapter_counts, AdapterInfo, CounterFrame};

const HARDWARE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

// MIB_IF_ROW2.InterfaceAndOperStatusFlags bit 0 is HardwareInterface.
const HARDWARE_INTERFACE_FLAG: u8 = 0x01;

// SAFETY: the table returned by GetIfTable2 owns the rows; the caller hands out a borrow that must
// not outlive this call, and the table is freed before returning.
unsafe fn with_if_table<R>(read: impl FnOnce(&[MIB_IF_ROW2]) -> R) -> Option<R> {
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    if GetIfTable2(&mut table).0 != 0 {
        return None;
    }
    let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize);
    let result = read(rows);
    FreeMibTable(table as *const core::ffi::c_void);
    Some(result)
}

fn adapter_info(row: &MIB_IF_ROW2) -> AdapterInfo {
    AdapterInfo {
        transmit_link_speed: row.TransmitLinkSpeed,
        receive_link_speed: row.ReceiveLinkSpeed,
        media_connected: row.MediaConnectState != MediaConnectStateDisconnected,
        physical_address_length: row.PhysicalAddressLength,
        hardware_interface: row.InterfaceAndOperStatusFlags._bitfield & HARDWARE_INTERFACE_FLAG
            != 0,
    }
}

fn adapter_alias(row: &MIB_IF_ROW2) -> Option<String> {
    alias_string(&row.Alias)
}

pub(crate) fn native_counters() -> Option<CounterFrame> {
    // SAFETY: with_if_table frees the table before returning, so the rows never escape.
    unsafe {
        with_if_table(|rows| {
            rows.iter()
                .filter(|row| adapter_counts(&adapter_info(row)))
                .filter_map(|row| {
                    adapter_alias(row).map(|alias| (alias, row.InOctets, row.OutOctets))
                })
                .collect()
        })
    }
}

fn alias_string(alias: &[u16]) -> Option<String> {
    let len = alias.iter().position(|&c| c == 0).unwrap_or(alias.len());
    if len > 0 {
        Some(String::from_utf16_lossy(&alias[..len]))
    } else {
        None
    }
}

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

    pub(crate) fn names(&self) -> Option<&HashSet<String>> {
        self.names.as_ref()
    }
}

fn hardware_names() -> Option<HashSet<String>> {
    // SAFETY: with_if_table frees the table before returning, so the rows never escape.
    unsafe {
        with_if_table(|rows| {
            rows.iter()
                .filter(|row| adapter_info(row).hardware_interface)
                .filter_map(adapter_alias)
                .collect()
        })
    }
}
