//! Public `Tree` type — the main user-facing API.
//!
//! ## Internal key encoding
//!
//! Every user-supplied key is padded with a trailing `\0` byte
//! before reaching the walker. This is a standard ART trick to
//! resolve the "strict prefix" case where one key (e.g. `"abc"`)
//! is a prefix of another (e.g. `"abcdef"`): the terminator
//! guarantees the two keys diverge somewhere inside the radix
//! tree (at the `\0` vs `'d'` byte in this example).
//!
//! ## Concurrency model
//!
//! Tree owns an `Arc<BufferManager>`. The BM keeps each cached
//! blob behind a `HybridLatch` (LeanStore-style 3-mode latch)
//! wrapping an `UnsafeCell<AlignedBlobBuf>`:
//!
//! - **Reads** (`get`) walk every blob in **optimistic** mode —
//!   wait-free, no real lock taken. The walker snapshots the
//!   latch version, reads the buffer, then validates; on a torn
//!   read it restarts from the root. Readers never block writers
//!   and writers never block readers.
//! - **Writes** (`put` / `delete`) take **exclusive** mode on
//!   each blob they touch (always starting with the root). This
//!   serialises concurrent mutators on the same blob without any
//!   Tree-wide writer mutex; mutations on disjoint child blobs
//!   can proceed in parallel.
//! - **`rename`** is multi-step (lookup probe + erase + insert)
//!   and must be atomic across all three. It takes the
//!   `rename_lock` (a `Mutex<()>` scoped to rename only) to
//!   prevent racing renames from interleaving.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::config::{Storage, TreeConfig};
use super::errors::{Error, Result};
use super::stats::{BlobStats, CheckpointerStats, TreeStats};
use crate::engine;
use crate::engine::RangeBuilder;
use crate::journal::reader::replay;
use crate::journal::txn_op::TxnOp;
use crate::journal::writer::WalWriter;
use crate::layout::{BlobGuid, PAGE_SIZE};
use crate::store::backend::{AlignedBlobBuf, Backend, MemoryBackend, PersistentBackend};
use crate::store::{BlobFrame, BlobFrameRef, BufferManager, CachedBlob};

use super::txn::{BatchOp, TxnBatch};

/// An `holt` tree — your handle to one metadata store.
///
/// Clone the handle to share the same backing store: the
/// `BufferManager` is held via `Arc`. Reads run lock-free against
/// the writer mutex; writers serialise through `write_lock`.
#[derive(Clone)]
pub struct Tree {
    cfg: TreeConfig,
    backend: Arc<BufferManager>,
    /// GUID of the blob holding the tree root. Currently a fixed
    /// sentinel; a multi-tenant manifest could allocate per-tree
    /// root GUIDs in the future.
    root_guid: BlobGuid,
    /// Cached pin on the root blob — held for the life of this
    /// `Tree` handle so every `get` / `put` / `delete` / `rename`
    /// skips the `BufferManager`'s `Mutex<HashMap>` lookup on
    /// the root hop. Cross-blob descents still pin children
    /// through the BM as normal.
    root_pin: Arc<CachedBlob>,
    /// Serialises **only `rename`** — the multi-step
    /// `lookup_multi(src)` + `erase_multi(src)` + `insert_multi(dst)`
    /// must appear atomic to other writers. `put` / `delete` /
    /// `get` never take this lock; they coordinate via the
    /// per-blob `HybridLatch` inside the BM.
    rename_lock: Arc<Mutex<()>>,
    /// Monotonically-increasing sequence stamped on every record.
    /// On open the tree replays the WAL and resumes at
    /// `highest_seq + 1`.
    next_seq: Arc<AtomicU64>,
    /// WAL handle — `Some` for persistent trees, `None` for
    /// memory trees (logging an in-memory engine has no point).
    wal: Option<Arc<Mutex<WalWriter>>>,
    /// Background checkpointer handle. `Some` iff
    /// `cfg.checkpoint.enabled`. Shared via `Arc` so the thread
    /// shuts down on the **last** `Tree` clone's drop, not the
    /// first. Exposed to `Tree::stats` for counter readout.
    checkpointer: Option<Arc<crate::checkpoint::Checkpointer>>,
}

impl std::fmt::Debug for Tree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tree")
            .field("storage", &self.cfg.storage)
            .field("root_guid", &self.root_guid)
            .finish_non_exhaustive()
    }
}

/// Fixed GUID of the root blob (single-tree mode). A future
/// multi-tenant manifest could allocate per-tree root GUIDs.
pub(crate) const ROOT_BLOB_GUID: BlobGuid = [0; 16];

/// Append the engine's internal terminator byte (`\0`) to a
/// user-supplied key. See the module docs.
#[inline]
fn pad_key(key: &[u8]) -> Vec<u8> {
    let mut padded = Vec::with_capacity(key.len() + 1);
    padded.extend_from_slice(key);
    padded.push(0u8);
    padded
}

impl Tree {
    /// Open a tree using the supplied configuration.
    ///
    /// `TreeConfig::new("/path")` opens a persistent tree at
    /// `"/path"` (the default). `TreeConfig::memory()` opens an
    /// in-memory tree.
    ///
    /// holt is Unix-only — the persistent backend uses `O_DIRECT`
    /// on Linux and `F_NOCACHE` on macOS. Building the crate on
    /// Windows fails at compile time (see the platform stance in
    /// `ROADMAP.md`).
    pub fn open(cfg: TreeConfig) -> Result<Self> {
        let backend: Arc<dyn Backend> = match &cfg.storage {
            Storage::Memory => Arc::new(MemoryBackend::new()),
            Storage::Persistent { dir } => Arc::new(PersistentBackend::open(dir)?),
        };
        // The auto-managed backend earns automatic WAL coverage.
        Self::open_inner(cfg, backend, /*attach_wal=*/ true)
    }

    /// Open a tree with a caller-supplied [`Backend`].
    ///
    /// **No WAL is attached.** The caller's backend has its own
    /// notion of durability (or is intentionally volatile —
    /// e.g. a `MemoryBackend` standing in for a real one in a
    /// test); holt stays out of that decision. If you want a
    /// WAL'd persistent tree, use [`Tree::open`] with a
    /// `Storage::Persistent` config.
    ///
    /// The supplied backend is **transparently wrapped** with a
    /// [`BufferManager`] of `cfg.buffer_pool_size` blobs.
    /// `BufferManager` owns the in-memory blob cache; the walker
    /// pins blobs from it for both reads and writes — no separate
    /// root buffer in `Tree`.
    ///
    /// If the backend doesn't yet contain a root blob, initialises
    /// an empty one and writes it through, flushing before
    /// returning.
    pub fn open_with_backend(cfg: TreeConfig, backend: Arc<dyn Backend>) -> Result<Self> {
        Self::open_inner(cfg, backend, /*attach_wal=*/ false)
    }

    fn open_inner(cfg: TreeConfig, backend: Arc<dyn Backend>, attach_wal: bool) -> Result<Self> {
        let bm: Arc<BufferManager> = Arc::new(BufferManager::new(backend, cfg.buffer_pool_size));
        let root_guid = ROOT_BLOB_GUID;
        if !bm.has_blob(root_guid)? {
            // Seed an empty root blob and write it through.
            let mut scratch = AlignedBlobBuf::zeroed();
            BlobFrame::init(scratch.as_mut_slice(), root_guid)?;
            bm.write_blob(root_guid, &scratch)?;
            bm.flush()?;
        }

        // Persistent trees keep a WAL alongside the data file.
        // Replay every durable record onto the BM-cached blob
        // image before exposing the tree to callers: the on-disk
        // blob lags the WAL between the last `Tree::checkpoint`
        // and now, so the WAL is the source of truth for any op
        // committed via `flush_on_write = true`.
        let (wal, next_seq) = if attach_wal {
            match cfg.wal_path() {
                None => (None, 1u64),
                Some(path) => {
                    let next_seq = if path.exists() {
                        replay_wal(&path, &bm, root_guid)?
                    } else {
                        1
                    };
                    let writer = WalWriter::open_or_create(&path, /*tree_id=*/ 0)?;
                    (Some(Arc::new(Mutex::new(writer))), next_seq)
                }
            }
        } else {
            (None, 1u64)
        };

        // Hot-path: pin the root blob once for the lifetime of
        // this `Tree` handle so every subsequent `get` / `put` /
        // `delete` / `rename` skips the BufferManager's per-pin
        // `Mutex<HashMap>` lookup on the root hop. `Tree::clone`
        // shares the pin via `Arc::clone`.
        let root_pin = bm.pin(root_guid)?;

        // Spawn the background checkpointer if opted-in.
        // `Checkpointer::spawn` returns `None` for disabled
        // configs, so the `Option` chain stays clean.
        let checkpointer = crate::checkpoint::Checkpointer::spawn(
            Arc::clone(&bm),
            wal.clone(),
            root_guid,
            cfg.checkpoint.clone(),
        )
        .map(Arc::new);

        Ok(Self {
            cfg,
            backend: bm,
            root_guid,
            root_pin,
            rename_lock: Arc::new(Mutex::new(())),
            next_seq: Arc::new(AtomicU64::new(next_seq)),
            wal,
            checkpointer,
        })
    }

    /// Look up `key`. Returns the value bytes, or `None` if no leaf
    /// matches.
    ///
    /// **Zero-copy and lock-free against the writer lock**: pins
    /// each blob via the [`BufferManager`] and walks the cached
    /// buffer under a shared `RwLock` read guard. N readers on
    /// different blobs progress in parallel; readers on the same
    /// blob also progress in parallel via the read-half.
    ///
    /// Transparently follows `BlobNode` crossings — the lookup may
    /// span multiple blobs when the tree has been split by
    /// spillover.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let padded = pad_key(key);
        engine::lookup_multi(&self.backend, &self.root_pin, &padded)
    }

    /// Insert or replace `(key, value)`. Returns the previous value
    /// if the key already existed.
    ///
    /// Walks across [`BlobNode`] crossings. When any blob hits
    /// `AllocError::OutOfSpace`, the walker automatically migrates
    /// a subtree out via `splitBlob` and retries — so trees may
    /// grow well past the 512 KB single-blob limit without caller
    /// involvement.
    ///
    /// Mutates the BM-pinned root buffer in place under an
    /// exclusive write guard. Cross-blob mutations (descent into
    /// a child blob, spillover creating a new child) stage their
    /// changes via `mark_dirty` / `install_new_blob`; the durable
    /// write to the inner backend happens when the WAL record
    /// covering this op is on disk — driven either by the
    /// background checkpoint round or by [`Tree::checkpoint`].
    /// Per-op `flush_on_write` mode drains the dirty set inline
    /// after the WAL append.
    ///
    /// [`BlobNode`]: crate::layout::BlobNode
    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<Option<Vec<u8>>> {
        let padded = pad_key(key);
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        // Concurrent writers are serialised by the per-blob
        // `HybridLatch` (root blob always taken exclusive); no
        // Tree-wide writer mutex needed.
        let outcome = engine::insert_multi(&self.backend, &self.root_pin, &padded, value, seq)?;
        // Root blob's cached image is now ahead of the backend
        // image. Tag it for the (foreground or background)
        // checkpoint round; the `flush_on_write` branch below
        // drains the entry inline, the WAL branch defers to the
        // checkpointer.
        self.backend.mark_dirty(self.root_guid, seq);

        // Durability model: the WAL flush is the **per-op
        // boundary**. The BM-cached blob image stays in memory
        // until `Tree::checkpoint`. A crash recovers by replaying
        // every record past the last checkpoint onto the blob
        // image that was durable at checkpoint time.
        //
        // The previous "commit BM per op" design double-counted
        // durability and made replay non-idempotent for `rename`
        // (the WAL re-inserted the source key on top of an
        // already-renamed blob).
        if let Some(wal) = &self.wal {
            let mut w = wal.lock().unwrap();
            // Fast-path: append the Insert record directly from
            // borrowed refs — skips the `TxnOp::Insert` enum's
            // three `Vec` clones (key, value, prev_value).
            w.append_insert(seq, 0, key, value, outcome.previous.as_deref())?;
            if self.cfg.wal_sync_on_commit {
                w.flush()?;
            }
        } else if self.cfg.flush_on_write {
            // No WAL (memory mode, or backend supplied by user).
            // `flush_on_write` still pushes the BM through its
            // `Backend`'s write-through path so callers that want
            // per-op durability against a custom backend keep
            // getting it. The walker may have dirtied child blobs
            // too (spillover, cross-blob descent), so drain the
            // full dirty set rather than just the root.
            self.flush_dirty_inline()?;
        }
        Ok(outcome.previous)
    }

    /// Remove `key`. Returns the value that was stored at `key`, or
    /// `None` if no leaf matched.
    ///
    /// Walks across [`BlobNode`] crossings. When a child blob
    /// becomes empty as a result of the erase, its parent's
    /// `BlobNode` is freed and the orphaned child blob is queued
    /// for deferred deletion via the BM — the actual
    /// `backend.delete_blob` runs from the checkpoint round
    /// after the WAL record covering this erase is durable
    /// (invariant W2D).
    ///
    /// [`BlobNode`]: crate::layout::BlobNode
    pub fn delete(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let padded = pad_key(key);
        // Pre-allocate the seq before the walker descends so any
        // child blob the walker touches can `mark_dirty(child, seq)`
        // — invariant W2D (see `BufferManager` module docs) demands
        // a single seq for the whole op across all blobs it dirties.
        // A no-op delete (key absent) still burns the seq; that's
        // fine — `next_seq` is monotonic and the unused seq doesn't
        // appear in any WAL record or dirty entry.
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let outcome = engine::erase_multi(&self.backend, &self.root_pin, &padded, seq)?;

        if let Some(wal) = &self.wal {
            if let Some(prev) = &outcome.previous {
                // Only mark the **root** dirty on an actual erase
                // — a no-op delete (key absent) leaves the root
                // image byte-identical to the backend, and the
                // walker already mark_dirty'd any child it touched.
                self.backend.mark_dirty(self.root_guid, seq);
                let mut w = wal.lock().unwrap();
                // Fast-path: skips the `TxnOp::Erase` enum's
                // two `Vec` clones (key, value).
                w.append_erase(seq, 0, key, prev)?;
                if self.cfg.wal_sync_on_commit {
                    w.flush()?;
                }
            }
            // No-op delete (key wasn't there) is not logged.
        } else if self.cfg.flush_on_write {
            if outcome.previous.is_some() {
                self.backend.mark_dirty(self.root_guid, seq);
            }
            // Flush every blob the walker touched (root + any
            // children) — no WAL means this is the sole durability
            // path. snapshot_dirty drains all entries; we commit
            // each through the backend.
            self.flush_dirty_inline()?;
            // Plus drain any deferred deletes the SubtreeGone path
            // queued — the cache image of those children is gone,
            // but the backend slot is still alive until we apply
            // `backend.delete_blob`.
            self.flush_pending_deletes_inline()?;
        }
        Ok(outcome.previous)
    }

    /// Move the value at `src` to `dst` in a single atomic step.
    ///
    /// - Returns [`Error::NotFound`] if `src` has no leaf.
    /// - Returns [`Error::DstExists`] if `dst` already has a leaf
    ///   **and** `force` is `false`.
    /// - When `force` is `true`, any existing leaf at `dst` is
    ///   overwritten.
    ///
    /// Atomic with respect to other renames (`rename_lock` is held
    /// for the whole sequence). Concurrent `put` / `delete` on
    /// disjoint subtrees are not blocked. The op emits a single
    /// `RenameObject` WAL record so its erase + insert phases
    /// recover atomically on replay.
    pub fn rename(&self, src: &[u8], dst: &[u8], force: bool) -> Result<()> {
        let src_padded = pad_key(src);
        let dst_padded = pad_key(dst);

        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let _r = self.rename_lock.lock().unwrap();

        // Probe src across all blobs — zero-copy via BM pin.
        let Some(value) = engine::lookup_multi(&self.backend, &self.root_pin, &src_padded)? else {
            return Err(Error::NotFound);
        };

        // Same key? No-op (seq is already bumped).
        if src == dst {
            return Ok(());
        }

        // Probe dst across all blobs unless overwrite is allowed.
        if !force && engine::lookup_multi(&self.backend, &self.root_pin, &dst_padded)?.is_some() {
            return Err(Error::DstExists);
        }

        // erase(src) + insert(dst, value). Both walk through
        // `BlobNode` crossings; any child blob the walker mutates
        // gets `mark_dirty(child_guid, seq)` so the checkpoint
        // round flushes them under invariant W2D (WAL-before-data).
        // Sharing one `seq` across both phases keeps the rename
        // atomic from the dirty-tracking perspective — failing
        // halfway leaves a coherent partial-dirty set rather than
        // two separately-staged ops.
        engine::erase_multi(&self.backend, &self.root_pin, &src_padded, seq)?;
        engine::insert_multi(&self.backend, &self.root_pin, &dst_padded, &value, seq)?;
        // Both walker calls mutated the root blob's cached image.
        self.backend.mark_dirty(self.root_guid, seq);

        if let Some(wal) = &self.wal {
            let mut w = wal.lock().unwrap();
            // Fast-path: skips the `TxnOp::RenameObject` enum's
            // two `Vec` clones (src_key, dst_key).
            w.append_rename_object(seq, 0, src, dst, force)?;
            if self.cfg.wal_sync_on_commit {
                w.flush()?;
            }
        } else if self.cfg.flush_on_write {
            // Walker may have dirtied child blobs across the
            // erase + insert sequence — drain the full set.
            // The erase half can also queue SubtreeGone deletes.
            self.flush_dirty_inline()?;
            self.flush_pending_deletes_inline()?;
        }
        Ok(())
    }

    /// Apply a batch of mutations under a single WAL record.
    ///
    /// The closure builds a [`TxnBatch`] by calling its `put` /
    /// `delete` / `rename` methods; on return, holt applies each
    /// op in order against the BM and emits **one** WAL record
    /// (`TxnOp::Batch`) covering the whole sequence. Either every
    /// op is replayed on recovery, or none — the batch is
    /// crash-atomic.
    ///
    /// ## Atomicity contract
    ///
    /// - **Crash atomicity**: yes. The single WAL record is the
    ///   commit point; a crash before it is written rolls back
    ///   the whole batch on next open (the BM cache is reloaded
    ///   from the last checkpoint and replay sees no batch).
    /// - **Runtime isolation**: best-effort. The batch holds
    ///   `rename_lock`, so it serialises against other `rename`
    ///   and `txn` calls but **not** against concurrent
    ///   `put` / `delete` — those still see per-blob exclusive
    ///   latching only. Treat the batch as "all-or-nothing under
    ///   crash recovery", not "fully serializable under load".
    /// - **Mid-batch failure**: if op `N` returns an `Err`
    ///   (e.g., rename `NotFound`), ops `0..N` are already
    ///   applied to the BM and the WAL record is NOT written —
    ///   so on the next open the partial work is lost via replay.
    ///   The current process still sees the partial work through
    ///   the BM cache. Best practice: keep batches to ops you
    ///   know will succeed, or follow a failed `txn` with
    ///   `Tree::checkpoint` only after recovering desired state.
    ///
    /// ## Example
    ///
    /// ```no_run
    /// # use holt::{Tree, TreeConfig};
    /// # let tree = Tree::open(TreeConfig::memory()).unwrap();
    /// tree.txn(|batch| {
    ///     batch.put(b"a", b"1");
    ///     batch.put(b"b", b"2");
    ///     batch.delete(b"c");
    /// })
    /// .unwrap();
    /// ```
    pub fn txn<F>(&self, build: F) -> Result<()>
    where
        F: FnOnce(&mut TxnBatch),
    {
        let mut batch = TxnBatch::default();
        build(&mut batch);
        if batch.pending.is_empty() {
            return Ok(());
        }
        self.apply_batch(batch.pending)
    }

    fn apply_batch(&self, pending: Vec<BatchOp>) -> Result<()> {
        let count = pending.len() as u64;
        // Serialise batches against renames + other batches so the
        // ops here see a coherent rename-free view across the
        // (multi-op) sequence.
        let _r = self.rename_lock.lock().unwrap();
        // Reserve a contiguous seq range so each inner op's seq is
        // `base + index` and replay can derive it without storing
        // per-inner seqs in the body.
        let base_seq = self.next_seq.fetch_add(count, Ordering::SeqCst);
        let mut wal_ops: Vec<TxnOp> = Vec::with_capacity(pending.len());

        for (i, op) in pending.into_iter().enumerate() {
            let seq = base_seq + i as u64;
            match op {
                BatchOp::Put { key, value } => {
                    let entry = self.apply_put_inner(&key, &value, seq)?;
                    wal_ops.push(entry);
                }
                BatchOp::Delete { key } => {
                    if let Some(entry) = self.apply_delete_inner(&key, seq)? {
                        wal_ops.push(entry);
                    }
                    // Pure no-op deletes (key absent) leave no WAL
                    // record, matching `Tree::delete`'s contract.
                }
                BatchOp::Rename { src, dst, force } => {
                    let entry = self.apply_rename_inner(&src, &dst, force, seq)?;
                    wal_ops.push(entry);
                }
            }
        }

        if let Some(wal) = &self.wal {
            let mut w = wal.lock().unwrap();
            let envelope = TxnOp::Batch {
                tree_id: 0,
                ops: wal_ops,
            };
            w.append(&envelope, base_seq)?;
            if self.cfg.wal_sync_on_commit {
                w.flush()?;
            }
        } else if self.cfg.flush_on_write {
            // Every inner op may have dirtied root + cross-blob
            // children — drain the whole set rather than just the
            // root. Inner deletes/renames may also have queued
            // SubtreeGone deferred deletes.
            self.flush_dirty_inline()?;
            self.flush_pending_deletes_inline()?;
        }
        Ok(())
    }

    /// Open a stateful range iterator anchored at this tree.
    ///
    /// Returns a [`RangeBuilder`] for chaining `prefix`,
    /// `start_after`, and `delimiter`. Call
    /// [`RangeBuilder::into_iter`] (or `for entry in builder`) to
    /// start emitting [`RangeEntry`](crate::RangeEntry) items in
    /// lex key order.
    ///
    /// Best-effort snapshot semantics: each iterator step
    /// re-acquires a shared read guard on its current blob; the
    /// iterator does NOT hold a write barrier across calls.
    /// Concurrent mutations between steps may cause a leaf to be
    /// skipped or visited twice (the path stack is raw
    /// `(blob_guid, slot)` pairs, mirroring the upstream
    /// `fa_iter`'s "invalid iterator(#1)" failure mode). For
    /// strict snapshot iteration, pause writes externally
    /// (e.g., call [`Tree::checkpoint`] and don't mutate during
    /// traversal).
    pub fn range(&self) -> RangeBuilder {
        RangeBuilder::new(Arc::clone(&self.backend), self.root_guid)
    }

    /// Drain the BM dirty map and synchronously commit each entry
    /// through the inner backend.
    ///
    /// Used by:
    /// - The no-WAL `flush_on_write` path, where every op must
    ///   reach backend before returning (no checkpointer to defer
    ///   to).
    /// - `Tree::checkpoint`, where the user explicitly asks for a
    ///   full-tree durability barrier.
    ///
    /// `snapshot_dirty` atomically drains the map; concurrent
    /// `mark_dirty` calls land in the fresh empty map and stay
    /// tracked for the next round. `BufferManager::commit` writes
    /// the cached image through and clears the dirty entry on
    /// success (or restores it on failure so a future flush retries).
    fn flush_dirty_inline(&self) -> Result<()> {
        let snap = self.backend.snapshot_dirty();
        for guid in snap.into_keys() {
            self.backend.commit(guid)?;
        }
        Ok(())
    }

    /// Drain the BM pending-delete queue and apply each
    /// `backend.delete_blob` synchronously.
    ///
    /// Companion to [`Self::flush_dirty_inline`] for the deferred
    /// delete protocol — `erase` ops that emptied a child blob
    /// stage the delete here so the manifest mutation can't reach
    /// disk before the WAL record covering the erase is durable
    /// (invariant W2D).
    ///
    /// Must run **after** `flush_dirty_inline` (any new bytes in
    /// dirty land first) and **before** the trailing
    /// `backend.flush` (which persists the manifest deletion).
    /// Restoration is automatic on individual failures — the
    /// remaining entries stay queued for the next attempt.
    fn flush_pending_deletes_inline(&self) -> Result<()> {
        let pending = self.backend.snapshot_pending_deletes();
        let mut failed: std::collections::HashMap<BlobGuid, u64> =
            std::collections::HashMap::new();
        let mut first_err: Option<Error> = None;
        for (guid, seq) in pending {
            if let Err(e) = self.backend.execute_pending_delete(guid) {
                failed.insert(guid, seq);
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        if !failed.is_empty() {
            self.backend.restore_pending_deletes(failed);
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        Ok(())
    }

    fn apply_put_inner(&self, key: &[u8], value: &[u8], seq: u64) -> Result<TxnOp> {
        let padded = pad_key(key);
        let outcome = engine::insert_multi(&self.backend, &self.root_pin, &padded, value, seq)?;
        self.backend.mark_dirty(self.root_guid, seq);
        Ok(TxnOp::Insert {
            tree_id: 0,
            seq,
            key: key.to_vec(),
            value: value.to_vec(),
            prev_value: outcome.previous,
        })
    }

    fn apply_delete_inner(&self, key: &[u8], seq: u64) -> Result<Option<TxnOp>> {
        let padded = pad_key(key);
        let outcome = engine::erase_multi(&self.backend, &self.root_pin, &padded, seq)?;
        if outcome.previous.is_some() {
            // Only an actual erase mutated bytes; the no-op path
            // leaves the cached image byte-identical to backend.
            self.backend.mark_dirty(self.root_guid, seq);
        }
        Ok(outcome.previous.map(|prev| TxnOp::Erase {
            tree_id: 0,
            seq,
            key: key.to_vec(),
            value: prev,
        }))
    }

    fn apply_rename_inner(&self, src: &[u8], dst: &[u8], force: bool, seq: u64) -> Result<TxnOp> {
        let src_padded = pad_key(src);
        let dst_padded = pad_key(dst);
        let Some(value) = engine::lookup_multi(&self.backend, &self.root_pin, &src_padded)? else {
            return Err(Error::NotFound);
        };
        if src != dst {
            if !force && engine::lookup_multi(&self.backend, &self.root_pin, &dst_padded)?.is_some()
            {
                return Err(Error::DstExists);
            }
            engine::erase_multi(&self.backend, &self.root_pin, &src_padded, seq)?;
            engine::insert_multi(&self.backend, &self.root_pin, &dst_padded, &value, seq)?;
            self.backend.mark_dirty(self.root_guid, seq);
        }
        Ok(TxnOp::RenameObject {
            tree_id: 0,
            seq,
            src_key: src.to_vec(),
            dst_key: dst.to_vec(),
            force,
        })
    }

    /// Make every previously-applied mutation durable and trim
    /// the WAL.
    ///
    /// Sequence:
    /// 1. Flush every buffered WAL record (`sync_data` on the log)
    ///    — invariant W2D: WAL must be durable before any byte
    ///    that mirrors a record reaches the data file.
    /// 2. Drain the BM dirty set and write each entry through to
    ///    the inner backend. Covers both the root and any
    ///    cross-blob children the walker has touched since the
    ///    last checkpoint.
    /// 3. Drain the BM pending-delete set and apply each
    ///    `backend.delete_blob` (manifest mutation, in-memory).
    ///    Must follow step 2 so any final bytes for a soon-deleted
    ///    blob are NOT written through (the slot is about to be
    ///    released).
    /// 4. `flush` the backend (`fdatasync` on persistent; no-op on
    ///    memory). Persists the manifest's delete entries in the
    ///    same syscall as the dirty bytes.
    /// 5. Truncate the WAL — its records are now redundant with
    ///    the freshly-durable blob images + manifest, so the next
    ///    replay starts from an empty log.
    ///
    /// `flush_on_write = false` callers rely on this to make
    /// batched writes survive a crash.
    pub fn checkpoint(&self) -> Result<()> {
        if let Some(wal) = &self.wal {
            wal.lock().unwrap().flush()?;
        }
        self.flush_dirty_inline()?;
        self.flush_pending_deletes_inline()?;
        self.backend.flush()?;
        if let Some(wal) = &self.wal {
            wal.lock().unwrap().truncate()?;
        }
        Ok(())
    }

    /// Snapshot per-blob and aggregate counters for every blob
    /// reachable from the root.
    ///
    /// Each blob is pinned + read under a single shared guard, so
    /// stats never block ongoing reads and only contend with writers
    /// on a blob-by-blob basis. Returned counters are a consistent
    /// snapshot of each individual blob but the aggregate is **not**
    /// linearised across blobs — a concurrent writer mid-traversal
    /// can shift one blob's counters before another's are read.
    /// Acceptable for observability; use [`Tree::checkpoint`] first
    /// if you need a quiescent snapshot.
    pub fn stats(&self) -> Result<TreeStats> {
        let guids = engine::collect_blob_guids(&self.backend, self.root_guid)?;
        let mut blobs: Vec<BlobStats> = Vec::with_capacity(guids.len());
        let mut total_space_used: u64 = 0;
        let mut total_gap_space: u64 = 0;
        let mut total_slots: u64 = 0;
        let mut total_compactions: u64 = 0;
        let mut total_tombstones: u64 = 0;
        for guid in &guids {
            let pin = self.backend.pin(*guid)?;
            let guard = pin.read();
            let frame = BlobFrameRef::wrap(guard.as_slice());
            let h = frame.header();
            let s = BlobStats {
                guid: *guid,
                space_used: h.space_used,
                gap_space: h.gap_space,
                num_slots: h.num_slots,
                num_ext_blobs: h.num_ext_blobs,
                compact_times: h.compact_times,
                tombstone_leaf_cnt: h.tombstone_leaf_cnt,
            };
            total_space_used += u64::from(s.space_used);
            total_gap_space += u64::from(s.gap_space);
            total_slots += u64::from(s.num_slots);
            total_compactions += u64::from(s.compact_times);
            total_tombstones += u64::from(s.tombstone_leaf_cnt);
            blobs.push(s);
        }
        let bm_dirty_count = self.backend.dirty_count();
        let checkpointer = self.checkpointer.as_ref().map(|ck| CheckpointerStats {
            rounds_attempted: ck.rounds_attempted(),
            rounds_succeeded: ck.rounds_succeeded(),
            blobs_flushed: ck.blobs_flushed(),
            merges_total: ck.merges_total(),
            truncates: ck.truncates(),
            evictions: ck.evictions(),
        });
        Ok(TreeStats {
            blob_count: blobs.len() as u32,
            total_space_used,
            total_gap_space,
            total_slots,
            total_compactions,
            total_tombstones,
            blobs,
            bm_dirty_count,
            checkpointer,
        })
    }

    /// Compact every blob reachable from the root in place, then
    /// fold every mergeable cross-blob crossing back into its
    /// parent.
    ///
    /// Two phases:
    ///
    /// 1. **Per-blob compact**: every reachable blob is rebuilt in
    ///    place, dropping tombstones and reclaiming bump-area waste.
    ///    `compact_times` bumps by one on each; `tombstone_leaf_cnt`
    ///    resets to zero.
    /// 2. **Tree-wide merge**: each parent blob is walked and every
    ///    mergeable `BlobNode` child is folded back into the parent,
    ///    then the child blob is queued for deferred deletion via
    ///    the BM. A heavy-erase workload that leaves children
    ///    mostly-empty collapses back toward a single root blob.
    ///
    /// Both phases stage their changes via `mark_dirty` /
    /// `mark_for_delete` on the [`crate::store::BufferManager`]
    /// rather than writing through to backend inline. This keeps
    /// compact compatible with invariant **W2D**: a naive
    /// `bm.commit(*guid)` per touched blob would push the cache
    /// image (including any user mutations whose WAL records
    /// aren't yet durable) straight to backend, and a crash
    /// before those WAL records flushed would leave the backend
    /// at a post-mutation state with no journal to reconcile
    /// against — silent data loss after a WAL replay rebuilds
    /// the cache to the pre-mutation state.
    ///
    /// Does **not** fsync the backend or touch the WAL — call
    /// [`Tree::checkpoint`] after if you want the rebuilt blobs
    /// durable on disk. Compaction is logically idempotent (the
    /// post-compact tree is observationally identical to the
    /// pre-compact one), so a crash mid-compact just means the
    /// next run re-does the work; the W2D protocol keeps the
    /// backend image consistent throughout.
    ///
    /// Single-pass merge: nested crossings (a mergeable child
    /// whose own children are themselves merge candidates) aren't
    /// unfolded recursively. Re-invoke `compact` for another pass
    /// if the workload has cascaded crossings.
    pub fn compact(&self) -> Result<()> {
        use crate::store::buffer_manager::STRUCTURAL_SEQ;

        // Phase 1 — per-blob compact.
        let guids = engine::collect_blob_guids(&self.backend, self.root_guid)?;
        for guid in &guids {
            let pin = self.backend.pin(*guid)?;
            {
                let mut guard = pin.write();
                engine::compact_blob(&mut guard)?;
            }
            drop(pin);
            // Stage the rebuilt image; let `Tree::checkpoint` (or
            // the bg checkpointer) push it through under W2D.
            self.backend.mark_dirty(*guid, STRUCTURAL_SEQ);
        }

        // Phase 1.5 — restore the `BlobNode.child_entry_ptr ==
        // child.header.root_slot` invariant that compact_blob broke
        // when it rebuilt each child's root inside its own blob in
        // isolation. Insert / erase rewrite the pair in lock-step
        // inline, so this sweep only matters after a compact.
        engine::refresh_blob_node_pointers(&self.backend, self.root_guid)?;

        // Phase 2 — tree-wide merge pass. Walk parents in BFS order
        // from the root; each parent's `try_merge_children` collapses
        // any direct `BlobNode` child whose blob is small enough to
        // inline. Snapshot the BFS list first; merges performed
        // earlier in the iteration delete the merged child blobs,
        // so later iterations may encounter guids that no longer
        // exist — skip those rather than re-pinning a missing blob.
        let parents = engine::collect_blob_guids(&self.backend, self.root_guid)?;
        for guid in parents {
            if !self.backend.has_blob(guid)? {
                continue;
            }
            let pin = self.backend.pin(guid)?;
            let merged = {
                let mut guard = pin.write();
                let mut frame = BlobFrame::wrap(guard.as_mut_slice());
                engine::try_merge_children(&self.backend, &mut frame, STRUCTURAL_SEQ)?
            };
            drop(pin);
            if merged.merged > 0 {
                self.backend.mark_dirty(guid, STRUCTURAL_SEQ);
            }
        }
        Ok(())
    }

    /// Borrow the active configuration.
    #[must_use]
    pub fn config(&self) -> &TreeConfig {
        &self.cfg
    }

    /// Total bytes a single blob frame consumes — useful for
    /// capacity sizing.
    #[must_use]
    pub const fn page_size() -> u32 {
        PAGE_SIZE
    }
}

/// Replay `path` onto the BM-cached blobs and return the
/// `next_seq` the tree should resume from.
///
/// Each record's logical mutation is re-applied through the
/// engine. Structural ops (`Split` / `Merge` / `Compact`) are
/// already reflected in the blob image on disk, so they're
/// no-ops during replay; `MemMarker` is the explicit
/// post-replay reconciliation marker and is ignored.
///
/// `RenameObject` is rebuilt as the same erase + insert it ran
/// originally. `Rename` (cross-tree) doesn't apply to the
/// single-tree API surface and is rejected. `NewTree` / `RmTree`
/// reserved for multi-tenant deployment; ignored here.
///
/// ## Dirty tracking on replay
///
/// Walker calls (`insert_multi` / `erase_multi`) mutate the
/// BM-cached root + any cross-blob children, but the root's
/// `mark_dirty` is the **caller's** responsibility (see
/// `Tree::put`). Replay must honour that contract — every
/// logical op that landed in cache is mirrored by
/// `bm.mark_dirty(root_guid, seq)`. Without this, a `Tree::open`
/// → `Tree::checkpoint` immediately after replay would find an
/// empty dirty set, write nothing to backend, then truncate the
/// WAL — silently losing every replayed record (the cached image
/// matched the in-memory state but the backend image was still
/// pre-replay; truncating the WAL removed the only durable copy).
fn replay_wal(path: &std::path::Path, bm: &Arc<BufferManager>, root_guid: BlobGuid) -> Result<u64> {
    // Pin the root once for the entire replay loop; saves a
    // BM-Mutex per op (replays can be thousands of ops on a
    // dirty WAL).
    let root_pin = bm.pin(root_guid)?;
    let mut highest = 0u64;
    let _ = replay(path, |op, seq, _off| {
        // Track the highest seq we've **seen** before any branch
        // can short-circuit (e.g. the `RenameObject` no-op arm).
        // `next_seq` must advance past every record on disk, even
        // ones whose effect was already reconciled by an earlier
        // replay pass — otherwise the writer could re-issue an
        // already-used seq.
        highest = highest.max(seq);

        // `touched_root` tracks whether this op actually mutated
        // the BM-cached root image. No-op replays (e.g. an erase
        // for a key already absent because a prior replay pass
        // reconciled it) leave the cache byte-identical to
        // backend — skipping `mark_dirty` for those is a small
        // win and matches `Tree::delete`'s same-shape branch.
        let touched_root = match op {
            TxnOp::Insert { key, value, .. } => {
                let padded = pad_key(key);
                engine::insert_multi(bm, &root_pin, &padded, value, seq)?;
                true
            }
            TxnOp::Erase { key, .. } => {
                let padded = pad_key(key);
                let outcome = engine::erase_multi(bm, &root_pin, &padded, seq)?;
                outcome.previous.is_some()
            }
            TxnOp::RenameObject {
                src_key,
                dst_key,
                force,
                ..
            } => {
                let src_padded = pad_key(src_key);
                let dst_padded = pad_key(dst_key);
                if engine::lookup_multi(bm, &root_pin, &src_padded)?.is_none() {
                    // Already reconciled in a prior replay pass —
                    // skip. `highest` was bumped above so the
                    // post-replay `next_seq` still advances past
                    // this record's seq.
                    return Ok(());
                }
                if !force && engine::lookup_multi(bm, &root_pin, &dst_padded)?.is_some() {
                    return Ok(());
                }
                let value = engine::lookup_multi(bm, &root_pin, &src_padded)?.unwrap_or_default();
                engine::erase_multi(bm, &root_pin, &src_padded, seq)?;
                engine::insert_multi(bm, &root_pin, &dst_padded, &value, seq)?;
                true
            }
            // Structural / multi-tenant / marker variants don't
            // affect logical state at the single-tree API surface.
            // `Batch` is unpacked into per-inner callbacks inside
            // `journal::reader::replay_bytes`, so it never reaches
            // this match — defensive arm only.
            TxnOp::Split { .. }
            | TxnOp::Merge { .. }
            | TxnOp::Compact { .. }
            | TxnOp::Rename { .. }
            | TxnOp::NewTree { .. }
            | TxnOp::RmTree { .. }
            | TxnOp::MemMarker { .. }
            | TxnOp::Batch { .. } => false,
        };
        if touched_root {
            // Honour the walker's caller-side `mark_dirty(root,
            // seq)` contract — see the module doc above. The
            // walker itself only marks cross-blob children dirty
            // (via `mark_dirty(child_guid, seq)` inside
            // `insert_at_blob_node` / `erase_at_blob_node`); the
            // root is always the caller's job.
            bm.mark_dirty(root_guid, seq);
        }
        Ok(())
    })?;
    // After commit, the blob image is durable; we still want the
    // next allocated seq to be strictly greater than anything
    // ever seen in the log.
    Ok(highest + 1)
}
