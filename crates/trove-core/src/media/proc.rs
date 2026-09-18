//! Subprocess slots for the import pipeline.
//!
//! A few stages shell out: `ffprobe` for the facts of a video whose container
//! the mp4 reader cannot open, `ffmpeg` for a poster frame, `heif-dec` for
//! HEIF/AVIF. Staging runs on a pool that can be a dozen threads wide (see
//! [`super::import`]), and left ungated a batch of videos or HEICs starts one
//! process per thread — twelve decoders fighting over the same cores finish
//! slower than four, and they make the whole machine unresponsive while they
//! do it. [`Cost::Proc`] names this cost; this module is what enforces it.
//!
//! Playback pipes are deliberately *not* gated: the user asked for one, and
//! there are at most a couple of them, owned by a window rather than by a
//! batch.
//!
//! [`Cost::Proc`]: super::pipeline::Cost::Proc

use std::sync::{Condvar, Mutex, OnceLock};

/// Concurrent subprocesses an import may run.
///
/// Sized against the machine rather than the pool: the work is CPU-bound per
/// process and each `ffmpeg` already uses several cores, so the useful width is
/// small — much smaller than the staging pool, which is the point.
const MAX_SLOTS: usize = 4;

/// One subprocess slot. The subprocess must finish (or the caller give up)
/// before the guard drops, which is what makes the cap a bound instead of a
/// leak.
#[must_use = "the slot is returned when the guard drops; keep it alive for the call"]
pub struct ProcessSlot {
    _private: (),
}

/// How many subprocess slots this machine offers.
///
/// [`PROC_SLOTS_ENV`] pins it when set to a positive integer, so the cap can be
/// swept from a real terminal (the same way `TROVE_STAGE_THREADS` sweeps the
/// staging pool) instead of being argued about. The value is read once per
/// process, when the first slot is taken.
pub fn slots() -> usize {
    if let Some(n) = std::env::var(PROC_SLOTS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
    {
        return n;
    }
    MAX_SLOTS.min(
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
    )
}

/// Environment override for the process cap, for benchmarks and for a machine
/// where the default is wrong.
pub const PROC_SLOTS_ENV: &str = "TROVE_PROC_SLOTS";

/// The counting behind [`slot`], separate from the global so a test can
/// exercise it without racing every other test in the process.
struct Semaphore {
    free: Mutex<usize>,
    ready: Condvar,
}

impl Semaphore {
    fn new(free: usize) -> Self {
        Self {
            free: Mutex::new(free),
            ready: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, usize> {
        self.free.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take a slot, returning `false` immediately when none is free and
    /// `wait` is false.
    fn take(&self, wait: bool) -> bool {
        let mut free = self.lock();
        if *free == 0 && !wait {
            return false;
        }
        while *free == 0 {
            free = self.ready.wait(free).unwrap_or_else(|e| e.into_inner());
        }
        *free -= 1;
        true
    }

    fn release(&self) {
        *self.lock() += 1;
        self.ready.notify_one();
    }
}

fn pool() -> &'static Semaphore {
    static POOL: OnceLock<Semaphore> = OnceLock::new();
    POOL.get_or_init(|| Semaphore::new(slots()))
}

/// Take a slot, waiting for one when they are all busy.
pub fn slot() -> ProcessSlot {
    pool().take(true);
    ProcessSlot { _private: () }
}

/// Take a slot if one is free, without waiting.
pub fn try_slot() -> Option<ProcessSlot> {
    pool().take(false).then_some(ProcessSlot { _private: () })
}

impl Drop for ProcessSlot {
    fn drop(&mut self) {
        pool().release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cap_bounds_and_dropping_a_guard_frees_a_slot() {
        let sem = Semaphore::new(2);
        assert!(sem.take(false));
        assert!(sem.take(false));
        assert!(!sem.take(false), "a full semaphore refuses a third taker");
        sem.release();
        assert!(sem.take(false));
    }

    #[test]
    fn this_machine_offers_at_least_one_slot() {
        assert!(slots() >= 1);
    }
}
