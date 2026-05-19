//! `TreeConfig` — the single argument to [`crate::Tree::open`].
//!
//! `TreeConfig` captures both **where** the tree lives ([`Storage`])
//! and how the engine internals are sized.
//!
//! The default — built via [`TreeConfig::new`] — is **persistent**
//! at the supplied directory. Override to memory mode with
//! [`TreeConfig::memory`] (or via [`crate::TreeBuilder::memory`]).

use std::path::PathBuf;

use crate::checkpoint::CheckpointConfig;

/// Where the tree's data lives.
///
/// `Persistent` is the production target. `Memory` is for tests,
/// scratch use, and platforms without a usable file-backed backend.
///
/// `#[non_exhaustive]` so adding new storage variants (e.g., a
/// future `RemoteObjectStore`) is a non-breaking change.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Storage {
    /// File-backed durable storage at `dir`. On Linux the
    /// [`crate::PersistentBackend`] opens the underlying file with
    /// `O_DIRECT` and (with the `io-uring` feature enabled) drives
    /// I/O through `io_uring`.
    Persistent {
        /// Directory holding `blobs.dat` + `manifest.bin` + `journal.wal`.
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
    /// **Memory-only** BM-commit toggle (no effect on
    /// persistent trees — the WAL + `Tree::checkpoint` is the
    /// durability path there; see [`Self::wal_sync_on_commit`]).
    ///
    /// For memory trees: `true` (the default) drains the BM
    /// dirty set into the backing `Backend` after every `put` /
    /// `delete` / `rename`, so custom backends supplied via
    /// [`crate::Tree::open_with_backend`] see state mirrored
    /// per op. `false` defers all writes to an explicit
    /// `Tree::checkpoint` call — useful in benches where the
    /// memcpy through `MemoryBackend` is uninteresting.
    pub memory_flush_on_write: bool,
    /// Background checkpointer policy. Default disabled —
    /// callers drive [`crate::Tree::checkpoint`] synchronously.
    /// Enable via [`CheckpointConfig::enabled`] or
    /// [`crate::TreeBuilder::checkpoint`].
    pub checkpoint: CheckpointConfig,
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
            wal_sync_on_commit: false,
            memory_flush_on_write: true,
            checkpoint: CheckpointConfig::default(),
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
