//! `Prefix` body — 128 bytes fixed.
//!
//! Carries an inline path-compressed prefix (up to 112 bytes)
//! and a single child slot. The walker matches the prefix
//! against `key[depth..]` and descends to `child` with
//! `depth += prefix_len`.

use std::mem::{offset_of, size_of};

/// Maximum inline prefix bytes a single Prefix node holds.
/// Longer prefixes chain through multiple Prefix nodes
/// (Prefix → Prefix → ... → child).
pub const PREFIX_MAX_INLINE: usize = 112;

/// 128-byte path-compressed prefix node.
///
/// Carries an inline byte string and a single child slot. The
/// walker's `.prefix` arm matches `bytes[..prefix_len]` against
/// `key[depth..]`, then descends to `child` with `depth +=
/// prefix_len`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Prefix {
    /// Always 1 — Prefix has exactly one child.
    pub count: u8,
    /// = `NodeType::Prefix.as_u8()` = 2.
    pub node_type: u8,
    _pad_2: [u8; 2],
    /// Number of valid bytes in `bytes`.
    pub prefix_len: u16,
    _pad_6: u16,
    /// Slot index of the single child node.
    pub child: u32,
    _pad_12: u32,
    /// Inline prefix bytes (only first `prefix_len` are valid).
    pub bytes: [u8; PREFIX_MAX_INLINE],
}

const _: () = assert!(size_of::<Prefix>() == 128);
const _: () = assert!(offset_of!(Prefix, count) == 0);
const _: () = assert!(offset_of!(Prefix, node_type) == 1);
const _: () = assert!(offset_of!(Prefix, prefix_len) == 4);
const _: () = assert!(offset_of!(Prefix, child) == 8);
const _: () = assert!(offset_of!(Prefix, bytes) == 16);

impl Prefix {
    /// Build a Prefix node holding `prefix_bytes` and pointing at
    /// `child_slot`. Panics if `prefix_bytes.len() >
    /// PREFIX_MAX_INLINE`.
    #[must_use]
    pub fn new(prefix_bytes: &[u8], child_slot: u32) -> Self {
        assert!(prefix_bytes.len() <= PREFIX_MAX_INLINE);
        let mut p = Self {
            count: 1,
            node_type: super::NodeType::Prefix.as_u8(),
            _pad_2: [0; 2],
            prefix_len: prefix_bytes.len() as u16,
            _pad_6: 0,
            child: child_slot,
            _pad_12: 0,
            bytes: [0; PREFIX_MAX_INLINE],
        };
        p.bytes[..prefix_bytes.len()].copy_from_slice(prefix_bytes);
        p
    }
}
