//! Windows CPU utilization with Task Manager's semantics for the running OS.
//!
//! Task Manager's whole-machine percentage changed formula with Windows 11
//! 24H2 (KB5058411, May 2025): from that build on it reports time-based
//! `\Processor Information(_Total)\% Processor Time`, while earlier builds
//! report `\Processor Information(_Total)\% Processor Utility`, which weights
//! busy time by the CPU's current frequency. The two differ by the boost
//! ratio (measured ~1.8-2.4x on one machine), so reading the counter that
//! does not match the local Task Manager makes the indicator sit permanently
//! above or below it. The counter is therefore chosen from the OS build
//! number at startup; callers fall back to sysinfo when PDH is unavailable.
//!
//! The counter name is added through `PdhAddEnglishCounterW`, so the lookup is
//! independent of the OS display language.

use windows::core::{w, PCWSTR};
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue,
    PdhOpenQueryW, PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

use super::cpu::task_manager_is_time_based;

/// PDH returns Win32 error codes; zero is `ERROR_SUCCESS`.
const ERROR_SUCCESS: u32 = 0;

/// Build number of the running OS, or `None` if it cannot be determined.
fn os_build() -> Option<u32> {
    unsafe {
        let mut info = OSVERSIONINFOW {
            dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
            ..Default::default()
        };
        // `NTSTATUS::ok()` yields a `Result`, not an `Option`, so build the
        // `Option` from `is_ok` directly.
        RtlGetVersion(&mut info)
            .is_ok()
            .then_some(info.dwBuildNumber)
    }
}

/// Whole-machine CPU percent counter matching Task Manager on this OS build:
/// frequency-weighted `% Processor Utility` below Windows 11 24H2, time-based
/// `% Processor Time` from 24H2 on.
pub(crate) struct CpuCounter {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
    /// Rate counters need two collections; the first read only seeds.
    primed: bool,
}

impl CpuCounter {
    pub(crate) fn new() -> Option<Self> {
        // An unknown build keeps the pre-24H2 utility counter (the legacy
        // behavior); `RtlGetVersion` succeeds on every real system.
        let time_based = os_build().is_some_and(task_manager_is_time_based);
        let path = if time_based {
            w!("\\Processor Information(_Total)\\% Processor Time")
        } else {
            w!("\\Processor Information(_Total)\\% Processor Utility")
        };
        unsafe {
            let mut query = PDH_HQUERY::default();
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != ERROR_SUCCESS {
                return None;
            }
            let mut counter = PDH_HCOUNTER::default();
            let added = PdhAddEnglishCounterW(query, path, 0, &mut counter);
            if added != ERROR_SUCCESS {
                let _ = PdhCloseQuery(query);
                return None;
            }
            Some(Self {
                query,
                counter,
                primed: false,
            })
        }
    }

    /// Collect the counter and return its percentage, or `None` while priming
    /// or when PDH reports anything unusable (missing counter / invalid data).
    pub(crate) fn read(&mut self) -> Option<f64> {
        unsafe {
            if PdhCollectQueryData(self.query) != ERROR_SUCCESS {
                return None;
            }
            if !self.primed {
                self.primed = true;
                return None;
            }
            let mut value = PDH_FMT_COUNTERVALUE::default();
            if PdhGetFormattedCounterValue(self.counter, PDH_FMT_DOUBLE, None, &mut value)
                != ERROR_SUCCESS
            {
                return None;
            }
            // PDH_CSTATUS_VALID_DATA (0) and PDH_CSTATUS_NEW_DATA (1) are the
            // usable states; anything else must not reach the display.
            if value.CStatus > 1 {
                return None;
            }
            let v = value.Anonymous.doubleValue;
            v.is_finite().then_some(v)
        }
    }
}

impl Drop for CpuCounter {
    fn drop(&mut self) {
        unsafe {
            let _ = PdhCloseQuery(self.query);
        }
    }
}
