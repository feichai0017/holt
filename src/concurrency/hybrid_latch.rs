//! LeanStore-style 3-mode hybrid latch.
//!
//! Three modes:
//! - `Optimistic` — no real lock taken. Reader snapshots a
//!   version counter, walks, then revalidates. Wait-free when
//!   uncontended. Validation fails if an exclusive holder lapped
//!   the snapshot.
//! - `Shared` — N readers, mutually exclusive with writers.
//! - `Exclusive` — single writer, mutually exclusive with all.
//!
//! State encoding (single `AtomicU32`):
//! - `0` = idle
//! - `1..=WRITER-1` = N shared readers
//! - `WRITER` (= `u32::MAX`) = exclusive held
//!
//! Plus an `AtomicU64` version counter, incremented on every
//! exclusive release (BEFORE clearing the exclusive flag — so
//! an optimistic reader who re-reads the flag and finds it clear
//! is guaranteed to also observe the post-bump version).

use std::hint::spin_loop;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

const WRITER: u32 = u32::MAX;

/// 3-mode latch protecting one blob frame's content.
#[derive(Debug)]
pub struct HybridLatch {
    /// Reader counter / exclusive flag.
    counter: AtomicU32,
    /// Bumped on every exclusive release.
    version: AtomicU64,
}

impl Default for HybridLatch {
    fn default() -> Self {
        Self::new()
    }
}

impl HybridLatch {
    /// Construct an idle latch.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            counter: AtomicU32::new(0),
            version: AtomicU64::new(0),
        }
    }

    // -- Optimistic --

    /// Snapshot the version. If a writer currently holds the
    /// latch, spin until it releases (a snapshot mid-write would
    /// always fail validation).
    #[must_use]
    pub fn acquire_optimistic(&self) -> u64 {
        loop {
            let v = self.version.load(Ordering::Acquire);
            if self.counter.load(Ordering::Acquire) != WRITER {
                return v;
            }
            spin_loop();
        }
    }

    /// Validate an earlier snapshot is still current. Returns
    /// false (caller must restart) if an exclusive writer
    /// released between the snapshot and now.
    #[must_use]
    pub fn validate(&self, snapshot: u64) -> bool {
        if self.counter.load(Ordering::Acquire) == WRITER {
            return false;
        }
        self.version.load(Ordering::Acquire) == snapshot
    }

    // -- Shared --

    /// Acquire shared (reader) lock. Blocks if a writer holds
    /// exclusive.
    pub fn acquire_shared(&self) {
        loop {
            let cur = self.counter.load(Ordering::Relaxed);
            if cur == WRITER || cur >= WRITER - 1 {
                spin_loop();
                continue;
            }
            if self
                .counter
                .compare_exchange_weak(cur, cur + 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Release a previously-acquired shared lock.
    pub fn release_shared(&self) {
        self.counter.fetch_sub(1, Ordering::Release);
    }

    // -- Exclusive --

    /// Acquire exclusive (writer) lock. Blocks until idle.
    pub fn acquire_exclusive(&self) {
        loop {
            if self
                .counter
                .compare_exchange_weak(0, WRITER, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
            spin_loop();
        }
    }

    /// Release a previously-acquired exclusive lock. Bumps the
    /// version counter so optimistic readers detect the change.
    pub fn release_exclusive(&self) {
        // Bump version FIRST, then clear the flag. This ordering
        // makes the "reader observes flag clear → reader observes
        // post-bump version" invariant hold under Acquire/Release.
        self.version.fetch_add(1, Ordering::Release);
        self.counter.store(0, Ordering::Release);
    }
}

/// RAII guard state — tracks what we currently hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardState {
    /// Nothing held (post-release).
    Unlocked,
    /// Optimistic snapshot taken, never validated yet.
    Optimistic,
    /// Shared / reader lock held.
    Shared,
    /// Exclusive / writer lock held.
    Exclusive,
}

/// RAII guard wrapping a `&HybridLatch`. Releases on `Drop`.
///
/// Use `Guard::optimistic(&latch)` / `Guard::shared(&latch)` /
/// `Guard::exclusive(&latch)` to acquire. Call `validate()`
/// before trusting an optimistic read.
#[derive(Debug)]
pub struct Guard<'a> {
    latch: &'a HybridLatch,
    state: GuardState,
    snapshot: u64,
}

impl<'a> Guard<'a> {
    /// Build an optimistic guard — no real lock is taken.
    #[must_use]
    pub fn optimistic(latch: &'a HybridLatch) -> Self {
        Self {
            latch,
            state: GuardState::Optimistic,
            snapshot: latch.acquire_optimistic(),
        }
    }

    /// Build a shared guard.
    #[must_use]
    pub fn shared(latch: &'a HybridLatch) -> Self {
        latch.acquire_shared();
        Self {
            latch,
            state: GuardState::Shared,
            snapshot: 0,
        }
    }

    /// Build an exclusive guard.
    #[must_use]
    pub fn exclusive(latch: &'a HybridLatch) -> Self {
        latch.acquire_exclusive();
        Self {
            latch,
            state: GuardState::Exclusive,
            snapshot: 0,
        }
    }

    /// Validate the optimistic snapshot is still current. For
    /// non-optimistic guards always returns true.
    #[must_use]
    pub fn validate(&self) -> bool {
        match self.state {
            GuardState::Optimistic => self.latch.validate(self.snapshot),
            GuardState::Shared | GuardState::Exclusive => true,
            GuardState::Unlocked => false,
        }
    }

    /// Release whatever we hold.
    pub fn release(&mut self) {
        match self.state {
            GuardState::Optimistic | GuardState::Unlocked => {}
            GuardState::Shared => self.latch.release_shared(),
            GuardState::Exclusive => self.latch.release_exclusive(),
        }
        self.state = GuardState::Unlocked;
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_bumps_version_on_release() {
        let l = HybridLatch::new();
        let v0 = l.version.load(Ordering::Relaxed);
        {
            let _g = Guard::exclusive(&l);
        }
        let v1 = l.version.load(Ordering::Relaxed);
        assert_eq!(v1, v0 + 1);
    }

    #[test]
    fn optimistic_validates_while_idle_invalidates_after_exclusive() {
        let l = HybridLatch::new();
        let g = Guard::optimistic(&l);
        assert!(g.validate());
        {
            let _w = Guard::exclusive(&l);
        }
        assert!(!g.validate());
    }

    #[test]
    fn shared_lock_counts_up_and_down() {
        let l = HybridLatch::new();
        let g1 = Guard::shared(&l);
        let g2 = Guard::shared(&l);
        assert_eq!(l.counter.load(Ordering::Relaxed), 2);
        drop(g1);
        drop(g2);
        assert_eq!(l.counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn concurrent_readers_writer_never_tear() {
        use std::sync::Arc;
        use std::thread;

        let latch = Arc::new(HybridLatch::new());
        let counter = Arc::new(AtomicU64::new(0));
        let wrong = Arc::new(AtomicU64::new(0));

        let mut handles = vec![];
        for _ in 0..4 {
            let l = latch.clone();
            let c = counter.clone();
            let w = wrong.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..500 {
                    loop {
                        let g = Guard::optimistic(&l);
                        let seen = c.load(Ordering::Relaxed);
                        if g.validate() {
                            let seen2 = c.load(Ordering::Relaxed);
                            if g.validate() && seen != seen2 {
                                w.fetch_add(1, Ordering::Relaxed);
                            }
                            break;
                        }
                    }
                }
            }));
        }
        let l = latch.clone();
        let c = counter.clone();
        let writer = thread::spawn(move || {
            for _ in 0..200 {
                let _g = Guard::exclusive(&l);
                let cur = c.load(Ordering::Relaxed);
                spin_loop();
                c.store(cur + 1, Ordering::Relaxed);
            }
        });

        for h in handles {
            h.join().unwrap();
        }
        writer.join().unwrap();

        assert_eq!(wrong.load(Ordering::Relaxed), 0);
        assert_eq!(counter.load(Ordering::Relaxed), 200);
    }
}
