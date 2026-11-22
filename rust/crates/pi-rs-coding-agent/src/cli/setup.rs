//! Native process setup. HTTP clients are configured independently by the runtime.

/// Set the Linux process display name. The binary already carries the app name
/// on other platforms. Native code does not have Node's warning hooks or need
/// an undici global dispatcher.
pub fn setup_cli() {
    #[cfg(target_os = "linux")]
    {
        // Linux exposes the initial thread's name as the process's comm field.
        // Use the safe nix wrapper; naming is cosmetic if the syscall is denied.
        if let Ok(name) = std::ffi::CString::new(crate::config::APP_NAME) {
            let _ = nix::sys::prctl::set_name(&name);
        }
    }
}
