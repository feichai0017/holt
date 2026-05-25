use std::hint::spin_loop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

const ENDPOINT_LOCK_SHARDS: usize = 256;
const ADAPTIVE_SPINS: u32 = 64;

/// Fixed-shard locks for multi-key operation endpoints.
///
/// Multi-key operations mutate two logical endpoints (`src`, `dst`).
/// Locking only those endpoint shards keeps unrelated operations
/// concurrent, while canonical shard ordering prevents AB/BA
/// deadlock.
pub(crate) struct EndpointLocks {
    shards: [EndpointShard; ENDPOINT_LOCK_SHARDS],
}

impl EndpointLocks {
    pub(crate) fn new() -> Self {
        Self {
            shards: std::array::from_fn(|_| EndpointShard::new()),
        }
    }

    pub(crate) fn lock_key<'a>(&'a self, key: &[u8]) -> EndpointLockGuard<'a> {
        EndpointLockGuard {
            _first: self.shards[shard_index(key)].lock(),
            _second: None,
        }
    }

    pub(crate) fn lock_pair<'a>(&'a self, src: &[u8], dst: &[u8]) -> EndpointLockGuard<'a> {
        let src_idx = shard_index(src);
        let dst_idx = shard_index(dst);
        if src_idx == dst_idx {
            return EndpointLockGuard {
                _first: self.shards[src_idx].lock(),
                _second: None,
            };
        }

        let (first_idx, second_idx) = if src_idx < dst_idx {
            (src_idx, dst_idx)
        } else {
            (dst_idx, src_idx)
        };
        let first = self.shards[first_idx].lock();
        let second = self.shards[second_idx].lock();
        EndpointLockGuard {
            _first: first,
            _second: Some(second),
        }
    }
}

pub(crate) struct EndpointLockGuard<'a> {
    _first: EndpointShardGuard<'a>,
    _second: Option<EndpointShardGuard<'a>>,
}

struct EndpointShard {
    locked: AtomicBool,
}

impl EndpointShard {
    const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> EndpointShardGuard<'_> {
        let mut spins = 0;
        loop {
            if self
                .locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return EndpointShardGuard { shard: self };
            }
            adaptive_wait(&mut spins);
        }
    }
}

struct EndpointShardGuard<'a> {
    shard: &'a EndpointShard,
}

impl Drop for EndpointShardGuard<'_> {
    fn drop(&mut self) {
        self.shard.locked.store(false, Ordering::Release);
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

fn shard_index(key: &[u8]) -> usize {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in key {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (h as usize) & (ENDPOINT_LOCK_SHARDS - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn reversed_endpoint_order_does_not_deadlock() {
        let locks = Arc::new(EndpointLocks::new());
        let a = b"bucket-a/object-1".to_vec();
        let b = b"bucket-b/object-2".to_vec();
        let (tx, rx) = std::sync::mpsc::channel();

        for _ in 0..2 {
            let locks = Arc::clone(&locks);
            let a = a.clone();
            let b = b.clone();
            let tx = tx.clone();
            thread::spawn(move || {
                for _ in 0..10_000 {
                    {
                        let _guard = locks.lock_pair(&a, &b);
                    }
                    {
                        let _guard = locks.lock_pair(&b, &a);
                    }
                }
                tx.send(()).unwrap();
            });
        }
        drop(tx);

        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }
}
