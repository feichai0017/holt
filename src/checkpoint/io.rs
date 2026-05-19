//! I/O worker thread — drains the bounded queue and runs
//! `backend.write_blob` / `backend.flush` on behalf of the
//! checkpoint planner.
//!
//! ## Why a separate thread
//!
//! Decouples I/O execution from planning so the planner can:
//! 1. Snapshot bytes under a brief shared read guard, then move on.
//! 2. Submit N flush tasks without serialising on each I/O.
//! 3. Plan the next round's merge pass while the previous round's
//!    Sync is still in flight on the I/O thread.
//!
//! For the current local-`pread`/`pwrite` backend the parallelism
//! gain is modest (single thread, single FD). The architecture
//! pays off once the io_uring backend lands (next commit) — the
//! I/O thread becomes the SQE submitter + CQE poller, and the
//! planner's submit-N-then-wait pattern naturally feeds the ring.
//!
//! ## Shutdown
//!
//! The thread terminates on receiving [`IoTask::Stop`]. The
//! `Checkpointer` orchestrator sends one at the end of its `Drop`
//! sequence, after the final synchronous round has drained
//! everything through this same queue.

use crossbeam_channel::{Receiver, Sender};
use std::sync::Arc;

use crate::api::errors::Result;
use crate::layout::BlobGuid;
use crate::store::backend::AlignedBlobBuf;

use super::Shared;

/// One-shot completion channel — sized `bounded(1)` so a `send`
/// never blocks. The I/O worker sends `Ok(())` on success and
/// `Err(_)` on failure; the orchestrator receives once.
pub(crate) type Completion = Sender<Result<()>>;

/// Work item handed to the I/O thread via the bounded queue.
pub(crate) enum IoTask {
    /// Push `bytes` to the inner backend under `guid`. Bytes are
    /// owned by the task (snapshotted from cache by the planner)
    /// so the I/O thread doesn't touch the BM's read guard during
    /// the write.
    ///
    /// `expected_seq` is the dirty-map value the planner observed
    /// when it drained the snapshot. The I/O worker only retires
    /// the dirty entry after a successful write if the current
    /// entry still equals this seq — guards against a racing
    /// writer that called `mark_dirty(guid, newer_seq)` after the
    /// drain (those bytes aren't in our snapshot, so we mustn't
    /// pretend the blob is clean).
    Flush {
        guid: BlobGuid,
        bytes: AlignedBlobBuf,
        expected_seq: u64,
        on_done: Completion,
    },
    /// `fdatasync` (via `Backend::flush`). The orchestrator sends
    /// this after a batch of `Flush` tasks completes so every
    /// blob's bytes are stable on disk before the WAL is
    /// truncated.
    Sync { on_done: Completion },
    /// Graceful stop signal. Sent once during `Checkpointer::Drop`
    /// after the planner has joined and the final round has run.
    Stop,
}

/// Main loop for the I/O thread.
pub(crate) fn run(shared: &Arc<Shared>, rx: Receiver<IoTask>) {
    while let Ok(task) = rx.recv() {
        match task {
            IoTask::Flush {
                guid,
                bytes,
                expected_seq,
                on_done,
            } => {
                let result = shared.bm.write_through(guid, &bytes, expected_seq);
                // `send` only fails if the orchestrator dropped
                // the receiver — which only happens if the round
                // aborted or the Tree is shutting down. Either
                // way, no recovery action here; we just move on.
                let _ = on_done.send(result);
            }
            IoTask::Sync { on_done } => {
                let result = shared.bm.backend_flush();
                let _ = on_done.send(result);
            }
            IoTask::Stop => break,
        }
    }
}
