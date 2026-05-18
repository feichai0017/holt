//! Public `Tree` type — the main user-facing API.
//!
//! Stage 2c (current): `Tree::open`, `Tree::get`, `Tree::put`,
//! `Tree::delete`, `Tree::rename` are all wired against the walker.
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
//! ## Cached root blob
//!
//! Tree keeps the root blob's 512 KB buffer pinned in memory in a
//! `Mutex<TreeState>`. Every `get` / `put` / `delete` / `rename`
//! operates on that cached buffer; mutations either flush-through
//! to the backend immediately (`flush_on_write = true`, the
//! default) or stay in cache until `checkpoint()` (`false`, useful
//! for batch / benchmark workloads).
//!
//! Cross-blob descent (Stage 2d phase A) still reads child blobs
//! from the backend per crossing — the cache is root-only for now.
//! Stage 6 BufferManager will pin arbitrary child blobs and add a
//! real LRU.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::config::{Storage, TreeConfig};
use super::errors::{Error, Result};
use crate::engine::{self, LookupResult};
use crate::layout::{BlobGuid, PAGE_SIZE};
use crate::store::backend::{AlignedBlobBuf, Backend, MemoryBackend};
use crate::store::BlobFrame;

#[cfg(unix)]
use crate::store::backend::PersistentBackend;

/// An `artisan` tree — your handle to one metadata store.
///
/// Clone the handle to share the same backing store: the backend
/// is held via `Arc` and writes serialise through the internal
/// `Mutex<TreeState>` (Stage 5 will swap the mutex for per-blob
/// `HybridLatch`).
#[derive(Clone)]
pub struct Tree {
    cfg: TreeConfig,
    backend: Arc<dyn Backend>,
    /// GUID of the blob holding the tree root. v0.1 uses a fixed
    /// sentinel; multi-blob support (Stage 2d) introduces a per-tree
    /// root manifest.
    root_guid: BlobGuid,
    /// Cached root buffer + serialisation lock. Mutating ops hold
    /// the mutex exclusively; read ops also hold it (because
    /// `BlobFrame::wrap` needs `&mut [u8]`). Stage 6 will swap for
    /// per-blob HybridLatch to allow concurrent optimistic reads.
    state: Arc<Mutex<TreeState>>,
    /// Monotonically-increasing sequence stamped on every new
    /// leaf. Stage 5 ties this to the WAL record number.
    next_seq: Arc<AtomicU64>,
}

/// In-memory cache of the root blob. Construction reads it once
/// from the backend; subsequent ops read/mutate this buffer
/// directly. `Tree::checkpoint` (and `Tree::put`/`delete`/`rename`
/// when `flush_on_write = true`) writes it back through the backend.
struct TreeState {
    root_buf: AlignedBlobBuf,
}

impl std::fmt::Debug for Tree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tree")
            .field("storage", &self.cfg.storage)
            .field("root_guid", &self.root_guid)
            .finish_non_exhaustive()
    }
}

/// Fixed GUID of the root blob in v0.1. Multi-root trees (Stage 2d
/// onwards) will allocate per-tree root GUIDs from a manifest.
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
    /// On non-Unix platforms, persistent mode is unavailable;
    /// passing a `Storage::Persistent` config there returns
    /// [`Error::NotYetImplemented`] — fall back to
    /// `TreeConfig::memory()` or supply your own [`Backend`] via
    /// [`Tree::open_with_backend`].
    pub fn open(cfg: TreeConfig) -> Result<Self> {
        let backend: Arc<dyn Backend> = match &cfg.storage {
            Storage::Memory => Arc::new(MemoryBackend::new()),
            Storage::Persistent { dir } => {
                #[cfg(unix)]
                {
                    Arc::new(PersistentBackend::open(dir)?)
                }
                #[cfg(not(unix))]
                {
                    let _ = dir;
                    return Err(Error::NotYetImplemented(
                        "PersistentBackend is Unix-only; use TreeConfig::memory() or supply a Backend via Tree::open_with_backend",
                    ));
                }
            }
        };
        Self::open_with_backend(cfg, backend)
    }

    /// Open a tree with a caller-supplied [`Backend`].
    ///
    /// Reads the root blob into the in-memory cache. If the backend
    /// doesn't yet contain a root blob, initialises an empty one
    /// and writes it through, flushing before returning.
    pub fn open_with_backend(cfg: TreeConfig, backend: Arc<dyn Backend>) -> Result<Self> {
        let root_guid = ROOT_BLOB_GUID;
        let mut root_buf = AlignedBlobBuf::zeroed();
        if backend.has_blob(root_guid)? {
            backend.read_blob(root_guid, &mut root_buf)?;
        } else {
            BlobFrame::init(root_buf.as_mut_slice(), root_guid)?;
            backend.write_blob(root_guid, &root_buf)?;
            backend.flush()?;
        }
        Ok(Self {
            cfg,
            backend,
            root_guid,
            state: Arc::new(Mutex::new(TreeState { root_buf })),
            next_seq: Arc::new(AtomicU64::new(1)),
        })
    }

    /// Look up `key`. Returns the value bytes, or `None` if no leaf
    /// matches.
    ///
    /// Transparently follows `BlobNode` crossings — the lookup may
    /// span multiple blobs when the tree has been split by Stage 2d
    /// spillover. The root blob descent happens against the
    /// in-memory cache (no backend hit); subsequent crossings load
    /// child blobs from the backend.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let padded = pad_key(key);

        // 1) First-hop descent in the cached root blob.
        let crossing = {
            let mut state = self.state.lock().unwrap();
            let frame = BlobFrame::wrap(state.root_buf.as_mut_slice());
            let root_slot = frame.header().root_slot;
            match engine::lookup_at(&frame, root_slot, &padded, 0)? {
                LookupResult::Found(v) => return Ok(Some(v.to_vec())),
                LookupResult::NotFound => return Ok(None),
                LookupResult::Crossing(c) => c,
            }
        };

        // 2) Cross-blob loop — read each child blob from the backend.
        let mut current_guid = crossing.child_guid;
        let mut start_slot = crossing.child_slot;
        let mut depth = crossing.child_depth;
        loop {
            let mut buf = AlignedBlobBuf::zeroed();
            self.backend.read_blob(current_guid, &mut buf)?;
            let frame = BlobFrame::wrap(buf.as_mut_slice());
            match engine::lookup_at(&frame, start_slot, &padded, depth)? {
                LookupResult::Found(v) => return Ok(Some(v.to_vec())),
                LookupResult::NotFound => return Ok(None),
                LookupResult::Crossing(c) => {
                    current_guid = c.child_guid;
                    start_slot = c.child_slot;
                    depth = c.child_depth;
                }
            }
        }
    }

    /// Insert or replace `(key, value)`. Returns the previous value
    /// if the key already existed.
    ///
    /// Walks across [`BlobNode`] crossings (Stage 2d phase B). When
    /// any blob hits `AllocError::OutOfSpace`, the walker
    /// automatically migrates a subtree out via `splitBlob` and
    /// retries — so trees may grow well past the 512 KB single-blob
    /// limit without caller involvement.
    ///
    /// Modifies the in-memory cached root blob; flushes to the
    /// backend immediately when `TreeConfig::flush_on_write` is
    /// `true` (the default). Newly-created child blobs are *always*
    /// written through the backend at the moment of spillover, so
    /// crash-recovery never finds a dangling BlobNode pointing at
    /// nothing.
    ///
    /// [`BlobNode`]: crate::layout::BlobNode
    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<Option<Vec<u8>>> {
        let padded = pad_key(key);
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);

        let mut state = self.state.lock().unwrap();
        let outcome = engine::insert_multi(
            &*self.backend,
            self.root_guid,
            &mut state.root_buf,
            &padded,
            value,
            seq,
        )?;
        if self.cfg.flush_on_write {
            self.backend.write_blob(self.root_guid, &state.root_buf)?;
        }
        Ok(outcome.previous)
    }

    /// Remove `key`. Returns the value that was stored at `key`, or
    /// `None` if no leaf matched.
    pub fn delete(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let padded = pad_key(key);

        let mut state = self.state.lock().unwrap();
        let outcome;
        {
            let mut frame = BlobFrame::wrap(state.root_buf.as_mut_slice());
            let root_slot = frame.header().root_slot;
            outcome = engine::walker::erase(&mut frame, root_slot, &padded)?;
            frame.header_mut().root_slot = outcome.new_root_slot;
        }
        if self.cfg.flush_on_write {
            self.backend.write_blob(self.root_guid, &state.root_buf)?;
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
    pub fn rename(&self, src: &[u8], dst: &[u8], force: bool) -> Result<()> {
        let src_padded = pad_key(src);
        let dst_padded = pad_key(dst);

        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let mut state = self.state.lock().unwrap();
        {
            let mut frame = BlobFrame::wrap(state.root_buf.as_mut_slice());
            let root_slot = frame.header().root_slot;

            // Stage 2d phase A: cross-blob rename surfaces
            // NotYetImplemented (insert + erase don't yet follow
            // BlobNode crossings — that's phase B). Single-blob
            // renames work the same as before.
            let value: Vec<u8> = match engine::lookup(&frame, root_slot, &src_padded)? {
                LookupResult::Found(v) => v.to_vec(),
                LookupResult::NotFound => return Err(Error::NotFound),
                LookupResult::Crossing(_) => {
                    return Err(Error::NotYetImplemented(
                        "Tree::rename across BlobNode — Stage 2d phase B",
                    ));
                }
            };

            // Same key? Treat as a no-op.
            if src == dst {
                return Ok(());
            }

            if !force {
                let dst_exists = match engine::lookup(&frame, root_slot, &dst_padded)? {
                    LookupResult::Found(_) => true,
                    LookupResult::NotFound => false,
                    LookupResult::Crossing(_) => {
                        return Err(Error::NotYetImplemented(
                            "Tree::rename dst probe across BlobNode — Stage 2d phase B",
                        ));
                    }
                };
                if dst_exists {
                    return Err(Error::DstExists);
                }
            }

            // erase(src)
            let erase_out = engine::walker::erase(&mut frame, root_slot, &src_padded)?;
            frame.header_mut().root_slot = erase_out.new_root_slot;

            // insert(dst, value)
            let new_root = frame.header().root_slot;
            let insert_out =
                engine::walker::insert(&mut frame, new_root, &dst_padded, &value, seq)?;
            frame.header_mut().root_slot = insert_out.new_root_slot;
        }
        if self.cfg.flush_on_write {
            self.backend.write_blob(self.root_guid, &state.root_buf)?;
        }
        Ok(())
    }

    /// Force-flush the cached root blob through the backend and
    /// run the backend's own durability protocol
    /// (`fdatasync` on persistent; no-op on memory).
    pub fn checkpoint(&self) -> Result<()> {
        let state = self.state.lock().unwrap();
        self.backend.write_blob(self.root_guid, &state.root_buf)?;
        self.backend.flush()?;
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
