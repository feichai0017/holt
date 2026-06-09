//! Parent-local merge pass — walks one parent blob's structure
//! looking for `BlobNode` crossings whose child blob is small
//! enough to fold back inline, and folds them via
//! [`super::merge_blob`].
//!
//! Triggered by candidate-driven maintenance in
//! [`crate::Tree::compact`] and checkpoint auto-merge. A
//! typical churn workload (lots of erases) leaves child blobs with
//! little data, and each queued parent can collapse its direct
//! children without forcing a whole-tree merge scan.

use crate::api::errors::{is_blob_store_not_found, Error, Result};
use crate::layout::{BlobNode, NodeType, BLOB_MAX_INLINE};
use crate::store::{decode_child_off, encode_child_off, BlobFrame, BufferManager};

use super::cast;
use super::migrate::{is_mergeable, merge_blob};
use super::readers::{
    child_offset, ntype_of, read_node16, read_node256, read_node4, read_node48, read_prefix,
};
use super::writers::{inner_update_child, set_prefix_child};

/// Counters for [`try_merge_children`]'s single pass.
#[derive(Debug, Clone, Copy, Default)]
pub struct MergeStats {
    /// Number of child blobs successfully folded into `parent_frame`
    /// during this pass.
    pub merged: u32,
    /// Number of `BlobNode` crossings inspected (mergeable or not).
    pub inspected: u32,
}

/// Walk `parent_frame`'s tree from its root and fold every
/// mergeable `BlobNode` child back into the parent.
///
/// Recurses through `Prefix` / `Node{4,16,48,256}` arms, rewiring
/// the immediate parent's child pointer (Prefix or inner-node) when
/// a `BlobNode` merge changes the slot. Updates
/// `parent_frame.header().root_slot` if the root itself is a
/// BlobNode that merged.
///
/// Each `BlobNode` is inspected at most once; if `is_mergeable`
/// returns `false`, the crossing is left as is. The pass is
/// single-shot — children merged in this call won't be recursively
/// re-checked for their own mergeable descendants. (`is_mergeable`
/// rejects any child with its own crossings via `num_ext_blobs`,
/// so nested merges are deferred to a future pass.)
/// `seq` is forwarded to [`merge_blob`] so the deferred-delete
/// entry it generates carries the correct WAL stamp. Internal
/// callers (compact / checkpoint round) pass
/// [`crate::store::STRUCTURAL_SEQ`].
pub fn try_merge_children(
    bm: &BufferManager,
    parent_frame: &mut BlobFrame<'_>,
    seq: u64,
) -> Result<MergeStats> {
    let mut stats = MergeStats::default();
    let root_off = decode_child_off(parent_frame.header().root_slot);
    let new_root = try_merge_subtree(bm, parent_frame, root_off, &mut stats, seq)?;
    if new_root != root_off {
        parent_frame.header_mut().root_slot = encode_child_off(new_root);
    }
    Ok(stats)
}

fn try_merge_subtree(
    bm: &BufferManager,
    frame: &mut BlobFrame<'_>,
    off: u32,
    stats: &mut MergeStats,
    seq: u64,
) -> Result<u32> {
    let ntype = ntype_of(frame.as_ref(), off)?;
    match ntype {
        NodeType::Invalid => Err(Error::node_corrupt(
            "try_merge_subtree: hit NodeType::Invalid",
        )),
        NodeType::EmptyRoot | NodeType::Leaf => Ok(off),
        NodeType::Prefix => merge_under_prefix(bm, frame, off, stats, seq),
        NodeType::Node4 | NodeType::Node16 | NodeType::Node48 | NodeType::Node256 => {
            merge_under_inner(bm, frame, off, ntype, stats, seq)
        }
        NodeType::Blob => merge_at_blob_node(bm, frame, off, stats, seq),
    }
}

fn merge_under_prefix(
    bm: &BufferManager,
    frame: &mut BlobFrame<'_>,
    pfx_off: u32,
    stats: &mut MergeStats,
    seq: u64,
) -> Result<u32> {
    let p = read_prefix(frame.as_ref(), pfx_off)?;
    let child_off = child_offset(p.child as u16);
    let new_child = try_merge_subtree(bm, frame, child_off, stats, seq)?;
    if new_child != child_off {
        set_prefix_child(frame, pfx_off, new_child)?;
    }
    Ok(pfx_off)
}

#[allow(clippy::too_many_lines)] // one match over 4 inner-node types
fn merge_under_inner(
    bm: &BufferManager,
    frame: &mut BlobFrame<'_>,
    inner_off: u32,
    ntype: NodeType,
    stats: &mut MergeStats,
    seq: u64,
) -> Result<u32> {
    // Snapshot child (byte, child_off) pairs once — the inner-node body
    // stays at a fixed offset through the walk, so reading once + then
    // mutating via `inner_update_child` is safe.
    let pairs: Vec<(u8, u32)> = match ntype {
        NodeType::Node4 => {
            let n = read_node4(frame.as_ref(), inner_off)?;
            let count = (n.count as usize).min(4);
            (0..count)
                .map(|i| (n.keys[i], child_offset(n.children[i])))
                .collect()
        }
        NodeType::Node16 => {
            let n = read_node16(frame.as_ref(), inner_off)?;
            let count = (n.count as usize).min(16);
            (0..count)
                .map(|i| (n.keys[i], child_offset(n.children[i])))
                .collect()
        }
        NodeType::Node48 => {
            let n = read_node48(frame.as_ref(), inner_off)?;
            let mut out = Vec::with_capacity(48);
            for b in 0..256usize {
                let idx = n.index[b];
                if idx == 0 {
                    continue;
                }
                let ci = idx as usize - 1;
                if ci >= 48 {
                    return Err(Error::node_corrupt(
                        "merge_under_inner: Node48 index out of range",
                    ));
                }
                out.push((b as u8, child_offset(n.children[ci])));
            }
            out
        }
        NodeType::Node256 => {
            let n = read_node256(frame.as_ref(), inner_off)?;
            let mut out = Vec::with_capacity(64);
            for (b, &c) in n.children.iter().enumerate() {
                if c != 0 {
                    out.push((b as u8, child_offset(c)));
                }
            }
            out
        }
        _ => {
            return Err(Error::node_corrupt(
                "merge_under_inner: called on a non-inner NodeType",
            ))
        }
    };

    for (byte, child_off) in pairs {
        let new_child = try_merge_subtree(bm, frame, child_off, stats, seq)?;
        if new_child != child_off {
            inner_update_child(frame, inner_off, ntype, byte, new_child)?;
        }
    }
    Ok(inner_off)
}

fn merge_at_blob_node(
    bm: &BufferManager,
    frame: &mut BlobFrame<'_>,
    bn_off: u32,
    stats: &mut MergeStats,
    seq: u64,
) -> Result<u32> {
    // Defensive: confirm the body really is a BlobNode + check its
    // prefix_len fits. If `is_mergeable` returns false, the BlobNode
    // stays put.
    {
        let body = frame.body_at_offset(bn_off).ok_or(Error::node_corrupt(
            "merge_at_blob_node: body resolution failed",
        ))?;
        let bn = cast::<BlobNode>(body);
        if (bn.prefix_len as usize) > BLOB_MAX_INLINE {
            return Err(Error::node_corrupt(
                "merge_at_blob_node: prefix_len exceeds inline buffer",
            ));
        }
    }
    stats.inspected += 1;
    let mergeable = match is_mergeable(bm, frame, bn_off) {
        Ok(mergeable) => mergeable,
        Err(e) if is_blob_store_not_found(&e) => return Ok(bn_off),
        Err(e) => return Err(e),
    };
    if !mergeable {
        return Ok(bn_off);
    }
    let new_off = match merge_blob(bm, frame, bn_off, seq) {
        Ok(new_off) => new_off,
        Err(e) if is_blob_store_not_found(&e) => return Ok(bn_off),
        Err(e) => return Err(e),
    };
    stats.merged += 1;
    Ok(new_off)
}
