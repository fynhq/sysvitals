//! Windows physical-adapter enumeration.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
use windows::Win32::NetworkManagement::Ndis::MediaConnectStateDisconnected;

use super::net::CounterFrame;

const HARDWARE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

pub(crate) fn native_counters() -> Option<CounterFrame> {
    unsafe {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut table).0 != 0 {
            return None;
        }
        let rows =
            std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize);
        let mut frame = Vec::new();
        for row in rows {
            if (row.TransmitLinkSpeed == 0 && row.ReceiveLinkSpeed == 0)
                || row.MediaConnectState == MediaConnectStateDisconnected
                || row.PhysicalAddressLength == 0
            {
                continue;
            }
            if row.InterfaceAndOperStatusFlags._bitfield & 0x01 == 0 {
                continue;
            }
            let Some(name) = alias_string(&row.Alias) else {
                continue;
            };
            frame.push((name, row.InOctets, row.OutOctets));
        }
        FreeMibTable(table as *const core::ffi::c_void);
        Some(frame)
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
    unsafe {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut table).0 != 0 {
            return None;
        }
        let rows =
            std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize);
        let mut names = HashSet::with_capacity(rows.len());
        for row in rows {
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
