//! Terminate a recorded p3 process when its graceful shutdown route is unavailable.
#![expect(
    unsafe_code,
    reason = "Win32 has no Rust standard library API to terminate a process by ID"
)]

use std::ffi::c_void;

#[link(name = "kernel32")]
// SAFETY: the declarations match the Win32 signatures and system calling convention.
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit_handle: i32, process_id: u32) -> *mut c_void;
    fn TerminateProcess(process: *mut c_void, exit_code: u32) -> i32;
    fn CloseHandle(object: *mut c_void) -> i32;
}

pub(crate) fn terminate(pid: u32) {
    const PROCESS_TERMINATE: u32 = 1;
    // SAFETY: request only termination rights and take ownership of a non-null handle.
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if !handle.is_null() {
        // SAFETY: the handle is owned here and closed exactly once after its last use.
        unsafe {
            TerminateProcess(handle, 1);
            CloseHandle(handle);
        }
    }
}
