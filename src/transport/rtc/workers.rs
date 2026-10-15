//! Peer tasks use the shared cancellation/admission/draining contract.
pub(super) use crate::lifecycle::TaskScope as SessionWorkers;

pub(super) fn std_mutex_lock<T>(lock: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
