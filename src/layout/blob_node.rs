//! `BlobNode` — in-tree blob crossing (ntype=3, 128 B fixed).
//!
//! A first-class node-type variant used when a tree spans
//! multiple 512 KB blob frames. The walker hits one, swaps to
//! the target blob, and continues at `child_entry_ptr`.

use std::mem::{offset_of, size_of};

use super::NodeType;

/// Maximum inline path-compressed prefix bytes a Blob node holds.
/// Longer prefixes chain through Prefix→Blob.
pub const BLOB_MAX_INLINE: usize = 96;

/// 128-byte in-tree blob crossing.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BlobNode {
    /// Always 1.
    pub count: u8,
    /// = `NodeType::Blob.as_u8()` = 3.
    pub node_type: u8,
    _pad_2: [u8; 2],
    /// Number of valid bytes in `bytes`.
    pub prefix_len: u16,
    _pad_6: u16,
    /// 128-bit identifier of the blob to walk into.
    pub child_blob_guid: [u8; 16],
    /// Slot index inside the child blob where the walk resumes.
    pub child_entry_ptr: u32,
    _pad_28: u32,
    /// Inline path-compressed prefix bytes (only first
    /// `prefix_len` are valid).
    pub bytes: [u8; BLOB_MAX_INLINE],
}

const _: () = assert!(size_of::<BlobNode>() == 128);
const _: () = assert!(offset_of!(BlobNode, child_blob_guid) == 8);
const _: () = assert!(offset_of!(BlobNode, child_entry_ptr) == 24);
const _: () = assert!(offset_of!(BlobNode, bytes) == 32);

impl BlobNode {
    /// Build a Blob crossing pointing at `(guid, entry_slot)`,
    /// optionally with a path-compressed prefix.
    #[must_use]
    pub fn new(prefix_bytes: &[u8], child_guid: [u8; 16], child_entry_slot: u32) -> Self {
        assert!(prefix_bytes.len() <= BLOB_MAX_INLINE);
        let mut b = Self {
            count: 1,
            node_type: NodeType::Blob.as_u8(),
            _pad_2: [0; 2],
            prefix_len: prefix_bytes.len() as u16,
            _pad_6: 0,
            child_blob_guid: child_guid,
            child_entry_ptr: child_entry_slot,
            _pad_28: 0,
            bytes: [0; BLOB_MAX_INLINE],
        };
        b.bytes[..prefix_bytes.len()].copy_from_slice(prefix_bytes);
        b
    }
}
