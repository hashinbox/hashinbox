//! The process's one poison policy: recover the guarded value.
//!
//! Every critical section in this server is a read, an insert or a replace of
//! a whole value, and the settings connection only ever holds a transaction
//! that rolls back when a panic drops it. A panic inside any of them leaves no
//! torn state behind, so a poisoned lock holds a value as valid as before, and
//! refusing it would turn one failed request into a dead process.

use std::sync::{Mutex, MutexGuard, PoisonError};

pub(crate) trait Lock<T> {
    /// The guarded value, whether or not a holder panicked.
    fn hold(&self) -> MutexGuard<'_, T>;
}

impl<T> Lock<T> for Mutex<T> {
    fn hold(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
