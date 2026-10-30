//! Windows CPU utilization with Task Manager's semantics.
//!
//! Task Manager's Processes and Performance tabs report
//! `\Processor Information(_Total)\% Processor Utility`, which weights busy
//! time by the CPU's current frequency. `sysinfo` reports the older
//! `\Processor(*)\% Idle Time` based "% Processor Time", which does not track
//! Task Manager on CPUs that boost above their nominal clock (the two can
//! differ by ~1.8x under identical load). Reading the utility counter directly
//! is what makes the indicator agree with Task Manager; callers fall back to
//! sysinfo when PDH is unavailable.
//!
//! The counter name is added through `PdhAddEnglishCounterW`, so the lookup is
//! independent of the OS display language.

use windows::core::{w, PCWSTR};
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue,
    PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
};

/// PDH returns Win32 error codes; zero is `ERROR_SUCCESS`.
const ERROR_SUCCESS: u32 = 0;

pub(crate) struct ProcessorUtility {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
    /// Rate counters need two collections; the first read only seeds.
    primed: bool,
}

impl ProcessorUtility {
    pub(crate) fn new() -> Option<Self> {
        unsafe {
            let mut query = PDH_HQUERY::default();
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != ERROR_SUCCESS {
                return None;
            }
            let mut counter = PDH_HCOUNTER::default();
            let added = PdhAddEnglishCounterW(
                query,
                w!("\\Processor Information(_Total)\\% Processor Utility"),
                0,
                &mut counter,
            );
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

impl Drop for ProcessorUtility {
    fn drop(&mut self) {
        unsafe {
            let _ = PdhCloseQuery(self.query);
        }
    }
}
