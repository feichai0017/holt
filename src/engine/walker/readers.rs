//! Read-side helpers — borrow into a [`BlobFrameRef`] and decode
//! node bodies (addressed by byte offset) or extract leaf extents.
//!
//! Everything here is `pub(super)` so the other walker submodules
//! (lookup / insert / erase / spillover / migrate) can share these
//! decoders. They do **not** mutate the frame; mutation lives in
//! [`super::writers`].
//!
//! ## Offset addressing
//!
//! Nodes are addressed by the absolute byte offset of their body
//! (v4). A child field (`children[N]`, `Prefix.child`,
//! `header.root_slot`) stores the *encoded* offset (see
//! `encode_child_off`); these helpers take the already-decoded
//! absolute `off: u32` and resolve `(NodeType, body)` from it with a
//! single load — no slot-table indirection.

use crate::api::errors::{Error, Result};
use crate::layout::{Leaf, Node16, Node256, Node4, Node48, NodeType, Prefix};
use crate::store::{decode_child_off, BlobFrameRef};
use std::mem::size_of;

use super::cast;

pub(super) fn resolve_typed(frame: BlobFrameRef<'_>, off: u32) -> Result<(NodeType, &[u8])> {
    let ntype = frame
        .ntype_at(off)
        .ok_or(Error::node_corrupt("walker: undecodable node type"))?;
    let body = frame
        .body_at_offset(off)
        .ok_or(Error::node_corrupt("walker: body resolution failed"))?;
    Ok((ntype, body))
}

pub(super) fn ntype_of(frame: BlobFrameRef<'_>, off: u32) -> Result<NodeType> {
    frame
        .ntype_at(off)
        .ok_or(Error::node_corrupt("walker: undecodable node type"))
}

/// Split a leaf's contiguous self-describing body
/// (`[16B header][key][value]`) into `(key, value)` slices.
///
/// `body` must be the full leaf body as returned by
/// `body_at_offset` (already sized to `align8(16 + key_len +
/// value_len)`), and `leaf` the header decoded from `body[..16]`.
fn split_leaf_body<'a>(body: &'a [u8], leaf: &Leaf) -> Result<(&'a [u8], &'a [u8])> {
    let key_len = leaf.key_len as usize;
    let value_len = leaf.value_len as usize;
    let key_end = 16usize
        .checked_add(key_len)
        .ok_or(Error::node_corrupt("leaf body: key length overflow"))?;
    let value_end = key_end
        .checked_add(value_len)
        .ok_or(Error::node_corrupt("leaf body: value length overflow"))?;
    if value_end > body.len() {
        return Err(Error::node_corrupt("leaf body: key/value out of range"));
    }
    Ok((&body[16..key_end], &body[key_end..value_end]))
}

/// Borrow `(key, value)` of the leaf at `off` from its contiguous
/// self-describing body.
pub(super) fn leaf_extent(frame: BlobFrameRef<'_>, off: u32) -> Result<(&[u8], &[u8])> {
    let body = frame
        .body_at_offset(off)
        .ok_or(Error::node_corrupt("leaf body resolution failed"))?;
    let leaf = *cast::<Leaf>(&body[..size_of::<Leaf>()]);
    split_leaf_body(body, &leaf)
}

/// Borrow the key bytes of the leaf at `off` from its contiguous
/// self-describing body.
pub(super) fn leaf_key_extent(frame: BlobFrameRef<'_>, off: u32) -> Result<&[u8]> {
    let (key, _value) = leaf_extent(frame, off)?;
    Ok(key)
}

/// Borrow the key and copy the small leaf header. Update and delete
/// walkers can decide key equality without allocating; the returned
/// key borrow must not cross a later frame mutation.
pub(super) fn read_leaf_key_ref(frame: BlobFrameRef<'_>, off: u32) -> Result<(&[u8], Leaf)> {
    let body = frame
        .body_at_offset(off)
        .ok_or(Error::node_corrupt("read_leaf_key_ref: body"))?;
    let leaf = *cast::<Leaf>(&body[..size_of::<Leaf>()]);
    let (key, _value) = split_leaf_body(body, &leaf)?;
    Ok((key, leaf))
}

/// Borrow the key of a leaf at `off`. With the flattened, single-
/// encoding leaf the key lives in the contiguous body at
/// `body[16..16+key_len]`. Used where a walker needs only key
/// ordering/equality.
pub(super) fn leaf_any_key(frame: BlobFrameRef<'_>, off: u32) -> Result<&[u8]> {
    let (_ntype, body) = resolve_typed(frame, off)?;
    let leaf = *cast::<Leaf>(&body[..size_of::<Leaf>()]);
    split_leaf_body(body, &leaf).map(|(key, _value)| key)
}

pub(super) fn read_prefix(frame: BlobFrameRef<'_>, off: u32) -> Result<Prefix> {
    let body = frame
        .body_at_offset(off)
        .ok_or(Error::node_corrupt("read_prefix: body"))?;
    Ok(*cast::<Prefix>(body))
}

pub(super) fn read_node4(frame: BlobFrameRef<'_>, off: u32) -> Result<Node4> {
    let body = frame
        .body_at_offset(off)
        .ok_or(Error::node_corrupt("read_node4: body"))?;
    Ok(*cast::<Node4>(body))
}

pub(super) fn read_node16(frame: BlobFrameRef<'_>, off: u32) -> Result<Node16> {
    let body = frame
        .body_at_offset(off)
        .ok_or(Error::node_corrupt("read_node16: body"))?;
    Ok(*cast::<Node16>(body))
}

pub(super) fn read_node48(frame: BlobFrameRef<'_>, off: u32) -> Result<Node48> {
    let body = frame
        .body_at_offset(off)
        .ok_or(Error::node_corrupt("read_node48: body"))?;
    Ok(*cast::<Node48>(body))
}

pub(super) fn read_node256(frame: BlobFrameRef<'_>, off: u32) -> Result<Node256> {
    let body = frame
        .body_at_offset(off)
        .ok_or(Error::node_corrupt("read_node256: body"))?;
    Ok(*cast::<Node256>(body))
}

/// Read the encoded child of a Node256 at routing `byte` (the raw
/// `u16`; `0` means "no child"). Returns the *encoded* value — the
/// caller decodes it via `decode_child_off` when non-zero.
pub(super) fn read_node256_child(frame: BlobFrameRef<'_>, off: u32, byte: u8) -> Result<u16> {
    let body = frame
        .body_at_offset(off)
        .ok_or(Error::node_corrupt("read_node256_child: body"))?;
    if body.len() != size_of::<Node256>() {
        return Err(Error::node_corrupt("read_node256_child: non-Node256 slot"));
    }
    Ok(cast::<Node256>(body).children[byte as usize])
}

/// Decode an encoded child field (`children[i]`, `Prefix.child`,
/// `header.root_slot`) into the child body's absolute byte offset.
/// The caller must have already rejected the `0` null sentinel.
#[inline]
pub(super) fn child_offset(encoded: u16) -> u32 {
    decode_child_off(encoded)
}
