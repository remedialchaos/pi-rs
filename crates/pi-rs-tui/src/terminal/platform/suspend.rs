use std::io;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, Ordering};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static PENDING: AtomicBool = AtomicBool::new(false);

extern "C" fn on_suspend(_signal: libc::c_int) {
    PENDING.store(true, Ordering::Release);
}

pub(crate) struct SuspendSignal(libc::sigaction);

impl SuspendSignal {
    pub(crate) fn register() -> io::Result<Self> {
        if ACTIVE.swap(true, Ordering::AcqRel) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "terminal suspend handler already installed",
            ));
        }
        PENDING.store(false, Ordering::Relaxed);
        // SAFETY: An all-zero sigaction is valid once the handler and mask are set below.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = on_suspend as *const () as libc::sighandler_t;
        let mut previous = MaybeUninit::uninit();
        // SAFETY: Both structures are writable, SIGTSTP is catchable, and the handler only stores to a lock-free atomic.
        let installed = unsafe {
            libc::sigemptyset(&mut action.sa_mask) == 0
                && libc::sigaction(libc::SIGTSTP, &action, previous.as_mut_ptr()) == 0
        };
        if installed {
            // SAFETY: Successful sigaction initialized the previous action.
            Ok(Self(unsafe { previous.assume_init() }))
        } else {
            ACTIVE.store(false, Ordering::Release);
            Err(io::Error::last_os_error())
        }
    }

    pub(crate) fn take(&self) -> bool {
        PENDING.swap(false, Ordering::AcqRel)
    }
}

impl Drop for SuspendSignal {
    fn drop(&mut self) {
        // SAFETY: self.0 is the action successfully read during registration.
        unsafe { libc::sigaction(libc::SIGTSTP, &self.0, std::ptr::null_mut()) };
        ACTIVE.store(false, Ordering::Release);
    }
}
