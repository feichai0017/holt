//! Walker types — public outcomes + internal signals.

use crate::layout::{BlobGuid, NodeType};
use crate::store::backend::AlignedBlobBuf;

// ---------- public types ----------

/// Outcome of a [`super::lookup`] descent.
#[derive(Debug)]
pub enum LookupResult<'a> {
    /// Match found — borrowed view of the value bytes.
    Found(&'a [u8]),
    /// No leaf in the tree matches `key`.
    NotFound,
    /// Descent reached a [`NodeType::Blob`] crossing. The caller
    /// (typically `Tree::get`) must load the child blob by its GUID
    /// and call [`super::lookup_at`] on the child frame starting at
    /// `child_slot` with `depth = child_depth`.
    Crossing(BlobNodeCrossing),
}

/// Where a single-blob walker descent stopped at a BlobNode.
#[derive(Debug, Clone, Copy)]
pub struct BlobNodeCrossing {
    /// GUID of the blob to walk into next.
    pub child_guid: BlobGuid,
    /// Slot inside the child blob where the walk resumes.
    pub child_slot: u16,
    /// `depth` to pass to the next [`super::lookup_at`] call (the
    /// parent blob's depth plus the BlobNode's inline prefix length).
    pub child_depth: usize,
}

/// Outcome of an [`super::insert::insert`] / [`super::insert::insert_multi`].
#[derive(Debug)]
pub struct InsertOutcome {
    /// The slot the tree's `root_slot` should now point at — may
    /// differ from the caller's input when a split promotes a new
    /// node above the existing root. Only consumed by single-blob
    /// test drivers; the multi-blob path updates `root_slot` in
    /// place under the BM's write guard.
    #[cfg_attr(not(test), allow(dead_code))]
    pub new_root_slot: u16,
    /// If the key already existed, the value it carried before.
    pub previous: Option<Vec<u8>>,
}

/// Outcome of an [`super::erase::erase`] / [`super::erase::erase_multi`].
#[derive(Debug)]
pub struct EraseOutcome {
    /// The slot the tree's `root_slot` should now point at — may
    /// differ from the caller's input when the root collapses
    /// (e.g. last leaf removed → fresh EmptyRoot sentinel; Node4
    /// shrinks to its lone child and that child is promoted). Only
    /// consumed by single-blob test drivers; the multi-blob path
    /// updates `root_slot` in place under the BM's write guard.
    #[cfg_attr(not(test), allow(dead_code))]
    pub new_root_slot: u16,
    /// If a matching leaf was removed, the value it carried.
    /// `None` means "key was not in the tree" — the call is then
    /// a no-op.
    pub previous: Option<Vec<u8>>,
}

/// Outcome of [`super::make_blob_from_node`] — a freshly-built blob
/// image holding a clone of the source subtree.
#[derive(Debug)]
pub struct MakeBlobOutcome {
    /// New blob's full 512 KB image — write this to the backend
    /// under `new_guid`.
    pub buf: AlignedBlobBuf,
    /// Slot inside the new blob where the cloned subtree's root
    /// lives. Equals `buf`'s `header.root_slot`.
    pub entry_slot: u16,
}

// ---------- internal types (pub(super) for sibling submodules) ----------

/// Return-value carried up an `insert_at` recursion.
#[derive(Debug)]
pub(super) struct InsertReturn {
    /// What slot the parent should now point at — may be the same
    /// as the input slot or may be a freshly-allocated promotion.
    pub(super) slot_after: u16,
    /// Prior value if the key already existed.
    pub(super) previous: Option<Vec<u8>>,
}

/// What an erase descent tells its parent to do.
#[derive(Debug)]
pub(super) enum EraseSignal {
    /// Slot stays as-is — nothing to rewire above.
    Unchanged,
    /// The subtree at this slot disappeared entirely. Parent should
    /// drop the corresponding child entry and (if it now has 0
    /// remaining children) free itself in turn.
    SubtreeGone,
    /// The subtree shrank to a single node. Parent should rewrite
    /// its child pointer to the carried slot.
    Replaced(u16),
}

#[derive(Debug)]
pub(super) struct EraseReturn {
    pub(super) signal: EraseSignal,
    pub(super) previous: Option<Vec<u8>>,
}

/// What kind of edge the parent of a victim subtree has.
#[derive(Debug, Clone, Copy)]
pub(super) enum VictimEdgeKind {
    Prefix,
    Inner(NodeType),
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Victim {
    /// Slot of the parent node that points at the victim.
    pub(super) parent_slot: u16,
    /// What kind of edge it is.
    pub(super) kind: VictimEdgeKind,
    /// The byte routing to the victim in the parent (irrelevant
    /// for `Prefix` edges).
    pub(super) byte: u8,
    /// Slot of the victim subtree's root.
    pub(super) victim_slot: u16,
    /// `true` iff the victim is reached via `header.root_slot`
    /// rather than via a regular parent node — used to dispatch
    /// the parent rewrite path.
    pub(super) via_header_root: bool,
}
