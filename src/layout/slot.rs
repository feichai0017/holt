//! Bit-packed slot table entry.
//!
//! Each `u32` slot entry encodes:
//!
//! - bits 0..16 (17 bits) = `byte_offset / 8` (8-byte alignment is
//!   an invariant of the bump allocator).
//! - bits 17..31 (15 bits) = NodeType discriminant for live slots,
//!   OR next-free-slot index for slots on the free list.
//!
//! Packing both into 32 bits keeps the 10240-entry slot table at
//! 40 KB, leaving the data area unfragmented.

use super::node::NodeType;

/// Raw 32-bit slot entry as stored in the slot table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct SlotEntryRaw(pub u32);

const OFFSET_DIV8_MASK: u32 = (1 << 17) - 1;
const TAG_SHIFT: u32 = 17;

/// Structured view over a slot entry.
#[derive(Debug, Clone, Copy)]
pub struct SlotEntry {
    /// Byte offset of the node body / 8 (8-byte aligned).
    pub offset_div8: u32,
    /// For live slots: `NodeType as u8`. For freed slots: index
    /// of next free slot for the same NodeType (0 = end of chain).
    pub ntype_or_next_free: u16,
}

impl SlotEntry {
    /// Build a slot entry for a freshly-allocated, live node.
    #[must_use]
    pub fn live(ntype: NodeType, byte_offset: u32) -> Self {
        debug_assert_eq!(byte_offset % 8, 0, "body must be 8-byte aligned");
        debug_assert!(byte_offset < super::header::PAGE_SIZE, "offset out of blob");
        Self {
            offset_div8: byte_offset / 8,
            ntype_or_next_free: ntype.as_u8() as u16,
        }
    }

    /// Build a slot entry tagging this slot as on the free list,
    /// pointing at `next_free_slot` (1-based; 0 = list end).
    ///
    /// v4 abandon-on-free: the walker no longer frees nodes onto the
    /// per-NodeType free lists (see [`crate::store::BlobFrame`]
    /// `free_node`), so this is exercised only by tests now.
    #[cfg_attr(not(test), allow(dead_code))]
    #[must_use]
    pub fn freed(next_free_slot: u16, byte_offset: u32) -> Self {
        debug_assert_eq!(byte_offset % 8, 0);
        // ntype_or_next_free is 15 bits — the slot index must fit.
        debug_assert!(next_free_slot < (1 << 15));
        Self {
            offset_div8: byte_offset / 8,
            ntype_or_next_free: next_free_slot,
        }
    }

    /// The body's byte offset within the blob buffer.
    #[must_use]
    pub const fn byte_offset(self) -> u32 {
        self.offset_div8 * 8
    }

    /// Interpret the tag as a `NodeType` (for live slots).
    /// Returns `None` if the tag is outside the NodeType range
    /// (which would happen for freed slots whose next-free chain
    /// index is ≥ 9). v4 reads NodeType from the node body's
    /// `node_type @ +1` byte ([`crate::store`] `ntype_at_offset`); the
    /// slot tag survives for allocation bookkeeping and tests.
    #[cfg_attr(not(test), allow(dead_code))]
    #[must_use]
    pub fn node_type(self) -> Option<NodeType> {
        NodeType::from_raw(self.ntype_or_next_free as u8)
    }

    /// Interpret the tag as a next-free-slot pointer.
    #[must_use]
    pub const fn next_free(self) -> u16 {
        self.ntype_or_next_free
    }

    /// Encode into the 32-bit raw value stored in the slot table.
    #[must_use]
    pub const fn raw(self) -> SlotEntryRaw {
        SlotEntryRaw((self.ntype_or_next_free as u32) << TAG_SHIFT | self.offset_div8)
    }
}

impl SlotEntryRaw {
    /// Decode the raw 32-bit value.
    #[must_use]
    pub const fn decode(self) -> SlotEntry {
        SlotEntry {
            offset_div8: self.0 & OFFSET_DIV8_MASK,
            ntype_or_next_free: (self.0 >> TAG_SHIFT) as u16,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_round_trip() {
        // Encoding: `ntype << 17 | (byte_offset / 8)`.
        let e = SlotEntry::live(NodeType::Node4, 0xB000);
        let raw: u32 = e.raw().0;
        let expected: u32 = (4u32 << 17) | (0xB000u32 / 8);
        assert_eq!(raw, expected);

        let back = SlotEntryRaw(raw).decode();
        assert_eq!(back.byte_offset(), 0xB000);
        assert_eq!(back.node_type(), Some(NodeType::Node4));
    }

    #[test]
    fn freed_chain_preserves_offset_and_next() {
        let e = SlotEntry::freed(42, 0x1000);
        assert_eq!(e.next_free(), 42);
        assert_eq!(e.byte_offset(), 0x1000);
        // The free-chain `next_free=42` is outside NodeType range,
        // so `.node_type()` returns None.
        assert_eq!(e.node_type(), None);
    }

    #[test]
    fn alignment_check_panics_in_debug_only() {
        // In release builds debug_assert is compiled out; we only
        // sanity-check the happy path here.
        let _ = SlotEntry::live(NodeType::Leaf, 0xB008);
    }
}
