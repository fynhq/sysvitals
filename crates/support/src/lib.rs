//! GPUI-free shared helpers for every SysVitals process.
//!
//! The indicator processes (`app-win` / `app-mac`) and the GPUI panels all
//! log to the same daily file, install the same crash hook and — on Windows
//! — read the same HKCU probes. Keeping that plumbing here removes three
//! drifting copies; the GPUI theme layer (`sysvitals-panel`) re-exports
//! these so panel call sites stay unchanged.

/// Local date (`YYYY-MM-DD`) and time (`HH:MM:SS.mmm`) for log stamps, from
/// chrono's local clock. The display layers used to format these per
/// platform (`GetLocalTime` on Windows, `NSDate` on macOS); one clock keeps
/// every process's lines comparable.
pub fn local_now() -> (String, String) {
    let now = chrono::Local::now();
    (
        now.format("%Y-%m-%d").to_string(),
        now.format("%H:%M:%S%.3f").to_string(),
    )
}

/// Append one line to today's log file under the data dir. Logging must
/// never break an indicator or panel, so failures are silently ignored.
pub fn log_event(level: &str, message: &str) {
    let Ok(dir) = sysvitals_core::config::data_dir() else {
        return;
    };
    let (date, time) = local_now();
    sysvitals_core::logger::log_line(&dir, &date, &time, level, message);
}

/// Remove log files older than the retention window. Run once at startup;
/// the logger itself prunes on every date roll while a process stays
/// resident.
pub fn prune_old_logs() {
    let Ok(dir) = sysvitals_core::config::data_dir() else {
        return;
    };
    let (date, _) = local_now();
    sysvitals_core::logger::prune_logs(&dir, &date, sysvitals_core::logger::DEFAULT_RETENTION_DAYS);
}

/// Rust panic hook writing the panic (with location and backtrace) to the
/// log before the process dies. Shared by every binary; the Windows overlay
/// additionally installs a Win32 unhandled-exception filter on top.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let message = if let Some(text) = info.payload().downcast_ref::<&str>() {
            (*text).to_string()
        } else if let Some(text) = info.payload().downcast_ref::<String>() {
            text.clone()
        } else {
            "unknown panic".to_string()
        };
        let location = info
            .location()
            .map(|loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()))
            .unwrap_or_else(|| "unknown location".to_string());
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_string();
        let backtrace = std::backtrace::Backtrace::force_capture();
        log_event(
            "FATAL",
            &format!("panic in thread {thread} at {location}: {message} | backtrace: {backtrace}"),
        );
    }));
}

/// Encode a Rust string as a NUL-terminated UTF-16 vector for `PCWSTR`.
#[cfg(windows)]
pub fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Read a DWORD from HKCU; `None` when the key or value is absent.
#[cfg(windows)]
pub fn hkcu_dword(key: &str, value: &str) -> Option<u32> {
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, REG_DWORD,
        REG_VALUE_TYPE,
    };
    let key_w = utf16(key);
    let value_w = utf16(value);
    let mut hkey = HKEY::default();
    if unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(key_w.as_ptr()),
            None,
            KEY_READ,
            &mut hkey,
        )
    }
    .is_err()
    {
        return None;
    }
    let mut data = [0u8; 4];
    let mut size = 4u32;
    let mut kind = REG_VALUE_TYPE::default();
    let queried = unsafe {
        RegQueryValueExW(
            hkey,
            windows::core::PCWSTR(value_w.as_ptr()),
            None,
            Some(&mut kind),
            Some(data.as_mut_ptr()),
            Some(&mut size),
        )
    };
    let _ = unsafe { RegCloseKey(hkey) };
    if queried.is_ok() && size >= 4 && kind == REG_DWORD {
        Some(u32::from_le_bytes(data))
    } else {
        None
    }
}

/// The Windows OS build number, or `None` if the version probe fails.
///
/// `RtlGetVersion` is the documented way to read the real build (the classic
/// `GetVersionEx` is version-helpered and lies on 8.1+).
#[cfg(windows)]
pub fn os_build_number() -> Option<u32> {
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
    let mut info = OSVERSIONINFOW::default();
    info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
    if unsafe { RtlGetVersion(&mut info) }.is_err() {
        return None;
    }
    Some(info.dwBuildNumber)
}

/// HKCU Explorer taskbar-alignment flag (`TaskbarAl`): `1` when the Windows
/// 11 taskbar centers its icons. Read by both the overlay (placement) and
/// the settings panel (anchor choices), which must agree on the very same
/// value.
#[cfg(windows)]
pub fn taskbar_al() -> Option<u32> {
    hkcu_dword(
        "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced",
        "TaskbarAl",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_now_shapes_match_the_logger_contract() {
        let (date, time) = local_now();
        assert_eq!(date.len(), 10, "YYYY-MM-DD");
        assert_eq!(date.as_bytes()[4], b'-');
        assert_eq!(date.as_bytes()[7], b'-');
        assert!(time.len() >= 8, "HH:MM:SS at minimum");
        assert_eq!(time.as_bytes()[2], b':');
        assert_eq!(time.as_bytes()[5], b':');
    }
}
