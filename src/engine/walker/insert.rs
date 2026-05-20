//! Insert path — `insert` / `insert_multi` + recursive
//! `insert_at` dispatch + per-NodeType arms.

use crate::api::errors::{Error, Result};
use crate::layout::{leaf_extent_size, BlobNode, Leaf, NodeType, BLOB_MAX_INLINE};
use std::sync::Arc;

use crate::store::buffer_manager::BlobWriteGuard;
use crate::store::{BlobFrame, BlobFrameRef, BufferManager, CachedBlob};

use super::cast;
use super::lookup::lookup_at;
use super::migrate::blob_needs_compaction;
use super::readers::{ntype_of, read_leaf_key_ref, read_prefix};
use super::spillover::{compact_blob, spillover_blob};
use super::types::{InsertOutcome, InsertReturn, LookupResult};
use super::writers::{
    inner_add_child, inner_find_child, inner_update_child, set_prefix_child, write_leaf,
    write_node4_with, write_prefix_chain, write_struct_to_slot,
};
use super::SearchKey;
use super::MAX_SPILLOVER_ATTEMPTS;

// ---------- public entry points ----------

/// Single-blob insert. Surfaces [`Error::NotYetImplemented`] if
/// the descent has to follow a matching [`NodeType::Blob`]
/// crossing — callers that need cross-blob support should use
/// [`insert_multi`]. Divergent BlobNode inline prefixes can still
/// be split locally in the current blob.
///
/// `seq` is the journal sequence number to stamp on the new leaf
/// (callers should pass a monotonically-increasing value). Updates
/// `header.root_slot` in place and returns the prior value if the
/// key already existed.
#[cfg(test)]
pub(super) fn insert(
    frame: &mut BlobFrame<'_>,
    root_slot: u16,
    key: &[u8],
    value: &[u8],
    seq: u64,
) -> Result<InsertOutcome> {
    let key = SearchKey::exact(key);
    if key.len() > u16::MAX as usize {
        return Err(Error::KeyTooLong { len: key.len() });
    }
    if value.len() > u16::MAX as usize {
        return Err(Error::ValueTooLong { len: value.len() });
    }
    // Single-blob `insert` is test-only today and always returns
    // the prior value — preserves the existing test surface.
    let r = insert_at(frame, root_slot, key, value, 0, seq, true)?;
    frame.header_mut().root_slot = r.slot_after;
    Ok(InsertOutcome {
        root_dirty: true,
        previous: r.previous,
    })
}

/// Multi-blob insert. Pins the root via the [`BufferManager`] and
/// walks across [`NodeType::Blob`] crossings, automatically
/// triggering `splitBlob` spillover when any blob hits
/// [`crate::store::AllocError::OutOfSpace`].
///
/// Child blobs encountered during descent are pinned in the same
/// BM cache and mutated in place. The walker tags every touched
/// child via `bm.mark_dirty(child_guid, seq)`; the actual
/// backend write is the checkpoint round's job (and only happens
/// after the WAL record for `seq` is durable — invariant W2D).
///
/// `wants_prev` controls whether the walker reads + clones the
/// existing leaf's value on a same-key update — set `true` for
/// [`crate::Tree::insert`] (returning API) and `false` for
/// [`crate::Tree::put`] (blind API). The blind path saves the
/// `value_size`-byte allocation + clone + `Option<Vec<u8>>`
/// plumbing per put; meaningful on path-shaped workloads where
/// the leaf value is the dominant per-op heap traffic.
pub fn insert_multi(
    bm: &BufferManager,
    root_pin: &Arc<CachedBlob>,
    key: SearchKey<'_>,
    value: &[u8],
    seq: u64,
    wants_prev: bool,
) -> Result<InsertOutcome> {
    if key.len() > u16::MAX as usize {
        return Err(Error::KeyTooLong { len: key.len() });
    }
    if value.len() > u16::MAX as usize {
        return Err(Error::ValueTooLong { len: value.len() });
    }

    let mut blob_hops = 0u64;
    let mut max_cross_blob_depth = 0usize;

    // Fast path for the large-tree steady state: the root blob is
    // often just a router to child blobs. Hold the root in shared
    // mode long enough to acquire the child write guard, then let
    // the normal lock-coupled writer mutate from that child down.
    // This preserves the parent->child edge-stability rule without
    // making every cross-blob put take the root's exclusive latch.
    {
        let root_read = root_pin.read();
        let frame = BlobFrameRef::wrap(root_read.as_slice());
        let root_slot = frame.header().root_slot;
        if let LookupResult::Crossing(crossing) = lookup_at(frame, root_slot, key, 0)? {
            let child_pin = bm.pin(crossing.child_guid)?;
            let child_guard = child_pin.write();
            drop(root_read);

            blob_hops = 1;
            let outcome = lock_coupled_insert_in_blob(
                bm,
                child_guard,
                crossing.child_guid,
                false,
                key,
                value,
                seq,
                wants_prev,
                crossing.child_depth,
                &mut blob_hops,
                &mut max_cross_blob_depth,
            );
            drop(child_pin);
            if outcome.is_ok() {
                bm.note_walker_blob_hops(blob_hops, max_cross_blob_depth);
            }
            return outcome;
        }
        drop(root_read);
    }

    // Root-local mutation fallback.
    let mut guard = root_pin.write();
    let root_guid = {
        let frame = guard.frame();
        frame.header().blob_guid
    };
    let outcome = lock_coupled_insert_in_blob(
        bm,
        guard,
        root_guid,
        true,
        key,
        value,
        seq,
        wants_prev,
        0,
        &mut blob_hops,
        &mut max_cross_blob_depth,
    );
    if outcome.is_ok() {
        bm.note_walker_blob_hops(blob_hops, max_cross_blob_depth);
    }
    outcome
}

#[derive(Debug, Clone, Copy)]
struct InsertBlobCrossing {
    child_guid: crate::layout::BlobGuid,
    child_depth: usize,
}

enum InsertStep {
    Done(InsertReturn),
    Crossing(InsertBlobCrossing),
}

#[allow(clippy::too_many_arguments)] // hot-path helper mirrors insert_at's call shape
fn lock_coupled_insert_in_blob(
    bm: &BufferManager,
    mut guard: BlobWriteGuard<'_>,
    current_guid: crate::layout::BlobGuid,
    is_top_blob: bool,
    key: SearchKey<'_>,
    value: &[u8],
    seq: u64,
    wants_prev: bool,
    depth: usize,
    blob_hops: &mut u64,
    max_cross_blob_depth: &mut usize,
) -> Result<InsertOutcome> {
    *blob_hops = blob_hops.saturating_add(1);
    *max_cross_blob_depth = (*max_cross_blob_depth).max(depth);
    let mut current_dirty = false;

    for _attempt in 0..MAX_SPILLOVER_ATTEMPTS {
        let r = {
            let mut frame = guard.frame();
            let root_slot = frame.header().root_slot;
            insert_at_step(
                &mut frame, root_slot, key, value, depth, seq, wants_prev, true,
            )
        };
        match r {
            Ok(InsertStep::Done(out)) => {
                let needs_compaction = {
                    let mut frame = guard.frame();
                    frame.header_mut().root_slot = out.slot_after;
                    blob_needs_compaction(frame.as_ref())
                };
                drop(guard);
                if needs_compaction {
                    bm.note_compaction_candidate(current_guid);
                }
                if !is_top_blob {
                    bm.mark_dirty(current_guid, seq);
                }

                return Ok(InsertOutcome {
                    root_dirty: is_top_blob,
                    previous: out.previous,
                });
            }
            Ok(InsertStep::Crossing(crossing)) => {
                let child_pin = bm.pin(crossing.child_guid)?;
                let child_guard = child_pin.write();
                drop(guard);

                let mut outcome = lock_coupled_insert_in_blob(
                    bm,
                    child_guard,
                    crossing.child_guid,
                    false,
                    key,
                    value,
                    seq,
                    wants_prev,
                    crossing.child_depth,
                    blob_hops,
                    max_cross_blob_depth,
                );
                drop(child_pin);

                if outcome.is_ok() && current_dirty && !is_top_blob {
                    bm.mark_dirty(current_guid, seq);
                }
                if let Ok(outcome) = &mut outcome {
                    outcome.root_dirty |= is_top_blob && current_dirty;
                }
                return outcome;
            }
            Err(Error::Alloc(crate::store::AllocError::OutOfSpace { .. })) => {
                {
                    let mut frame = guard.frame();
                    spillover_blob(bm, &mut frame, seq)
                        .map_err(|e| e.with_blob_guid(current_guid))?;
                }
                bm.note_merge_candidate(current_guid);
                bm.note_spillover();
                compact_blob(&mut guard).map_err(|e| e.with_blob_guid(current_guid))?;
                current_dirty = true;
            }
            Err(e) => return Err(e.with_blob_guid(current_guid)),
        }
    }

    Err(Error::NotYetImplemented(
        "lock_coupled_insert_in_blob: spillover retry loop exhausted",
    ))
}

// ---------- recursive dispatch ----------

#[cfg(test)]
#[allow(clippy::too_many_arguments)] // wants_prev threads through every arm
pub(super) fn insert_at(
    frame: &mut BlobFrame<'_>,
    slot: u16,
    key: SearchKey<'_>,
    value: &[u8],
    depth: usize,
    seq: u64,
    wants_prev: bool,
) -> Result<InsertReturn> {
    match insert_at_step(frame, slot, key, value, depth, seq, wants_prev, false)? {
        InsertStep::Done(r) => Ok(r),
        InsertStep::Crossing(_) => Err(Error::NotYetImplemented(
            "walker::insert_at: BlobNode crossing requires BufferManager — use insert_multi",
        )),
    }
}

#[allow(clippy::too_many_arguments)] // wants_prev threads through every arm
fn insert_at_step(
    frame: &mut BlobFrame<'_>,
    slot: u16,
    key: SearchKey<'_>,
    value: &[u8],
    depth: usize,
    seq: u64,
    wants_prev: bool,
    allow_crossing: bool,
) -> Result<InsertStep> {
    let ntype = ntype_of(frame.as_ref(), slot)?;
    match ntype {
        NodeType::Invalid => Err(Error::node_corrupt(
            "walker::insert_at: hit NodeType::Invalid",
        )),
        NodeType::EmptyRoot => {
            insert_into_empty_root(frame, slot, key, value, seq).map(InsertStep::Done)
        }
        NodeType::Leaf => {
            insert_into_leaf(frame, slot, key, value, depth, seq, wants_prev).map(InsertStep::Done)
        }
        NodeType::Prefix => insert_into_prefix_step(
            frame,
            slot,
            key,
            value,
            depth,
            seq,
            wants_prev,
            allow_crossing,
        ),
        NodeType::Node4 | NodeType::Node16 | NodeType::Node48 | NodeType::Node256 => {
            insert_into_inner_step(
                frame,
                slot,
                ntype,
                key,
                value,
                depth,
                seq,
                wants_prev,
                allow_crossing,
            )
        }
        NodeType::Blob => {
            blob_node_insert_step(frame, slot, key, value, depth, seq, allow_crossing)
        }
    }
}

fn blob_node_insert_step(
    frame: &mut BlobFrame<'_>,
    slot: u16,
    key: SearchKey<'_>,
    value: &[u8],
    depth: usize,
    seq: u64,
    allow_crossing: bool,
) -> Result<InsertStep> {
    let body = frame.body_of_slot(slot).ok_or(Error::node_corrupt(
        "blob_node_insert_step: BlobNode body resolution failed",
    ))?;
    let bn = *cast::<BlobNode>(body);
    let plen = bn.prefix_len as usize;
    if plen > BLOB_MAX_INLINE {
        return Err(Error::node_corrupt(
            "blob_node_insert_step: BlobNode prefix_len exceeds inline buffer",
        ));
    }
    let prefix = &bn.bytes[..plen];
    let common = key.common_prefix_with_slice(depth, prefix);

    if common == plen {
        if !allow_crossing {
            return Err(Error::NotYetImplemented(
                "walker::insert_at: BlobNode crossing requires BufferManager — use insert_multi",
            ));
        }
        return Ok(InsertStep::Crossing(InsertBlobCrossing {
            child_guid: bn.child_blob_guid,
            child_depth: depth + plen,
        }));
    }

    let Some(new_div_byte) = key.byte_at(depth + common) else {
        return Err(Error::NotYetImplemented(
            "blob_node_insert_step: key terminates inside BlobNode prefix",
        ));
    };
    let existing_div_byte = prefix[common];
    debug_assert_ne!(existing_div_byte, new_div_byte);

    // Keep the old BlobNode slot so parent pointers do not move.
    // The branch byte is consumed by the new Node4, so the BlobNode
    // only keeps the remaining inline tail before crossing to the
    // unchanged child blob.
    let existing_tail = &prefix[common + 1..];
    let new_leaf = write_leaf(frame, key, value, seq)?;
    let n4 = write_node4_with(
        frame,
        &[
            (existing_div_byte, u32::from(slot)),
            (new_div_byte, u32::from(new_leaf)),
        ],
    )?;
    let final_slot = if common == 0 {
        n4
    } else {
        write_prefix_chain(frame, &prefix[..common], n4)?
    };

    let adjusted = BlobNode::new(existing_tail, bn.child_blob_guid);
    write_struct_to_slot(frame, slot, &adjusted)?;

    Ok(InsertStep::Done(InsertReturn {
        slot_after: final_slot,
        previous: None,
    }))
}

fn insert_into_empty_root(
    frame: &mut BlobFrame<'_>,
    empty_slot: u16,
    key: SearchKey<'_>,
    value: &[u8],
    seq: u64,
) -> Result<InsertReturn> {
    let new_slot = write_leaf(frame, key, value, seq)?;
    frame.free_node(empty_slot)?;
    Ok(InsertReturn {
        slot_after: new_slot,
        previous: None,
    })
}

struct LeafSplitPlan {
    common_prefix: Vec<u8>,
    byte_existing: u8,
    byte_new: u8,
}

fn insert_into_leaf(
    frame: &mut BlobFrame<'_>,
    leaf_slot: u16,
    new_key: SearchKey<'_>,
    new_value: &[u8],
    depth: usize,
    seq: u64,
    wants_prev: bool,
) -> Result<InsertReturn> {
    enum LeafInsertPlan {
        SameKey(Leaf),
        Split(LeafSplitPlan),
    }

    // Always read the existing key (needed for both same-key
    // update and divergence-split paths), but keep it borrowed
    // from the blob. Only the split path materialises the shared
    // prefix bytes because subsequent writes mutate the frame.
    let plan = {
        let (existing_key, existing_leaf) = read_leaf_key_ref(frame.as_ref(), leaf_slot)?;
        if new_key.eq_slice(existing_key) {
            LeafInsertPlan::SameKey(existing_leaf)
        } else {
            let suffix_a = &existing_key[depth..];
            let common_len = new_key.common_prefix_with_slice(depth, suffix_a);

            if common_len == suffix_a.len() || common_len == new_key.remaining_len(depth) {
                return Err(Error::NotYetImplemented(
                    "walker::insert_into_leaf: one key is a strict prefix of the other",
                ));
            }

            LeafInsertPlan::Split(LeafSplitPlan {
                common_prefix: suffix_a[..common_len].to_vec(),
                byte_existing: suffix_a[common_len],
                byte_new: new_key
                    .byte_at(depth + common_len)
                    .expect("new key has divergence byte"),
            })
        }
    };

    let split = match plan {
        LeafInsertPlan::SameKey(existing_leaf) => {
            // Same-key update path (covers two semantic cases via the
            // same alloc machinery):
            //
            // 1. **Resurrect**: the existing leaf is tombstoned — the
            //    user just put the key back after deleting it. From
            //    the user's view this is a fresh insert (`previous`
            //    is `None`) and the blob's `tombstone_leaf_cnt` drops
            //    by one because the slot leaves the tombstone state.
            // 2. **Update**: the existing leaf is live — return the
            //    prior value and overwrite (in place when extents fit;
            //    fall back to alloc-fresh + free-old when the value
            //    grew past the existing extent).
            //
            // `Leaf::live` always pins `tombstone = 0` so both write
            // paths naturally clear the bit in the new leaf body.
            let was_tombstoned = existing_leaf.tombstone != 0;
            // Only materialise the prev value on the returning API
            // (`Tree::insert`). The blind `Tree::put` path skips the
            // `leaf_extent` walk + `.to_vec()` entirely.
            let prev = if wants_prev && !was_tombstoned {
                let (_k, v) = super::readers::leaf_extent(frame.as_ref(), &existing_leaf)?;
                Some(v.to_vec())
            } else {
                None
            };
            let key_off = existing_leaf.key_offset;
            let key_len_u32 = new_key.len() as u32;
            let old_extent_size =
                leaf_extent_size(key_len_u32, u32::from(existing_leaf.value_size));
            let new_extent_size = leaf_extent_size(key_len_u32, new_value.len() as u32);

            if new_extent_size <= old_extent_size {
                let value_offset = key_off + 2 + key_len_u32;
                let value_room = old_extent_size - 2 - key_len_u32;
                let region =
                    frame
                        .bytes_at_mut(value_offset, value_room)
                        .ok_or(Error::node_corrupt(
                            "insert_into_leaf: extent value range out of bounds",
                        ))?;
                region[..new_value.len()].copy_from_slice(new_value);
                for b in &mut region[new_value.len()..] {
                    *b = 0;
                }
                let new_leaf = Leaf::live(key_off, new_value.len() as u16, seq);
                write_struct_to_slot(frame, leaf_slot, &new_leaf)?;
                if was_tombstoned {
                    let h = frame.header_mut();
                    h.tombstone_leaf_cnt = h.tombstone_leaf_cnt.saturating_sub(1);
                }
                return Ok(InsertReturn {
                    slot_after: leaf_slot,
                    previous: prev,
                });
            }

            // Value grew past the existing extent — fall back to alloc-
            // fresh + free-old. The old extent bytes leak until
            // `compact_blob` reclaims; the old leaf slot returns to its
            // per-NodeType free list.
            let new_slot = write_leaf(frame, new_key, new_value, seq)?;
            frame.free_node(leaf_slot)?;
            if was_tombstoned {
                let h = frame.header_mut();
                h.tombstone_leaf_cnt = h.tombstone_leaf_cnt.saturating_sub(1);
            }
            return Ok(InsertReturn {
                slot_after: new_slot,
                previous: prev,
            });
        }
        LeafInsertPlan::Split(split) => split,
    };

    // Two different keys: split into [Prefix?] -> Node4 -> {old leaf, new leaf}.
    let final_slot = write_leaf_split(frame, leaf_slot, new_key, new_value, seq, &split)?;
    Ok(InsertReturn {
        slot_after: final_slot,
        previous: None,
    })
}

fn write_leaf_split(
    frame: &mut BlobFrame<'_>,
    leaf_slot: u16,
    new_key: SearchKey<'_>,
    new_value: &[u8],
    seq: u64,
    split: &LeafSplitPlan,
) -> Result<u16> {
    let new_leaf = write_leaf(frame, new_key, new_value, seq)?;
    let n4 = write_node4_with(
        frame,
        &[
            (split.byte_existing, u32::from(leaf_slot)),
            (split.byte_new, u32::from(new_leaf)),
        ],
    )?;

    let final_slot = if split.common_prefix.is_empty() {
        n4
    } else {
        write_prefix_chain(frame, &split.common_prefix, n4)?
    };

    Ok(final_slot)
}

#[allow(clippy::too_many_arguments)] // wants_prev added by API split
fn insert_into_prefix_step(
    frame: &mut BlobFrame<'_>,
    pfx_slot: u16,
    key: SearchKey<'_>,
    value: &[u8],
    depth: usize,
    seq: u64,
    wants_prev: bool,
    allow_crossing: bool,
) -> Result<InsertStep> {
    // `Prefix` is `Copy` and `read_prefix` returns it by value, so
    // `p` is owned on the stack. The inline prefix bytes live in
    // `p.bytes` — no need to allocate a `Vec` to keep them alive
    // across the `frame.*` mutations below (those don't borrow
    // from `p`). Previously this path called `p.bytes[..plen].to_vec()`
    // on every Prefix descent, which dominated put cost on path-
    // shaped workloads (objstore / fs) where Prefix chains are
    // common.
    let p = read_prefix(frame.as_ref(), pfx_slot)?;
    let plen = p.prefix_len as usize;
    let prefix_bytes = &p.bytes[..plen];
    let child_slot = p.child as u16;

    let common = key.common_prefix_with_slice(depth, prefix_bytes);

    if common == plen {
        let r = insert_at_step(
            frame,
            child_slot,
            key,
            value,
            depth + plen,
            seq,
            wants_prev,
            allow_crossing,
        )?;
        let InsertStep::Done(r) = r else {
            return Ok(r);
        };
        if r.slot_after != child_slot {
            set_prefix_child(frame, pfx_slot, u32::from(r.slot_after))?;
        }
        return Ok(InsertStep::Done(InsertReturn {
            slot_after: pfx_slot,
            previous: r.previous,
        }));
    }

    if depth + common >= key.len() {
        return Err(Error::NotYetImplemented(
            "walker::insert_into_prefix: key terminates inside a prefix",
        ));
    }

    let existing_div_byte = prefix_bytes[common];
    let tail_bytes = &prefix_bytes[common + 1..];
    let existing_branch_slot = if tail_bytes.is_empty() {
        child_slot
    } else {
        write_prefix_chain(frame, tail_bytes, child_slot)?
    };

    let new_div_byte = key
        .byte_at(depth + common)
        .expect("new key has prefix divergence byte");
    let new_leaf = write_leaf(frame, key, value, seq)?;
    let n4 = write_node4_with(
        frame,
        &[
            (existing_div_byte, u32::from(existing_branch_slot)),
            (new_div_byte, u32::from(new_leaf)),
        ],
    )?;

    let final_slot = if common == 0 {
        n4
    } else {
        write_prefix_chain(frame, &prefix_bytes[..common], n4)?
    };

    frame.free_node(pfx_slot)?;

    Ok(InsertStep::Done(InsertReturn {
        slot_after: final_slot,
        previous: None,
    }))
}

#[allow(clippy::too_many_arguments)] // mirrors insert_at's call shape
fn insert_into_inner_step(
    frame: &mut BlobFrame<'_>,
    inner_slot: u16,
    ntype: NodeType,
    key: SearchKey<'_>,
    value: &[u8],
    depth: usize,
    seq: u64,
    wants_prev: bool,
    allow_crossing: bool,
) -> Result<InsertStep> {
    let Some(byte) = key.byte_at(depth) else {
        return Err(Error::NotYetImplemented(
            "walker::insert_into_inner: key terminates at an inner node",
        ));
    };

    if let Some(child_slot) = inner_find_child(frame, inner_slot, ntype, byte)? {
        let r = insert_at_step(
            frame,
            child_slot,
            key,
            value,
            depth + 1,
            seq,
            wants_prev,
            allow_crossing,
        )?;
        let InsertStep::Done(r) = r else {
            return Ok(r);
        };
        if r.slot_after != child_slot {
            inner_update_child(frame, inner_slot, ntype, byte, u32::from(r.slot_after))?;
        }
        return Ok(InsertStep::Done(InsertReturn {
            slot_after: inner_slot,
            previous: r.previous,
        }));
    }

    let new_leaf = write_leaf(frame, key, value, seq)?;
    let possibly_grown = inner_add_child(frame, inner_slot, ntype, byte, u32::from(new_leaf))?;
    Ok(InsertStep::Done(InsertReturn {
        slot_after: possibly_grown,
        previous: None,
    }))
}
