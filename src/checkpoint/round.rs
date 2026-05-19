//! One checkpoint round — the planner's main work unit, also
//! invoked synchronously by `Checkpointer::Drop` to drain in-flight
//! dirty state before the Tree handle disappears.
//!
//! ## Sequence
//!
//! 0. **Merge pass** (optional, controlled by
//!    `CheckpointConfig::auto_merge`) — walks every reachable blob
//!    and folds any mergeable child back into its parent. Inline
//!    `bm.commit` per merge so the manifest deletion + parent's
//!    new bytes both reach the backend before the round's `Sync`
//!    at step 5.
//! 1. **Snapshot dirty** — atomically drain the BM dirty map.
//!    Concurrent writers' new `mark_dirty` lands in a fresh empty
//!    map and gets picked up by the next round.
//! 2. **Flush WAL** — `sync_data` the writer so every record that
//!    mirrors a snapshotted seq is durable before we drop it.
//! 3. **Submit `Flush` tasks** — snapshot bytes per dirty blob via
//!    `bm.snapshot_bytes` (memcpy under a brief shared read guard),
//!    move the bytes into an `IoTask::Flush`, and push the task to
//!    the I/O thread.
//! 4. **Collect completions** — wait for each task's one-shot
//!    completion. On any failure, restore the corresponding dirty
//!    entry via `bm.restore_dirty` so the next round retries.
//! 5. **Submit `Sync`** — one `IoTask::Sync` after every `Flush`
//!    landed. `fdatasync` of the inner backend, including the
//!    PersistentBackend's manifest persist.
//! 6. **Truncate WAL** — only when (a) no `Flush` failed AND (b)
//!    `bm.dirty_count() == 0` checked **under the WAL lock**. The
//!    interlock with the writer-side `mark_dirty → wal.lock`
//!    ordering ensures we never drop a record whose effect isn't
//!    already in backend.
//!
//! This function is called from two places:
//!
//! - The `checkpoint_thread` main loop in [`super::mod`]
//!   (background path).
//! - `Checkpointer::Drop` (synchronous final round on the calling
//!   thread, after the planner has joined and writers are
//!   guaranteed to be gone).

use crossbeam_channel::bounded;
use std::collections::HashMap;
use std::sync::Arc;

use crate::api::errors::{Error, Result};
use crate::engine;
use crate::layout::BlobGuid;
use crate::store::backend::Backend;
use crate::store::BlobFrame;

use super::io::IoTask;
use super::Shared;

// The round is intentionally a single linear function so the 6
// phases stay readable as one story. Splitting it into helpers
// would hide the interlock between WAL flush / per-blob commit /
// dirty restore / truncate gate.
#[allow(clippy::too_many_lines)]
pub(super) fn run_round(shared: &Arc<Shared>) -> Result<()> {
    use std::sync::atomic::Ordering;

    shared.rounds_attempted.fetch_add(1, Ordering::Relaxed);

    // 0. Optional tree-wide merge pass.
    let merged = if shared.cfg.auto_merge {
        match run_merge_pass(shared) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("holt: checkpoint merge pass failed: {e}");
                0
            }
        }
    } else {
        0
    };
    shared.merges_total.fetch_add(merged, Ordering::Relaxed);

    #[cfg(feature = "tracing")]
    let round_start = std::time::Instant::now();

    // 1+2. Snapshot dirty AND pending-deletes + flush WAL, all
    // under the same wal lock so both sets we drain are closed
    // against in-flight writers (W2D-strict). The writer-side
    // protocol holds wal.lock for the duration of walker.mutate
    // + mark_dirty / mark_for_delete + wal.append, so any entry
    // visible here has its WAL record already buffered in the
    // writer — the trailing `wal.flush` makes it durable before
    // we touch backend.
    //
    // If `snapshot_pending_deletes` were taken outside this
    // wal.lock block, a writer could (a) take the lock, (b)
    // walker.erase that hits `SubtreeGone` (which calls
    // `mark_for_delete`), (c) wal.append the erase record, (d)
    // release the lock, before we snapshot pending; we'd then
    // execute `backend.delete_blob` and re-Sync manifest while
    // the writer's WAL record was still only in the writer's
    // buffer. A crash there would leave the manifest ahead of
    // WAL — exactly the W2D violation deferred-delete was
    // designed to prevent.
    //
    // No-WAL trees (memory mode, user-supplied backend) skip
    // the lock; concurrency safety there is the user's contract.
    let (snap, pending) = if let Some(wal) = &shared.wal {
        let mut w = wal.lock().unwrap();
        let snap = shared.bm.snapshot_dirty();
        let pending = shared.bm.snapshot_pending_deletes();
        if let Err(e) = w.flush() {
            shared.bm.restore_dirty(snap);
            shared.bm.restore_pending_deletes(pending);
            return Err(e);
        }
        (snap, pending)
    } else {
        (
            shared.bm.snapshot_dirty(),
            shared.bm.snapshot_pending_deletes(),
        )
    };
    let snap_count = snap.len();
    shared.last_dirty_count.store(snap_count, Ordering::Relaxed);

    // Early-skip only when nothing at all needs attention. A
    // pending deferred-delete from a previous round (e.g. one
    // whose `backend.delete_blob` or trailing Sync failed and
    // got restored) was already drained above; check the
    // snapshot's length so we don't bail out on something we
    // just picked up.
    if snap.is_empty() && merged == 0 && pending.is_empty() {
        shared.rounds_succeeded.fetch_add(1, Ordering::Relaxed);
        #[cfg(feature = "tracing")]
        tracing::trace!(target: "holt::checkpoint", "round skipped — nothing dirty");
        return Ok(());
    }

    // 3. Snapshot bytes + submit Flush tasks.
    let mut completions: Vec<(BlobGuid, u64, crossbeam_channel::Receiver<Result<()>>)> =
        Vec::with_capacity(snap.len());
    let mut failed: HashMap<BlobGuid, u64> = HashMap::new();

    for (guid, txn_id) in &snap {
        // If the blob isn't in cache (eviction raced us, or it was
        // never loaded), skip — `mark_dirty` should never have
        // fired on an uncached blob, but be defensive.
        let Some(bytes) = shared.bm.snapshot_bytes(*guid) else {
            continue;
        };
        let (tx, rx) = bounded(1);
        let task = IoTask::Flush {
            guid: *guid,
            bytes,
            // Carry the drained dirty seq so the I/O worker can
            // tell "no writer raced us" (safe to retire the dirty
            // entry) from "racer landed" (must leave the new
            // entry alone for the next round).
            expected_seq: *txn_id,
            on_done: tx,
        };
        if shared.io_tx.send(task).is_err() {
            // I/O thread is gone (Drop is mid-sequence on another
            // path) — restore EVERYTHING we drained at step 1 so
            // the next round retries:
            //  - dirty entries we haven't yet handed off to the
            //    worker, AND those still in-flight as completions
            //    we'll never collect;
            //  - the whole `pending` snapshot — we never reached
            //    phase 6, and dropping it would lose unlink intent.
            for (g, t) in &snap {
                failed.entry(*g).or_insert(*t);
            }
            shared.bm.restore_dirty(failed);
            shared.bm.restore_pending_deletes(pending);
            return Err(Error::Internal("checkpoint: I/O worker channel closed mid-round",
            ));
        }
        completions.push((*guid, *txn_id, rx));
    }

    // 4. Collect completions.
    for (guid, txn_id, rx) in completions {
        match rx.recv() {
            Ok(Ok(())) => {
                shared.blobs_flushed.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Err(e)) => {
                eprintln!(
                    "holt: checkpoint flush failed for blob {:02x?} (min_txn={txn_id}): {e}",
                    &guid[..4]
                );
                failed.insert(guid, txn_id);
            }
            Err(_) => {
                // Sender dropped before sending — I/O thread died.
                failed.insert(guid, txn_id);
            }
        }
    }

    let had_dirty_failure = !failed.is_empty();
    if had_dirty_failure {
        shared.bm.restore_dirty(failed.clone());
    }

    // 5. Pre-delete Sync — every successful Flush above retired
    //    its dirty entry via write_through CAS; we must still
    //    fsync so those bytes are stable on disk before phase 6
    //    mutates the manifest. Each early-return path restores
    //    `pending` because phase 6 won't run.
    let (sync_tx, sync_rx) = bounded(1);
    if shared
        .io_tx
        .send(IoTask::Sync { on_done: sync_tx })
        .is_err()
    {
        shared.bm.restore_pending_deletes(pending);
        return Err(Error::Internal("checkpoint: I/O worker channel closed before Sync",
        ));
    }
    match sync_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            eprintln!("holt: checkpoint backend Sync failed: {e}");
            shared.bm.restore_pending_deletes(pending);
            return Err(e);
        }
        Err(_) => {
            shared.bm.restore_pending_deletes(pending);
            return Err(Error::Internal("checkpoint: I/O worker dropped Sync completion",
            ));
        }
    }

    // 5.5. Abort-on-dirty-failure gate. A failed parent write must
    //      NOT propagate to a manifest delete of its dependent
    //      child — that would orphan the parent's `BlobNode`
    //      pointer (parent on-disk still points to the child;
    //      manifest no longer has the child; WAL replay's walker
    //      descent would fail to read the deleted child). Restore
    //      `pending` and bail; the next round retries the parent
    //      write and only then processes its child's deletion.
    if had_dirty_failure {
        shared.bm.restore_pending_deletes(pending);
        return Err(Error::Internal("checkpoint: dirty write failed — pending deletes deferred to next round"));
    }

    // 6. Apply pending deletes — `pending` was already drained in
    //    step 1 under the wal.lock, so the writer-side WAL records
    //    covering each unlink op are durable on disk (via the
    //    step-2 wal.flush). Phase 5 has fsync'd the per-blob writes
    //    that the manifest delete is allowed to follow. Safe to
    //    mutate the manifest now; the trailing re-Sync at step 7
    //    persists it.
    let pending_count = pending.len();
    let mut pending_failed: HashMap<BlobGuid, u64> = HashMap::new();
    for (guid, seq) in &pending {
        if let Err(e) = shared.bm.execute_pending_delete(*guid) {
            eprintln!(
                "holt: checkpoint deferred delete failed for blob {:02x?} (seq={seq}): {e}",
                &guid[..4]
            );
            pending_failed.insert(*guid, *seq);
        }
    }
    if !pending_failed.is_empty() {
        shared.bm.restore_pending_deletes(pending_failed.clone());
    }

    // 7. Re-Sync iff we actually deleted anything — the manifest
    //    mutation at step 6 is in-memory until `backend.flush`
    //    rewrites the manifest file. Skip the syscall when the
    //    pending set was empty.
    let applied_deletes = pending_count - pending_failed.len();
    // Helper: on Sync failure here the manifest deletions we
    // already applied at step 6 are stuck in-memory. We can't
    // re-`execute_pending_delete` them (the slot is already
    // gone from the manifest map and the call is idempotent),
    // but we MUST keep them in the pending-delete set so the
    // truncate gate stays closed and the next round retries the
    // Sync. Re-registering with the same seq is idempotent
    // (min-merge in `restore_pending_deletes`).
    let restore_applied = || -> HashMap<BlobGuid, u64> {
        pending
            .iter()
            .filter(|(g, _)| !pending_failed.contains_key(*g))
            .map(|(g, s)| (*g, *s))
            .collect()
    };
    if applied_deletes > 0 {
        let (sync_tx2, sync_rx2) = bounded(1);
        if shared
            .io_tx
            .send(IoTask::Sync { on_done: sync_tx2 })
            .is_err()
        {
            shared.bm.restore_pending_deletes(restore_applied());
            return Err(Error::Internal("checkpoint: I/O worker channel closed before Sync (deletes)",
            ));
        }
        match sync_rx2.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                eprintln!("holt: checkpoint backend Sync (deletes) failed: {e}");
                shared.bm.restore_pending_deletes(restore_applied());
                return Err(e);
            }
            Err(_) => {
                shared.bm.restore_pending_deletes(restore_applied());
                return Err(Error::Internal("checkpoint: I/O worker dropped Sync (deletes) completion",
                ));
            }
        }
    }

    // 8. Truncate WAL atomically iff every snapshot landed AND no
    //    racing writer has re-dirtied (under WAL-lock check), AND
    //    no deferred deletes are still queued. The pending-delete
    //    gate is essential: a queued delete means a WAL record
    //    "this blob is unlinked" hasn't yet propagated to the
    //    manifest, so truncating would orphan the unlink.
    if failed.is_empty() && pending_failed.is_empty() {
        if let Some(wal) = &shared.wal {
            let mut w = wal.lock().unwrap();
            if shared.bm.dirty_count() == 0 && shared.bm.pending_delete_count() == 0 {
                w.truncate()?;
                shared.truncates.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    shared.rounds_succeeded.fetch_add(1, Ordering::Relaxed);

    #[cfg(feature = "tracing")]
    {
        let elapsed = round_start.elapsed();
        let truncated = failed.is_empty()
            && pending_failed.is_empty()
            && shared.wal.is_some()
            && shared.bm.dirty_count() == 0
            && shared.bm.pending_delete_count() == 0;
        tracing::info!(
            target: "holt::checkpoint",
            dirty_snapshot = snap_count,
            blobs_flushed = snap_count - failed.len(),
            blobs_failed = failed.len(),
            blobs_deleted = applied_deletes,
            merged = merged,
            truncated_wal = truncated,
            elapsed_us = elapsed.as_micros() as u64,
            "round complete",
        );
    }

    Ok(())
}

/// Tree-wide merge pass — fold every mergeable `BlobNode` child
/// back into its parent. Stages the mutations via the unified
/// `mark_dirty` + `mark_for_delete` protocol so the round's
/// later phases (WAL flush → Flush tasks → Sync → pending
/// deletes → re-Sync → truncate) handle persistence under W2D.
///
/// Returns the cumulative count of children folded.
///
/// An inline `bm.commit(parent)` + `bm.delete_blob(child)` would
/// be wrong here — both happen pre-Sync, pre-WAL. `bm.commit`
/// would push cache bytes (potentially including user mutations
/// whose WAL records aren't yet durable) directly to backend, and
/// `bm.delete_blob` would mutate the manifest in-memory which a
/// later `backend.flush` could persist while the corresponding
/// user WAL records still hadn't reached disk. Staging through
/// dirty / pending-delete avoids both: the only flush path is
/// the round's own `IoTask::Flush`, which runs strictly after
/// step 2's WAL flush.
fn run_merge_pass(shared: &Arc<Shared>) -> Result<u64> {
    use crate::store::buffer_manager::STRUCTURAL_SEQ;

    let parents = engine::collect_blob_guids(shared.bm.as_ref(), shared.root_guid)?;
    let mut merged_total = 0u64;
    for guid in parents {
        if !shared.bm.has_blob(guid)? {
            continue;
        }
        let pin = shared.bm.pin(guid)?;
        let stats = {
            let mut guard = pin.write();
            let mut frame = BlobFrame::wrap(guard.as_mut_slice());
            engine::try_merge_children(shared.bm.as_ref(), &mut frame, STRUCTURAL_SEQ)?
        };
        drop(pin);
        if stats.merged > 0 {
            shared.bm.mark_dirty(guid, STRUCTURAL_SEQ);
            merged_total += u64::from(stats.merged);
        }
    }
    Ok(merged_total)
}
