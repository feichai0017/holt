//! Spillover infra — pick a subtree to migrate when a blob fills,
//! stage it as a fresh dirty child blob, free the source's slots,
//! and install a `BlobNode` placeholder.
//!
//! Also hosts:
//! - `free_subtree` (recursive slot reclaim after migration)
//! - `fresh_blob_guid` (cheap process-local GUIDs)
//! - `compact_blob` (in-place repack, re-exported from
//!   [`super::migrate`])

use crate::api::errors::{Error, Result};
use crate::layout::{
    leaf_extent_size, size_of_node, BlobGuid, BlobNode, Node16, Node256, Node4, Node48, NodeType,
    Prefix, DATA_AREA_START, PAGE_SIZE,
};
use crate::store::{BlobFrame, BufferManager};

use super::super::simd;
use super::cast;
use super::migrate::make_blob_from_node_in;
use super::readers::{
    ntype_of, read_leaf_key_ref, read_node16, read_node256, read_node4, read_node48, read_prefix,
};
use super::types::{Victim, VictimEdgeKind};
use super::writers::{inner_update_child, set_prefix_child, write_struct_to_slot};

// Re-export `compact_blob` so `insert_multi` can reach it via
// `super::spillover::compact_blob`.
pub(super) use super::migrate::compact_blob;

const SPILLOVER_TARGET_CHILD_FILL_PCT: u32 = 70;
const SPILLOVER_MIN_CHILD_FILL_PCT: u32 = 35;

#[derive(Debug, Clone, Copy)]
struct SubtreeFootprint {
    nodes: u32,
    bytes: u32,
}

#[derive(Debug, Clone, Copy)]
struct VictimCandidate {
    victim: Victim,
    footprint: SubtreeFootprint,
    boundary: BoundaryQuality,
    boundary_depth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoundaryQuality {
    Arbitrary,
    PathComponent,
}

fn spillover_data_capacity() -> u32 {
    PAGE_SIZE - DATA_AREA_START
}

fn spillover_target_child_bytes() -> u32 {
    spillover_data_capacity() * SPILLOVER_TARGET_CHILD_FILL_PCT / 100
}

fn spillover_min_child_bytes() -> u32 {
    spillover_data_capacity() * SPILLOVER_MIN_CHILD_FILL_PCT / 100
}

/// Trigger spillover on `frame`: migrate a subtree out to a fresh
/// child blob (via [`make_blob_from_node`]), free the migrated
/// slots, and install a [`BlobNode`] placeholder at the migrated
/// location.
///
/// Heuristic: pick an occupancy-aware non-Blob subtree at the
/// root's first branching node (i.e. skip BlobNode children —
/// those are already migrated). The target is a child blob around
/// 70% full, with node count used only as a tie-breaker. This keeps
/// blob hops and follow-up split pressure stable at multi-million
/// key scale instead of repeatedly peeling off the largest branch
/// into near-full child blobs.
///
/// Returns the BlobNode slot installed in `frame` so callers /
/// tests can verify. The new blob lives in the BM cache + dirty
/// map; its store write happens during the next checkpoint round
/// (after the WAL record for the spillover-triggering op is
/// durable — invariant W2D).
///
/// `seq` is the WAL seq the caller pre-allocated for the op that
/// triggered spillover (insert / rename / batched put). The new
/// child blob is tagged `mark_dirty(new_guid, seq)` so the
/// checkpoint round's truncate gate can't drop the WAL record
/// before the blob's bytes are durable.
pub(super) fn spillover_blob(
    bm: &BufferManager,
    frame: &mut BlobFrame<'_>,
    seq: u64,
) -> Result<u16> {
    let root_slot = frame.header().root_slot;
    let victim = pick_victim_subtree(frame, root_slot)?;

    let new_guid = fresh_blob_guid();
    let outcome = make_blob_from_node_in(bm, frame, victim.victim_slot, new_guid)?;

    // Stage the new blob via the unified `mark_dirty → checkpoint
    // round` protocol — the bytes stay in cache until the round
    // flushes WAL **first** and then writes them through. An
    // inline `bm.write_blob(new_guid, ...) + bm.flush()` here
    // would violate invariant W2D: a crash between the inline
    // write and the user's WAL flush would leave an orphan in
    // store AND the parent's BlobNode staged only in cache —
    // and a racing checkpointer could flush the parent's
    // BlobNode before the user's WAL record was durable,
    // leaving the on-disk parent pointing at the pre-spillover
    // orphan position.
    bm.install_new_blob(new_guid, outcome.buf, seq);

    // Free the migrated subtree's slots in the source blob.
    free_subtree(frame, victim.victim_slot)?;

    // Allocate a BlobNode pointing at the child blob. The child
    // blob's own header.root_slot is the only entry slot.
    let bn_alloc = frame.alloc_node(NodeType::Blob)?;
    let bn = BlobNode::new(&[], new_guid);
    write_struct_to_slot(frame, bn_alloc.slot, &bn)?;

    // Wire the parent of the migrated subtree to point at the new
    // BlobNode instead of the now-freed victim slot.
    if victim.parent_slot == root_slot && victim.via_header_root {
        frame.header_mut().root_slot = bn_alloc.slot;
    } else {
        match victim.kind {
            VictimEdgeKind::Prefix => {
                set_prefix_child(frame, victim.parent_slot, u32::from(bn_alloc.slot))?;
            }
            VictimEdgeKind::Inner(parent_ntype) => {
                inner_update_child(
                    frame,
                    victim.parent_slot,
                    parent_ntype,
                    victim.byte,
                    u32::from(bn_alloc.slot),
                )?;
            }
        }
    }

    #[cfg(feature = "tracing")]
    tracing::debug!(
        target: "holt::engine::spillover",
        new_child_guid = ?&new_guid[..4],
        victim_slot = victim.victim_slot,
        bn_slot = bn_alloc.slot,
        "spillover: migrated subtree to fresh child blob",
    );

    Ok(bn_alloc.slot)
}

fn subtree_footprint(frame: &BlobFrame<'_>, root: u16) -> Result<SubtreeFootprint> {
    let ntype = ntype_of(frame.as_ref(), root)?;
    if ntype == NodeType::Invalid {
        return Err(Error::node_corrupt("subtree_footprint: Invalid"));
    }
    let body = frame.body_of_slot(root).ok_or(Error::node_corrupt(
        "subtree_footprint: body resolution failed",
    ))?;
    let mut out = SubtreeFootprint {
        nodes: 1,
        bytes: size_of_node(ntype),
    };
    match ntype {
        NodeType::Invalid => unreachable!("handled before size_of_node"),
        NodeType::Leaf => {
            let (key, leaf) = read_leaf_key_ref(frame.as_ref(), root)?;
            out.bytes = out.bytes.saturating_add(leaf_extent_size(
                key.len() as u32,
                u32::from(leaf.value_size),
            ));
        }
        NodeType::EmptyRoot | NodeType::Blob => {}
        NodeType::Prefix => {
            let p = cast::<Prefix>(body);
            out = out.saturating_add(subtree_footprint(frame, p.child as u16)?);
        }
        NodeType::Node4 => {
            let n = cast::<Node4>(body);
            for i in 0..(n.count as usize).min(4) {
                out = out.saturating_add(subtree_footprint(frame, n.children[i] as u16)?);
            }
        }
        NodeType::Node16 => {
            let n = cast::<Node16>(body);
            for i in 0..(n.count as usize).min(16) {
                out = out.saturating_add(subtree_footprint(frame, n.children[i] as u16)?);
            }
        }
        NodeType::Node48 => {
            let n = cast::<Node48>(body);
            let mut i = 0usize;
            while let Some(next_i) = simd::find_next_nonzero_u32(&n.children, i) {
                i = next_i + 1;
                out = out.saturating_add(subtree_footprint(frame, n.children[next_i] as u16)?);
            }
        }
        NodeType::Node256 => {
            let n = cast::<Node256>(body);
            let mut i = 0usize;
            while let Some(next_i) = simd::find_next_nonzero_u32(&n.children, i) {
                i = next_i + 1;
                out = out.saturating_add(subtree_footprint(frame, n.children[next_i] as u16)?);
            }
        }
    }
    Ok(out)
}

impl SubtreeFootprint {
    fn saturating_add(mut self, rhs: Self) -> Self {
        self.nodes = self.nodes.saturating_add(rhs.nodes);
        self.bytes = self.bytes.saturating_add(rhs.bytes);
        self
    }
}

/// Pick an occupancy-aware non-`BlobNode` subtree below
/// `start_slot`. Direct children are considered first; if a child
/// is already larger than the target child fill, the search descends
/// inside that child to find a healthier prefix boundary.
///
/// **Heuristic rationale:**
/// - Skipping `Blob` children avoids spillover-stutter (previously-
///   migrated children would otherwise get re-migrated into
///   wrapper blobs without freeing any actual data).
/// - Choosing a subtree close to the target child fill ratio avoids
///   creating child blobs that are immediately full, which is what
///   turns path-shaped 2M+ put workloads into repeated blob hops.
/// - Among healthy fill-ratio candidates, prefer boundaries that
///   end on `/`. Object-store and filesystem keys are component-
///   shaped; cutting a child blob at a component boundary improves
///   top-route cache reuse and avoids long low-reuse prefix hops.
#[allow(clippy::too_many_lines)] // intentional — one match over NodeType arms
fn pick_victim_subtree(frame: &BlobFrame<'_>, start_slot: u16) -> Result<Victim> {
    let mut best: Option<VictimCandidate> = None;
    collect_victim_candidates(frame, start_slot, 0, &mut best)?;
    best.map(|candidate| candidate.victim)
        .ok_or(Error::NotYetImplemented(
            "spillover: no non-Blob subtree to migrate",
        ))
}

#[allow(clippy::too_many_lines)] // one match over NodeType arms
fn collect_victim_candidates(
    frame: &BlobFrame<'_>,
    current: u16,
    depth: usize,
    best: &mut Option<VictimCandidate>,
) -> Result<()> {
    let ntype = ntype_of(frame.as_ref(), current)?;
    match ntype {
        NodeType::Node4 => {
            let n = read_node4(frame.as_ref(), current)?;
            for i in 0..(n.count as usize).min(4) {
                let child_depth = depth + 1;
                visit_child_edge(
                    frame,
                    Victim {
                        parent_slot: current,
                        kind: VictimEdgeKind::Inner(NodeType::Node4),
                        byte: n.keys[i],
                        victim_slot: n.children[i] as u16,
                        via_header_root: false,
                    },
                    boundary_quality_for_byte(n.keys[i]),
                    child_depth,
                    best,
                )?;
            }
        }
        NodeType::Node16 => {
            let n = read_node16(frame.as_ref(), current)?;
            for i in 0..(n.count as usize).min(16) {
                let child_depth = depth + 1;
                visit_child_edge(
                    frame,
                    Victim {
                        parent_slot: current,
                        kind: VictimEdgeKind::Inner(NodeType::Node16),
                        byte: n.keys[i],
                        victim_slot: n.children[i] as u16,
                        via_header_root: false,
                    },
                    boundary_quality_for_byte(n.keys[i]),
                    child_depth,
                    best,
                )?;
            }
        }
        NodeType::Node48 => {
            let n = read_node48(frame.as_ref(), current)?;
            let mut b = 0usize;
            while let Some(next_b) = simd::find_next_nonzero_byte(&n.index, b) {
                b = next_b + 1;
                let idx = n.index[next_b];
                let ci = idx as usize - 1;
                if ci >= 48 {
                    return Err(Error::node_corrupt(
                        "collect_victim_candidates: Node48 index out of range",
                    ));
                }
                let child_depth = depth + 1;
                visit_child_edge(
                    frame,
                    Victim {
                        parent_slot: current,
                        kind: VictimEdgeKind::Inner(NodeType::Node48),
                        byte: next_b as u8,
                        victim_slot: n.children[ci] as u16,
                        via_header_root: false,
                    },
                    boundary_quality_for_byte(next_b as u8),
                    child_depth,
                    best,
                )?;
            }
        }
        NodeType::Node256 => {
            let n = read_node256(frame.as_ref(), current)?;
            let mut b = 0usize;
            while let Some(next_b) = simd::find_next_nonzero_u32(&n.children, b) {
                b = next_b + 1;
                let child = n.children[next_b];
                let child_depth = depth + 1;
                visit_child_edge(
                    frame,
                    Victim {
                        parent_slot: current,
                        kind: VictimEdgeKind::Inner(NodeType::Node256),
                        byte: next_b as u8,
                        victim_slot: child as u16,
                        via_header_root: false,
                    },
                    boundary_quality_for_byte(next_b as u8),
                    child_depth,
                    best,
                )?;
            }
        }
        NodeType::Prefix => {
            let p = read_prefix(frame.as_ref(), current)?;
            let plen = p.prefix_len as usize;
            let prefix = &p.bytes[..plen.min(p.bytes.len())];
            let child_depth = depth + plen;
            visit_child_edge(
                frame,
                Victim {
                    parent_slot: current,
                    kind: VictimEdgeKind::Prefix,
                    byte: 0,
                    victim_slot: p.child as u16,
                    via_header_root: false,
                },
                boundary_quality_for_prefix(prefix),
                child_depth,
                best,
            )?;
        }
        NodeType::Leaf | NodeType::EmptyRoot | NodeType::Blob => {}
        NodeType::Invalid => {
            return Err(Error::node_corrupt("collect_victim_candidates: Invalid"));
        }
    }
    Ok(())
}

fn visit_child_edge(
    frame: &BlobFrame<'_>,
    victim: Victim,
    boundary: BoundaryQuality,
    boundary_depth: usize,
    best: &mut Option<VictimCandidate>,
) -> Result<()> {
    let child_ntype = ntype_of(frame.as_ref(), victim.victim_slot)?;
    match child_ntype {
        NodeType::Invalid => {
            return Err(Error::node_corrupt("visit_child_edge: Invalid child"));
        }
        NodeType::Blob => return Ok(()),
        _ => {}
    }

    let footprint = subtree_footprint(frame, victim.victim_slot)?;
    let candidate = VictimCandidate {
        victim,
        footprint,
        boundary,
        boundary_depth,
    };
    if best
        .as_ref()
        .is_none_or(|current| candidate_is_better(candidate, *current))
    {
        *best = Some(candidate);
    }
    if footprint.bytes > spillover_target_child_bytes() {
        collect_victim_candidates(frame, victim.victim_slot, boundary_depth, best)?;
    }
    Ok(())
}

fn boundary_quality_for_byte(byte: u8) -> BoundaryQuality {
    if byte == b'/' {
        BoundaryQuality::PathComponent
    } else {
        BoundaryQuality::Arbitrary
    }
}

fn boundary_quality_for_prefix(prefix: &[u8]) -> BoundaryQuality {
    prefix
        .last()
        .copied()
        .map_or(BoundaryQuality::Arbitrary, boundary_quality_for_byte)
}

fn candidate_is_better(candidate: VictimCandidate, current: VictimCandidate) -> bool {
    let c = candidate.footprint;
    let b = current.footprint;
    let target = spillover_target_child_bytes();
    let min = spillover_min_child_bytes();
    let c_in_band = c.bytes >= min && c.bytes <= target;
    let b_in_band = b.bytes >= min && b.bytes <= target;
    if c_in_band != b_in_band {
        return c_in_band;
    }
    if c_in_band {
        return candidate_tie_in_band(
            candidate,
            current,
            c.bytes.abs_diff(target),
            b.bytes.abs_diff(target),
        );
    }

    let c_below = c.bytes < min;
    let b_below = b.bytes < min;
    if c_below && b_below {
        return candidate_tie(candidate, current, b.bytes, c.bytes);
    }

    let c_over = c.bytes > target;
    let b_over = b.bytes > target;
    if c_over && b_over {
        return candidate_tie(candidate, current, c.bytes, b.bytes);
    }

    candidate_tie(
        candidate,
        current,
        c.bytes.abs_diff(target),
        b.bytes.abs_diff(target),
    )
}

fn candidate_tie_in_band(
    candidate: VictimCandidate,
    current: VictimCandidate,
    candidate_score: u32,
    current_score: u32,
) -> bool {
    if candidate.boundary != current.boundary {
        return candidate.boundary == BoundaryQuality::PathComponent;
    }
    candidate_tie(candidate, current, candidate_score, current_score)
}

fn candidate_tie(
    candidate: VictimCandidate,
    current: VictimCandidate,
    candidate_score: u32,
    current_score: u32,
) -> bool {
    if candidate_score != current_score {
        return candidate_score < current_score;
    }
    if candidate.boundary != current.boundary {
        return candidate.boundary == BoundaryQuality::PathComponent;
    }
    if candidate.boundary_depth != current.boundary_depth {
        return candidate.boundary_depth < current.boundary_depth;
    }
    candidate.footprint.nodes > current.footprint.nodes
}

/// Recursively free every slot of the subtree rooted at `root` in
/// `frame`. Used by spillover to reclaim source-side slot entries
/// after `make_blob_from_node` has copied them out.
pub(super) fn free_subtree(frame: &mut BlobFrame<'_>, root: u16) -> Result<()> {
    let ntype = ntype_of(frame.as_ref(), root)?;
    // Snapshot the body bytes before mutating the slot table so the
    // following `frame.free_node` calls can't invalidate them.
    let body_copy = frame
        .body_of_slot(root)
        .ok_or(Error::node_corrupt("free_subtree: body resolution failed"))?
        .to_vec();

    match ntype {
        NodeType::Invalid => {
            return Err(Error::node_corrupt("free_subtree: Invalid in source"));
        }
        NodeType::Leaf | NodeType::EmptyRoot | NodeType::Blob => {}
        NodeType::Prefix => {
            let p = cast::<Prefix>(&body_copy);
            free_subtree(frame, p.child as u16)?;
        }
        NodeType::Node4 => {
            let n = cast::<Node4>(&body_copy);
            for i in 0..(n.count as usize).min(4) {
                free_subtree(frame, n.children[i] as u16)?;
            }
        }
        NodeType::Node16 => {
            let n = cast::<Node16>(&body_copy);
            for i in 0..(n.count as usize).min(16) {
                free_subtree(frame, n.children[i] as u16)?;
            }
        }
        NodeType::Node48 => {
            let n = cast::<Node48>(&body_copy);
            let mut i = 0usize;
            while let Some(next_i) = simd::find_next_nonzero_u32(&n.children, i) {
                i = next_i + 1;
                free_subtree(frame, n.children[next_i] as u16)?;
            }
        }
        NodeType::Node256 => {
            let n = cast::<Node256>(&body_copy);
            let mut i = 0usize;
            while let Some(next_i) = simd::find_next_nonzero_u32(&n.children, i) {
                i = next_i + 1;
                free_subtree(frame, n.children[next_i] as u16)?;
            }
        }
    }

    frame.free_node(root)?;
    Ok(())
}

/// Produce a fresh 128-bit blob GUID with cross-process,
/// cross-restart uniqueness — UUIDv7-ish layout:
///
/// - **bytes 0..8** — `nanos_since_epoch` big-endian: time-orders
///   GUIDs for debug-friendly manifest dumps.
/// - **bytes 8..12** — per-process atomic counter big-endian:
///   resolves ties when many GUIDs are minted in the same
///   nanosecond.
/// - **bytes 12..15** — three random bytes from the OS entropy
///   source (`getrandom` on Linux, `getentropy` on the BSDs).
///   Closes the cross-process collision class "process A
///   crashes, process B starts on the same machine, OS reuses
///   pid, counter resets to 1 → identical GUID; new spillover
///   overwrites the crashed process's orphan blob in store".
/// - **byte 15** — magic tag `0xD4` so a fresh GUID can never
///   collide with `ROOT_BLOB_GUID = [0; 16]`.
///
/// Time-based prefix doesn't compromise privacy here: the GUID
/// lives inside an internal `manifest.bin` and never escapes the
/// process.
pub(crate) fn fresh_blob_guid() -> BlobGuid {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(1);

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed) as u32;

    let mut tail = [0u8; 3];
    if !fill_os_entropy(&mut tail) {
        // Fallback: derive from nanos + counter via a 64-bit
        // mixer. Deterministic-but-non-colliding even under
        // sandbox restrictions that block `getrandom`/
        // `getentropy`. Time prefix still dominates uniqueness
        // across restarts, so this only ever weakens the
        // intra-tick tiebreaker.
        let m = nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ u64::from(c);
        tail[0] = (m >> 16) as u8;
        tail[1] = (m >> 24) as u8;
        tail[2] = (m >> 32) as u8;
    }

    let mut g = [0u8; 16];
    g[0..8].copy_from_slice(&nanos.to_be_bytes());
    g[8..12].copy_from_slice(&c.to_be_bytes());
    g[12] = tail[0];
    g[13] = tail[1];
    g[14] = tail[2];
    g[15] = 0xD4; // tag — see fn doc
    g
}

/// Best-effort OS entropy read. Returns `true` on full fill.
///
/// holt is Unix-only (see project memory). Linux uses
/// `getrandom(2)`; the BSD family (macOS, FreeBSD, OpenBSD,
/// NetBSD) uses `getentropy(2)`. Both syscalls are blocking and
/// return cryptographically-strong bytes; we don't need crypto
/// strength here, only "different between processes / restarts".
fn fill_os_entropy(buf: &mut [u8]) -> bool {
    #[cfg(target_os = "linux")]
    unsafe {
        let r = libc::getrandom(buf.as_mut_ptr().cast(), buf.len(), 0);
        r >= 0 && (r as usize) == buf.len()
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    unsafe {
        // `getentropy` max is 256 bytes per call; our 3-byte read
        // is comfortably under.
        libc::getentropy(buf.as_mut_ptr().cast(), buf.len()) == 0
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    )))]
    {
        let _ = buf;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::super::insert::insert;
    use super::*;
    use crate::layout::PAGE_SIZE;
    use crate::store::BlobFrame;

    fn candidate(bytes: u32, nodes: u32) -> VictimCandidate {
        candidate_with_boundary(bytes, nodes, BoundaryQuality::Arbitrary, 16)
    }

    fn path_candidate(bytes: u32, nodes: u32) -> VictimCandidate {
        candidate_with_boundary(bytes, nodes, BoundaryQuality::PathComponent, 16)
    }

    fn candidate_with_boundary(
        bytes: u32,
        nodes: u32,
        boundary: BoundaryQuality,
        boundary_depth: usize,
    ) -> VictimCandidate {
        VictimCandidate {
            victim: Victim {
                parent_slot: 0,
                kind: VictimEdgeKind::Prefix,
                byte: 0,
                victim_slot: 0,
                via_header_root: false,
            },
            footprint: SubtreeFootprint { nodes, bytes },
            boundary,
            boundary_depth,
        }
    }

    #[test]
    fn spillover_scoring_prefers_target_band_over_largest() {
        let target = spillover_target_child_bytes();
        assert!(candidate_is_better(
            candidate(target - 1024, 10),
            candidate(target + 80_000, 100),
        ));
        assert!(!candidate_is_better(
            candidate(target + 80_000, 100),
            candidate(target - 1024, 10),
        ));
    }

    #[test]
    fn spillover_scoring_avoids_tiny_and_overfull_children() {
        let min = spillover_min_child_bytes();
        let target = spillover_target_child_bytes();

        assert!(candidate_is_better(
            candidate(min - 1024, 20),
            candidate(min / 2, 100),
        ));
        assert!(candidate_is_better(
            candidate(target + 1024, 20),
            candidate(target + 90_000, 100),
        ));
    }

    #[test]
    fn spillover_scoring_uses_node_count_only_as_tie_breaker() {
        let target = spillover_target_child_bytes();
        assert!(candidate_is_better(
            candidate(target - 4096, 20),
            candidate(target - 4096, 10),
        ));
    }

    #[test]
    fn spillover_scoring_prefers_path_boundary_within_target_band() {
        let target = spillover_target_child_bytes();
        assert!(candidate_is_better(
            path_candidate(target - 32_000, 10),
            candidate(target - 1024, 100),
        ));
    }

    #[test]
    fn spillover_scoring_keeps_fill_band_before_path_boundary() {
        let min = spillover_min_child_bytes();
        let target = spillover_target_child_bytes();
        assert!(candidate_is_better(
            candidate(target - 1024, 10),
            path_candidate(min - 1024, 100),
        ));
    }

    fn put(frame: &mut BlobFrame<'_>, key: &[u8], value: &[u8], seq: u64) {
        let root = frame.header().root_slot;
        insert(frame, root, key, value, seq).unwrap();
    }

    #[test]
    fn victim_search_descends_into_overfull_path_branch() {
        let mut buf = vec![0u8; PAGE_SIZE as usize];
        BlobFrame::init(&mut buf, [0x31; 16]).unwrap();
        let mut frame = BlobFrame::wrap(&mut buf);
        let value = vec![0x5A; 1024];

        let mut seq = 1u64;
        for i in 0..240u32 {
            let key = format!("a/x/file-{i:06}").into_bytes();
            put(&mut frame, &key, &value, seq);
            seq += 1;
        }
        for i in 0..120u32 {
            let key = format!("a/y/file-{i:06}").into_bytes();
            put(&mut frame, &key, &value, seq);
            seq += 1;
        }
        put(&mut frame, b"b/tiny", b"v", seq);

        let victim = pick_victim_subtree(&frame, frame.header().root_slot).unwrap();
        let footprint = subtree_footprint(&frame, victim.victim_slot).unwrap();

        assert!(
            footprint.bytes >= spillover_min_child_bytes(),
            "victim too small: {footprint:?}",
        );
        assert!(
            footprint.bytes <= spillover_target_child_bytes(),
            "victim should be a nested in-band branch, not the overfull direct branch: {footprint:?}",
        );
        assert_eq!(victim.byte, b'x');
    }
}
