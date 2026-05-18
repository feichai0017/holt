//! `TreeConfig` — the single argument to [`crate::Tree::open`].
//!
//! `TreeConfig` captures both **where** the tree lives ([`Storage`])
//! and how the engine internals are sized.
//!
//! The default — built via [`TreeConfig::new`] — is **persistent**
//! at the supplied directory. Override to memory mode with
//! [`TreeConfig::memory`] (or via [`crate::TreeBuilder::memory`]).

use std::path::PathBuf;

/// Where the tree's data lives.
///
/// `Persistent` is the production target. `Memory` is for tests,
/// scratch use, and platforms without a usable file-backed backend.
#[derive(Debug, Clone)]
pub enum Storage {
    /// File-backed durable storage at `dir`. On Linux the
    /// [`crate::PersistentBackend`] opens the underlying file with
    /// `O_DIRECT` and (in Stage 7) drives I/O through `io_uring`.
    Persistent {
        /// Directory holding `blobs.dat` + `manifest.bin` (+ WAL,
        /// once Stage 5 lands).
        dir: PathBuf,
    },
    /// In-memory only — volatile, drops on the last `Tree` handle.
    Memory,
}

/// Configuration passed to [`crate::Tree::open`].
#[derive(Debug, Clone)]
pub struct TreeConfig {
    /// Where the tree's data lives.
    pub storage: Storage,
    /// How many 512 KB blob frames to keep pinned in the buffer
    /// pool (Stage 6). Default 64 (= 32 MB resident).
    pub buffer_pool_size: usize,
    /// `true` = fsync the WAL on every commit (durable + slow).
    /// `false` = batched (faster, may lose the last few ops on a
    /// crash). Stage 5 wires this up.
    pub wal_sync_on_commit: bool,
    /// Bytes appended to the WAL before triggering an automatic
    /// checkpoint. Stage 5 wires this up. Default 16 MB.
    pub checkpoint_byte_interval: u64,
    /// If `true` (the default), every `Tree::put` / `delete` /
    /// `rename` synchronously writes the mutated root blob through
    /// the backend. Set to `false` to keep mutations in the
    /// in-memory cache only — the caller must then invoke
    /// [`crate::Tree::checkpoint`] to make changes durable. Used by
    /// benchmarks (matches the "no-WAL, batched flush" mode of
    /// other embedded engines).
    pub flush_on_write: bool,
}

impl TreeConfig {
    /// Persistent tree rooted at `dir`. This is the **default**
    /// shape — `Tree::open(TreeConfig::new("/var/lib/myapp"))` is
    /// what production code typically writes.
    #[must_use]
    pub fn new<P: Into<PathBuf>>(dir: P) -> Self {
        Self {
            storage: Storage::Persistent { dir: dir.into() },
            buffer_pool_size: 64,
            wal_sync_on_commit: false,
            checkpoint_byte_interval: 16 * 1024 * 1024,
            flush_on_write: true,
        }
    }

    /// In-memory tree — volatile, for tests + scratch use.
    #[must_use]
    pub fn memory() -> Self {
        Self {
            storage: Storage::Memory,
            buffer_pool_size: 64,
            wal_sync_on_commit: false,
            checkpoint_byte_interval: 16 * 1024 * 1024,
            flush_on_write: true,
        }
    }

    /// `true` iff [`Storage::Memory`].
    #[must_use]
    pub fn is_memory(&self) -> bool {
        matches!(self.storage, Storage::Memory)
    }

    /// Path of the WAL file for this configuration, if any.
    /// Persistent trees keep their log next to the data file at
    /// `<dir>/journal.wal`; memory trees have no WAL.
    #[must_use]
    pub fn wal_path(&self) -> Option<PathBuf> {
        match &self.storage {
            Storage::Persistent { dir } => Some(dir.join("journal.wal")),
            Storage::Memory => None,
        }
    }
}
