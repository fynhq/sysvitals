//! Windows CPU utilization with Task Manager semantics.

use windows::core::{w, PCWSTR};
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue,
    PdhOpenQueryW, PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

use super::cpu::task_manager_is_time_based;

const ERROR_SUCCESS: u32 = 0;

fn os_build() -> Option<u32> {
    unsafe {
        let mut info = OSVERSIONINFOW {
            dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
            ..Default::default()
        };
        RtlGetVersion(&mut info)
            .is_ok()
            .then_some(info.dwBuildNumber)
    }
}

pub(crate) struct CpuCounter {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
    primed: bool,
}

impl CpuCounter {
    pub(crate) fn new() -> Option<Self> {
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
