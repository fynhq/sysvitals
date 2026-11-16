//! Panic hook and Win32 unhandled-exception logging.

use super::*;

// Panic hook + Win32 unhandled-exception filter both log the crash reason for silent-death diagnosis.
pub(crate) fn install_crash_hooks() {
    sysvitals_support::install_panic_hook();
    unsafe {
        SetUnhandledExceptionFilter(Some(unhandled_exception_filter));
    }
}

unsafe extern "system" fn unhandled_exception_filter(info: *const EXCEPTION_POINTERS) -> i32 {
    let mut code = 0u32;
    let mut addr = 0usize;
    if !info.is_null() && !(*info).ExceptionRecord.is_null() {
        let rec = &*(*info).ExceptionRecord;
        code = rec.ExceptionCode.0 as u32;
        addr = rec.ExceptionAddress as usize;
    }
    let module = module_name_for_address(addr as *mut std::ffi::c_void);
    log_event(
            "FATAL",
            &format!(
                "unhandled exception: 0x{code:08X} at 0x{addr:016X} in {module} — process will terminate"
            ),
        );
    1
}

pub(crate) unsafe fn module_name_for_address(addr: *mut std::ffi::c_void) -> String {
    let mut hmod = windows::Win32::Foundation::HMODULE::default();
    let ok = GetModuleHandleExW(
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        windows::core::PCWSTR(addr as *const u16),
        &mut hmod,
    )
    .is_ok();
    if !ok {
        return "<unknown module>".to_string();
    }
    let mut buf = [0u16; 512];
    let n = GetModuleFileNameW(Some(hmod), &mut buf);
    if n == 0 {
        return "<unknown module>".to_string();
    }
    let path = String::from_utf16_lossy(&buf[..n as usize]);
    path.rsplit(['\\', '/'])
        .next()
        .unwrap_or("<unknown module>")
        .to_string()
}
