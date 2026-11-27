//! Unix signal delivery for the CLI and its isolated worker process groups.
#![expect(
    unsafe_code,
    reason = "POSIX kill is the OS interface for process groups"
)]

// SAFETY: signature matches the POSIX kill(pid_t, int) ABI on supported Unix targets.
unsafe extern "C" {
    fn kill(pid: std::ffi::c_int, sig: std::ffi::c_int) -> std::ffi::c_int;
}

pub(crate) fn signal(pid: u32, name: &str) {
    let sig = match name {
        "TERM" => 15,
        "KILL" => 9,
        _ => return,
    };
    if let Ok(pid @ 1..) = i32::try_from(pid) {
        // SAFETY: kill takes only integers. A positive pid addresses just that process.
        unsafe { kill(pid, sig) };
    }
}

pub(crate) fn kill_group(pid: u32) -> bool {
    if let Ok(pid @ 2..) = i32::try_from(pid) {
        // SAFETY: the worker starts as a group leader. Excluding 0 and 1 prevents
        // addressing the caller's process group or all signalable processes.
        return unsafe { kill(-pid, 9) } == 0;
    }
    false
}
