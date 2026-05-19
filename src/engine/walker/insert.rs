//! Insert path — `insert` / `insert_multi` + recursive
//! `insert_at` dispatch + per-NodeType arms +
//! `insert_at_blob_node` cross-blob arm.

use crate::api::errors::{Error, Result};
use crate::layout::{leaf_extent_size, BlobNode, Leaf, NodeType, BLOB_MAX_INLINE};
use std::sync::Arc;

use crate::store::{BlobFrame, BlobFrameRef, BufferManager, CachedBlob};

use super::cast;
use super::readers::{longest_common, ntype_of, read_leaf_kv, read_prefix};
use super::spillover::{compact_blob, spillover_blob};
use super::types::{InsertOutcome, InsertReturn};
use super::writers::{
    inner_add_child, inner_find_child, inner_update_child, set_prefix_child, write_leaf,
    write_node4_with, write_prefix_chain, write_struct_to_slot,
};
use super::MAX_SPILLOVER_ATTEMPTS;

// ---------- public entry points ----------

/// Single-blob insert. Surfaces [`Error::NotYetImplemented`] if
/// the descent reaches a [`NodeType::Blob`] crossing — callers
/// that need cross-blob support should use [`insert_multi`].
///
/// `seq` is the journal sequence number to stamp on the new leaf
/// (callers should pass a monotonically-increasing value). Returns
/// the new root slot (caller updates `header.root_slot`) and the
/// prior value if the key already existed.
#[cfg_attr(not(test), allow(dead_code))]
pub fn insert(
    frame: &mut BlobFrame<'_>,
    root_slot: u16,
    key: &[u8],
    value: &[u8],
    seq: u64,
) -> Result<InsertOutcome> {
    if key.len() > u16::MAX as usize {
        return Err(Error::KeyTooLong { len: key.len() });
    }
    if value.len() > u16::MAX as usize {
        return Err(Error::ValueTooLong { len: value.len() });
    }
    let r = insert_at(None, frame, root_slot, key, value, 0, seq)?;
    Ok(InsertOutcome {
        new_root_slot: r.slot_after,
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
pub fn insert_multi(
    bm: &BufferManager,
    root_pin: &Arc<CachedBlob>,
    key: &[u8],
    value: &[u8],
    seq: u64,
) -> Result<InsertOutcome> {
    if key.len() > u16::MAX as usize {
        return Err(Error::KeyTooLong { len: key.len() });
    }
    if value.len() > u16::MAX as usize {
        return Err(Error::ValueTooLong { len: value.len() });
    }

    // The caller (typically `Tree`) keeps `root_pin` alive across
    // every op so we skip `BufferManager`'s pin-Mutex on the hot
    // path. Cross-blob writes still pin children through `bm`.
    // Hold the exclusive guard for the **entire** insert. Every
    // observable mutation (walker descent, header.root_slot bump,
    // spillover, compact) happens inside one continuous critical
    // section — releasing between phases would let another writer
    // observe an inconsistent intermediate state (e.g. freed
    // EmptyRoot slot before header is bumped) and lose updates to
    // the racy header rewrite.
    let mut guard = root_pin.write();

    // Retry loop. On every `OutOfSpace`, run **spillover + compact
    // back-to-back**:
    //
    // - `spillover_blob` picks the largest non-Blob subtree,
    //   migrates it to a fresh child blob via `make_blob_from_node`,
    //   and rewires the parent's child pointer through a freshly-
    //   allocated `BlobNode` (uses the bump area's
    //   `SPILLOVER_RESERVATION`).
    // - `compact_blob` then deep-clones the source's live tree into
    //   a new image and copies it back — reclaiming the just-
    //   migrated subtree's leaf-extent bytes.
    //
    // Child blobs that OOM run the same loop inside
    // `insert_at_blob_node`.
    for _attempt in 0..MAX_SPILLOVER_ATTEMPTS {
        let r = {
            let mut frame = BlobFrame::wrap(guard.as_mut_slice());
            let root_slot = frame.header().root_slot;
            insert_at(Some(bm), &mut frame, root_slot, key, value, 0, seq)
        };
        match r {
            Ok(out) => {
                {
                    let mut frame = BlobFrame::wrap(guard.as_mut_slice());
                    frame.header_mut().root_slot = out.slot_after;
                }
                return Ok(InsertOutcome {
                    new_root_slot: out.slot_after,
                    previous: out.previous,
                });
            }
            Err(Error::Alloc(crate::store::AllocError::OutOfSpace { .. })) => {
                {
                    let mut frame = BlobFrame::wrap(guard.as_mut_slice());
                    spillover_blob(bm, &mut frame, seq)?;
                }
                compact_blob(&mut guard)?;
            }
            Err(other) => return Err(other),
        }
    }
    Err(Error::NotYetImplemented(
        "insert_multi: spillover retry loop exhausted",
    ))
}

// ---------- recursive dispatch ----------

pub(super) fn insert_at(
    bm: Option<&BufferManager>,
    frame: &mut BlobFrame<'_>,
    slot: u16,
    key: &[u8],
    value: &[u8],
    depth: usize,
    seq: u64,
) -> Result<InsertReturn> {
    let ntype = ntype_of(frame.as_ref(), slot)?;
    match ntype {
        NodeType::Invalid => Err(Error::NodeCorrupt {
            context: "walker::insert_at: hit NodeType::Invalid",
        }),
        NodeType::EmptyRoot => insert_into_empty_root(frame, slot, key, value, seq),
        NodeType::Leaf => insert_into_leaf(frame, slot, key, value, depth, seq),
        NodeType::Prefix => insert_into_prefix(bm, frame, slot, key, value, depth, seq),
        NodeType::Node4 | NodeType::Node16 | NodeType::Node48 | NodeType::Node256 => {
            insert_into_inner(bm, frame, slot, ntype, key, value, depth, seq)
        }
        NodeType::Blob => match bm {
            Some(b) => insert_at_blob_node(b, frame, slot, key, value, depth, seq),
            None => Err(Error::NotYetImplemented(
                "walker::insert_at: BlobNode crossing requires BufferManager — use insert_multi",
            )),
        },
    }
}

fn insert_into_empty_root(
    frame: &mut BlobFrame<'_>,
    empty_slot: u16,
    key: &[u8],
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

fn insert_into_leaf(
    frame: &mut BlobFrame<'_>,
    leaf_slot: u16,
    new_key: &[u8],
    new_value: &[u8],
    depth: usize,
    seq: u64,
) -> Result<InsertReturn> {
    let (existing_key, existing_value) = read_leaf_kv(frame.as_ref(), leaf_slot)?;

    if existing_key == new_key {
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
        let existing_leaf = {
            let body = frame.body_of_slot(leaf_slot).ok_or(Error::NodeCorrupt {
                context: "insert_into_leaf: body resolution failed",
            })?;
            *cast::<Leaf>(body)
        };
        let was_tombstoned = existing_leaf.tombstone != 0;
        let prev = if was_tombstoned {
            None
        } else {
            Some(existing_value.clone())
        };
        let key_off = existing_leaf.key_offset;
        let key_len_u32 = new_key.len() as u32;
        let old_extent_size = leaf_extent_size(key_len_u32, u32::from(existing_value.len() as u16));
        let new_extent_size = leaf_extent_size(key_len_u32, new_value.len() as u32);

        if new_extent_size <= old_extent_size {
            let value_offset = key_off + 2 + key_len_u32;
            let value_room = old_extent_size - 2 - key_len_u32;
            let region =
                frame
                    .bytes_at_mut(value_offset, value_room)
                    .ok_or(Error::NodeCorrupt {
                        context: "insert_into_leaf: extent value range out of bounds",
                    })?;
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

    // Two different keys: split into [Prefix?] -> Node4 -> {old leaf, new leaf}.
    let suffix_a = &existing_key[depth..];
    let suffix_b = &new_key[depth..];
    let common_len = longest_common(suffix_a, suffix_b);

    if common_len == suffix_a.len() || common_len == suffix_b.len() {
        return Err(Error::NotYetImplemented(
            "walker::insert_into_leaf: one key is a strict prefix of the other",
        ));
    }

    let new_leaf = write_leaf(frame, new_key, new_value, seq)?;
    let byte_existing = suffix_a[common_len];
    let byte_new = suffix_b[common_len];
    let n4 = write_node4_with(
        frame,
        &[
            (byte_existing, u32::from(leaf_slot)),
            (byte_new, u32::from(new_leaf)),
        ],
    )?;

    let final_slot = if common_len == 0 {
        n4
    } else {
        write_prefix_chain(frame, &suffix_a[..common_len], n4)?
    };

    Ok(InsertReturn {
        slot_after: final_slot,
        previous: None,
    })
}

fn insert_into_prefix(
    bm: Option<&BufferManager>,
    frame: &mut BlobFrame<'_>,
    pfx_slot: u16,
    key: &[u8],
    value: &[u8],
    depth: usize,
    seq: u64,
) -> Result<InsertReturn> {
    let p = read_prefix(frame.as_ref(), pfx_slot)?;
    let plen = p.prefix_len as usize;
    let prefix_bytes_copy: Vec<u8> = p.bytes[..plen].to_vec();
    let child_slot = p.child as u16;

    let key_tail = &key[depth.min(key.len())..];
    let common = longest_common(&prefix_bytes_copy, key_tail);

    if common == plen {
        let r = insert_at(bm, frame, child_slot, key, value, depth + plen, seq)?;
        if r.slot_after != child_slot {
            set_prefix_child(frame, pfx_slot, u32::from(r.slot_after))?;
        }
        return Ok(InsertReturn {
            slot_after: pfx_slot,
            previous: r.previous,
        });
    }

    if depth + common >= key.len() {
        return Err(Error::NotYetImplemented(
            "walker::insert_into_prefix: key terminates inside a prefix",
        ));
    }

    let existing_div_byte = prefix_bytes_copy[common];
    let tail_bytes = &prefix_bytes_copy[common + 1..];
    let existing_branch_slot = if tail_bytes.is_empty() {
        child_slot
    } else {
        write_prefix_chain(frame, tail_bytes, child_slot)?
    };

    let new_div_byte = key[depth + common];
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
        write_prefix_chain(frame, &prefix_bytes_copy[..common], n4)?
    };

    frame.free_node(pfx_slot)?;

    Ok(InsertReturn {
        slot_after: final_slot,
        previous: None,
    })
}

#[allow(clippy::too_many_arguments)] // 8 args mirror insert_at's call shape
fn insert_into_inner(
    bm: Option<&BufferManager>,
    frame: &mut BlobFrame<'_>,
    inner_slot: u16,
    ntype: NodeType,
    key: &[u8],
    value: &[u8],
    depth: usize,
    seq: u64,
) -> Result<InsertReturn> {
    if depth >= key.len() {
        return Err(Error::NotYetImplemented(
            "walker::insert_into_inner: key terminates at an inner node",
        ));
    }
    let byte = key[depth];

    if let Some(child_slot) = inner_find_child(frame, inner_slot, ntype, byte)? {
        let r = insert_at(bm, frame, child_slot, key, value, depth + 1, seq)?;
        if r.slot_after != child_slot {
            inner_update_child(frame, inner_slot, ntype, byte, u32::from(r.slot_after))?;
        }
        return Ok(InsertReturn {
            slot_after: inner_slot,
            previous: r.previous,
        });
    }

    let new_leaf = write_leaf(frame, key, value, seq)?;
    let possibly_grown = inner_add_child(frame, inner_slot, ntype, byte, u32::from(new_leaf))?;
    Ok(InsertReturn {
        slot_after: possibly_grown,
        previous: None,
    })
}

// ---------- multi-blob arm ----------

/// Insert across a [`NodeType::Blob`] crossing.
///
/// Pins the child blob in the BM, runs the recursive insert in
/// place (with its own spillover+compact retry loop), then stages
/// the mutation via `bm.mark_dirty(child_guid, seq)` so the
/// checkpoint round can flush it under invariant W2D.
///
/// **Inline-prefix split limitation**: if the BlobNode's inline
/// prefix doesn't match the key, this returns
/// [`Error::NotYetImplemented`]. A full implementation would
/// split the BlobNode into `Prefix + Node4{old_bn, new_subtree}`,
/// similar to `insert_into_prefix`'s diverged path. Common-case
/// workloads rarely hit this since spillover always installs a
/// BlobNode with an empty inline prefix.
fn insert_at_blob_node(
    bm: &BufferManager,
    parent_frame: &mut BlobFrame<'_>,
    bn_slot: u16,
    key: &[u8],
    value: &[u8],
    depth: usize,
    seq: u64,
) -> Result<InsertReturn> {
    let bn = {
        let body = parent_frame
            .body_of_slot(bn_slot)
            .ok_or(Error::NodeCorrupt {
                context: "insert_at_blob_node: body resolution failed",
            })?;
        *cast::<BlobNode>(body)
    };
    let plen = bn.prefix_len as usize;
    if plen > BLOB_MAX_INLINE {
        return Err(Error::NodeCorrupt {
            context: "insert_at_blob_node: prefix_len exceeds inline buffer",
        });
    }
    if depth + plen > key.len() || key[depth..depth + plen] != bn.bytes[..plen] {
        return Err(Error::NotYetImplemented(
            "insert_at_blob_node: BlobNode inline-prefix split is not yet implemented",
        ));
    }

    let child_guid = bn.child_blob_guid;
    let mut child_entry = bn.child_entry_ptr as u16;
    let child_depth = depth + plen;

    // Pin the child blob in the BM cache for the duration of the
    // recursion. Every iteration takes a fresh write-guard against
    // the same pinned buffer — no 512 KB memcpy per attempt.
    let child_pin = bm.pin(child_guid)?;

    let child_result = {
        let mut last_err: Option<Error> = None;
        let mut done = None;
        for _attempt in 0..MAX_SPILLOVER_ATTEMPTS {
            let r = {
                let mut guard = child_pin.write();
                let mut cf = BlobFrame::wrap(guard.as_mut_slice());
                insert_at(Some(bm), &mut cf, child_entry, key, value, child_depth, seq)
            };
            match r {
                Ok(out) => {
                    done = Some(out);
                    break;
                }
                Err(Error::Alloc(crate::store::AllocError::OutOfSpace { .. })) => {
                    {
                        let mut guard = child_pin.write();
                        let mut cf = BlobFrame::wrap(guard.as_mut_slice());
                        spillover_blob(bm, &mut cf, seq)?;
                    }
                    {
                        let mut guard = child_pin.write();
                        compact_blob(&mut guard)?;
                    }
                    // `compact_blob` rebuilds the child blob in
                    // place and renumbers every slot index — the
                    // entry slot we cached from the parent's
                    // `BlobNode.child_entry_ptr` is now stale.
                    // Re-pick it from the child's freshly-written
                    // `header.root_slot` so the next retry walks
                    // a valid slot. (The parent's BlobNode pointer
                    // gets refreshed below after the loop exits via
                    // the `slot_after` rewire.)
                    child_entry = {
                        let guard = child_pin.read();
                        let cf = BlobFrameRef::wrap(guard.as_slice());
                        cf.header().root_slot
                    };
                }
                Err(e) => {
                    last_err = Some(e);
                    break;
                }
            }
        }
        match (done, last_err) {
            (Some(r), _) => r,
            (None, Some(e)) => return Err(e),
            (None, None) => {
                return Err(Error::NotYetImplemented(
                    "insert_at_blob_node: child spillover retry loop exhausted",
                ));
            }
        }
    };

    // Update child blob's header.root_slot if the entry slot
    // changed. Keeps the child blob self-describing for any
    // future `make_blob_from_node` migrating *out* of it.
    {
        let mut guard = child_pin.write();
        let mut cf = BlobFrame::wrap(guard.as_mut_slice());
        cf.header_mut().root_slot = child_result.slot_after;
    }

    if u32::from(child_result.slot_after) != bn.child_entry_ptr {
        let mut new_bn = bn;
        new_bn.child_entry_ptr = u32::from(child_result.slot_after);
        write_struct_to_slot(parent_frame, bn_slot, &new_bn)?;
    }

    drop(child_pin);
    // Hand the child blob to the unified checkpoint protocol —
    // it's now dirty at this op's seq. Flushing the bytes to
    // backend is the checkpoint round's job (and only happens
    // **after** the WAL record for this op is durable). An inline
    // `bm.commit(child_guid)` here would let child bytes reach
    // backend before WAL — invariant W2D-broken; see
    // `BufferManager` module docs.
    bm.mark_dirty(child_guid, seq);

    Ok(InsertReturn {
        slot_after: bn_slot,
        previous: child_result.previous,
    })
}
