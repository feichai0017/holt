//! Write-side helpers — allocate fresh slots in a [`BlobFrame`]
//! (for bookkeeping) and populate node bodies, addressing them by
//! byte offset.
//!
//! Everything here is `pub(super)`, scoped to walker submodules.
//! Pure mutation; no recursion into the walker's descent logic.
//!
//! ## Offset addressing (v4)
//!
//! A node is addressed by its body's absolute byte offset. Allocation
//! still goes through `alloc_node` / `alloc_leaf` (which return a slot
//! for `num_slots` / `MAX_SLOTS` bookkeeping); the walker immediately
//! resolves the slot's offset and works with that. Every `write_*`
//! returns the **byte offset** of the node it wrote. Child fields
//! (`children[N]`, `Prefix.child`) take a child **byte offset** and
//! the helpers encode it via `encode_child_off` before storing it as
//! the body's `u16` — so call sites never touch the encoding.

use std::mem::size_of;

use crate::api::errors::{Error, Result};
use crate::engine::simd;
use crate::layout::{
    leaf_body_size, BlobGuid, BlobNode, Leaf, Node16, Node256, Node4, Node48, NodeType, Prefix,
    PREFIX_MAX_INLINE,
};
use crate::store::{encode_child_off, BlobFrame};

use super::cast;
use super::readers::{
    child_offset, read_node16, read_node256, read_node256_child, read_node4, read_node48,
    read_prefix,
};
use super::SearchKey;

/// Resolve the byte offset of the body just allocated in `slot`.
fn slot_offset(frame: &BlobFrame<'_>, slot: u16) -> Result<u32> {
    frame
        .offset_of_slot(slot)
        .ok_or(Error::node_corrupt("walker: alloc slot has no offset"))
}

/// Write `v` into the node body at absolute `off`.
///
/// Addresses the body by raw offset (`bytes_at_mut`) rather than
/// `body_at_offset_mut`: a freshly-allocated body has a zero
/// `node_type @ +1` byte, so it isn't yet offset-resolvable — and `v`
/// itself carries the correct `node_type` byte, which this write
/// installs. The body size is `size_of::<T>()` (every fixed node type;
/// leaves use the dedicated `write_leaf` path, never this helper).
pub(super) fn write_struct_at<T>(frame: &mut BlobFrame<'_>, off: u32, v: &T) -> Result<()> {
    {
        let body = frame
            .bytes_at_mut(off, size_of::<T>() as u32)
            .ok_or(Error::node_corrupt("write_struct_at: body"))?;
        debug_assert_eq!(body.len(), size_of::<T>());
        // SAFETY: layout types are #[repr(C)] POD; body sized and
        // aligned per BlobFrame invariants.
        let bytes = unsafe {
            std::slice::from_raw_parts(std::ptr::from_ref::<T>(v).cast::<u8>(), size_of::<T>())
        };
        body.copy_from_slice(bytes);
    }
    Ok(())
}

/// Repoint an existing `BlobNode`'s child GUID. Used by copy-on-write
/// forking to redirect a parent frame's crossing at byte offset `off`
/// from the shared child frame to its freshly-installed private fork.
/// The parent frame must be exclusively latched by the caller.
pub(super) fn repoint_blob_node(
    frame: &mut BlobFrame<'_>,
    off: u32,
    new_child: BlobGuid,
) -> Result<()> {
    let mut bn = *cast::<BlobNode>(
        frame
            .body_at_offset(off)
            .ok_or(Error::node_corrupt("repoint_blob_node: body"))?,
    );
    bn.child_blob_guid = new_child;
    write_struct_at(frame, off, &bn)
}

/// Write a fresh leaf node and return its body **byte offset**.
pub(super) fn write_leaf(
    frame: &mut BlobFrame<'_>,
    key: SearchKey<'_>,
    value: &[u8],
    seq: u64,
) -> Result<u32> {
    // A leaf is a single, contiguous, self-describing node:
    // `[16B header][key bytes][value bytes]`. Allocate one variable-
    // size node sized `align8(16 + key_len + value_len)` and write the
    // header + key + value into it in place — no separate extent.
    let key_len = key.len();
    let value_len = value.len();
    let total = leaf_body_size(key_len as u32, value_len as u32);
    let out = frame.alloc_leaf(total)?;
    let leaf = Leaf::live(key_len as u16, value_len as u16, seq, key.fingerprint());
    let body_off = slot_offset(frame, out.slot)?;
    {
        // The freshly-allocated leaf body's header is still zero, so
        // address the region by its byte offset and write
        // `[header][key][value]` directly; the leaf becomes self-
        // describing (and offset-resolvable via its `node_type @ +1`)
        // once the header lands.
        let body = frame
            .bytes_at_mut(body_off, total)
            .ok_or(Error::node_corrupt("write_leaf: body out of range"))?;
        let hdr = unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref::<Leaf>(&leaf).cast::<u8>(),
                size_of::<Leaf>(),
            )
        };
        body[..16].copy_from_slice(hdr);
        key.write_to_slice(&mut body[16..16 + key_len]);
        body[16 + key_len..16 + key_len + value_len].copy_from_slice(value);
    }
    Ok(body_off)
}

/// Build a Prefix-node chain spanning `bytes`, ending at child byte
/// offset `child_off`. `bytes` may exceed `PREFIX_MAX_INLINE`; if so,
/// multiple chained Prefix nodes are allocated. Returns the head
/// Prefix's byte offset.
pub(super) fn write_prefix_chain(
    frame: &mut BlobFrame<'_>,
    bytes: &[u8],
    child_off: u32,
) -> Result<u32> {
    debug_assert!(!bytes.is_empty(), "write_prefix_chain on empty bytes");
    let mut next_child = child_off;
    let mut remaining = bytes;
    let mut head = 0u32;
    while !remaining.is_empty() {
        let chunk_len = remaining.len().min(PREFIX_MAX_INLINE);
        let chunk_start = remaining.len() - chunk_len;
        let chunk = &remaining[chunk_start..];
        let out = frame.alloc_node(NodeType::Prefix)?;
        let off = slot_offset(frame, out.slot)?;
        // `Prefix.child` stores the encoded child offset.
        let p = Prefix::new(chunk, u32::from(encode_child_off(next_child)));
        write_struct_at(frame, off, &p)?;
        next_child = off;
        head = off;
        remaining = &remaining[..chunk_start];
    }
    Ok(head)
}

/// Build a fresh Node4 with the given `(byte, child_off)` pairs.
/// Keys are sorted ascending inside the Node4. Returns the Node4's
/// byte offset.
pub(super) fn write_node4_with(frame: &mut BlobFrame<'_>, children: &[(u8, u32)]) -> Result<u32> {
    debug_assert!(!children.is_empty() && children.len() <= 4);
    let out = frame.alloc_node(NodeType::Node4)?;
    let off = slot_offset(frame, out.slot)?;
    let mut n = Node4::empty();
    let mut sorted = children.to_vec();
    sorted.sort_by_key(|(b, _)| *b);
    n.count = sorted.len() as u8;
    for (i, (b, c)) in sorted.iter().enumerate() {
        n.keys[i] = *b;
        n.children[i] = encode_child_off(*c);
    }
    write_struct_at(frame, off, &n)?;
    Ok(off)
}

/// Repoint a Prefix node's child at byte offset `pfx_off` to the
/// child at byte offset `new_child_off`.
pub(super) fn set_prefix_child(
    frame: &mut BlobFrame<'_>,
    pfx_off: u32,
    new_child_off: u32,
) -> Result<()> {
    let mut p = read_prefix(frame.as_ref(), pfx_off)?;
    p.child = u32::from(encode_child_off(new_child_off));
    write_struct_at(frame, pfx_off, &p)
}

// ---------- inner-node ops (find / update / add+grow) ----------

/// Find the child of inner node at `off` routing on `byte`, returning
/// the child's **byte offset** when present.
pub(super) fn inner_find_child(
    frame: &BlobFrame<'_>,
    off: u32,
    ntype: NodeType,
    byte: u8,
) -> Result<Option<u32>> {
    match ntype {
        NodeType::Node4 => {
            let n = read_node4(frame.as_ref(), off)?;
            let count = (n.count as usize).min(4);
            for i in 0..count {
                if n.keys[i] == byte {
                    return Ok(Some(child_offset(n.children[i])));
                }
                if n.keys[i] > byte {
                    break;
                }
            }
            Ok(None)
        }
        NodeType::Node16 => {
            let n = read_node16(frame.as_ref(), off)?;
            if let Some(i) = simd::node16_find_byte(&n.keys, n.count, byte) {
                Ok(Some(child_offset(n.children[i as usize])))
            } else {
                Ok(None)
            }
        }
        NodeType::Node48 => {
            let n = read_node48(frame.as_ref(), off)?;
            let idx = n.index[byte as usize];
            if idx == 0 {
                Ok(None)
            } else {
                Ok(Some(child_offset(n.children[idx as usize - 1])))
            }
        }
        NodeType::Node256 => {
            let encoded = read_node256_child(frame.as_ref(), off, byte)?;
            if encoded == 0 {
                Ok(None)
            } else {
                Ok(Some(child_offset(encoded)))
            }
        }
        _ => Err(Error::node_corrupt("inner_find_child: not an inner node")),
    }
}

/// Repoint the existing child routing on `byte` in the inner node at
/// `off` to the child at byte offset `new_child_off`.
pub(super) fn inner_update_child(
    frame: &mut BlobFrame<'_>,
    off: u32,
    ntype: NodeType,
    byte: u8,
    new_child_off: u32,
) -> Result<()> {
    let encoded = encode_child_off(new_child_off);
    match ntype {
        NodeType::Node4 => {
            let mut n = read_node4(frame.as_ref(), off)?;
            let count = (n.count as usize).min(4);
            for i in 0..count {
                if n.keys[i] == byte {
                    n.children[i] = encoded;
                    return write_struct_at(frame, off, &n);
                }
            }
            Err(Error::node_corrupt(
                "inner_update_child: byte not found in Node4",
            ))
        }
        NodeType::Node16 => {
            let mut n = read_node16(frame.as_ref(), off)?;
            if let Some(i) = simd::node16_find_byte(&n.keys, n.count, byte) {
                n.children[i as usize] = encoded;
                return write_struct_at(frame, off, &n);
            }
            Err(Error::node_corrupt(
                "inner_update_child: byte not found in Node16",
            ))
        }
        NodeType::Node48 => {
            let mut n = read_node48(frame.as_ref(), off)?;
            let idx = n.index[byte as usize];
            if idx == 0 {
                return Err(Error::node_corrupt(
                    "inner_update_child: byte not found in Node48",
                ));
            }
            n.children[idx as usize - 1] = encoded;
            write_struct_at(frame, off, &n)
        }
        NodeType::Node256 => {
            let mut n = read_node256(frame.as_ref(), off)?;
            n.children[byte as usize] = encoded;
            write_struct_at(frame, off, &n)
        }
        _ => Err(Error::node_corrupt("inner_update_child: not an inner node")),
    }
}

/// Add `(byte, child_off)` to an inner node at `off`, growing to the
/// next NodeType variant if the current one is full. Returns the byte
/// offset to be used as the parent's child pointer (changes on
/// growth — the grown node is a fresh allocation and the old one is
/// abandoned).
pub(super) fn inner_add_child(
    frame: &mut BlobFrame<'_>,
    off: u32,
    ntype: NodeType,
    byte: u8,
    new_child_off: u32,
) -> Result<u32> {
    let encoded = encode_child_off(new_child_off);
    match ntype {
        NodeType::Node4 => {
            let n = read_node4(frame.as_ref(), off)?;
            if n.count < 4 {
                let mut new = n;
                node4_insert_sorted(&mut new, byte, encoded);
                write_struct_at(frame, off, &new)?;
                Ok(off)
            } else {
                let n16_off = grow_node4_to_node16(frame, n)?;
                frame.note_abandoned(off); // abandon-on-free: old Node4
                inner_add_child(frame, n16_off, NodeType::Node16, byte, new_child_off)
            }
        }
        NodeType::Node16 => {
            let n = read_node16(frame.as_ref(), off)?;
            if n.count < 16 {
                let mut new = n;
                node16_insert_sorted(&mut new, byte, encoded);
                write_struct_at(frame, off, &new)?;
                Ok(off)
            } else {
                let n48_off = grow_node16_to_node48(frame, n)?;
                frame.note_abandoned(off); // abandon-on-free: old Node16
                inner_add_child(frame, n48_off, NodeType::Node48, byte, new_child_off)
            }
        }
        NodeType::Node48 => {
            let n = read_node48(frame.as_ref(), off)?;
            if n.count < 48 {
                let mut new = n;
                node48_insert(&mut new, byte, encoded)?;
                write_struct_at(frame, off, &new)?;
                Ok(off)
            } else {
                let n256_off = grow_node48_to_node256(frame, n)?;
                frame.note_abandoned(off); // abandon-on-free: old Node48
                inner_add_child(frame, n256_off, NodeType::Node256, byte, new_child_off)
            }
        }
        NodeType::Node256 => {
            let mut n = read_node256(frame.as_ref(), off)?;
            if n.children[byte as usize] != 0 {
                return Err(Error::node_corrupt(
                    "inner_add_child: byte already present on Node256",
                ));
            }
            n.children[byte as usize] = encoded;
            // Node256 capacity is 256 but `count` is u8 (max 255).
            // Saturate at 255 — the bit-set in `children[]` is the
            // authoritative population check; `count` is a stat
            // used by spillover / shrink heuristics.
            n.count = n.count.saturating_add(1);
            write_struct_at(frame, off, &n)?;
            Ok(off)
        }
        _ => Err(Error::node_corrupt("inner_add_child: not an inner node")),
    }
}

fn node4_insert_sorted(n: &mut Node4, byte: u8, child: u16) {
    let count = n.count as usize;
    debug_assert!(count < 4);
    let mut pos = count;
    for i in 0..count {
        if n.keys[i] > byte {
            pos = i;
            break;
        }
    }
    let mut i = count;
    while i > pos {
        n.keys[i] = n.keys[i - 1];
        n.children[i] = n.children[i - 1];
        i -= 1;
    }
    n.keys[pos] = byte;
    n.children[pos] = child;
    n.count += 1;
}

fn node16_insert_sorted(n: &mut Node16, byte: u8, child: u16) {
    let count = n.count as usize;
    debug_assert!(count < 16);
    let mut pos = count;
    for i in 0..count {
        if n.keys[i] > byte {
            pos = i;
            break;
        }
    }
    let mut i = count;
    while i > pos {
        n.keys[i] = n.keys[i - 1];
        n.children[i] = n.children[i - 1];
        i -= 1;
    }
    n.keys[pos] = byte;
    n.children[pos] = child;
    n.count += 1;
}

fn node48_insert(n: &mut Node48, byte: u8, child: u16) -> Result<()> {
    if n.index[byte as usize] != 0 {
        return Err(Error::node_corrupt("node48_insert: byte already present"));
    }
    for i in 0..48 {
        if n.children[i] == 0 {
            n.children[i] = child;
            n.index[byte as usize] = (i + 1) as u8;
            n.count += 1;
            return Ok(());
        }
    }
    Err(Error::node_corrupt(
        "node48_insert: no free children[] slot despite count < 48",
    ))
}

// ---------- node growth (abandon-on-grow) ----------
//
// A grow allocates the larger variant, copies the (already-encoded)
// `children[]` across verbatim — child offsets are stable, so no
// re-encode is needed — and returns the new node's byte offset. The
// old node is NOT freed: it becomes unreachable and is reclaimed at
// the next compaction (abandon-on-free). This removes the need for an
// offset->slot reverse lookup that `free_node` would have required.

fn grow_node4_to_node16(frame: &mut BlobFrame<'_>, old: Node4) -> Result<u32> {
    let out = frame.alloc_node(NodeType::Node16)?;
    let off = slot_offset(frame, out.slot)?;
    let mut n = Node16::empty();
    n.count = old.count;
    for i in 0..old.count as usize {
        n.keys[i] = old.keys[i];
        n.children[i] = old.children[i];
    }
    write_struct_at(frame, off, &n)?;
    Ok(off)
}

fn grow_node16_to_node48(frame: &mut BlobFrame<'_>, old: Node16) -> Result<u32> {
    let out = frame.alloc_node(NodeType::Node48)?;
    let off = slot_offset(frame, out.slot)?;
    let mut n = Node48::empty();
    n.count = old.count;
    for i in 0..old.count as usize {
        n.children[i] = old.children[i];
        n.index[old.keys[i] as usize] = (i + 1) as u8;
    }
    write_struct_at(frame, off, &n)?;
    Ok(off)
}

fn grow_node48_to_node256(frame: &mut BlobFrame<'_>, old: Node48) -> Result<u32> {
    let out = frame.alloc_node(NodeType::Node256)?;
    let off = slot_offset(frame, out.slot)?;
    let mut n = Node256::empty();
    let mut count = 0u16;
    for byte in 0..256usize {
        let idx = old.index[byte];
        if idx != 0 {
            n.children[byte] = old.children[idx as usize - 1];
            count += 1;
        }
    }
    n.count = count.min(255) as u8;
    write_struct_at(frame, off, &n)?;
    Ok(off)
}

// ---------- node shrink (erase-time, hysteresis-aware) ----------

/// Threshold below which a `Node16` shrinks to `Node4`. The
/// `Node4` capacity is 4; we shrink at `count ≤ 3` so the next
/// insert at this byte position doesn't immediately re-grow.
pub(super) const SHRINK_NODE16_TO_NODE4_AT: u8 = 3;

/// Threshold below which a `Node48` shrinks to `Node16`. The
/// `Node16` capacity is 16; we shrink at `count ≤ 12` for
/// hysteresis vs the grow threshold.
pub(super) const SHRINK_NODE48_TO_NODE16_AT: u8 = 12;

/// Threshold below which a `Node256` shrinks to `Node48`. The
/// `Node48` capacity is 48; we shrink at `count ≤ 37` for
/// hysteresis vs the grow threshold.
pub(super) const SHRINK_NODE256_TO_NODE48_AT: u8 = 37;

pub(super) fn shrink_node16_to_node4(frame: &mut BlobFrame<'_>, old: Node16) -> Result<u32> {
    debug_assert!(old.count <= 4, "shrink target must fit in Node4");
    let out = frame.alloc_node(NodeType::Node4)?;
    let off = slot_offset(frame, out.slot)?;
    let mut n = Node4::empty();
    n.count = old.count;
    for i in 0..old.count as usize {
        n.keys[i] = old.keys[i];
        n.children[i] = old.children[i];
    }
    write_struct_at(frame, off, &n)?;
    Ok(off)
}

pub(super) fn shrink_node48_to_node16(frame: &mut BlobFrame<'_>, old: Node48) -> Result<u32> {
    debug_assert!(old.count <= 16, "shrink target must fit in Node16");
    let out = frame.alloc_node(NodeType::Node16)?;
    let off = slot_offset(frame, out.slot)?;
    let mut n = Node16::empty();
    n.count = old.count;
    let mut i = 0usize;
    let mut byte = 0usize;
    while let Some(next_byte) = simd::find_next_nonzero_byte(&old.index, byte) {
        byte = next_byte + 1;
        let idx = old.index[next_byte];
        n.keys[i] = next_byte as u8;
        n.children[i] = old.children[idx as usize - 1];
        i += 1;
    }
    debug_assert_eq!(i, old.count as usize);
    write_struct_at(frame, off, &n)?;
    Ok(off)
}

pub(super) fn shrink_node256_to_node48(frame: &mut BlobFrame<'_>, old: Node256) -> Result<u32> {
    debug_assert!(old.count <= 48, "shrink target must fit in Node48");
    let out = frame.alloc_node(NodeType::Node48)?;
    let off = slot_offset(frame, out.slot)?;
    let mut n = Node48::empty();
    n.count = old.count;
    // Pack the populated children into the first `count` slots of the
    // destination `children[]`; rewrite `index[byte]` to point at the
    // new packed position. Child offsets carry across verbatim.
    let mut packed = 0usize;
    let mut byte = 0usize;
    while let Some(next_byte) = simd::find_next_nonzero_u16(&old.children, byte) {
        byte = next_byte + 1;
        let child = old.children[next_byte];
        n.children[packed] = child;
        n.index[next_byte] = (packed + 1) as u8;
        packed += 1;
    }
    debug_assert_eq!(packed, old.count as usize);
    write_struct_at(frame, off, &n)?;
    Ok(off)
}

/// Shared collapse / writeback for the Node4 + Node16 arms whose
/// `keys[]` array is sorted in-place; `surviving_byte` and
/// `surviving_child` (the latter the *encoded* `children[0]`) are
/// only consulted when `new_count == 1`.
///
/// Structural collapse abandons the old node rather than freeing it
/// (abandon-on-free): the parent is repointed at the returned offset
/// and the old body becomes unreachable until the next compaction.
pub(super) fn finish_inner_with_sorted<T>(
    frame: &mut BlobFrame<'_>,
    off: u32,
    new_count: u8,
    body: &T,
    surviving_byte: u8,
    surviving_child_enc: u16,
) -> Result<super::types::EraseSignal> {
    use super::types::EraseSignal;
    if new_count == 0 {
        frame.note_abandoned(off); // abandon-on-free
        return Ok(EraseSignal::SubtreeGone);
    }
    if new_count == 1 {
        let new_off =
            write_prefix_chain(frame, &[surviving_byte], child_offset(surviving_child_enc))?;
        frame.note_abandoned(off); // abandon-on-free: collapsed inner node
        return Ok(EraseSignal::Replaced(new_off));
    }
    write_struct_at(frame, off, body)?;
    Ok(EraseSignal::Unchanged)
}
