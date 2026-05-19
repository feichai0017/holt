//! Tree-wide merge pass — walks a parent blob's structure looking
//! for `BlobNode` crossings whose child blob is small enough to
//! fold back inline, and folds them via [`super::merge_blob`].
//!
//! Holt's v0.1 trigger is [`crate::api::Tree::compact`]: after the
//! per-blob `compact_blob` reduces each blob to its live core,
//! this walker scans for mergeable BlobNode children and inlines
//! them — a typical churn workload (lots of erases) leaves child
//! blobs with little data, which this pass collapses back into a
//! single parent so the BFS shrinks back toward one root blob.

use crate::api::errors::{Error, Result};
use crate::layout::{BlobNode, NodeType, BLOB_MAX_INLINE};
use crate::store::{BlobFrame, BufferManager};

use super::cast;
use super::migrate::{is_mergeable, merge_blob};
use super::readers::{ntype_of, read_node16, read_node256, read_node4, read_node48, read_prefix};
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
pub fn try_merge_children(
    bm: &BufferManager,
    parent_frame: &mut BlobFrame<'_>,
) -> Result<MergeStats> {
    let mut stats = MergeStats::default();
    let root = parent_frame.header().root_slot;
    let new_root = try_merge_subtree(bm, parent_frame, root, &mut stats)?;
    if new_root != root {
        parent_frame.header_mut().root_slot = new_root;
    }
    Ok(stats)
}

fn try_merge_subtree(
    bm: &BufferManager,
    frame: &mut BlobFrame<'_>,
    slot: u16,
    stats: &mut MergeStats,
) -> Result<u16> {
    let ntype = ntype_of(frame.as_ref(), slot)?;
    match ntype {
        NodeType::Invalid => Err(Error::NodeCorrupt {
            context: "try_merge_subtree: hit NodeType::Invalid",
        }),
        NodeType::EmptyRoot | NodeType::Leaf => Ok(slot),
        NodeType::Prefix => merge_under_prefix(bm, frame, slot, stats),
        NodeType::Node4 | NodeType::Node16 | NodeType::Node48 | NodeType::Node256 => {
            merge_under_inner(bm, frame, slot, ntype, stats)
        }
        NodeType::Blob => merge_at_blob_node(bm, frame, slot, stats),
    }
}

fn merge_under_prefix(
    bm: &BufferManager,
    frame: &mut BlobFrame<'_>,
    pfx_slot: u16,
    stats: &mut MergeStats,
) -> Result<u16> {
    let p = read_prefix(frame.as_ref(), pfx_slot)?;
    let child_slot = p.child as u16;
    let new_child = try_merge_subtree(bm, frame, child_slot, stats)?;
    if new_child != child_slot {
        set_prefix_child(frame, pfx_slot, u32::from(new_child))?;
    }
    Ok(pfx_slot)
}

#[allow(clippy::too_many_lines)] // one match over 4 inner-node types
fn merge_under_inner(
    bm: &BufferManager,
    frame: &mut BlobFrame<'_>,
    inner_slot: u16,
    ntype: NodeType,
    stats: &mut MergeStats,
) -> Result<u16> {
    // Snapshot child (byte, slot) pairs once — the inner-node body
    // stays at a fixed slot through the walk, so reading once + then
    // mutating via `inner_update_child` is safe.
    let pairs: Vec<(u8, u16)> = match ntype {
        NodeType::Node4 => {
            let n = read_node4(frame.as_ref(), inner_slot)?;
            let count = (n.count as usize).min(4);
            (0..count)
                .map(|i| (n.keys[i], n.children[i] as u16))
                .collect()
        }
        NodeType::Node16 => {
            let n = read_node16(frame.as_ref(), inner_slot)?;
            let count = (n.count as usize).min(16);
            (0..count)
                .map(|i| (n.keys[i], n.children[i] as u16))
                .collect()
        }
        NodeType::Node48 => {
            let n = read_node48(frame.as_ref(), inner_slot)?;
            let mut out = Vec::with_capacity(48);
            for b in 0..256usize {
                let idx = n.index[b];
                if idx == 0 {
                    continue;
                }
                let ci = idx as usize - 1;
                if ci >= 48 {
                    return Err(Error::NodeCorrupt {
                        context: "merge_under_inner: Node48 index out of range",
                    });
                }
                out.push((b as u8, n.children[ci] as u16));
            }
            out
        }
        NodeType::Node256 => {
            let n = read_node256(frame.as_ref(), inner_slot)?;
            let mut out = Vec::with_capacity(64);
            for (b, &c) in n.children.iter().enumerate() {
                if c != 0 {
                    out.push((b as u8, c as u16));
                }
            }
            out
        }
        _ => {
            return Err(Error::NodeCorrupt {
                context: "merge_under_inner: called on a non-inner NodeType",
            })
        }
    };

    for (byte, child_slot) in pairs {
        let new_child = try_merge_subtree(bm, frame, child_slot, stats)?;
        if new_child != child_slot {
            inner_update_child(frame, inner_slot, ntype, byte, u32::from(new_child))?;
        }
    }
    Ok(inner_slot)
}

fn merge_at_blob_node(
    bm: &BufferManager,
    frame: &mut BlobFrame<'_>,
    bn_slot: u16,
    stats: &mut MergeStats,
) -> Result<u16> {
    // Defensive: confirm the body really is a BlobNode + check its
    // prefix_len fits. If `is_mergeable` returns false, the BlobNode
    // stays put.
    {
        let body = frame.body_of_slot(bn_slot).ok_or(Error::NodeCorrupt {
            context: "merge_at_blob_node: body resolution failed",
        })?;
        let bn = cast::<BlobNode>(body);
        if (bn.prefix_len as usize) > BLOB_MAX_INLINE {
            return Err(Error::NodeCorrupt {
                context: "merge_at_blob_node: prefix_len exceeds inline buffer",
            });
        }
    }
    stats.inspected += 1;
    if !is_mergeable(bm, frame, bn_slot)? {
        return Ok(bn_slot);
    }
    let new_slot = merge_blob(bm, frame, bn_slot)?;
    stats.merged += 1;
    Ok(new_slot)
}
