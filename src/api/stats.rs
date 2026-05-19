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
}
