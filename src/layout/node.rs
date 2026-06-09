//! `NodeType` enum + size table.

/// NodeType discriminant.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeType {
    /// Sentinel — never appears in a valid tree. Reading a slot
    /// tagged `invalid` panics.
    Invalid = 0,
    /// Key-value leaf. A single variable-size, self-describing node:
    /// `[16B header][key bytes][value bytes]`, allocated contiguously
    /// in the data area. `size_of_node` reports the 16-byte header
    /// size; the true body size is recovered from the header's
    /// `key_len`/`value_len` by `body_of_slot`.
    Leaf = 1,
    /// Path-compressed prefix (128-byte fixed body; up to 112
    /// inline bytes).
    Prefix = 2,
    /// In-tree blob crossing (128-byte body carrying
    /// `child_blob_guid` plus an inline path prefix).
    Blob = 3,
    /// 1..4 children, parallel sorted `keys[4]` + `children[4]`.
    Node4 = 4,
    /// 5..16 children, sorted `keys[16]` for SIMD scan.
    Node16 = 5,
    /// 17..48 children, byte-indexed `index[256]` → `children[48]`.
    Node48 = 6,
    /// 49..256 children, direct `children[256]`.
    Node256 = 7,
    /// Empty-tree sentinel: 8 bytes all zero. Allocated once on
    /// `BlobFrame::init` and stored at `header.root_slot`.
    EmptyRoot = 8,
}

impl NodeType {
    /// Convert a raw byte (e.g. from a `SlotEntry`'s
    /// `ntype_or_next_free` field) into a `NodeType`. Returns
    /// `None` for values outside 0..=8.
    #[must_use]
    pub fn from_raw(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Invalid),
            1 => Some(Self::Leaf),
            2 => Some(Self::Prefix),
            3 => Some(Self::Blob),
            4 => Some(Self::Node4),
            5 => Some(Self::Node16),
            6 => Some(Self::Node48),
            7 => Some(Self::Node256),
            8 => Some(Self::EmptyRoot),
            _ => None,
        }
    }

    /// Underlying byte representation.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Per-NodeType allocation sizes (bytes), indexed by `ntype - 1`.
///
/// Sizes are chosen so the four ART-internal variants
/// (Node{4,16,48,256}) fit their children + index arrays exactly
/// with no slack. `Leaf` is reported here as its 16-byte HEADER only;
/// a leaf node is variable-size (`[16B header][key][value]`) and its
/// true allocation is recovered from `key_len`/`value_len` by
/// `body_of_slot`. Prefix and Blob are both 128 B so their inline
/// path-compressed bytes fit comfortably.
pub const SIZE_BY_TYPE: [u32; 8] = [
    16,  // Leaf (header only — see note above)
    128, // Prefix
    128, // Blob
    16,  // Node4   (u16 children)
    56,  // Node16  (u16 children)
    360, // Node48  (u16 children)
    520, // Node256 (u16 children)
    8,   // EmptyRoot
];

/// Bytes a single allocation of the given NodeType consumes.
///
/// Panics on `NodeType::Invalid` (which has no associated size).
///
/// **Note:** for `NodeType::Leaf` this returns the 16-byte HEADER
/// size only — a leaf is variable-size and its true body length is
/// `leaf_body_size(key_len, value_len)`, recovered from the header by
/// `body_of_slot`. Sizing a leaf slot via `size_of_node` alone would
/// under-read the key/value bytes.
#[must_use]
pub fn size_of_node(ntype: NodeType) -> u32 {
    assert!(ntype != NodeType::Invalid, "size_of_node(Invalid)");
    let idx = ntype as usize - 1;
    SIZE_BY_TYPE[idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ntype_round_trip_via_raw() {
        let all = [
            NodeType::Invalid,
            NodeType::Leaf,
            NodeType::Prefix,
            NodeType::Blob,
            NodeType::Node4,
            NodeType::Node16,
            NodeType::Node48,
            NodeType::Node256,
            NodeType::EmptyRoot,
        ];
        for t in all {
            assert_eq!(NodeType::from_raw(t.as_u8()), Some(t));
        }
        // Values 9 and above are not in the enum.
        assert_eq!(NodeType::from_raw(9), None);
        assert_eq!(NodeType::from_raw(255), None);
    }

    #[test]
    fn size_table_per_node_type() {
        assert_eq!(size_of_node(NodeType::Leaf), 16);
        assert_eq!(size_of_node(NodeType::Prefix), 128);
        assert_eq!(size_of_node(NodeType::Blob), 128);
        assert_eq!(size_of_node(NodeType::Node4), 16);
        assert_eq!(size_of_node(NodeType::Node16), 56);
        assert_eq!(size_of_node(NodeType::Node48), 360);
        assert_eq!(size_of_node(NodeType::Node256), 520);
        assert_eq!(size_of_node(NodeType::EmptyRoot), 8);
    }
}
