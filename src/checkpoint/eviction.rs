//! Eviction worker thread — periodically scans the BM cache and
//! drops cold non-dirty entries.
//!
//! ## Why a separate thread
//!
//! The inline overflow LRU on `insert_into_cache` only runs when
//! the cache is growing past `capacity`. Once the buffer pool
//! stops growing, no eviction happens — every loaded blob would
//! stay resident.
//!
//! The eviction thread runs on its own cadence
//! (`CheckpointConfig::eviction_interval`) and uses a
//! `last_touched` tick per entry (stamped by
//! `BufferManager::get_cached` / `pin`) to find genuinely cold
//! entries — ones not accessed in the last
//! `eviction_idle_ticks` operations on the BM.
//!
//! ## Safety
//!
//! Eviction is non-blocking for readers/writers: it scans a clone
//! of the BM cache map (`snapshot_entries`), filters candidates,
//! and only calls `try_evict_cold` for entries where the snapshot
//! had `strong_count > 1` (the snapshot's own Arc clone). Inside
//! `try_evict_cold` the BM re-checks `strong_count == 1` under
//! the cache mutex (the snapshot's Arc clone has been dropped by
//! then) before removing.
//!
//! Dirty entries are exempt — `try_evict_cold` consults the BM
//! dirty map before evicting.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;

use super::Shared;

pub(super) fn run(shared: &Arc<Shared>) {
    loop {
        if shared.eviction_stop.load(Ordering::Acquire) {
            break;
        }
        thread::park_timeout(shared.cfg.eviction_interval);
        if shared.eviction_stop.load(Ordering::Acquire) {
            break;
        }
        let evicted = run_scan(shared);
        shared.evictions.fetch_add(evicted, Ordering::Relaxed);

        #[cfg(feature = "tracing")]
        if evicted > 0 {
            tracing::debug!(
                target: "holt::checkpoint::eviction",
                evicted = evicted,
                "eviction scan complete",
            );
        }
    }
}

fn run_scan(shared: &Arc<Shared>) -> u64 {
    let now = shared.bm.clock_tick();
    let threshold = shared.cfg.eviction_idle_ticks;

    // Snapshot under brief BM-state lock, then release. Each
    // entry in `snapshot` carries its own `Arc<CachedBlob>` clone,
    // so `try_evict_cold` calls below see `strong_count >= 2` for
    // every snapshotted GUID until we drop the local clone.
    let snapshot = shared.bm.snapshot_entries();

    let mut evicted = 0u64;
    for (guid, entry) in snapshot {
        let last = entry.last_touched();
        // Wrap-safe staleness check: `now >= last` always, since
        // ticks are monotonic and `last` was stamped before `now`
        // was sampled. Threshold gate prevents evicting fresh
        // entries.
        if now.saturating_sub(last) < threshold {
            continue;
        }
        // Drop our snapshot's Arc clone so `try_evict_cold` sees
        // `strong_count == 1` (just the BM cache map).
        drop(entry);
        if shared.bm.try_evict_cold(guid) {
            evicted += 1;
        }
    }
    evicted
}
