//! Shared/exclusive admission gate.
//!
//! This is deliberately smaller than `std::sync::RwLock`: callers
//! only need admission control, not protected data access. Shared
//! entry is one atomic CAS in the uncontended case; exclusive entry
//! sets a pending bit before waiting for shared entrants to drain so
//! new shared entrants cannot starve it.

use std::hint::spin_loop;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

const WRITE_BIT: usize = 1usize << (usize::BITS - 1);
const COUNT_MASK: usize = WRITE_BIT - 1;
const ADAPTIVE_SPINS: u32 = 64;

#[derive(Debug)]
pub(crate) struct Gate {
    state: AtomicUsize,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    #[must_use]
    pub(crate) const fn new() -> Self {
        Self {
            state: AtomicUsize::new(0),
        }
    }

    pub(crate) fn enter_shared(&self) -> GateReadGuard<'_> {
        let mut spins = 0;
        loop {
            let cur = self.state.load(Ordering::Relaxed);
            if cur & WRITE_BIT == 0
                && cur & COUNT_MASK != COUNT_MASK
                && self
                    .state
                    .compare_exchange_weak(cur, cur + 1, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            {
                return GateReadGuard { gate: self };
            }
            adaptive_wait(&mut spins);
        }
    }

    pub(crate) fn enter_exclusive(&self) -> GateWriteGuard<'_> {
        let mut spins = 0;
        loop {
            let cur = self.state.load(Ordering::Relaxed);
            if cur & WRITE_BIT != 0 {
                adaptive_wait(&mut spins);
                continue;
            }
            if self
                .state
                .compare_exchange_weak(cur, cur | WRITE_BIT, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                let mut drain_spins = 0;
                while self.state.load(Ordering::Acquire) & COUNT_MASK != 0 {
                    adaptive_wait(&mut drain_spins);
                }
                return GateWriteGuard { gate: self };
            }
            adaptive_wait(&mut spins);
        }
    }

    fn leave_shared(&self) {
        self.state.fetch_sub(1, Ordering::Release);
    }

    fn leave_exclusive(&self) {
        self.state.store(0, Ordering::Release);
    }

    #[cfg(test)]
    fn writer_pending_for_test(&self) -> bool {
        self.state.load(Ordering::Acquire) & WRITE_BIT != 0
    }
}

fn adaptive_wait(spins: &mut u32) {
    if *spins < ADAPTIVE_SPINS {
        *spins += 1;
        spin_loop();
    } else {
        thread::yield_now();
    }
}

#[derive(Debug)]
pub(crate) struct GateReadGuard<'a> {
    gate: &'a Gate,
}

impl Drop for GateReadGuard<'_> {
    fn drop(&mut self) {
        self.gate.leave_shared();
    }
}

#[derive(Debug)]
pub(crate) struct GateWriteGuard<'a> {
    gate: &'a Gate,
}

impl Drop for GateWriteGuard<'_> {
    fn drop(&mut self) {
        self.gate.leave_exclusive();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn exclusive_waits_for_shared_guard() {
        let gate = Arc::new(Gate::new());
        let shared = gate.enter_shared();
        let worker_gate = Arc::clone(&gate);
        let (started_tx, started_rx) = sync_channel(0);
        let (done_tx, done_rx) = sync_channel(0);
        let handle = thread::spawn(move || {
            started_tx.send(()).unwrap();
            let _exclusive = worker_gate.enter_exclusive();
            done_tx.send(()).unwrap();
        });

        started_rx.recv().unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(shared);
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn pending_exclusive_blocks_new_shared_entries() {
        let gate = Arc::new(Gate::new());
        let shared = gate.enter_shared();

        let exclusive_gate = Arc::clone(&gate);
        let (exclusive_started_tx, exclusive_started_rx) = sync_channel(0);
        let (release_tx, release_rx) = sync_channel(0);
        let exclusive = thread::spawn(move || {
            exclusive_started_tx.send(()).unwrap();
            let _exclusive = exclusive_gate.enter_exclusive();
            release_rx.recv().unwrap();
        });
        exclusive_started_rx.recv().unwrap();
        while !gate.writer_pending_for_test() {
            spin_loop();
        }

        let shared_gate = Arc::clone(&gate);
        let (shared_done_tx, shared_done_rx) = sync_channel(0);
        let shared_waiter = thread::spawn(move || {
            let _shared = shared_gate.enter_shared();
            shared_done_tx.send(()).unwrap();
        });

        drop(shared);
        assert!(
            shared_done_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "new shared entrant must wait behind pending exclusive"
        );

        release_tx.send(()).unwrap();
        exclusive.join().unwrap();
        shared_done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        shared_waiter.join().unwrap();
    }
}
