//! Execution modes for the native pi front door.

pub mod diagnostics_stderr;
pub mod interactive;
pub mod json_event;
pub mod print;
pub mod rpc;

#[cfg(unix)]
fn termination_signal() -> std::io::Result<impl Future<Output = u8>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut hup = signal(SignalKind::hangup())?;
    Ok(async move {
        tokio::select! {
            _ = term.recv() => 143,
            _ = hup.recv() => 129,
        }
    })
}

#[cfg(not(unix))]
fn termination_signal() -> std::io::Result<impl Future<Output = u8>> {
    Ok(std::future::pending())
}
