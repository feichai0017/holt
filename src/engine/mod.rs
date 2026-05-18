//! ART walker — descent / insert / erase / scan / rename / compact.
//!
//! Stage 2a–2c (current): single-blob lookup + insert + erase land.
//! Stage 2d: multi-blob descent (BlobNode crossing + splitBlob).
//!
//! [`simd`] hosts SIMD hot paths the walker calls into (Node16
//! byte search, longest-common-prefix).

pub mod walker;
pub mod compact;
pub mod iter;
pub mod simd;

pub use walker::{
    compact_blob, erase, erase_multi, insert, insert_multi, lookup, lookup_at, lookup_multi,
    make_blob_from_node, BlobNodeCrossing, CompactStats, EraseOutcome, InsertOutcome,
    LookupResult, MakeBlobOutcome,
};
