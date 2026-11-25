use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use super::RawMode;

struct ExitState {
    session: u64,
    raw: Option<RawMode>,
    before: String,
    protocols: [&'static str; 4],
    after: String,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static STATE: Mutex<Option<ExitState>> = Mutex::new(None);
static INSTALLED: LazyLock<bool> = LazyLock::new(|| {
    // SAFETY: restore is a process-lifetime C ABI function with no Rust captures.
    unsafe { libc::atexit(restore) == 0 }
});

extern "C" fn restore() {
    // Do not take the stdout lock: process::exit can run while another thread is writing.
    let mut state = match STATE.try_lock() {
        Ok(state) => state,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return,
    };
    let Some(state) = state.take() else { return };
    for part in std::iter::once(state.before.as_str())
        .chain(state.protocols)
        .chain([state.after.as_str()])
    {
        let mut bytes = part.as_bytes();
        while !bytes.is_empty() {
            #[cfg(windows)]
            let count = bytes.len().min(u32::MAX as usize) as libc::c_uint;
            #[cfg(unix)]
            let count = bytes.len();
            // SAFETY: fd 1 is the current output fd; `bytes` points to `count` readable bytes.
            let written = unsafe { libc::write(1, bytes.as_ptr().cast(), count) };
            if written <= 0 {
                break;
            }
            bytes = &bytes[written as usize..];
        }
    }
    if let Some(raw) = state.raw {
        let _ = raw.restore();
    }
}

pub(in crate::terminal) struct ExitRegistration(u64);

impl ExitRegistration {
    pub(in crate::terminal) fn begin(
        raw: Option<RawMode>,
        before: &str,
        after: &str,
    ) -> io::Result<Self> {
        if !*INSTALLED {
            return Err(io::Error::other(
                "could not register terminal exit restoration",
            ));
        }
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let state = ExitState {
            session: id,
            raw,
            before: before.to_owned(),
            after: after.to_owned(),
            protocols: [""; 4],
        };
        *STATE.lock().unwrap_or_else(|error| error.into_inner()) = Some(state);
        Ok(Self(id))
    }

    pub(in crate::terminal) fn protocols(&self, parts: [&'static str; 4]) {
        if let Some(state) = STATE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_mut()
            .filter(|s| s.session == self.0)
        {
            state.protocols = parts;
        }
    }

    pub(in crate::terminal) fn framing(&self, before: &str, after: &str) {
        if let Some(state) = STATE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_mut()
            .filter(|s| s.session == self.0)
        {
            before.clone_into(&mut state.before);
            after.clone_into(&mut state.after);
        }
    }
}

impl Drop for ExitRegistration {
    fn drop(&mut self) {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        if state.as_ref().is_some_and(|state| state.session == self.0) {
            state.take();
        }
    }
}
