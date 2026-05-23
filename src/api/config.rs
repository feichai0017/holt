//! `TreeConfig` — the single argument to [`crate::Tree::open`].
//!
//! `TreeConfig` captures both **where** the tree lives ([`Storage`])
//! and how the engine internals are sized.
//!
//! The default — built via [`TreeConfig::new`] — is a file-backed
//! durable tree at the supplied directory. Override to memory mode with
//! [`TreeConfig::memory`] (or via [`crate::TreeBuilder::memory`]).

use std::path::PathBuf;

use crate::checkpoint::CheckpointConfig;

/// Where the tree's data lives.
///
/// `File` is the production target. `Memory` is for tests,
/// scratch use, and platforms without a usable file-backed store.
///
/// `#[non_exhaustive]` so adding new storage variants (e.g., a
/// future `RemoteObjectStore`) is a non-breaking change.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Storage {
    /// File-backed durable storage at `dir`. On Linux the
    /// [`crate::FileBlobStore`] opens the underlying file with
    /// `O_DIRECT` and, with default features, uses `io_uring` for
    /// data-file I/O. Non-Linux Unix targets use the normal file
    /// backend; build Linux with `--no-default-features` to force it.
    File {
        /// Directory holding `blobs.dat`, `manifest.bin`,
        /// `manifest.log`, and `journal.wal`.
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
    /// pool. Default 64 (= 32 MB resident).
    pub buffer_pool_size: usize,
    /// If `true`, each file-backed mutation waits until the
    /// journal worker calls `sync_data`. The default `false`
    /// returns after the journal worker queue accepts the encoded
    /// WAL record.
    pub wal_sync: bool,
    /// **Memory-only** BM-commit toggle (no effect on
    /// file-backed trees — the WAL + `Tree::checkpoint` is the
    /// durability path there; see [`Self::wal_sync`]).
    ///
    /// For memory trees: `true` (the default) drains the BM
    /// dirty set into the backing `BlobStore` after every `put` /
    /// `delete` / `rename`, so custom stores supplied via
    /// [`crate::Tree::open_with_blob_store`] see state mirrored
    /// per op. `false` defers all writes to an explicit
    /// `Tree::checkpoint` call — useful in benches where the
    /// memcpy through `MemoryBlobStore` is uninteresting.
    pub memory_flush_on_write: bool,
    /// Background checkpointer policy. Default enabled for
    /// file-backed service use; set `enabled = false` when callers
    /// want to drive [`crate::Tree::checkpoint`] manually.
    pub checkpoint: CheckpointConfig,
}

impl TreeConfig {
    /// File-backed durable tree rooted at `dir`. This is the **default**
    /// shape — `Tree::open(TreeConfig::new("/var/lib/myapp"))` is
    /// what production code typically writes.
    #[must_use]
    pub fn new<P: Into<PathBuf>>(dir: P) -> Self {
        Self {
            storage: Storage::File { dir: dir.into() },
            buffer_pool_size: 64,
            wal_sync: false,
            memory_flush_on_write: true,
            checkpoint: CheckpointConfig::default(),
        }
    }

    /// In-memory tree — volatile, for tests + scratch use.
    #[must_use]
    pub fn memory() -> Self {
        Self {
            storage: Storage::Memory,
            buffer_pool_size: 64,
            wal_sync: false,
            memory_flush_on_write: true,
            checkpoint: CheckpointConfig {
                enabled: false,
                ..CheckpointConfig::default()
            },
        }
    }

    /// `true` iff [`Storage::Memory`].
    #[must_use]
    pub fn is_memory(&self) -> bool {
        matches!(self.storage, Storage::Memory)
    }

    /// Path of the WAL file for this configuration, if any.
    /// File-backed trees keep their log next to the data file at
    /// `<dir>/journal.wal`; memory trees have no WAL.
    #[must_use]
    pub fn wal_path(&self) -> Option<PathBuf> {
        match &self.storage {
            Storage::File { dir } => Some(dir.join("journal.wal")),
            Storage::Memory => None,
        }
    }
}
