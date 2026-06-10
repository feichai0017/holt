//! Per-blob bloom filter — stage 6.0 of the cold-read fix
//! (`docs/design/io-optimization.md`, `docs/design/cold-read-oracle.md`).
//!
//! Metadata workloads are negative-heavy (`open`/`stat`/`head` of keys
//! that do not exist). A routed cold read still pays one leaf-page
//! round-trip to prove a key is absent within a blob. A per-blob bloom
//! lets the descent answer "definitely not here" **without** that leaf
//! read — eliminating the single remaining round-trip on the negative
//! path, which dominates the workload.
//!
//! This module is the **data structure only** (stage 6.0): build a
//! filter from a blob's leaf keys, serialize it to bytes, and query
//! those bytes. It has no `BlobStore` / `BufferManager` / walker
//! integration yet — those land in stages 6.1 (build at compaction) and
//! 6.2 (BM-resident query in `descend_routed`).
//!
//! ## The contract: never a false negative
//!
//! A bloom answers **"maybe"** or **"definitely not"**, never
//! "definitely yes". [`bloom_contains`] returning `false` means the key
//! is *provably* not in the set the filter was built from; `true` means
//! *maybe* (read the leaf to be sure). The cold read uses only the
//! `false` answer to skip a leaf read; a `true` falls through to the
//! authoritative leaf compare. So a bloom can only ever **save** a read,
//! never change `get()` semantics — the same pure-accelerator discipline
//! as the routed read itself.
//!
//! ## Serialization
//!
//! A filter is fully described by its raw bit bytes plus the
//! `bits_per_key` it was built with (`k`, the number of probes, is
//! derived deterministically from `bits_per_key` by both build and
//! query — so only the bytes + `bits_per_key` need to travel). This is
//! exactly what the reserved [`crate::layout`] header fields
//! (`filter_len_pages`, `filter_bits_per_key`) and the BM sidecar will
//! carry.

// Stage 6.0 lands the data structure in isolation; its first non-test
// consumers are stage 6.1 (build at compaction) and 6.2 (query in the
// routed cold read). Until then the items below are exercised only by
// this module's tests. Remove when 6.2 wires it in.
#![allow(dead_code)]

/// Default bits per key — ~1% false-positive rate at the optimal probe
/// count. Tunable per blob (the header reserves `filter_bits_per_key`).
pub(crate) const BLOOM_BITS_PER_KEY: u8 = 8;

/// Floor on filter size in bytes, so a blob with a handful of leaves
/// still gets a usable (if generously sized) filter instead of a
/// 1-byte filter with a useless FPR. 32 bytes = 256 bits.
const MIN_BLOOM_BYTES: usize = 32;

/// Number of probe positions `k` for a given `bits_per_key`.
///
/// The FPR-optimal `k` is `ln(2) * bits_per_key ≈ 0.693 * bpk`. Computed
/// identically at build and query time so a filter is self-consistent
/// from `bits_per_key` alone. Clamped to `[1, 30]`.
#[inline]
#[must_use]
fn probe_count(bits_per_key: u8) -> u32 {
    // round(0.693 * bpk) via integer math: (bpk * 693 + 500) / 1000.
    let k = (u32::from(bits_per_key) * 693 + 500) / 1000;
    k.clamp(1, 30)
}

/// Byte length a filter for `num_keys` at `bits_per_key` will occupy
/// (rounded up to a `u64` word, floored at [`MIN_BLOOM_BYTES`]).
#[must_use]
pub(crate) fn bloom_byte_len(num_keys: usize, bits_per_key: u8) -> usize {
    let raw_bits = num_keys.saturating_mul(usize::from(bits_per_key));
    let raw_bytes = raw_bits.div_ceil(8);
    // Round up to 8 bytes so the bit array is a whole number of u64
    // words (query reads words), and floor at MIN_BLOOM_BYTES.
    raw_bytes.next_multiple_of(8).max(MIN_BLOOM_BYTES)
}

/// One 64-bit hash of `key`, split into two 32-bit halves for
/// Kirsch–Mitzenmacher double hashing (`bit_i = h1 + i*h2`). Uses the
/// same FNV-multiply + splitmix64 finalizer as the buffer-manager GUID
/// hasher, so it has full avalanche.
#[inline]
#[must_use]
fn key_hash(key: &[u8]) -> (u32, u32) {
    let mut acc: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
    let mut chunks = key.chunks_exact(8);
    for c in &mut chunks {
        let v = u64::from_le_bytes(c.try_into().unwrap());
        acc = (acc ^ v).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
    let rem = chunks.remainder();
    if rem.is_empty() {
        // Fold the length in so keys that are prefixes of one another
        // (e.g. with/without a terminator) hash distinctly.
        acc ^= key.len() as u64;
    } else {
        let mut buf = [0u8; 8];
        buf[..rem.len()].copy_from_slice(rem);
        acc = (acc ^ u64::from_le_bytes(buf) ^ (key.len() as u64))
            .wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
    // splitmix64 finalizer.
    let mut z = acc.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // h2 is forced odd so it is coprime with the (power-of-two-free) bit
    // count under modular stepping and never degenerates to 0.
    let h1 = z as u32;
    let h2 = ((z >> 32) as u32) | 1;
    (h1, h2)
}

/// Accumulates leaf keys into a bloom filter, then serializes the bit
/// bytes. Sized once up front from the (known-at-compaction) leaf count.
pub(crate) struct BloomBuilder {
    bytes: Vec<u8>,
    m_bits: u32,
    k: u32,
}

impl BloomBuilder {
    /// A builder for `num_keys` keys at `bits_per_key`.
    #[must_use]
    pub(crate) fn new(num_keys: usize, bits_per_key: u8) -> Self {
        let len = bloom_byte_len(num_keys, bits_per_key);
        Self {
            bytes: vec![0u8; len],
            // m_bits fits u32: len is bounded by PAGE_SIZE-class sizes.
            m_bits: (len * 8) as u32,
            k: probe_count(bits_per_key),
        }
    }

    /// Set the `k` probe bits for `key`.
    pub(crate) fn add(&mut self, key: &[u8]) {
        let (h1, h2) = key_hash(key);
        let mut h = h1;
        for _ in 0..self.k {
            let bit = h % self.m_bits;
            self.bytes[(bit / 8) as usize] |= 1u8 << (bit % 8);
            h = h.wrapping_add(h2);
        }
    }

    /// The serialized bit bytes. Pair with the `bits_per_key` used to
    /// build (so [`bloom_contains`] can recompute `k`).
    #[must_use]
    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// Query a serialized filter. Returns `true` (**maybe present** — read
/// the leaf) or `false` (**definitely absent** — skip the leaf read).
///
/// `bits_per_key` MUST be the value the filter was built with;
/// `filter_bytes` MUST be a filter produced by [`BloomBuilder`] (its
/// length defines `m`). A zero-length or malformed `filter_bytes`
/// conservatively returns `true` (maybe) so the caller falls back to the
/// authoritative leaf read — never a false negative.
#[must_use]
pub(crate) fn bloom_contains(filter_bytes: &[u8], bits_per_key: u8, key: &[u8]) -> bool {
    if filter_bytes.is_empty() {
        return true; // no filter ⇒ "maybe" ⇒ read the leaf
    }
    let m_bits = (filter_bytes.len() * 8) as u32;
    let k = probe_count(bits_per_key);
    let (h1, h2) = key_hash(key);
    let mut h = h1;
    for _ in 0..k {
        let bit = h % m_bits;
        if filter_bytes[(bit / 8) as usize] & (1u8 << (bit % 8)) == 0 {
            return false; // a clear bit ⇒ provably absent
        }
        h = h.wrapping_add(h2);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_key(n: u64) -> Vec<u8> {
        let mut k = b"obj/bucket-7/".to_vec();
        k.extend_from_slice(&n.to_le_bytes());
        k
    }

    #[test]
    fn probe_count_is_optimal_and_clamped() {
        assert_eq!(probe_count(8), 6); // round(0.693*8)=round(5.54)=6
        assert_eq!(probe_count(1), 1); // round(0.693)=1, clamp floor
        assert_eq!(probe_count(0), 1); // clamp floor
        assert_eq!(probe_count(255), 30); // clamp ceil
    }

    #[test]
    fn byte_len_rounds_and_floors() {
        assert_eq!(bloom_byte_len(0, 8), MIN_BLOOM_BYTES);
        assert_eq!(bloom_byte_len(1, 8), MIN_BLOOM_BYTES); // 1B raw → floored
        // 1000 keys * 8 bpk = 8000 bits = 1000 bytes (already /8).
        assert_eq!(bloom_byte_len(1000, 8), 1000);
        // Always a whole number of u64 words.
        assert_eq!(bloom_byte_len(123, 8) % 8, 0);
    }

    #[test]
    fn no_false_negatives_every_present_key_is_maybe() {
        // The load-bearing contract: a key that was added MUST query
        // true. Tested across sizes and bits_per_key.
        for &n in &[1usize, 10, 100, 1000, 5000] {
            for &bpk in &[4u8, 8, 12, 16] {
                let mut b = BloomBuilder::new(n, bpk);
                let keys: Vec<_> = (0..n as u64).map(make_key).collect();
                for k in &keys {
                    b.add(k);
                }
                let bytes = b.into_bytes();
                for k in &keys {
                    assert!(
                        bloom_contains(&bytes, bpk, k),
                        "false negative: present key missed (n={n}, bpk={bpk})"
                    );
                }
            }
        }
    }

    #[test]
    fn false_positive_rate_is_near_target() {
        // 8 bits/key should give ~1-2% FPR. Build over N keys, probe N
        // disjoint absent keys, assert the FPR is well under a loose 5%
        // ceiling (the math target is ~2%; the ceiling absorbs variance).
        let n = 5000usize;
        let bpk = 8u8;
        let mut b = BloomBuilder::new(n, bpk);
        for i in 0..n as u64 {
            b.add(&make_key(i));
        }
        let bytes = b.into_bytes();

        let probes = 20_000u64;
        let mut fp = 0u64;
        for i in 0..probes {
            // Absent keys: a different namespace, no overlap with build.
            let mut k = b"absent/".to_vec();
            k.extend_from_slice(&(i ^ 0xDEAD_BEEF).to_le_bytes());
            if bloom_contains(&bytes, bpk, &k) {
                fp += 1;
            }
        }
        let rate = fp as f64 / probes as f64;
        assert!(rate < 0.05, "FPR too high: {rate:.4} (expected ~0.02)");
    }

    #[test]
    fn empty_or_malformed_filter_is_maybe() {
        // Zero-length filter ⇒ conservative "maybe" (never a false
        // negative): the caller reads the leaf.
        assert!(bloom_contains(&[], 8, b"anything"));
    }

    #[test]
    fn distinct_filters_are_independent() {
        // A key in filter A but not B must be absent from B (sanity that
        // the hash actually depends on the key, not a constant).
        let mut a = BloomBuilder::new(100, 8);
        a.add(b"only-in-a");
        let abytes = a.into_bytes();
        // A fresh empty-content filter of the same size: the key should
        // (almost certainly) miss.
        let bbytes = BloomBuilder::new(100, 8).into_bytes();
        assert!(bloom_contains(&abytes, 8, b"only-in-a"));
        assert!(!bloom_contains(&bbytes, 8, b"only-in-a"));
    }
}
