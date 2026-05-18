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
    /// Controls the WAL durability boundary on every `put` /
    /// `delete` / `rename`:
    ///
    /// - `true` → call `sync_data` on the WAL file. Per-op
    ///   durable past a power failure; **slow** (one fsync per
    ///   op, ~ms on consumer SSDs).
    /// - `false` (the default) → leave the record in the WAL
    ///   writer's pending buffer / OS page cache. Survives a
    ///   process crash (the auto-flush drains to the page cache
    ///   at 64 KB); does **not** survive a power loss until
    ///   `Tree::checkpoint` runs.
    ///
    /// Matches `disable_wal=false, sync=false` in RocksDB's
    /// default. Production deployments that need power-safe
    /// per-op durability flip this to `true`.
    pub wal_sync_on_commit: bool,
    /// Bytes appended to the WAL before triggering an automatic
    /// checkpoint. Reserved — Stage 5d's auto-flush bounds the
    /// in-memory buffer; the user is still responsible for
    /// calling `Tree::checkpoint` to truncate the on-disk log.
    /// Default 16 MB.
    pub checkpoint_byte_interval: u64,
    /// Memory-backend BM-commit toggle.
    ///
    /// For **memory** trees: `true` (the default) writes the
    /// BM-cached root blob through the (memory-backed) `Backend`
    /// after every `put` / `delete` / `rename`. Useful with
    /// custom backends that want to mirror state out per op;
    /// benchmarks set this to `false` to skip the redundant
    /// memcpy.
    ///
    /// For **persistent** trees this flag has no effect — the
    /// WAL is the per-op durability path and the blob image only
    /// flushes at `Tree::checkpoint`. See [`Self::wal_sync_on_commit`].
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
