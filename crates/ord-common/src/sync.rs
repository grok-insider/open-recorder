//! Poisoned-lock-tolerant mutex access (the project-wide `lock_tolerant`
//! pattern, see AGENTS.md).
//!
//! A panicked worker thread must degrade the program, not cascade panics
//! through every other thread that touches the same `Mutex`. All crates —
//! daemon, UI, overlay — lock through this helper instead of `lock().unwrap()`.

use std::sync::{Mutex, MutexGuard};

/// Lock a mutex, recovering from poisoning instead of panicking.
///
/// The guarded data is still consistent for our use cases (frame queues,
/// counters, small state), and continuing beats killing the daemon's
/// capture-drain pump or the UI thread.
pub fn lock_tolerant<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn locks_normally() {
        let m = Mutex::new(7);
        assert_eq!(*lock_tolerant(&m), 7);
    }

    #[test]
    fn recovers_from_poison() {
        let m = Arc::new(Mutex::new(1));
        let m2 = Arc::clone(&m);
        let _ = std::thread::spawn(move || {
            let _guard = m2.lock().unwrap();
            panic!("poison the lock");
        })
        .join();
        assert!(m.is_poisoned());
        // A tolerant lock still works and sees the data.
        *lock_tolerant(&m) += 1;
        assert_eq!(*lock_tolerant(&m), 2);
    }
}
