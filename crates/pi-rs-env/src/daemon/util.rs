//! Helpers shared by the daemon modules.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Lock a mutex, recovering the guarded value if a previous holder panicked.
///
/// The daemon keeps no state behind a mutex that is unsafe to reuse after a panic: a poisoned
/// guard only means a request thread unwound while holding it. Recovering keeps one failed
/// request from breaking every later request on the same connection.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
