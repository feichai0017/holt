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
//! - **Reads** (`get`) take the shared maintenance gate, then walk
//!   every blob in **optimistic** mode. The walker snapshots the
//!   latch version, reads the buffer, then validates; on a torn
//!   read it restarts from the root. Readers never block writers
//!   and writers never block readers; only structural maintenance
//!   (`compact` / merge) takes the exclusive side.
//! - **Writes** (`put` / `delete`) take **exclusive** mode on
//!   each blob they touch. Persistent trees additionally enter a
//!   short commit-publish critical section so checkpoint snapshots
//!   never clone bytes that lack an admitted WAL record. Durable
//!   fsync waiting happens after that section through the journal
//!   group-commit worker.
//! - **Structural maintenance** (`compact` and background merge)
//!   takes a narrow tree-wide maintenance gate. Normal reads and
//!   writers take the shared side while they may cross `BlobNode`
//!   boundaries; blob-local access still relies on per-blob
//!   optimistic validation.
//! - **`rename`** is multi-step (lookup probe + erase + insert)
//!   and must be atomic across all three. It takes the
//!   `rename_lock` (a `Mutex<()>` scoped to rename only) to
//!   prevent racing renames from interleaving.

use std::mem::MaybeUninit;
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::config::{Storage, TreeConfig};
use super::errors::{Error, Result};
use super::stats::{BlobStats, CheckpointerStats, JournalStats, TreeStats};
use crate::concurrency::MaintenanceGate;
use crate::engine;
use crate::engine::RangeBuilder;
use crate::journal::codec::{
    encode_erase_record, encode_insert_record, encode_rename_object_record, BatchEncoder,
};
use crate::journal::group_commit::Journal;
use crate::journal::reader::replay;
use crate::journal::txn_op::TxnOp;
use crate::layout::{BlobGuid, PAGE_SIZE};
use crate::store::backend::{AlignedBlobBuf, Backend, MemoryBackend, PersistentBackend};
use crate::store::{BlobFrame, BlobFrameRef, BufferManager, CachedBlob};

use super::txn::{BatchOp, TxnBatch};

/// An `holt` tree — your handle to one metadata store.
///
/// Clone the handle to share the same backing store: the
/// internal `BufferManager` is held via `Arc`.
///
/// ## Concurrency
///
/// - **Reads** (`get`, `range`, `scan_prefix`) take the shared
///   maintenance gate, then run against
///   `HybridLatch::read_optimistic` — they capture each blob's
///   latch version, read the bytes, then `validate()`. Restarts
///   from the root on a torn read. Never blocks foreground writers
///   and never block each other.
/// - **Writes** (`put`, `delete`) hold the per-blob `HybridLatch`
///   exclusively for the blobs they touch. Persistent trees also
///   take `commit_lock` while publishing the mutation to dirty
///   tracking and the journal queue; the expensive durable wait
///   happens after that lock through the group-commit worker.
/// - **Maintenance** (`compact`, background merge) takes the
///   exclusive side of `maintenance_gate`; foreground reads and
///   writers enter the shared side around tree traversal. This
///   blocks subtree merge/delete while an operation is crossing
///   blobs, while keeping all ordinary operations mutually
///   concurrent.
/// - **`rename`** holds `rename_lock` (a `Mutex<()>` scoped to
///   rename only) so its multi-step
///   `lookup → erase → insert` appears atomic to other writers.
///   `put` / `delete` / `get` never take `rename_lock`.
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
    /// `lookup_multi_with(src)` + `erase_multi(src)` + `insert_multi(dst)`
    /// must appear atomic to other writers. `put` / `delete` /
    /// `get` never take this lock; they coordinate via the
    /// per-blob `HybridLatch` inside the BM.
    rename_lock: Arc<Mutex<()>>,
    /// Tree-wide structural-maintenance gate.
    ///
    /// Foreground read and mutation paths enter the shared side
    /// while they may cross `BlobNode` boundaries. `compact()` and
    /// the background merge pass enter the exclusive side before
    /// folding a child blob back into its parent and queuing the
    /// child for delete.
    /// Point reads participate too: per-blob optimistic validation
    /// handles in-place rewrites, while the maintenance gate keeps
    /// a merge pass from deleting a child blob after a reader has
    /// observed the parent `BlobNode` but before it pins the child.
    maintenance_gate: Arc<MaintenanceGate>,
    /// Monotonically-increasing sequence stamped on every record.
    /// On open the tree replays the WAL and resumes at
    /// `highest_seq + 1`.
    next_seq: Arc<AtomicU64>,
    /// Serialises the cache-mutation → dirty-publish →
    /// journal-submit boundary in persistent mode. Checkpoint
    /// snapshots take the same lock while draining dirty entries
    /// and cloning their bytes, so no backend write can include
    /// a mutation whose WAL record was not already admitted.
    commit_lock: Arc<Mutex<()>>,
    /// Group-commit WAL worker — `Some` for persistent trees,
    /// `None` for memory trees.
    journal: Option<Arc<Journal>>,
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

const INLINE_PADDED_KEY_CAP: usize = 256;

struct PaddedKey {
    inline: [MaybeUninit<u8>; INLINE_PADDED_KEY_CAP],
    len: usize,
    heap: Option<Vec<u8>>,
}

impl PaddedKey {
    #[inline]
    fn as_slice(&self) -> &[u8] {
        match &self.heap {
            Some(heap) => heap,
            None => {
                // SAFETY: `pad_key` initializes exactly `len`
                // bytes in the inline buffer: the user key bytes
                // plus the trailing internal terminator.
                unsafe { std::slice::from_raw_parts(self.inline.as_ptr().cast::<u8>(), self.len) }
            }
        }
    }
}

impl Deref for PaddedKey {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

/// Append the engine's internal terminator byte (`\0`) to a
/// user-supplied key. See the module docs.
#[inline]
fn pad_key(key: &[u8]) -> PaddedKey {
    let len = key.len() + 1;
    if len <= INLINE_PADDED_KEY_CAP {
        let mut inline = [MaybeUninit::uninit(); INLINE_PADDED_KEY_CAP];
        let ptr = inline.as_mut_ptr().cast::<u8>();
        // SAFETY: `len <= INLINE_PADDED_KEY_CAP`; we write the
        // first `key.len()` bytes from `key`, then exactly one
        // terminator byte after them.
        unsafe {
            ptr.copy_from_nonoverlapping(key.as_ptr(), key.len());
            ptr.add(key.len()).write(0);
        }
        return PaddedKey {
            inline,
            len,
            heap: None,
        };
    }

    let mut padded = Vec::with_capacity(len);
    padded.extend_from_slice(key);
    padded.push(0u8);
    PaddedKey {
        inline: [MaybeUninit::uninit(); INLINE_PADDED_KEY_CAP],
        len: 0,
        heap: Some(padded),
    }
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
    /// `BufferManager` of `cfg.buffer_pool_size` blobs.
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
        // committed via `memory_flush_on_write = true`.
        let (journal, next_seq) = if attach_wal {
            match cfg.wal_path() {
                None => (None, 1u64),
                Some(path) => {
                    let next_seq = if path.exists() {
                        replay_wal(&path, &bm, root_guid)?
                    } else {
                        1
                    };
                    let journal = Journal::open_or_create(&path, /*tree_id=*/ 0)?;
                    (Some(Arc::new(journal)), next_seq)
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

        // Shared structural gate for foreground writers, manual
        // compact, and the background merge pass.
        let maintenance_gate = Arc::new(MaintenanceGate::new());
        let commit_lock = Arc::new(Mutex::new(()));

        // Spawn the background checkpointer if opted-in.
        // `Checkpointer::spawn` returns `None` for disabled
        // configs, so the `Option` chain stays clean.
        let checkpointer = crate::checkpoint::Checkpointer::spawn(
            Arc::clone(&bm),
            journal.clone(),
            root_guid,
            Arc::clone(&maintenance_gate),
            Arc::clone(&commit_lock),
            cfg.checkpoint.clone(),
        )
        .map(Arc::new);

        Ok(Self {
            cfg,
            backend: bm,
            root_guid,
            root_pin,
            rename_lock: Arc::new(Mutex::new(())),
            maintenance_gate,
            next_seq: Arc::new(AtomicU64::new(next_seq)),
            commit_lock,
            journal,
            checkpointer,
        })
    }

    /// Look up `key`. Returns the value bytes, or `None` if no leaf
    /// matches.
    ///
    /// Pays one allocation + memcpy per hit; on a miss returns
    /// `Ok(None)` with no allocation. The walker itself reads
    /// cached blobs optimistically and restarts from the root when
    /// a concurrent writer invalidates its snapshot.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let _maintenance = self.maintenance_gate.enter_shared();
        let padded = pad_key(key);
        engine::lookup_multi_with(&self.backend, &self.root_pin, &padded, <[u8]>::to_vec)
    }

    /// Insert or replace `(key, value)`. Returns `Ok(())`.
    ///
    /// Blind hot path: the walker does **not** read or clone the
    /// existing leaf's value on a same-key update. Pair with
    /// [`Tree::insert`] when the caller actually needs the prior
    /// value back — that variant pays the read + clone.
    ///
    /// Walks across `BlobNode` crossings. When any blob hits
    /// `AllocError::OutOfSpace`, the walker automatically migrates
    /// a subtree out via `splitBlob` and retries — so trees may
    /// grow well past the 512 KB single-blob limit without caller
    /// involvement.
    ///
    /// Mutates the BM-pinned root buffer in place under an
    /// exclusive write guard. Cross-blob mutations stage their
    /// changes via `mark_dirty` / `install_new_blob`; the durable
    /// write to the inner backend happens when the WAL record
    /// covering this op is on disk — driven either by the
    /// background checkpoint round or by [`Tree::checkpoint`].
    /// Per-op `memory_flush_on_write` mode drains the dirty set
    /// inline after the WAL append.
    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.put_inner(key, value, /*wants_prev=*/ false)
            .map(|_| ())
    }

    /// Insert or replace `(key, value)`. Returns the previous value
    /// if the key already existed (`None` on a fresh key or
    /// resurrected tombstone).
    ///
    /// Pays the per-op cost of reading + cloning the existing
    /// leaf's value on a same-key update. Use [`Tree::put`] when
    /// you don't need the prior value — that's the blind hot
    /// path and the right default for metadata workloads.
    pub fn insert(&self, key: &[u8], value: &[u8]) -> Result<Option<Vec<u8>>> {
        self.put_inner(key, value, /*wants_prev=*/ true)
    }

    /// Shared implementation behind [`Tree::put`] (blind) and
    /// [`Tree::insert`] (returning). `wants_prev` controls whether
    /// the walker materialises the existing leaf's value bytes —
    /// only the caller-visible return path is affected; the WAL
    /// record is identical for both variants (key + value only,
    /// no prev_value field on disk since v0.3.1 / format v3).
    ///
    /// W2D-strict protocol (WAL mode): walker descent, `mark_dirty`,
    /// and journal submission happen inside `commit_lock`. The
    /// checkpoint round drains dirty entries and snapshots bytes
    /// under the same lock, so any backend image it writes has a
    /// WAL record admitted before the clone. Durable `sync_data`
    /// waiting runs outside the lock through the journal worker.
    fn put_inner(&self, key: &[u8], value: &[u8], wants_prev: bool) -> Result<Option<Vec<u8>>> {
        let _maintenance = self.maintenance_gate.enter_shared();
        let padded = pad_key(key);
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);

        let (outcome, journal_ack) = if let Some(journal) = &self.journal {
            let _commit = self.commit_lock.lock().unwrap();
            let outcome = engine::insert_multi(
                &self.backend,
                &self.root_pin,
                &padded,
                value,
                seq,
                wants_prev,
            )?;
            if outcome.root_dirty {
                self.backend.mark_dirty(self.root_guid, seq);
            }
            let mut record = Vec::new();
            encode_insert_record(&mut record, seq, 0, key, value);
            let ack = journal.submit(record, self.cfg.wal_sync_on_commit)?;
            (outcome, ack)
        } else {
            // No WAL — no journal/checkpoint publish boundary to
            // race with. `memory_flush_on_write` (if set)
            // flushes dirty + pending-delete sets inline before
            // returning.
            let outcome = engine::insert_multi(
                &self.backend,
                &self.root_pin,
                &padded,
                value,
                seq,
                wants_prev,
            )?;
            if outcome.root_dirty {
                self.backend.mark_dirty(self.root_guid, seq);
            }
            if self.cfg.memory_flush_on_write {
                self.flush_dirty_inline()?;
                self.flush_pending_deletes_inline()?;
            }
            (outcome, None)
        };
        if let Some(ack) = journal_ack {
            ack.wait()?;
        }
        Ok(outcome.previous)
    }

    /// Remove `key`. Returns `Ok(true)` if a leaf was removed,
    /// `Ok(false)` if no leaf matched.
    ///
    /// Blind hot path: the walker does **not** read or clone the
    /// existing leaf's value before tombstoning it. Pair with
    /// [`Tree::remove`] when the caller actually needs the prior
    /// value back.
    ///
    /// Walks across `BlobNode` crossings. Child-local mutations
    /// are staged through the BM dirty set; any conservative
    /// fallback that unlinks a child blob queues the manifest
    /// delete through the same W2D-safe pending-delete protocol.
    pub fn delete(&self, key: &[u8]) -> Result<bool> {
        self.delete_inner(key, /*wants_prev=*/ false)
            .map(|outcome| outcome.mutated)
    }

    /// Remove `key` and return the value that was stored there
    /// (`None` if no leaf matched).
    ///
    /// Pays the per-op cost of reading + cloning the existing
    /// leaf's value before tombstoning it. Use [`Tree::delete`]
    /// when you only need to know whether the key existed.
    pub fn remove(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.delete_inner(key, /*wants_prev=*/ true)
            .map(|outcome| outcome.previous)
    }

    /// Shared implementation behind [`Tree::delete`] (blind bool)
    /// and [`Tree::remove`] (returning prev). W2D-strict protocol
    /// mirrors [`Self::put_inner`].
    ///
    /// Blind delete (`wants_prev = false`) saves the walker
    /// leaf-extent value read; the WAL record itself is identical
    /// for both variants (key only since v0.3.1 / format v3).
    /// `EraseOutcome.mutated` is the authoritative "anything
    /// happened" signal, independent of `EraseOutcome.previous`.
    fn delete_inner(&self, key: &[u8], wants_prev: bool) -> Result<engine::EraseOutcome> {
        let _maintenance = self.maintenance_gate.enter_shared();
        let padded = pad_key(key);
        // Pre-allocate the seq before the walker descends so any
        // child blob the walker touches can `mark_dirty(child, seq)`
        // — invariant W2D (see `BufferManager` module docs) demands
        // a single seq for the whole op across all blobs it dirties.
        // A no-op delete (key absent) still burns the seq; that's
        // fine — `next_seq` is monotonic and the unused seq doesn't
        // appear in any WAL record or dirty entry.
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);

        let (outcome, journal_ack) = if let Some(journal) = &self.journal {
            let _commit = self.commit_lock.lock().unwrap();
            let outcome =
                engine::erase_multi(&self.backend, &self.root_pin, &padded, seq, wants_prev)?;
            if outcome.mutated {
                // Only mark the root if the root blob changed.
                // Cross-blob erases mark their child blob inside
                // the walker; absent-key no-ops mark nothing.
                if outcome.root_dirty {
                    self.backend.mark_dirty(self.root_guid, seq);
                }
                let mut record = Vec::new();
                encode_erase_record(&mut record, seq, 0, key);
                let ack = journal.submit(record, self.cfg.wal_sync_on_commit)?;
                (outcome, ack)
            } else {
                (outcome, None)
            }
            // No-op delete (key wasn't there) is not logged.
        } else {
            let outcome =
                engine::erase_multi(&self.backend, &self.root_pin, &padded, seq, wants_prev)?;
            if outcome.mutated && outcome.root_dirty {
                self.backend.mark_dirty(self.root_guid, seq);
            }
            if self.cfg.memory_flush_on_write {
                // Flush every blob the walker touched (root + any
                // children) — no WAL means this is the sole
                // durability path. snapshot_dirty drains all
                // entries; we commit each through the backend.
                self.flush_dirty_inline()?;
                // Plus drain any deferred deletes queued by a
                // conservative parent-unlink path.
                self.flush_pending_deletes_inline()?;
            }
            (outcome, None)
        };
        if let Some(ack) = journal_ack {
            ack.wait()?;
        }
        Ok(outcome)
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
        let _maintenance = self.maintenance_gate.enter_shared();
        let src_padded = pad_key(src);
        let dst_padded = pad_key(dst);

        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let _r = self.rename_lock.lock().unwrap();

        // Probe src across all blobs — zero-copy via BM pin.
        let Some(value) =
            engine::lookup_multi_with(&self.backend, &self.root_pin, &src_padded, <[u8]>::to_vec)?
        else {
            return Err(Error::NotFound);
        };

        // Same key? No-op (seq is already bumped).
        if src == dst {
            return Ok(());
        }

        // Probe dst across all blobs unless overwrite is allowed.
        if !force
            && engine::lookup_multi_with(
                &self.backend,
                &self.root_pin,
                &dst_padded,
                <[u8]>::to_vec,
            )?
            .is_some()
        {
            return Err(Error::DstExists);
        }

        // W2D-strict protocol: walker + mark_dirty + journal
        // submission all happen under `commit_lock`. Sharing one
        // `seq` across both erase + insert phases keeps the rename
        // atomic from the dirty-tracking perspective — failing
        // halfway leaves a coherent partial-dirty set rather than
        // two separately-staged ops.
        //
        // Both walker calls pass `wants_prev=false`: the rename
        // already read the src value (above) and the dst existence
        // check (or `force=true`) gates the insert side, so the
        // walker-materialised previous values would just be
        // dropped on the floor.
        let journal_ack = if let Some(journal) = &self.journal {
            let _commit = self.commit_lock.lock().unwrap();
            let erase_out =
                engine::erase_multi(&self.backend, &self.root_pin, &src_padded, seq, false)?;
            let insert_out = engine::insert_multi(
                &self.backend,
                &self.root_pin,
                &dst_padded,
                &value,
                seq,
                false,
            )?;
            if erase_out.root_dirty || insert_out.root_dirty {
                self.backend.mark_dirty(self.root_guid, seq);
            }
            let mut record = Vec::new();
            encode_rename_object_record(&mut record, seq, 0, src, dst, force);
            journal.submit(record, self.cfg.wal_sync_on_commit)?
        } else {
            let erase_out =
                engine::erase_multi(&self.backend, &self.root_pin, &src_padded, seq, false)?;
            let insert_out = engine::insert_multi(
                &self.backend,
                &self.root_pin,
                &dst_padded,
                &value,
                seq,
                false,
            )?;
            if erase_out.root_dirty || insert_out.root_dirty {
                self.backend.mark_dirty(self.root_guid, seq);
            }
            if self.cfg.memory_flush_on_write {
                // Walker may have dirtied child blobs across the
                // erase + insert sequence — drain the full set.
                // The erase half can also queue SubtreeGone deletes.
                self.flush_dirty_inline()?;
                self.flush_pending_deletes_inline()?;
            }
            None
        };
        if let Some(ack) = journal_ack {
            ack.wait()?;
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
        let _maintenance = self.maintenance_gate.enter_shared();
        let count = pending.len() as u64;
        // Serialise batches against renames + other batches so the
        // ops here see a coherent rename-free view across the
        // (multi-op) sequence.
        let _r = self.rename_lock.lock().unwrap();
        // Reserve a contiguous seq range so each inner op's seq is
        // `base + index` and replay can derive it without storing
        // per-inner seqs in the body. Pure no-op deletes burn their
        // seq (BM dirty tracking is unaffected since no mutation
        // happened); `next_seq` is monotonic regardless.
        let base_seq = self.next_seq.fetch_add(count, Ordering::Relaxed);

        // W2D-strict protocol: all inner ops' walker mutations +
        // `mark_dirty` calls, plus the single envelope WAL submit,
        // happen under `commit_lock` — see `Tree::put_inner`.
        if let Some(journal) = &self.journal {
            let ack = {
                let _commit = self.commit_lock.lock().unwrap();
                let mut record = Vec::new();
                let mut enc = BatchEncoder::begin(&mut record, base_seq, 0);
                self.apply_batch_walker_inline(pending, base_seq, Some(&mut enc))?;
                let _n = enc.finish();
                journal.submit(record, self.cfg.wal_sync_on_commit)?
            };
            if let Some(ack) = ack {
                ack.wait()?;
            }
        } else {
            self.apply_batch_walker_inline(pending, base_seq, None)?;
            if self.cfg.memory_flush_on_write {
                // Every inner op may have dirtied root + cross-blob
                // children — drain the whole set rather than just
                // the root. Some fallback paths may also have
                // queued deferred deletes.
                self.flush_dirty_inline()?;
                self.flush_pending_deletes_inline()?;
            }
        }
        Ok(())
    }

    /// Walker-mutation + optional WAL-encode loop, shared between
    /// the WAL-on and WAL-off branches of [`Self::apply_batch`].
    ///
    /// When `enc` is `Some`, each successful walker mutation is
    /// followed by a `push_*` call on the encoder; when `None`, the
    /// walker mutations run alone (memory-only mode). Pulling the
    /// loop out keeps the two batch paths from drifting and makes
    /// the per-variant arms readable without macro tricks.
    fn apply_batch_walker_inline(
        &self,
        pending: Vec<BatchOp>,
        base_seq: u64,
        mut enc: Option<&mut crate::journal::codec::BatchEncoder<'_>>,
    ) -> Result<()> {
        for (i, op) in pending.into_iter().enumerate() {
            let seq = base_seq + i as u64;
            match op {
                BatchOp::Put { key, value } => {
                    let padded = pad_key(&key);
                    let outcome = engine::insert_multi(
                        &self.backend,
                        &self.root_pin,
                        &padded,
                        &value,
                        seq,
                        false,
                    )?;
                    if outcome.root_dirty {
                        self.backend.mark_dirty(self.root_guid, seq);
                    }
                    if let Some(enc) = enc.as_deref_mut() {
                        enc.push_insert(0, &key, &value);
                    }
                }
                BatchOp::Delete { key } => {
                    let padded = pad_key(&key);
                    let outcome =
                        engine::erase_multi(&self.backend, &self.root_pin, &padded, seq, false)?;
                    if outcome.mutated {
                        if outcome.root_dirty {
                            self.backend.mark_dirty(self.root_guid, seq);
                        }
                        if let Some(enc) = enc.as_deref_mut() {
                            enc.push_erase(0, &key);
                        }
                    }
                    // Pure no-op deletes leave no WAL inner op,
                    // matching `Tree::delete`'s contract.
                }
                BatchOp::Rename { src, dst, force } => {
                    let src_padded = pad_key(&src);
                    let dst_padded = pad_key(&dst);
                    let Some(value) = engine::lookup_multi_with(
                        &self.backend,
                        &self.root_pin,
                        &src_padded,
                        <[u8]>::to_vec,
                    )?
                    else {
                        return Err(Error::NotFound);
                    };
                    if src != dst {
                        if !force
                            && engine::lookup_multi_with(
                                &self.backend,
                                &self.root_pin,
                                &dst_padded,
                                |_| (),
                            )?
                            .is_some()
                        {
                            return Err(Error::DstExists);
                        }
                        let erase_out = engine::erase_multi(
                            &self.backend,
                            &self.root_pin,
                            &src_padded,
                            seq,
                            false,
                        )?;
                        let insert_out = engine::insert_multi(
                            &self.backend,
                            &self.root_pin,
                            &dst_padded,
                            &value,
                            seq,
                            false,
                        )?;
                        if erase_out.root_dirty || insert_out.root_dirty {
                            self.backend.mark_dirty(self.root_guid, seq);
                        }
                    }
                    if let Some(enc) = enc.as_deref_mut() {
                        enc.push_rename_object(0, &src, &dst, force);
                    }
                }
            }
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
    /// re-acquires the shared maintenance gate plus a shared read
    /// guard on its current blob; the iterator does NOT hold a
    /// write barrier across calls.
    /// Concurrent mutations between steps may cause a leaf to be
    /// skipped or visited twice (the path stack is raw
    /// `(blob_guid, slot)` pairs, mirroring the upstream
    /// `fa_iter`'s "invalid iterator(#1)" failure mode). For
    /// strict snapshot iteration, pause writes externally
    /// (e.g., call [`Tree::checkpoint`] and don't mutate during
    /// traversal).
    pub fn range(&self) -> RangeBuilder {
        RangeBuilder::new(
            Arc::clone(&self.backend),
            self.root_guid,
            Arc::clone(&self.maintenance_gate),
        )
    }

    /// Shorthand for `tree.range().prefix(p)` — the
    /// common-90%-of-queries case.
    ///
    /// Returns a [`RangeBuilder`] already anchored to `prefix`;
    /// chain additional filters (`start_after`, `delimiter`)
    /// before iterating.
    pub fn scan_prefix(&self, prefix: &[u8]) -> RangeBuilder {
        self.range().prefix(prefix)
    }

    /// Drain the BM dirty map and synchronously push each entry
    /// to the inner backend via `write_through` (CAS-on-seq).
    ///
    /// Used by:
    /// - The no-WAL `memory_flush_on_write` path, where every op must
    ///   reach backend before returning (no checkpointer to defer
    ///   to).
    /// - `Tree::checkpoint`, where the user explicitly asks for
    ///   a full-tree durability barrier.
    ///
    /// `snapshot_dirty` atomically drains the map; concurrent
    /// `mark_dirty` calls land in the fresh empty map and stay
    /// tracked for the next round. `write_through(expected_seq)`
    /// matches the checkpoint round's protocol: the dirty entry
    /// is retired only when no racing writer has bumped its seq
    /// in the meantime (snapshot 的 expected_seq 反映了我们抓到的
    /// 那个 entry；之后 racing writer 写的 newer-seq 留给下一次
    /// flush).
    fn flush_dirty_inline(&self) -> Result<()> {
        let snap = self.backend.snapshot_dirty();
        let mut failed: std::collections::HashMap<BlobGuid, u64> = std::collections::HashMap::new();
        let mut first_err: Option<Error> = None;
        for (guid, expected_seq) in snap {
            // `snapshot_bytes` clones the cached image under a
            // brief shared read guard so we hand owned bytes to
            // `write_through`. `None` means the blob was evicted
            // between snapshot_dirty and snapshot_bytes — drop
            // the dirty entry on the floor; the eviction path
            // already cleared `dirty` for it.
            if let Some(bytes) = self.backend.snapshot_bytes(guid) {
                if let Err(e) = self.backend.write_through(guid, &bytes, expected_seq) {
                    failed.insert(guid, expected_seq);
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }
        if !failed.is_empty() {
            self.backend.restore_dirty(failed);
        }
        if let Some(e) = first_err {
            return Err(e);
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
        let mut failed: std::collections::HashMap<BlobGuid, u64> = std::collections::HashMap::new();
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

    /// Make every previously-applied mutation durable and trim
    /// the WAL.
    ///
    /// Mirrors the background checkpoint round's protocol so a
    /// manual checkpoint is just as concurrency-safe against
    /// in-flight writers as the background path. Phases are kept
    /// strictly ordered around the W2D invariant; every error
    /// path restores any snapshot it drained so the next round
    /// retries.
    ///
    /// 1. **Snapshot + journal flush** (under `commit_lock`):
    ///    drain BM dirty + pending-delete sets, force the journal
    ///    durable, and clone each snapshotted blob's bytes before
    ///    releasing the lock. Journal flush failure → restore both
    ///    snapshots, return.
    /// 2. **Per-blob write-through** with CAS-on-seq. The CAS
    ///    retires the dirty entry only if no racing writer bumped
    ///    it; failures stay in `dirty` for the next round.
    /// 3. **Pre-delete sync** — `backend.flush` (`sync_data` on
    ///    the data file + persist the manifest) so step 2's
    ///    writes hit stable storage *before* any manifest delete
    ///    runs. Sync failure → restore pending, return.
    /// 4. **Abort-on-dirty-failure gate**. If any write_through
    ///    at step 2 failed, the round must NOT apply pending
    ///    deletes: a parent that didn't flush might still
    ///    reference a child that's about to be removed from the
    ///    manifest, leaving the on-disk parent pointing into a
    ///    deleted slot. Restore pending and return the dirty
    ///    error. The next round will retry the parent write and
    ///    only then process its child's deletion.
    /// 5. **Apply pending deletes** (manifest mutation
    ///    in-memory). Each `execute_pending_delete` is idempotent
    ///    against a missing entry; failures are restored.
    /// 6. **Post-delete sync** — re-`backend.flush` iff any delete
    ///    actually applied. Failure → restore the
    ///    already-applied entries so the truncate gate stays
    ///    closed and the next round retries the sync (the manifest
    ///    delete is idempotent on the second pass).
    /// 7. **Conditional WAL truncate** — only if
    ///    `dirty_count == 0` AND `pending_delete_count == 0`
    ///    *now*. A racing writer or a restored failure must keep
    ///    the WAL alive until a future flush.
    ///
    /// `memory_flush_on_write = false` callers rely on this to make
    /// batched writes survive a crash.
    pub fn checkpoint(&self) -> Result<()> {
        use std::collections::HashMap;

        let _maintenance = self.maintenance_gate.enter_shared();

        // Phase 1: snapshot dirty/pending, force the journal
        // durable, and clone the snapshotted bytes under
        // `commit_lock`. This closes the subtle W2D hole where a
        // foreground writer mutates a blob after the dirty snapshot
        // but before `snapshot_bytes`: without the shared lock, the
        // checkpoint could write bytes whose WAL record was not in
        // the flushed snapshot.
        let (_snap_dirty, snap_pending, snap_bytes) = if let Some(journal) = &self.journal {
            let _commit = self.commit_lock.lock().unwrap();
            let snap_dirty = self.backend.snapshot_dirty();
            let snap_pending = self.backend.snapshot_pending_deletes();
            if let Err(e) = journal.flush() {
                self.backend.restore_dirty(snap_dirty);
                self.backend.restore_pending_deletes(snap_pending);
                return Err(e);
            }
            let mut snap_bytes = Vec::with_capacity(snap_dirty.len());
            for (guid, expected_seq) in &snap_dirty {
                let Some(bytes) = self.backend.snapshot_bytes(*guid) else {
                    self.backend.restore_dirty(snap_dirty);
                    self.backend.restore_pending_deletes(snap_pending);
                    return Err(Error::Internal(
                        "checkpoint: dirty entry lost cache image — invariant I1 violated",
                    ));
                };
                snap_bytes.push((*guid, *expected_seq, bytes));
            }
            (snap_dirty, snap_pending, snap_bytes)
        } else {
            let snap_dirty = self.backend.snapshot_dirty();
            let snap_pending = self.backend.snapshot_pending_deletes();
            let mut snap_bytes = Vec::with_capacity(snap_dirty.len());
            for (guid, expected_seq) in &snap_dirty {
                let Some(bytes) = self.backend.snapshot_bytes(*guid) else {
                    self.backend.restore_dirty(snap_dirty);
                    self.backend.restore_pending_deletes(snap_pending);
                    return Err(Error::Internal(
                        "checkpoint: dirty entry lost cache image — invariant I1 violated",
                    ));
                };
                snap_bytes.push((*guid, *expected_seq, bytes));
            }
            (snap_dirty, snap_pending, snap_bytes)
        };

        // Phase 2: per-blob write_through with CAS-on-seq.
        //
        // A drained dirty entry **must** have a cache image —
        // invariant I1 (dirty ⟺ cache newer than backend). If
        // `snapshot_bytes` returns `None`, the BM's eviction
        // policy regressed and dropped a dirty cache image; that
        // would otherwise be a silent data-loss path (the next
        // checkpoint sees `dirty == 0` and truncates the WAL).
        // Restore both snapshots and bail loud.
        let mut dirty_failed: HashMap<BlobGuid, u64> = HashMap::new();
        let mut first_dirty_err: Option<Error> = None;
        for (guid, expected_seq, bytes) in &snap_bytes {
            if let Err(e) = self.backend.write_through(*guid, bytes, *expected_seq) {
                dirty_failed.insert(*guid, *expected_seq);
                if first_dirty_err.is_none() {
                    first_dirty_err = Some(e);
                }
            }
        }
        let had_dirty_failure = !dirty_failed.is_empty();
        if had_dirty_failure {
            self.backend.restore_dirty(dirty_failed);
        }

        // Phase 3: pre-delete sync. Even when some writes failed at
        // phase 2, the successful ones already retired their dirty
        // entries via the write_through CAS — we must still fsync
        // so those bytes are stable on disk. On sync failure,
        // pending deletes haven't been applied yet, so restore them
        // and bail.
        if let Err(e) = self.backend.flush() {
            self.backend.restore_pending_deletes(snap_pending);
            return Err(e);
        }

        // Phase 4: abort-on-dirty-failure gate. A failed parent
        // write_through must NOT propagate to a manifest delete of
        // its dependent child — that would orphan the parent's
        // BlobNode pointer (parent on-disk still has the child
        // pointer; manifest no longer has the child entry; WAL
        // replay's walker descent would fail to read the deleted
        // child). Restore the entire pending snapshot and surface
        // the dirty error.
        if had_dirty_failure {
            self.backend.restore_pending_deletes(snap_pending);
            return Err(first_dirty_err.expect("had_dirty_failure ⇒ first_dirty_err set"));
        }

        // Phase 5: apply pending deletes (manifest mutation).
        let mut pending_failed: HashMap<BlobGuid, u64> = HashMap::new();
        let mut first_pending_err: Option<Error> = None;
        for (guid, seq) in &snap_pending {
            if let Err(e) = self.backend.execute_pending_delete(*guid) {
                pending_failed.insert(*guid, *seq);
                if first_pending_err.is_none() {
                    first_pending_err = Some(e);
                }
            }
        }
        if !pending_failed.is_empty() {
            self.backend.restore_pending_deletes(pending_failed.clone());
        }

        // Phase 6: post-delete sync iff any delete actually applied.
        // On sync failure, restore the already-applied entries to
        // pending — the manifest mutation we did at phase 5 is
        // stuck in-memory until the next sync succeeds, so the
        // truncate gate at phase 7 must stay closed. Re-executing
        // `execute_pending_delete` on the restored entries is a
        // no-op (HashMap::remove on a missing key).
        let applied_deletes = snap_pending.len() - pending_failed.len();
        if applied_deletes > 0 {
            if let Err(e) = self.backend.flush() {
                let restore_applied: HashMap<BlobGuid, u64> = snap_pending
                    .iter()
                    .filter(|(g, _)| !pending_failed.contains_key(*g))
                    .map(|(g, s)| (*g, *s))
                    .collect();
                self.backend.restore_pending_deletes(restore_applied);
                return Err(e);
            }
        }

        if let Some(e) = first_pending_err {
            return Err(e);
        }

        // 6. Conditional truncate. A writer that landed a
        //    mark_dirty between our snapshot and here has its
        //    entry still in `dirty` (write_through's CAS won't
        //    retire newer-seq entries, and snapshot only drained
        //    what we observed at step 1); leave the WAL alone so
        //    that entry's WAL record stays recoverable. Same
        //    logic for pending_delete_count.
        if let Some(journal) = &self.journal {
            let _commit = self.commit_lock.lock().unwrap();
            if self.backend.dirty_count() == 0 && self.backend.pending_delete_count() == 0 {
                journal.truncate()?;
            }
        }
        Ok(())
    }

    /// Snapshot per-blob and aggregate counters for every blob
    /// reachable from the root.
    ///
    /// Each blob is pinned + read under a single shared guard, so
    /// stats never block ongoing reads and only contend with writers
    /// on a blob-by-blob basis. The maintenance read gate prevents a
    /// concurrent merge/compact pass from deleting a child while the
    /// tree shape is being enumerated. Returned counters are a
    /// consistent snapshot of each individual blob but the aggregate
    /// is **not** linearised across foreground writers — a concurrent
    /// writer mid-traversal can shift one blob's counters before
    /// another's are read. Acceptable for observability; pause writes
    /// externally if you need a quiescent snapshot.
    pub fn stats(&self) -> Result<TreeStats> {
        let _maintenance = self.maintenance_gate.enter_shared();
        // `Tree::stats` is an introspection path — used by users
        // checking on the tree, and (via `holt::metrics`) by
        // Prometheus scrapes that read `bm_cache_hits`,
        // `bm_cache_misses`, `bm_optimistic_restarts`, etc. We
        // walk every reachable blob to gather per-blob stats; if
        // that walk went through `BufferManager::pin`, every
        // hit would bump `cache_hits` and every entry's
        // `last_touched` tick would be refreshed — the scrape
        // would (a) inflate the very counters it's reporting
        // and (b) hand-rescue cold entries from the eviction
        // sweep just by looking at them. Both paths use the
        // `_silent` variants instead.
        let guids = engine::collect_blob_guids_silent(&self.backend, self.root_guid)?;
        let mut blobs: Vec<BlobStats> = Vec::with_capacity(guids.len());
        let mut total_space_used: u64 = 0;
        let mut total_gap_space: u64 = 0;
        let mut total_slots: u64 = 0;
        let mut total_compactions: u64 = 0;
        let mut total_tombstones: u64 = 0;
        for guid in &guids {
            let pin = self.backend.pin_silent(*guid)?;
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
        let bm_pending_delete_count = self.backend.pending_delete_count();
        let bm_cache_hits = self.backend.cache_hits();
        let bm_cache_misses = self.backend.cache_misses();
        let bm_optimistic_restarts = self.backend.optimistic_restarts();
        let bm_walker_ops = self.backend.walker_ops();
        let bm_walker_blob_hops = self.backend.walker_blob_hops();
        let bm_max_blob_hops = self.backend.max_blob_hops();
        let bm_max_cross_blob_depth = self.backend.max_cross_blob_depth();
        let bm_spillovers = self.backend.spillover_count();
        let bm_merges = self.backend.merge_count();
        let journal = self.journal.as_ref().map(|j| {
            let s = j.stats();
            JournalStats {
                appends: s.appends,
                batches: s.batches,
                syncs: s.syncs,
            }
        });
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
            bm_pending_delete_count,
            bm_cache_hits,
            bm_cache_misses,
            bm_optimistic_restarts,
            bm_walker_ops,
            bm_walker_blob_hops,
            bm_max_blob_hops,
            bm_max_cross_blob_depth,
            bm_spillovers,
            bm_merges,
            journal,
            checkpointer,
        })
    }

    /// Compact every blob reachable from the root in place, then
    /// fold every mergeable cross-blob crossing back into its
    /// parent.
    ///
    /// ## Concurrency
    ///
    /// Safe to run while point reads are active: reads and
    /// foreground writers take the shared maintenance gate, while
    /// `compact()` takes the exclusive side. Phase 1 still rewrites
    /// each blob under that blob's exclusive latch, so optimistic
    /// readers either validate or restart if they race a blob-local
    /// rewrite. Phase 2 cannot fold/delete a child while any
    /// operation is lock-coupling through a `BlobNode`. Range
    /// iterators remain best-effort snapshots; if the caller needs
    /// strict full-iterator stability, consume the iterator during
    /// an external quiescent window.
    ///
    /// ## Two phases:
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
    /// `mark_for_delete` on the internal `BufferManager`
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

        let _maintenance = self.maintenance_gate.enter_exclusive();

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
                let mut frame = guard.frame();
                engine::try_merge_children(&self.backend, &mut frame, STRUCTURAL_SEQ)?
            };
            drop(pin);
            if merged.merged > 0 {
                self.backend.mark_dirty(guid, STRUCTURAL_SEQ);
                self.backend.note_merges(u64::from(merged.merged));
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
/// BM-cached root + any cross-blob children. The walker marks
/// touched child blobs dirty itself; the root's `mark_dirty` is
/// the **caller's** responsibility when the returned outcome says
/// `root_dirty`. Replay must honour that contract. Without this,
/// a `Tree::open` → `Tree::checkpoint` immediately after replay
/// could find an empty dirty set, write nothing to backend, then
/// truncate the WAL — silently losing every replayed record.
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

        // `root_dirty` tracks whether this op actually mutated
        // the BM-cached root image. No-op replays (e.g. an erase
        // for a key already absent because a prior replay pass
        // reconciled it) leave the cache byte-identical to
        // backend — skipping `mark_dirty` for those is a small
        // win and matches `Tree::delete`'s same-shape branch.
        let root_dirty = match op {
            TxnOp::Insert { key, value, .. } => {
                let padded = pad_key(key);
                engine::insert_multi(bm, &root_pin, &padded, value, seq, false)?.root_dirty
            }
            TxnOp::Erase { key, .. } => {
                let padded = pad_key(key);
                engine::erase_multi(bm, &root_pin, &padded, seq, false)?.root_dirty
            }
            TxnOp::RenameObject {
                src_key,
                dst_key,
                force,
                ..
            } => {
                let src_padded = pad_key(src_key);
                let dst_padded = pad_key(dst_key);
                // Existence probes pass a `|_| ()` closure so the
                // walker doesn't even allocate / copy the value.
                if engine::lookup_multi_with(bm, &root_pin, &src_padded, |_| ())?.is_none() {
                    // Already reconciled in a prior replay pass —
                    // skip. `highest` was bumped above so the
                    // post-replay `next_seq` still advances past
                    // this record's seq.
                    return Ok(());
                }
                if !force
                    && engine::lookup_multi_with(bm, &root_pin, &dst_padded, |_| ())?.is_some()
                {
                    return Ok(());
                }
                let value = engine::lookup_multi_with(bm, &root_pin, &src_padded, <[u8]>::to_vec)?
                    .unwrap_or_default();
                let erase_out = engine::erase_multi(bm, &root_pin, &src_padded, seq, false)?;
                let insert_out =
                    engine::insert_multi(bm, &root_pin, &dst_padded, &value, seq, false)?;
                erase_out.root_dirty || insert_out.root_dirty
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
        if root_dirty {
            // Honour the walker's caller-side `mark_dirty(root,
            // seq)` contract — see the module doc above.
            bm.mark_dirty(root_guid, seq);
        }
        Ok(())
    })?;
    // After commit, the blob image is durable; we still want the
    // next allocated seq to be strictly greater than anything
    // ever seen in the log.
    Ok(highest + 1)
}

#[cfg(test)]
mod tests {
    use super::{pad_key, Tree, INLINE_PADDED_KEY_CAP};
    use crate::api::config::TreeConfig;
    use std::sync::mpsc::sync_channel;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn pad_key_short_key_stays_inline() {
        let padded = pad_key(b"abc");
        assert!(padded.heap.is_none());
        assert_eq!(&*padded, b"abc\0");
    }

    #[test]
    fn pad_key_long_key_uses_heap_fallback() {
        let key = vec![b'x'; INLINE_PADDED_KEY_CAP];
        let padded = pad_key(&key);
        assert!(padded.heap.is_some());
        assert_eq!(padded.len(), INLINE_PADDED_KEY_CAP + 1);
        assert_eq!(padded[INLINE_PADDED_KEY_CAP], 0);
    }

    #[test]
    fn compact_waits_for_maintenance_read_guard() {
        let tree = Tree::open(TreeConfig::memory()).unwrap();
        tree.put(b"k", b"v").unwrap();

        let read_guard = tree.maintenance_gate.enter_shared();
        let worker_tree = tree.clone();
        let (started_tx, started_rx) = sync_channel(0);
        let (done_tx, done_rx) = sync_channel(0);
        let handle = thread::spawn(move || {
            started_tx.send(()).unwrap();
            worker_tree.compact().unwrap();
            done_tx.send(()).unwrap();
        });

        started_rx.recv().unwrap();
        assert!(
            done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "compact must wait behind active shared maintenance readers"
        );

        drop(read_guard);
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        handle.join().unwrap();
    }
}
