//! Public `Tree` type — the main user-facing API.
//!
//! Stage 2c (current): `Tree::open`, `Tree::get`, `Tree::put`,
//! `Tree::delete` are all wired against the walker. `Tree::rename`
//! lands once the walker has atomic-rename support.

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
/// is held via `Arc` and writes serialise through a single
/// internal mutex (Stage 5 will swap the mutex for per-blob
/// `HybridLatch`).
#[derive(Clone)]
pub struct Tree {
    cfg: TreeConfig,
    backend: Arc<dyn Backend>,
    /// GUID of the blob holding the tree root. v0.1 uses a fixed
    /// sentinel; multi-blob support (Stage 2d) introduces a per-tree
    /// root manifest.
    root_guid: BlobGuid,
    /// Serialises mutations against the root blob. Stage 5
    /// (BufferManager + HybridLatch) makes this per-blob.
    write_lock: Arc<Mutex<()>>,
    /// Monotonically-increasing sequence stamped on every new
    /// leaf. Stage 5 ties this to the WAL record number.
    next_seq: Arc<AtomicU64>,
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
    /// Use this when you want to plug in something other than the
    /// built-in memory / persistent backends — e.g. a network-backed
    /// store, an instrumented wrapper, or a fault-injection harness.
    pub fn open_with_backend(cfg: TreeConfig, backend: Arc<dyn Backend>) -> Result<Self> {
        let root_guid = ROOT_BLOB_GUID;
        if !backend.has_blob(root_guid)? {
            let mut buf = AlignedBlobBuf::zeroed();
            BlobFrame::init(buf.as_mut_slice(), root_guid)?;
            backend.write_blob(root_guid, &buf)?;
            backend.flush()?;
        }
        Ok(Self {
            cfg,
            backend,
            root_guid,
            write_lock: Arc::new(Mutex::new(())),
            next_seq: Arc::new(AtomicU64::new(1)),
        })
    }

    /// Look up `key`. Returns the value bytes, or `None` if no leaf
    /// matches.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut buf = AlignedBlobBuf::zeroed();
        self.backend.read_blob(self.root_guid, &mut buf)?;
        let frame = BlobFrame::wrap(buf.as_mut_slice());
        let root_slot = frame.header().root_slot;
        match engine::lookup(&frame, root_slot, key)? {
            LookupResult::Found(v) => Ok(Some(v.to_vec())),
            LookupResult::NotFound => Ok(None),
        }
    }

    /// Insert or replace `(key, value)`. Returns the previous value
    /// if the key already existed.
    ///
    /// Stage 2b limitation: returns
    /// [`Error::NotYetImplemented`] when one key is a strict prefix
    /// of another (handled by Stage 2b' with a terminator byte) or
    /// when the inserting key would terminate at an inner node.
    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<Option<Vec<u8>>> {
        let _guard = self.write_lock.lock().unwrap();
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);

        let mut buf = AlignedBlobBuf::zeroed();
        self.backend.read_blob(self.root_guid, &mut buf)?;

        let outcome;
        {
            let mut frame = BlobFrame::wrap(buf.as_mut_slice());
            let root_slot = frame.header().root_slot;
            outcome = engine::walker::insert(&mut frame, root_slot, key, value, seq)?;
            frame.header_mut().root_slot = outcome.new_root_slot;
        }

        self.backend.write_blob(self.root_guid, &buf)?;
        Ok(outcome.previous)
    }

    /// Remove `key`. Returns the value that was stored at `key`, or
    /// `None` if no leaf matched.
    pub fn delete(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let _guard = self.write_lock.lock().unwrap();

        let mut buf = AlignedBlobBuf::zeroed();
        self.backend.read_blob(self.root_guid, &mut buf)?;

        let outcome;
        {
            let mut frame = BlobFrame::wrap(buf.as_mut_slice());
            let root_slot = frame.header().root_slot;
            outcome = engine::walker::erase(&mut frame, root_slot, key)?;
            frame.header_mut().root_slot = outcome.new_root_slot;
        }

        // Even on NotFound we still rewrite — the frame is unchanged
        // (idempotent), so this is a 512 KB no-op write. Stage 6
        // BufferManager will skip the write when nothing was dirtied.
        self.backend.write_blob(self.root_guid, &buf)?;
        Ok(outcome.previous)
    }

    /// Atomic in-tree rename. Will land alongside the walker's
    /// atomic-rename primitive (Stage 2c+).
    pub fn rename(&self, _src: &[u8], _dst: &[u8], _force: bool) -> Result<()> {
        Err(Error::NotYetImplemented(
            "Tree::rename — needs atomic single-latch rename in walker",
        ))
    }

    /// Flush every previously-returned write through the backend.
    ///
    /// On the persistent backend this issues `fdatasync` on the
    /// underlying blobs file and rewrites the manifest. On the
    /// memory backend this is a no-op.
    pub fn checkpoint(&self) -> Result<()> {
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
