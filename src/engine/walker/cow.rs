//! Copy-on-write frame forking at blob-frame crossings.
//!
//! Shared by the insert and erase walkers. Before a mutation descends
//! into a child frame that a live snapshot may still reference, the
//! frame is forked to a fresh private GUID and the parent's `BlobNode`
//! is repointed at the fork, so the original stays frozen for the
//! snapshot. See [`crate::api::snapshot`].
//!
//! With no live snapshot the buffer manager's fork barrier is `0`, so
//! both checks below short-circuit on a single atomic load — zero work
//! on the steady-state write path.

use std::sync::Arc;

use crate::api::errors::Result;
use crate::layout::{frame_created_epoch, BlobGuid};
use crate::store::{BlobWriteGuard, BufferManager, CachedBlob};

use super::fresh_blob_guid;
use super::writers::repoint_blob_node;

/// Whether `child` may be visible to a live snapshot and so must be
/// forked before an in-place mutation.
///
/// Takes a shared latch to read the frame's creation epoch. Used on the
/// fast paths that have only *pinned* the child (not yet write-latched
/// it) and want to bail to the exclusive root path — which performs the
/// fork — without taking a write latch on a frame they will not mutate.
pub(super) fn child_is_snapshot_shared(bm: &BufferManager, child: &CachedBlob) -> bool {
    let barrier = bm.fork_barrier();
    barrier != 0 && {
        let probe = child.read();
        frame_created_epoch(probe.as_slice()) <= barrier
    }
}

/// Fork the child frame whose current image is `child_bytes` if a live
/// snapshot may reference it, repointing `parent`'s `BlobNode` at
/// `parent_slot` to the fresh private fork.
///
/// Returns the fork's GUID + pin for the caller to descend into, or
/// `None` when no live snapshot can see the child (so the caller
/// mutates it in place). `parent` must be exclusively latched by the
/// caller, which also holds the child's write guard whose bytes are
/// passed as `child_bytes`.
pub(super) fn fork_child_if_shared(
    bm: &BufferManager,
    parent: &mut BlobWriteGuard<'_>,
    child_bytes: &[u8],
    parent_slot: u16,
    seq: u64,
) -> Result<Option<(BlobGuid, Arc<CachedBlob>)>> {
    let barrier = bm.fork_barrier();
    if barrier == 0 || frame_created_epoch(child_bytes) > barrier {
        return Ok(None);
    }
    let fork_guid = fresh_blob_guid();
    let fork_pin = bm.fork_frame(child_bytes, fork_guid, seq)?;
    {
        let mut frame = parent.frame();
        repoint_blob_node(&mut frame, parent_slot, fork_guid)?;
    }
    Ok(Some((fork_guid, fork_pin)))
}
