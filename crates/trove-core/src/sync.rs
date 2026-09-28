//! Poison-tolerant locking.
//!
//! Every shared lock in the app is held across code that can panic — a task
//! body, a render callback, a plugin hook. A panic poisons the mutex, and the
//! ordinary `.lock().unwrap()` then turns the *next* access into a second
//! panic: one bad job would take the whole task board down with it, long after
//! the failure it came from.
//!
//! [`lock`] reads through the poison instead (`PoisonError::into_inner`). That
//! is sound for the state here because the guards protect short, self-contained
//! mutations — push an event, bump a counter, swap a `Mode` — never a
//! multi-step invariant that a panic mid-way could tear in half. A lock whose
//! data *could* be left inconsistent should be redesigned, not recovered from.

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Lock `mutex`, recovering the guard if a previous holder panicked.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Take a read guard, recovering if the lock was poisoned.
pub fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Take a write guard, recovering if the lock was poisoned.
pub fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::{lock, read, write};
    use std::sync::{Mutex, RwLock};

    #[test]
    fn a_poisoned_lock_still_hands_out_its_data() {
        let mutex = Mutex::new(7_u32);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = mutex.lock().unwrap();
            panic!("poison the mutex");
        }));
        assert_eq!(*lock(&mutex), 7, "the value survives a poisoned lock");
    }

    #[test]
    fn a_poisoned_rwlock_reads_and_writes_again() {
        let rw = RwLock::new(1_u32);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = rw.write().unwrap();
            panic!("poison the rwlock");
        }));
        assert_eq!(*read(&rw), 1);
        *write(&rw) = 2;
        assert_eq!(*read(&rw), 2);
    }
}
