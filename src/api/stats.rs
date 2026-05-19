//! Per-blob and tree-wide counter snapshots returned by
//! [`Tree::stats`](crate::Tree::stats).
//!
//! These mirror the [`BlobHeader`](crate::layout::BlobHeader)
//! counter fields and are read in a single shared-guard pass
//! per blob, so a snapshot of any one blob is internally
//! consistent but the cross-blob aggregate is not linearised
//! against concurrent writers.

use crate::layout::BlobGuid;

/// Per-blob counters captured by [`Tree::stats`](crate::Tree::stats).
///
/// Each field mirrors a [`BlobHeader`](crate::layout::BlobHeader)
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
    /// Background checkpointer telemetry, or `None` if the bg
    /// thread group isn't running (the default; opt in via
    /// [`crate::CheckpointConfig::enabled`]).
    pub checkpointer: Option<CheckpointerStats>,
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
