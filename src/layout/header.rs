//! `BlobHeader` — the 4096-byte fixed header at the start of every
//! 512 KB blob frame.
//!
//! Field offsets are pinned at compile time via
//! `const _: () = assert!(...)` blocks. If a field is ever moved,
//! the build fails — preventing accidental on-disk format drift
//! across releases.

use std::mem::{offset_of, size_of};

/// Total bytes per blob frame. The whole engine assumes 524288.
pub const PAGE_SIZE: u32 = 0x80000;

/// Header reserves the first 4096 bytes. Slot table starts at +0x1000.
pub const HEADER_SIZE: u32 = 0x1000;

/// Hard cap on slots in one blob.
pub const MAX_SLOTS: u32 = 0x2800;

/// Slot table size = MAX_SLOTS × `sizeof(u32)` = 40 KB.
pub const SLOT_TABLE_SIZE: u32 = MAX_SLOTS * 4;

/// Data area starts after the header + slot table.
pub const DATA_AREA_START: u32 = HEADER_SIZE + SLOT_TABLE_SIZE;

const _: () = assert!(DATA_AREA_START == 0xB000);

/// 128-bit blob identifier (stored as 16 bytes).
pub type BlobGuid = [u8; 16];

/// Fixed GUID of the root blob in single-tree mode.
pub(crate) const ROOT_BLOB_GUID: BlobGuid = [0; 16];

/// On-disk header for a 512 KB blob frame.
///
/// 4096 bytes fixed. Field positions are chosen for natural
/// alignment + room to grow without breaking compatibility:
/// counter fields are clustered near the front, the per-NodeType
/// free list lives at +0x70, the blob GUID at +0xa0, and the
/// remainder is reserved for future metadata.
///
/// Padding bytes (`_pad_NN`) are reserved space; future versions
/// may name them without moving any existing field's offset.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BlobHeader {
    _pad_0: [u8; 0x50],
    /// Reserved counter slot.
    pub field_50: u16,
    /// Reserved counter slot.
    pub field_52: u16,
    /// Slot-table high-water mark — new slots allocated at this index.
    pub num_slots: u16,
    /// Encoded byte offset of the tree root's node body — the same
    /// biased child encoding used by `children[N]` / `Prefix.child`
    /// (see `encode_child_off`): `(byte_offset / 8) -
    /// (DATA_AREA_START / 8) + 1`. `0` is never valid (it's the null
    /// sentinel); after init it points at the empty-tree EmptyRoot
    /// sentinel at `DATA_AREA_START`, which encodes to `1`.
    ///
    /// (Historically a 1-based slot index; v4 switched node addressing
    /// from slots to body offsets so a node hop is a single load. The
    /// field name is retained for on-disk-offset stability; semantics
    /// are now "encoded root offset".)
    pub root_slot: u16,
    /// Absolute byte offset of the next free byte in the data area.
    /// Starts at `DATA_AREA_START` (= 0xB000) after init.
    pub space_used: u32,
    /// Count of external-blob references currently held.
    pub num_ext_blobs: u16,
    /// Reserved counter slot.
    pub field_5e: u16,
    /// Number of times the engine's in-place compactor has rebuilt
    /// this blob. Bumped at the end of every successful compaction.
    /// Surfaced via [`Tree::stats`](crate::Tree::stats).
    pub compact_times: u32,
    /// Bytes of node bodies abandoned (made unreachable) since the
    /// last compaction. v4 structural ops (node grow/shrink/collapse,
    /// leaf value-grow realloc, EmptyRoot replacement, prefix split)
    /// no longer free their old node — they allocate the replacement,
    /// repoint the parent at it, and leave the old body unreachable
    /// (abandon-on-free). This counter accumulates that dead weight so
    /// `blob_needs_compaction` can trigger a rebuild before a churny
    /// blob bloats. Reset to 0 at the end of every successful
    /// compaction. (Was reserved padding `_pad_64`; v4 named it.)
    pub dead_bytes: u32,
    /// Cumulative count of size-table bytes ever allocated for
    /// nodes (used to drive compaction triggers).
    pub gap_space: u32,
    /// Count of leaves in this blob currently in tombstone state
    /// (soft-deleted, awaiting reclaim by compaction). Bumped on
    /// `erase`, decremented on `insert` resurrection, reset to 0
    /// at the end of every successful in-place compaction.
    pub tombstone_leaf_cnt: u32,
    /// Per-NodeType free-list head. Index 0 = ntype 1 (Leaf),
    /// index 1 = ntype 2 (Prefix), …, index 7 = ntype 8 (EmptyRoot).
    /// Value `0` means the list is empty.
    pub free_list_head: [u16; 8],
    /// Monotonic global epoch at which this frame version was
    /// created. Drives copy-on-write snapshots: a frame may be
    /// visible to a snapshot taken at epoch `E` only if
    /// `created_epoch <= E`. `0` means "older than any snapshot" —
    /// the conservative default for frames written before this field
    /// existed, which forces a copy-on-write on first mutation under
    /// any live snapshot (safe, just not maximally lazy).
    pub created_epoch: u64,
    /// Root-frame-only epoch high-water mark. Persisted so a reopened
    /// tree restores `current_epoch` above every frame's `created_epoch`,
    /// keeping snapshots taken after reopen correct. Stamped on the live
    /// root at each snapshot; ignored on non-root frames.
    pub epoch_high_water: u64,
    _pad_90: [u8; 0x10],
    /// 128-bit blob identifier.
    pub blob_guid: BlobGuid,
    /// Page-granular cold-read routing region (see
    /// `docs/design/cold-read-oracle.md`). When a blob is compacted into the
    /// routing layout, every internal node is clustered into
    /// `[routing_off, routing_off + routing_len)` and leaves are page-aligned
    /// at/after `leaf_region_start`, so a cold lookup reads the small routing
    /// region + one leaf page instead of pinning the whole 512 KB frame.
    ///
    /// `routing_len == 0` means **not in routing layout** → read the whole
    /// frame. This is the safe default for every blob written before this
    /// field existed (`BlobFrameMut::init` zeroes the whole frame) and for
    /// every blob not yet rewritten by the routing-aware compactor.
    pub routing_off: u32,
    pub routing_len: u32,
    pub leaf_region_start: u32,
    /// Set by the routing-aware compactor when pass-0 measured this blob
    /// as routable (`blob_would_route`) but the routed clone overran and
    /// it fell back to the legacy layout — a `routing_budget` /
    /// `clone_subtree` drift a correct build never hits (a `debug_assert`
    /// fires there). The maintenance scheduler treats a blob with this
    /// set as un-routable, so such a drift cannot make a settled blob
    /// recompact on every maintenance cycle (unbounded write
    /// amplification). Every compaction rebuilds the frame from zero, so
    /// it is naturally cleared on the next (e.g. churn-driven)
    /// compaction, which re-evaluates routability. `0` is the safe
    /// default for every existing blob (`init` zeroes the frame).
    pub routing_unfit: u32,
    /// Per-blob bloom over this blob's live leaf keys (cold-read stage
    /// 6). Built at compaction and placed at the **tail of the routing
    /// region** (`[routing_off, leaf_region_start)`), so the cold routed
    /// read loads it for free alongside the routing region and a
    /// within-blob *negative* lookup can answer `NotFound` without the
    /// one leaf-page read it would otherwise cost. `bloom_len == 0` means
    /// no bloom (legacy blob, didn't fit, or not yet rebuilt) → always
    /// read the leaf. `bloom_off` is the absolute byte offset of the
    /// filter bytes; `bloom_bits_per_key` is the build parameter
    /// `bloom_contains` needs to recompute the probe count. A bloom only
    /// ever *skips* a leaf read on a provable miss, so it cannot change
    /// `get()` semantics. See `docs/design/io-optimization.md`.
    pub bloom_off: u32,
    pub bloom_len: u32,
    pub bloom_bits_per_key: u32,
    _pad_cc: [u8; (HEADER_SIZE as usize) - 0xcc],
}

/// The cold-read routing region recorded in a [`BlobHeader`].
///
/// Stage 1 lands the reader; the cold routed-read path (stage 3,
/// `cold_read_routed`) is its first production consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoutingRegion {
    /// Byte offset of the contiguous internal-node region within the frame.
    pub off: u32,
    /// Byte length of the routing region.
    pub len: u32,
    /// First byte offset of the page-aligned leaf region. A child offset
    /// `>= leaf_region_start` is a leaf (read via a targeted page read);
    /// below it is an internal node inside the routing region.
    pub leaf_region_start: u32,
}

impl BlobHeader {
    /// The cold-read routing region, or `None` if this blob is in the legacy
    /// whole-frame layout (descend over the entire 512 KB frame).
    ///
    /// Returns `None` not only for `routing_len == 0` (the legacy
    /// sentinel) but also for any header whose routing bounds fall
    /// outside the frame's data area — `routing_off < DATA_AREA_START`,
    /// `leaf_region_start > PAGE_SIZE`, a non-ascending
    /// `routing_off .. leaf_region_start`, or a routing length that
    /// runs past the leaf region. A compactor-written routed blob always
    /// satisfies these; a header that fails them is corrupt (bit rot /
    /// torn write), and treating it as legacy keeps the cold routed read
    /// a *pure accelerator*: its caller slices `[routing_off,
    /// leaf_region_start)` out of a `PAGE_SIZE` buffer, so an out-of-range
    /// bound here must steer it to the authoritative full-frame pin
    /// rather than panic on an out-of-bounds slice.
    #[must_use]
    pub fn routing_region(&self) -> Option<RoutingRegion> {
        if self.routing_len == 0 {
            return None;
        }
        let off = self.routing_off;
        let lrs = self.leaf_region_start;
        let routing_end = off.checked_add(self.routing_len)?;
        // Bounds the cold read relies on (see doc above). All must hold
        // for a compactor-written routed blob; any failure ⇒ corrupt ⇒
        // legacy fallback.
        if off < DATA_AREA_START || off >= lrs || lrs > PAGE_SIZE || routing_end > lrs {
            return None;
        }
        Some(RoutingRegion {
            off,
            len: self.routing_len,
            leaf_region_start: lrs,
        })
    }

    /// The per-blob bloom as `(off, len, bits_per_key)`, or `None` when
    /// there is no usable bloom.
    ///
    /// Returns `None` for `bloom_len == 0` (legacy / no bloom) and for
    /// any bloom whose bytes do not lie wholly inside the routing region
    /// `[DATA_AREA_START, leaf_region_start)` (the span the cold read
    /// loads) — a corrupt/torn header is treated as "no bloom", so the
    /// cold read falls back to the authoritative leaf compare. Never a
    /// false negative.
    #[must_use]
    pub fn bloom_region(&self) -> Option<(u32, u32, u8)> {
        if self.bloom_len == 0 {
            return None;
        }
        let off = self.bloom_off;
        let end = off.checked_add(self.bloom_len)?;
        // The bloom must sit inside the routing span the cold read reads
        // (after the internal nodes, before the page-aligned leaves).
        if off < DATA_AREA_START || end > self.leaf_region_start {
            return None;
        }
        let bpk = u8::try_from(self.bloom_bits_per_key).ok()?;
        if bpk == 0 {
            return None;
        }
        Some((off, self.bloom_len, bpk))
    }
}

// Pin every field offset at compile time. Drift breaks the build.
const _: () = assert!(size_of::<BlobHeader>() == HEADER_SIZE as usize);
const _: () = assert!(offset_of!(BlobHeader, field_50) == 0x50);
const _: () = assert!(offset_of!(BlobHeader, num_slots) == 0x54);
const _: () = assert!(offset_of!(BlobHeader, root_slot) == 0x56);
const _: () = assert!(offset_of!(BlobHeader, space_used) == 0x58);
const _: () = assert!(offset_of!(BlobHeader, num_ext_blobs) == 0x5c);
const _: () = assert!(offset_of!(BlobHeader, compact_times) == 0x60);
const _: () = assert!(offset_of!(BlobHeader, dead_bytes) == 0x64);
const _: () = assert!(offset_of!(BlobHeader, gap_space) == 0x68);
const _: () = assert!(offset_of!(BlobHeader, tombstone_leaf_cnt) == 0x6c);
const _: () = assert!(offset_of!(BlobHeader, free_list_head) == 0x70);
const _: () = assert!(offset_of!(BlobHeader, created_epoch) == 0x80);
const _: () = assert!(offset_of!(BlobHeader, epoch_high_water) == 0x88);
const _: () = assert!(offset_of!(BlobHeader, blob_guid) == 0xa0);
const _: () = assert!(offset_of!(BlobHeader, routing_off) == 0xb0);
const _: () = assert!(offset_of!(BlobHeader, routing_len) == 0xb4);
const _: () = assert!(offset_of!(BlobHeader, leaf_region_start) == 0xb8);
const _: () = assert!(offset_of!(BlobHeader, routing_unfit) == 0xbc);
const _: () = assert!(offset_of!(BlobHeader, bloom_off) == 0xc0);
const _: () = assert!(offset_of!(BlobHeader, bloom_len) == 0xc4);
const _: () = assert!(offset_of!(BlobHeader, bloom_bits_per_key) == 0xc8);

/// Byte offset of [`BlobHeader::created_epoch`] within a frame buffer.
pub const CREATED_EPOCH_OFFSET: usize = offset_of!(BlobHeader, created_epoch);

/// Stamp the per-frame creation epoch into an already-formatted frame
/// buffer. Written in native byte order to match the `#[repr(C)]`
/// [`BlobHeader`] field read elsewhere. The caller guarantees `buf` is
/// at least [`HEADER_SIZE`] bytes.
#[inline]
pub fn set_frame_created_epoch(buf: &mut [u8], epoch: u64) {
    buf[CREATED_EPOCH_OFFSET..CREATED_EPOCH_OFFSET + size_of::<u64>()]
        .copy_from_slice(&epoch.to_ne_bytes());
}

/// Read the per-frame creation epoch from a formatted frame buffer.
///
/// Inverse of [`set_frame_created_epoch`]; called on the mutation hot
/// path to decide whether a frame must be forked before an in-place
/// overwrite. The caller guarantees `buf` is at least [`HEADER_SIZE`]
/// bytes.
#[inline]
#[must_use]
pub fn frame_created_epoch(buf: &[u8]) -> u64 {
    u64::from_ne_bytes(
        buf[CREATED_EPOCH_OFFSET..CREATED_EPOCH_OFFSET + size_of::<u64>()]
            .try_into()
            .expect("frame buffer is at least HEADER_SIZE bytes"),
    )
}

/// Byte offset of [`BlobHeader::epoch_high_water`] within a frame buffer.
pub const EPOCH_HIGH_WATER_OFFSET: usize = offset_of!(BlobHeader, epoch_high_water);

/// Stamp the root-frame epoch high-water mark (see the field docs). The
/// caller guarantees `buf` is at least [`HEADER_SIZE`] bytes.
#[inline]
pub fn set_frame_epoch_high_water(buf: &mut [u8], epoch: u64) {
    buf[EPOCH_HIGH_WATER_OFFSET..EPOCH_HIGH_WATER_OFFSET + size_of::<u64>()]
        .copy_from_slice(&epoch.to_ne_bytes());
}

/// Read the root-frame epoch high-water mark.
#[inline]
#[must_use]
pub fn frame_epoch_high_water(buf: &[u8]) -> u64 {
    u64::from_ne_bytes(
        buf[EPOCH_HIGH_WATER_OFFSET..EPOCH_HIGH_WATER_OFFSET + size_of::<u64>()]
            .try_into()
            .expect("frame buffer is at least HEADER_SIZE bytes"),
    )
}

/// Byte offset of [`BlobHeader::blob_guid`] within a frame buffer.
pub const BLOB_GUID_OFFSET: usize = offset_of!(BlobHeader, blob_guid);

/// Overwrite the self-GUID in an already-formatted frame buffer.
///
/// Used when forking a frame to a fresh identity for copy-on-write
/// snapshots: the rest of the frame is position-independent (slots
/// address one another by intra-frame index, and `BlobNode`s address
/// *children* by GUID), so a raw byte copy plus this single patch
/// yields a valid frame under the new GUID. The caller guarantees
/// `buf` is at least [`HEADER_SIZE`] bytes.
#[inline]
pub fn set_frame_blob_guid(buf: &mut [u8], guid: BlobGuid) {
    buf[BLOB_GUID_OFFSET..BLOB_GUID_OFFSET + size_of::<BlobGuid>()]
        .copy_from_slice(guid.as_slice());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_size_and_offsets() {
        assert_eq!(size_of::<BlobHeader>(), 4096);
        assert_eq!(offset_of!(BlobHeader, num_slots), 0x54);
        assert_eq!(offset_of!(BlobHeader, root_slot), 0x56);
        assert_eq!(offset_of!(BlobHeader, space_used), 0x58);
        assert_eq!(offset_of!(BlobHeader, compact_times), 0x60);
        assert_eq!(offset_of!(BlobHeader, gap_space), 0x68);
        assert_eq!(offset_of!(BlobHeader, tombstone_leaf_cnt), 0x6c);
        assert_eq!(offset_of!(BlobHeader, free_list_head), 0x70);
        assert_eq!(offset_of!(BlobHeader, created_epoch), 0x80);
        assert_eq!(offset_of!(BlobHeader, epoch_high_water), 0x88);
        assert_eq!(offset_of!(BlobHeader, blob_guid), 0xa0);
    }

    #[test]
    fn created_epoch_round_trips_through_buffer() {
        let mut buf = vec![0u8; PAGE_SIZE as usize];
        let span = CREATED_EPOCH_OFFSET..CREATED_EPOCH_OFFSET + 8;
        assert_eq!(&buf[span.clone()], &[0u8; 8]);
        set_frame_created_epoch(&mut buf, 0x1234_5678_9abc_def0);
        assert_eq!(&buf[span], &0x1234_5678_9abc_def0_u64.to_ne_bytes());
        // Stamping must not disturb the adjacent guid field at 0xa0.
        assert_eq!(&buf[0xa0..0xb0], &[0u8; 16]);
    }

    #[test]
    fn constants_consistent() {
        assert_eq!(PAGE_SIZE, 524_288);
        assert_eq!(HEADER_SIZE, 4096);
        assert_eq!(MAX_SLOTS, 10_240);
        assert_eq!(DATA_AREA_START, 0xB000);
    }

    #[test]
    fn zeroed_header_is_legacy_layout() {
        // The cold-read routing fields live in former pad, which
        // `BlobFrameMut::init` zeroes — so every pre-existing blob, and every
        // blob not yet rewritten by the routing-aware compactor, reports the
        // legacy whole-frame layout and is read by pinning the full frame.
        let header: BlobHeader = unsafe { core::mem::zeroed() };
        assert_eq!(header.routing_off, 0);
        assert_eq!(header.routing_len, 0);
        assert_eq!(header.leaf_region_start, 0);
        assert_eq!(header.routing_region(), None);

        // A routing-laid-out blob reports its region.
        let routed = BlobHeader {
            routing_len: 0x40,
            routing_off: DATA_AREA_START,
            leaf_region_start: DATA_AREA_START + 0x40,
            ..header
        };
        assert_eq!(
            routed.routing_region(),
            Some(RoutingRegion {
                off: DATA_AREA_START,
                len: 0x40,
                leaf_region_start: DATA_AREA_START + 0x40,
            })
        );
    }

    #[test]
    fn corrupt_routing_bounds_report_legacy() {
        // A non-zero `routing_len` with out-of-range bounds (bit rot /
        // torn write) must report the legacy layout, NOT a region the
        // cold routed read would slice out of bounds. Each case flips one
        // bound past its limit.
        let base = BlobHeader {
            routing_len: 0x40,
            routing_off: DATA_AREA_START,
            leaf_region_start: DATA_AREA_START + 0x40,
            ..unsafe { core::mem::zeroed::<BlobHeader>() }
        };
        // Box the headers so the assertions don't stack a `[BlobHeader; N]`
        // array (each header is 4 KB).
        let assert_legacy = |h: Box<BlobHeader>, label: &str| {
            assert_eq!(h.routing_region(), None, "{label} must be legacy");
        };
        // routing_off before the data area.
        assert_legacy(
            Box::new(BlobHeader {
                routing_off: DATA_AREA_START - 8,
                ..base
            }),
            "routing_off before data area",
        );
        // leaf_region_start past the end of the frame.
        assert_legacy(
            Box::new(BlobHeader {
                leaf_region_start: PAGE_SIZE + 8,
                ..base
            }),
            "leaf_region_start past frame",
        );
        // Non-ascending: leaf_region_start at/below routing_off.
        assert_legacy(
            Box::new(BlobHeader {
                leaf_region_start: DATA_AREA_START,
                ..base
            }),
            "leaf_region_start <= routing_off",
        );
        // routing_len runs past leaf_region_start.
        assert_legacy(
            Box::new(BlobHeader {
                routing_len: 0x80,
                leaf_region_start: DATA_AREA_START + 0x40,
                ..base
            }),
            "routing_len past leaf region",
        );
        // routing_off + routing_len overflows u32.
        assert_legacy(
            Box::new(BlobHeader {
                routing_off: u32::MAX - 4,
                routing_len: 0x40,
                ..base
            }),
            "routing_off + routing_len overflow",
        );
    }
}
