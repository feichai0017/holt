//! Checkpoint image — a serialized, point-in-time image of a whole
//! [`crate::DB`]: every live family's key/values.
//!
//! Produced by [`crate::DB::export_checkpoint`] and consumed by
//! [`crate::DB::install_checkpoint`]. The image is an opaque Holt
//! encoding for archive, transfer, and fresh-DB bootstrap. It carries
//! no external log or replication metadata.
//!
//! Layout (little-endian):
//! ```text
//!   magic[8] "holtdbi1" | family_count:u32
//!   family*: name_len:u32 name | block_len:u32 block
//!   block = (key_len:u32 key  val_len:u32 val)*
//! ```

use crate::api::errors::{Error, Result};

const MAGIC: &[u8; 8] = b"holtdbi1";
const HEADER_LEN: usize = 8 + 4;

/// A serialized whole-`DB` checkpoint. See the module docs.
#[derive(Debug, Clone)]
pub struct CheckpointImage {
    bytes: Vec<u8>,
}

impl CheckpointImage {
    /// Wrap raw checkpoint bytes (e.g. read back from durable storage).
    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// The serialized bytes — write these to durable storage.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consume into the raw bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Validate the complete checkpoint image.
    ///
    /// This walks every encoded family and key/value block, catching
    /// truncated bodies, trailing bytes, and malformed length prefixes
    /// before a caller stages the image for installation.
    pub fn validate(&self) -> Result<()> {
        decode(&self.bytes).map(|_| ())
    }

    pub(crate) fn from_raw(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }
}

// ---------- encode (used by DB::export_checkpoint) ----------

/// Start a checkpoint buffer with the header. Families are appended
/// with [`put_family`].
pub(crate) fn begin(family_count: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_LEN);
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&family_count.to_le_bytes());
    buf
}

/// Append a length-prefixed key/value into a family `block`.
pub(crate) fn put_kv(block: &mut Vec<u8>, key: &[u8], value: &[u8]) {
    block.extend_from_slice(&(key.len() as u32).to_le_bytes());
    block.extend_from_slice(key);
    block.extend_from_slice(&(value.len() as u32).to_le_bytes());
    block.extend_from_slice(value);
}

/// Append a family (name + its key/value block) to the buffer.
pub(crate) fn put_family(buf: &mut Vec<u8>, name: &[u8], block: &[u8]) {
    buf.extend_from_slice(&(name.len() as u32).to_le_bytes());
    buf.extend_from_slice(name);
    buf.extend_from_slice(&(block.len() as u32).to_le_bytes());
    buf.extend_from_slice(block);
}

// ---------- decode (used by DB::install_checkpoint) ----------

fn corrupt(what: &'static str) -> Error {
    Error::node_corrupt(what)
}

fn parse_header(bytes: &[u8]) -> Result<u32> {
    if bytes.len() < HEADER_LEN || &bytes[0..8] != MAGIC {
        return Err(corrupt("checkpoint image: bad magic or truncated header"));
    }
    let family_count = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    Ok(family_count)
}

/// Read a `u32` length + that many bytes at `*off`, advancing `*off`.
fn take<'a>(bytes: &'a [u8], off: &mut usize) -> Result<&'a [u8]> {
    let start = *off;
    if start + 4 > bytes.len() {
        return Err(corrupt("checkpoint image: truncated length"));
    }
    let len = u32::from_le_bytes(bytes[start..start + 4].try_into().unwrap()) as usize;
    let data_start = start + 4;
    let data_end = data_start
        .checked_add(len)
        .filter(|&e| e <= bytes.len())
        .ok_or_else(|| corrupt("checkpoint image: truncated body"))?;
    *off = data_end;
    Ok(&bytes[data_start..data_end])
}

/// One key/value pair borrowed from the image.
pub(crate) type Kv<'a> = (&'a [u8], &'a [u8]);
/// One decoded family: its name + key/values, borrowed from the image.
pub(crate) type Family<'a> = (&'a [u8], Vec<Kv<'a>>);

/// Decoded view of a checkpoint. Borrows the image bytes.
pub(crate) struct Decoded<'a> {
    pub families: Vec<Family<'a>>,
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Decoded<'_>> {
    let family_count = parse_header(bytes)?;
    let mut off = HEADER_LEN;
    let mut families = Vec::with_capacity(family_count as usize);
    for _ in 0..family_count {
        let name = take(bytes, &mut off)?;
        let block = take(bytes, &mut off)?;
        let mut kv = Vec::new();
        let mut boff = 0usize;
        while boff < block.len() {
            let key = take(block, &mut boff)?;
            let value = take(block, &mut boff)?;
            kv.push((key, value));
        }
        families.push((name, kv));
    }
    if off != bytes.len() {
        return Err(corrupt("checkpoint image: trailing bytes"));
    }
    Ok(Decoded { families })
}
