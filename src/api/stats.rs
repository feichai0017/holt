//! Per-blob and tree-wide counter snapshots returned by
//! [`Tree::stats`](crate::Tree::stats).
//!
//! These mirror the `BlobHeader`
//! counter fields and are read in a single shared-guard pass
//! per blob, so a snapshot of any one blob is internally
//! consistent but the cross-blob aggregate is not linearised
//! against concurrent writers.

use crate::layout::BlobGuid;

/// Per-blob counters captured by [`Tree::stats`](crate::Tree::stats).
///
/// Each field mirrors a `BlobHeader`
/// counter and is read in a single shared-guard pass over the blob.
#[derive(Debug, Clone, Copy)]
pub struct BlobStats {
    /// GUID identifying this blob within the tree.
    pub guid: BlobGuid,
    /// Bytes currently consumed in the blob's data area (bump
    /// cursor, monotonically advancing).
    pub space_used: u32,
    /// Bytes lost to bump-allocator waste (extents freed without
    /// recycling). Compaction reclaims this to zero.
    pub gap_space: u32,
    /// High-water slot count — slot indices `[1, num_slots)` have
    /// ever held a node body in this blob.
    pub num_slots: u16,
    /// Count of cross-blob crossings (`BlobNode` slots) currently
    /// installed in this blob.
    pub num_ext_blobs: u16,
    /// Number of times this blob has been rebuilt by the in-place
    /// compactor. Bumped at the end of every successful compaction.
    pub compact_times: u32,
    /// Count of leaves in this blob currently in tombstone state
    /// (soft-deleted, awaiting reclaim by compaction).
    pub tombstone_leaf_cnt: u32,
}

/// Tree-wide aggregate counters from [`Tree::stats`](crate::Tree::stats).
///
/// `blobs` carries the per-blob breakdown in BFS order from the
/// root; the totals are pre-summed for the common "how big is the
/// tree?" question. All bytes / counts are sums over `blobs`.
#[derive(Debug, Clone)]
pub struct TreeStats {
    /// Number of distinct blobs reachable from the tree root.
    pub blob_count: u32,
    /// Sum of `space_used` over every blob.
    pub total_space_used: u64,
    /// Sum of `gap_space` over every blob.
    pub total_gap_space: u64,
    /// Sum of `num_slots` over every blob.
    pub total_slots: u64,
    /// Sum of `compact_times` over every blob (lifetime
    /// compactions across the whole tree).
    pub total_compactions: u64,
    /// Sum of `tombstone_leaf_cnt` over every blob.
    pub total_tombstones: u64,
    /// Per-blob breakdown in BFS order from the root.
    pub blobs: Vec<BlobStats>,
    /// Number of blobs currently dirty in the BufferManager —
    /// modified in cache but not yet flushed to backend. With the
    /// background checkpointer enabled this stays bounded by the
    /// checkpoint cadence; without it, it tracks the user's
    /// explicit `Tree::checkpoint` schedule.
    pub bm_dirty_count: usize,
    /// Number of blobs queued for deferred backend deletion —
    /// children unlinked by an erase walker's `SubtreeGone` path
    /// or by a merge pass, waiting for the next checkpoint round
    /// (or `Tree::checkpoint`) to issue the actual
    /// `backend.delete_blob` + manifest re-sync.
    pub bm_pending_delete_count: usize,
    /// Cumulative cache lookups served from BM cache without
    /// going to the inner backend. Read by external observers to
    /// derive a hit rate (`bm_cache_hits / (bm_cache_hits +
    /// bm_cache_misses)`); higher is better.
    pub bm_cache_hits: u64,
    /// Cumulative cache lookups that fell through to
    /// `inner_backend.read_blob` because the entry was absent or
    /// evicted. Tracks cold-start + eviction churn.
    pub bm_cache_misses: u64,
    /// Cumulative wait-free read restarts in `Tree::get` — each
    /// one means a concurrent writer lapped an optimistic
    /// snapshot and the lookup walked the tree from scratch.
    /// Spikes here indicate writer/reader contention.
    pub bm_optimistic_restarts: u64,
    /// Cumulative mutation walker invocations (`insert_multi` /
    /// `erase_multi`). `rename` and `txn` count their inner walker
    /// calls separately.
    pub bm_walker_ops: u64,
    /// Total blob hops across mutation walker invocations. Divide
    /// by [`Self::bm_walker_ops`] for the average.
    pub bm_walker_blob_hops: u64,
    /// Maximum blob hops observed for one mutation walker call.
    pub bm_max_blob_hops: u64,
    /// Largest key-depth at which a mutation walker entered a blob.
    /// This is a cross-blob boundary-depth signal, not a full
    /// per-node ART-depth trace.
    pub bm_max_cross_blob_depth: u64,
    /// Successful foreground spillover events.
    pub bm_spillovers: u64,
    /// `BlobNode` children folded back into parents by manual
    /// compact or background merge passes.
    pub bm_merges: u64,
    /// WAL/journal worker counters, or `None` for memory trees and
    /// caller-supplied backends opened without holt's WAL.
    pub journal: Option<JournalStats>,
    /// Background checkpointer telemetry, or `None` if the bg
    /// thread group isn't running (the default; opt in via
    /// [`crate::CheckpointConfig::enabled`]).
    pub checkpointer: Option<CheckpointerStats>,
}

impl TreeStats {
    /// Average blob hops per mutation walker invocation.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // observability gauge; exact integer totals are exposed too
    pub fn bm_avg_blob_hops(&self) -> f64 {
        if self.bm_walker_ops == 0 {
            0.0
        } else {
            self.bm_walker_blob_hops as f64 / self.bm_walker_ops as f64
        }
    }
}

/// Snapshot of the WAL group-commit worker's counters.
#[derive(Debug, Clone, Copy)]
pub struct JournalStats {
    /// Number of WAL append requests submitted by foreground
    /// mutation paths.
    pub appends: u64,
    /// Number of append batches processed by the journal worker.
    /// Under concurrent durable writers this should be lower than
    /// [`Self::appends`].
    pub batches: u64,
    /// Number of `sync_data` calls issued by the journal worker,
    /// including explicit checkpoint flush barriers.
    pub syncs: u64,
}

/// Snapshot of the background checkpointer's accumulated
/// counters. Returned inside [`TreeStats::checkpointer`] when the
/// thread group is enabled. All counters are cumulative since
/// the threads were spawned.
#[derive(Debug, Clone, Copy)]
pub struct CheckpointerStats {
    /// Rounds the planner has started — succeeded + failed
    /// combined. Empty rounds (no dirty entries, nothing to
    /// merge) still increment.
    pub rounds_attempted: u64,
    /// Rounds that completed without an error path. A successful
    /// round can still have done no flush work — see
    /// [`Self::blobs_flushed`] for actual durability progress.
    pub rounds_succeeded: u64,
    /// Cumulative number of blob commits the I/O thread has
    /// processed across all rounds.
    pub blobs_flushed: u64,
    /// Total `BlobNode` crossings folded back into their parents
    /// across every merge pass.
    pub merges_total: u64,
    /// WAL `truncate` calls — once per round where the planner
    /// observed `dirty_count == 0` under the WAL lock after
    /// flushing.
    pub truncates: u64,
    /// Cache entries the eviction thread has dropped because
    /// they were cold + non-dirty + held only by the cache.
    pub evictions: u64,
}
