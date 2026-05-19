//! Storage layer.
//!
//! - [`BlobFrame`] — typed view over one 512 KB blob, with bump
//!   allocator + per-NodeType free list.
//! - [`backend`] — pluggable storage backend trait
//!   (memory / persistent / future io_uring).
//! - [`BufferManager`] — LRU-bounded cache wrapping any `Backend`,
//!   itself implementing `Backend` so it's transparent.

pub mod backend;
mod blob_frame;
// `pub(crate)` so internal crates can name
// `crate::store::buffer_manager::STRUCTURAL_SEQ` directly; the
// public API still goes through the re-exports below.
pub(crate) mod buffer_manager;

pub use blob_frame::{AllocError, BlobFrame, BlobFrameRef, FreeError};
pub use buffer_manager::{BufferManager, CachedBlob};
