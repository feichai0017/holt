//! `TreeBuilder` — fluent constructor for [`Tree`].

use std::path::PathBuf;
use std::sync::Arc;

use super::config::{Storage, TreeConfig};
use super::tree::Tree;
use crate::api::errors::Result;
use crate::checkpoint::CheckpointConfig;
use crate::store::backend::Backend;

/// Fluent constructor for [`Tree`].
///
/// ```ignore
/// // Persistent (the default):
/// let tree = holt::TreeBuilder::new("/var/lib/myapp")
///     .buffer_pool_size(128)
///     .wal_sync_on_commit(true)
///     .open()?;
///
/// // In-memory (volatile, for tests / scratch):
/// let tree = holt::TreeBuilder::new("scratch")
///     .memory()
///     .open()?;
/// ```
#[derive(Debug, Clone)]
#[must_use = "TreeBuilder is consumed by `.open()` / `.open_with_backend()`; chained setters return a fresh builder you must use"]
pub struct TreeBuilder {
    cfg: TreeConfig,
}

impl TreeBuilder {
    /// Start a builder targeting `data_dir` in persistent mode
    /// (the default).
    pub fn new<P: Into<PathBuf>>(data_dir: P) -> Self {
        Self {
            cfg: TreeConfig::new(data_dir),
        }
    }

    /// Flip the builder to **in-memory** mode. The supplied
    /// `data_dir` becomes informational only.
    pub fn memory(mut self) -> Self {
        self.cfg.storage = Storage::Memory;
        self
    }

    /// Set buffer pool size (in number of 512 KB blob frames).
    pub fn buffer_pool_size(mut self, n: usize) -> Self {
        self.cfg.buffer_pool_size = n;
        self
    }

    /// fsync the WAL on every commit (slow + durable) vs batched.
    pub fn wal_sync_on_commit(mut self, on: bool) -> Self {
        self.cfg.wal_sync_on_commit = on;
        self
    }

    /// Background checkpointer policy.
    ///
    /// Default is disabled — callers drive
    /// [`Tree::checkpoint`] synchronously. Pass
    /// [`CheckpointConfig::enabled`] (or any config with
    /// `enabled = true`) to spawn the background thread that
    /// drains the dirty set + truncates the WAL.
    pub fn checkpoint(mut self, cfg: CheckpointConfig) -> Self {
        self.cfg.checkpoint = cfg;
        self
    }

    /// Open with the configured storage mode.
    pub fn open(self) -> Result<Tree> {
        Tree::open(self.cfg)
    }

    /// Open with a caller-supplied [`Backend`] (overrides the
    /// builder's storage mode).
    pub fn open_with_backend(self, backend: Arc<dyn Backend>) -> Result<Tree> {
        Tree::open_with_backend(self.cfg, backend)
    }
}
