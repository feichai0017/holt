//! `Leaf` header (16 bytes) + variable-size self-describing body.
//!
//! A leaf is a SINGLE contiguous, variable-size region in the blob's
//! data area:
//!
//! ```text
//!   [16B header][key bytes][value bytes]
//! ```
//!
//! The 16-byte header is `#[repr(C)]`, 8-byte aligned, with the exact
//! field order/offsets:
//!
//! - `key_fp:    u8  @ +0`
//! - `node_type: u8  @ +1`
//! - `value_len: u16 @ +2`
//! - `key_len:   u16 @ +4`
//! - `tombstone: u8  @ +6`
//! - `_pad:      u8  @ +7`
//! - `seq:       u64 @ +8`
//!
//! The `node_type @ +1` byte (always `NodeType::Leaf`) makes the leaf
//! self-describing in the same way every inner node / Prefix / BlobNode
//! is: with offset-addressed children a node's `NodeType` is read from
//! `body[off + 1]` without a slot-table lookup (see
//! [`crate::store::ntype_at_offset`]). `seq` stays 8-byte aligned at
//! `+8` so its native read/write are aligned.
//!
//! The key bytes live at `body[16 .. 16 + key_len]` and the value at
//! `body[16 + key_len .. 16 + key_len + value_len]`. The whole leaf
//! is allocated as one node (registered in the slot table) sized
//! `align8(16 + key_len + value_len)` — there is no separate extent
//! and no `key_offset`. The slot table only records the byte offset
//! of the header; the leaf is self-describing (its size is read back
//! from `key_len`/`value_len` in the header — see `body_of_slot`).
//!
//! The key still includes the ART terminator byte (written via
//! `SearchKey::write_to_slice`); `key_len` counts it, since
//! `SearchKey::len()` includes the virtual terminator.

use std::mem::{offset_of, size_of};

/// 16-byte leaf header. The key/value bytes follow immediately after
/// it in the same contiguous, slot-registered node body — see the
/// module docs for the full layout. `size_of_node(NodeType::Leaf)`
/// returns 16 (the header only); the real allocation is variable and
/// is recovered from `key_len`/`value_len` by `body_of_slot`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Leaf {
    /// One-byte fingerprint of the full key (a non-zero hash). A
    /// point lookup compares it before touching the key/value bytes,
    /// so ~255/256 of non-matching leaves are rejected without the
    /// full key compare. `0` means "no fingerprint" — the reader then
    /// always falls back to the full key compare. Never a false
    /// negative: a mismatch only fires when the keys truly differ.
    pub key_fp: u8,
    /// = `NodeType::Leaf.as_u8()` = 1. Lives at `+1` like every other
    /// node body's `node_type` byte so an offset-addressed reader can
    /// resolve a leaf's NodeType from `body[off + 1]` with no slot
    /// lookup.
    pub node_type: u8,
    /// Size in bytes of the value portion of the body.
    pub value_len: u16,
    /// Size in bytes of the key portion of the body (includes the
    /// ART terminator byte).
    pub key_len: u16,
    /// 0 = live leaf, 1 = tombstone (soft-deleted; pending
    /// reclaim via compactBlob).
    pub tombstone: u8,
    /// Reserved padding so `seq` lands 8-byte aligned at `+8` and the
    /// header is exactly 16 bytes.
    pub _pad: u8,
    /// Monotonic record sequence, bumped on every write that
    /// touches this slot. Used for CAS tokens and WAL replay.
    pub seq: u64,
}

const _: () = assert!(size_of::<Leaf>() == 16);
const _: () = assert!(offset_of!(Leaf, key_fp) == 0);
const _: () = assert!(offset_of!(Leaf, node_type) == 1);
const _: () = assert!(offset_of!(Leaf, value_len) == 2);
const _: () = assert!(offset_of!(Leaf, key_len) == 4);
const _: () = assert!(offset_of!(Leaf, tombstone) == 6);
const _: () = assert!(offset_of!(Leaf, _pad) == 7);
const _: () = assert!(offset_of!(Leaf, seq) == 8);

impl Leaf {
    /// Construct a live (non-tombstone) leaf header. `key_len` and
    /// `value_len` describe the bytes that follow the header in the
    /// same contiguous body; `key_fp` is the one-byte key fingerprint
    /// (non-zero) the lookup uses to skip the key compare on a
    /// mismatch (pass `0` to disable it).
    #[must_use]
    pub const fn live(key_len: u16, value_len: u16, seq: u64, key_fp: u8) -> Self {
        Self {
            key_fp,
            node_type: super::NodeType::Leaf.as_u8(),
            value_len,
            key_len,
            tombstone: 0,
            _pad: 0,
            seq,
        }
    }
}

/// Compute the 8-byte-aligned total size of a leaf node body holding
/// a `key_len`-byte key and `value_len`-byte value after the 16-byte
/// header: `align8(16 + key_len + value_len)`.
#[must_use]
pub const fn leaf_body_size(key_len: u32, value_len: u32) -> u32 {
    let raw = 16 + key_len + value_len;
    (raw + 7) & !7
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_size_alignment() {
        assert_eq!(leaf_body_size(0, 0), 16); // 16 → 16
        assert_eq!(leaf_body_size(3, 5), 24); // 16+3+5=24
        assert_eq!(leaf_body_size(4, 4), 24); // 16+4+4=24
        assert_eq!(leaf_body_size(1, 0), 24); // 16+1=17 → 24
        assert_eq!(leaf_body_size(10, 5), 32); // 16+10+5=31 → 32
        assert_eq!(leaf_body_size(100, 200), (16 + 100 + 200 + 7) & !7);
    }

    #[test]
    fn body_size_always_aligned_to_8() {
        for key_len in 0..64 {
            for value_len in 0..64 {
                let s = leaf_body_size(key_len, value_len);
                assert_eq!(s % 8, 0, "leaf_body_size({key_len}, {value_len}) = {s}");
                let need = 16 + key_len + value_len;
                assert!(s >= need);
                assert!(s < need + 8);
            }
        }
    }
}
