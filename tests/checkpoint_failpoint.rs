//! Fault-injection tests for the checkpoint round's
//! deferred-delete + Sync paths.
//!
//! Wraps a real backend (`MemoryBackend` or `PersistentBackend`)
//! in a [`FailpointBackend`] that can be told to fail the N-th
//! `delete_blob` / `flush` / `write_blob` call. The tests verify
//! that:
//!
//! - A failed `backend.delete_blob` keeps the entry in
//!   `pending_deletes` so a subsequent round retries.
//! - A failed `backend.flush` after partial deletes restores the
//!   already-applied entries so the next round re-Syncs (and
//!   `delete_blob` retry is idempotent).
//! - A failed `write_blob` keeps the entry in `dirty` so a
//!   subsequent round retries the byte flush.

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tempfile::tempdir;

use holt::{AlignedBlobBuf, Backend, CheckpointConfig, MemoryBackend, Tree, TreeConfig};

// ---------- failpoint backend ----------

/// Backend wrapper that counts every call and can fail the N-th
/// `delete_blob` / `flush` / `write_blob`. The fault counter is
/// **one-shot** — once it fires (the call N matches), the counter
/// is reset to `usize::MAX`; subsequent calls succeed via the
/// inner backend. Tests can rearm with `arm_*` between rounds.
struct FailpointBackend {
    inner: Arc<dyn Backend>,
    delete_calls: AtomicUsize,
    flush_calls: AtomicUsize,
    write_calls: AtomicUsize,
    fail_delete_at: AtomicUsize, // 1-based ordinal; usize::MAX = disarmed
    fail_flush_at: AtomicUsize,
    fail_write_at: AtomicUsize,
}

impl FailpointBackend {
    fn new(inner: Arc<dyn Backend>) -> Self {
        Self {
            inner,
            delete_calls: AtomicUsize::new(0),
            flush_calls: AtomicUsize::new(0),
            write_calls: AtomicUsize::new(0),
            fail_delete_at: AtomicUsize::new(usize::MAX),
            fail_flush_at: AtomicUsize::new(usize::MAX),
            fail_write_at: AtomicUsize::new(usize::MAX),
        }
    }
    fn arm_delete(&self, nth: usize) {
        self.fail_delete_at.store(nth, Ordering::SeqCst);
    }
    fn arm_flush(&self, nth: usize) {
        self.fail_flush_at.store(nth, Ordering::SeqCst);
    }
    fn arm_write(&self, nth: usize) {
        self.fail_write_at.store(nth, Ordering::SeqCst);
    }
    fn delete_count(&self) -> usize {
        self.delete_calls.load(Ordering::SeqCst)
    }
}

fn failpoint_err(msg: &'static str) -> holt::Error {
    holt::Error::BackendIo(io::Error::other(msg))
}

impl Backend for FailpointBackend {
    fn read_blob(&self, guid: holt::BlobGuid, dst: &mut AlignedBlobBuf) -> holt::Result<()> {
        self.inner.read_blob(guid, dst)
    }
    fn write_blob(&self, guid: holt::BlobGuid, src: &AlignedBlobBuf) -> holt::Result<()> {
        let n = self.write_calls.fetch_add(1, Ordering::SeqCst) + 1;
        let armed = self.fail_write_at.load(Ordering::SeqCst);
        if n == armed {
            self.fail_write_at.store(usize::MAX, Ordering::SeqCst);
            return Err(failpoint_err("failpoint: write_blob"));
        }
        self.inner.write_blob(guid, src)
    }
    fn delete_blob(&self, guid: holt::BlobGuid) -> holt::Result<()> {
        let n = self.delete_calls.fetch_add(1, Ordering::SeqCst) + 1;
        let armed = self.fail_delete_at.load(Ordering::SeqCst);
        if n == armed {
            self.fail_delete_at.store(usize::MAX, Ordering::SeqCst);
            return Err(failpoint_err("failpoint: delete_blob"));
        }
        self.inner.delete_blob(guid)
    }
    fn list_blobs(&self) -> holt::Result<Vec<holt::BlobGuid>> {
        self.inner.list_blobs()
    }
    fn flush(&self) -> holt::Result<()> {
        let n = self.flush_calls.fetch_add(1, Ordering::SeqCst) + 1;
        let armed = self.fail_flush_at.load(Ordering::SeqCst);
        if n == armed {
            self.fail_flush_at.store(usize::MAX, Ordering::SeqCst);
            return Err(failpoint_err("failpoint: flush"));
        }
        self.inner.flush()
    }
    fn has_blob(&self, guid: holt::BlobGuid) -> holt::Result<bool> {
        self.inner.has_blob(guid)
    }
}

// ---------- tests ----------

/// Build a tree on a failpoint-wrapped memory backend and stage
/// at least one deferred delete in the BM's `pending_deletes`
/// set via the **merge pass**: insert enough to force spillover,
/// delete most of one child's keys so it becomes mergeable,
/// then run `Tree::compact` so phase 2's `merge_blob` queues a
/// `mark_for_delete` on the now-empty / now-small child.
fn setup_with_pending_delete() -> (Arc<dyn Backend>, Arc<FailpointBackend>, Tree) {
    let inner: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let fp = Arc::new(FailpointBackend::new(Arc::clone(&inner)));
    let fp_dyn: Arc<dyn Backend> = fp.clone();
    let mut cfg = TreeConfig::memory();
    cfg.memory_flush_on_write = false;
    let tree = Tree::open_with_backend(cfg, fp_dyn).unwrap();

    // Stuff enough data to force at least one spillover, then
    // delete the bulk of it so the remaining shape is mergeable.
    let payload = vec![b'z'; 1024];
    for i in 0..1000u32 {
        let k = format!("k{i:05}");
        tree.put(k.as_bytes(), &payload).unwrap();
    }
    // Tombstone the lower 95% so the merge pass sees a small
    // child blob it can fold back into its parent.
    for i in 0..950u32 {
        let k = format!("k{i:05}");
        let _ = tree.delete(k.as_bytes()).unwrap();
    }
    // `compact` rebuilds each blob (dropping tombstones) and
    // then runs the merge pass. The merge pass queues
    // `mark_for_delete` for every folded child — that's our
    // guaranteed source of pending deletes.
    tree.compact().unwrap();
    (inner, fp, tree)
}

#[test]
fn pending_delete_execute_failure_is_retried_next_round() {
    let (inner, fp, tree) = setup_with_pending_delete();

    // Confirm the workload actually queued at least one
    // deferred delete — otherwise the test isn't exercising
    // the path we mean to.
    let stats_before = tree.stats().unwrap();
    assert!(
        stats_before.bm_pending_delete_count > 0,
        "setup must produce at least one pending delete (got {})",
        stats_before.bm_pending_delete_count,
    );

    // Arm: fail the NEXT `delete_blob` call — that's the first
    // one in `tree.checkpoint`'s deferred-delete phase. The
    // failpoint is one-shot, so only one entry hits an error;
    // any further entries in the same round succeed.
    fp.arm_delete(fp.delete_count() + 1);

    // First checkpoint: the failpoint trips. `tree.checkpoint`
    // restores the failed entries to `pending_deletes` and
    // surfaces the error.
    let result1 = tree.checkpoint();
    assert!(
        result1.is_err(),
        "checkpoint must surface the first delete_blob failure",
    );
    // The retry-protected entry survives.
    assert!(
        tree.stats().unwrap().bm_pending_delete_count > 0,
        "failed delete entry must stay queued for retry",
    );

    // Second checkpoint: failpoint disarmed. Drains the
    // pending-delete set fully.
    tree.checkpoint().unwrap();
    assert_eq!(tree.stats().unwrap().bm_pending_delete_count, 0);

    // Final state: backend manifest count equals tree blob count
    // (no orphan slots, no missing slots).
    let backend_blobs = inner.list_blobs().unwrap();
    let stats = tree.stats().unwrap();
    assert_eq!(
        backend_blobs.len() as u32,
        stats.blob_count,
        "after retry, backend manifest count = tree blob count",
    );
}

#[test]
fn pending_delete_sync_failure_keeps_state_for_retry() {
    // Inject failure into the **second** `backend.flush` — the
    // one that persists the manifest after the deferred-delete
    // phase. The pre-delete data Sync (step 3 of
    // `Tree::checkpoint`) should succeed; only the post-delete
    // Sync trips.
    let (inner, fp, tree) = setup_with_pending_delete();
    assert!(
        tree.stats().unwrap().bm_pending_delete_count > 0,
        "setup precondition: pending delete queued",
    );

    // `Tree::checkpoint`'s flush calls in order:
    //   1. journal flush — but no WAL here, so doesn't hit
    //      backend (no FailpointBackend::flush call).
    //   2. `backend.flush` (data Sync, step 3) — call #1.
    //   3. `backend.flush` (manifest Sync, step 5) — call #2.
    // Arm to fail flush call #2.
    fp.arm_flush(2);
    let result1 = tree.checkpoint();
    assert!(
        result1.is_err(),
        "checkpoint must surface the post-delete Sync failure",
    );

    // The applied-but-unsynced entries must be restored to
    // pending so the next checkpoint re-Syncs.
    assert!(
        tree.stats().unwrap().bm_pending_delete_count > 0,
        "post-delete Sync failure must restore applied entries to pending",
    );

    // Second checkpoint: failpoint disarmed. Re-execute
    // `delete_blob` is idempotent; the trailing Sync now
    // succeeds.
    tree.checkpoint().unwrap();
    assert_eq!(tree.stats().unwrap().bm_pending_delete_count, 0);

    let backend_blobs = inner.list_blobs().unwrap();
    let stats = tree.stats().unwrap();
    assert_eq!(
        backend_blobs.len() as u32,
        stats.blob_count,
        "after retry, backend manifest count = tree blob count",
    );
}

#[test]
fn dirty_write_failure_is_retried_next_round() {
    // Failpoint inject into `write_blob` — the byte flush path.
    // The dirty entry must survive into the next round for retry.
    let inner: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let fp = Arc::new(FailpointBackend::new(Arc::clone(&inner)));
    let fp_clone: Arc<dyn Backend> = fp.clone();
    let mut cfg = TreeConfig::memory();
    cfg.memory_flush_on_write = false;
    let tree = Tree::open_with_backend(cfg, fp_clone).unwrap();

    tree.put(b"k1", b"v1").unwrap();
    let writes_pre = fp.write_calls.load(Ordering::SeqCst);
    fp.arm_write(writes_pre + 1);

    // First checkpoint: the next write_blob fails.
    let r1 = tree.checkpoint();
    assert!(
        r1.is_err(),
        "first checkpoint should surface failpoint write error"
    );

    // Tree internal dirty set should still have the entry so
    // the next checkpoint retries.
    assert!(
        tree.stats().unwrap().bm_dirty_count >= 1,
        "failed write must leave dirty entry for retry",
    );

    // Second checkpoint: disarmed, succeeds.
    tree.checkpoint().unwrap();
    assert_eq!(tree.stats().unwrap().bm_dirty_count, 0);

    // Verify the value is durable.
    assert_eq!(tree.get(b"k1").unwrap().as_deref(), Some(&b"v1"[..]),);
}

#[test]
fn dirty_write_failure_does_not_propagate_to_pending_delete() {
    // Regression for the bug where a failed parent write_through
    // didn't stop the round from applying a dependent child's
    // manifest delete — leaving "old parent on-disk still
    // references a now-deleted child" on a crash before the next
    // round caught up. The fix: on any write failure, the
    // pre-delete sync still runs (to fsync the writes that DID
    // succeed), but no manifest delete fires; the whole pending
    // snapshot is restored for the next round.
    let (_inner, fp, tree) = setup_with_pending_delete();
    let stats_before = tree.stats().unwrap();
    assert!(
        stats_before.bm_dirty_count > 0,
        "setup precondition: dirty entry queued (got {})",
        stats_before.bm_dirty_count,
    );
    assert!(
        stats_before.bm_pending_delete_count > 0,
        "setup precondition: pending delete queued (got {})",
        stats_before.bm_pending_delete_count,
    );
    let pending_before = stats_before.bm_pending_delete_count;
    let deletes_before = fp.delete_count();

    // Arm the NEXT write_blob to fail — that's the first
    // write_through inside `tree.checkpoint`'s phase 2.
    let writes_pre = fp.write_calls.load(Ordering::SeqCst);
    fp.arm_write(writes_pre + 1);

    let result = tree.checkpoint();
    assert!(
        result.is_err(),
        "checkpoint must surface the write_through failure",
    );

    let stats_after = tree.stats().unwrap();
    assert!(
        stats_after.bm_dirty_count >= 1,
        "failed dirty write must stay in `dirty` for next round (got {})",
        stats_after.bm_dirty_count,
    );
    assert_eq!(
        stats_after.bm_pending_delete_count, pending_before,
        "dirty failure must NOT apply any pending delete — whole \
         snapshot restored (got {} vs {})",
        stats_after.bm_pending_delete_count, pending_before,
    );

    assert_eq!(
        fp.delete_count(),
        deletes_before,
        "no manifest delete attempt must run while dirty write failed",
    );

    // Second checkpoint with no fault — drains everything.
    tree.checkpoint().unwrap();
    let stats_done = tree.stats().unwrap();
    assert_eq!(stats_done.bm_dirty_count, 0);
    assert_eq!(stats_done.bm_pending_delete_count, 0);
}

#[test]
fn pre_delete_sync_failure_restores_pending() {
    // Regression for the bug where the pre-delete `backend.flush`
    // failure path drained `pending` (the checkpoint snapshot) but
    // never restored it — losing every queued unlink intent. The
    // fix restores `pending` on every Sync-failure return path
    // before phase 6.
    let (inner, fp, tree) = setup_with_pending_delete();
    let pending_before = tree.stats().unwrap().bm_pending_delete_count;
    assert!(pending_before > 0, "setup precondition");

    // First `backend.flush` call inside `tree.checkpoint` is the
    // pre-delete data Sync at phase 3 — arm to fail it.
    let flushes_pre = fp.flush_calls.load(Ordering::SeqCst);
    fp.arm_flush(flushes_pre + 1);

    let result = tree.checkpoint();
    assert!(
        result.is_err(),
        "checkpoint must surface the pre-delete Sync failure",
    );

    let stats_after = tree.stats().unwrap();
    assert_eq!(
        stats_after.bm_pending_delete_count, pending_before,
        "pre-delete Sync failure must restore the entire pending \
         snapshot (got {} vs {})",
        stats_after.bm_pending_delete_count, pending_before,
    );

    // Phase 6 didn't run, so no manifest delete applied.
    let backend_blobs = inner.list_blobs().unwrap();
    let stats = tree.stats().unwrap();
    assert_eq!(
        backend_blobs.len() as u32,
        stats.blob_count,
        "no manifest delete must have applied while pre-delete Sync failed",
    );

    // Recovery: next checkpoint drains the restored snapshot.
    tree.checkpoint().unwrap();
    assert_eq!(tree.stats().unwrap().bm_pending_delete_count, 0);
}

#[test]
fn bg_checkpointer_recovers_from_transient_failure() {
    // Same shape but with the background checkpointer driving
    // the round, not manual `tree.checkpoint`. Verify the
    // bg loop eventually drains everything.
    let inner: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let fp = Arc::new(FailpointBackend::new(Arc::clone(&inner)));
    let fp_clone: Arc<dyn Backend> = fp.clone();

    let dir = tempdir().unwrap();
    // Use TreeBuilder with WAL + bg checkpointer to exercise
    // the full round path.
    let _ = dir; // we use open_with_backend (no WAL), so dir unused

    let mut cfg = TreeConfig::memory();
    cfg.memory_flush_on_write = false;
    cfg.checkpoint = CheckpointConfig {
        enabled: true,
        idle_interval: Duration::from_millis(10),
        dirty_blob_threshold: 1,
        auto_merge: false,
        ..CheckpointConfig::default()
    };
    let tree = Tree::open_with_backend(cfg, fp_clone).unwrap();

    // Stuff some data + arm a transient write failure.
    tree.put(b"k1", b"v1").unwrap();
    let writes_pre = fp.write_calls.load(Ordering::SeqCst);
    fp.arm_write(writes_pre + 1);

    // Wait until the bg checkpointer has drained the dirty set
    // — it must retry after the transient failure.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let dirty = tree.stats().unwrap().bm_dirty_count;
        if dirty == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "bg checkpointer didn't recover from failpoint (dirty_count = {dirty})",
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
