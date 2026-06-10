//! In-memory store.
//!
//! Stores each blob as an [`AlignedBlobBuf`] inside an
//! `RwLock<HashMap>`. Read-heavy workloads scale across cores; the
//! write path takes a brief exclusive lock to insert/replace.

use std::collections::HashMap;
use std::io;
use std::sync::RwLock;

use crate::api::errors::{Error, Result};
use crate::layout::BlobGuid;

use super::{AlignedBlobBuf, BlobStore};

/// Concurrent in-memory blob store.
///
/// Suitable for tests, ephemeral trees, and embedded use cases where
/// the working set fits comfortably in RAM and durability is not
/// required.
#[derive(Debug, Default)]
pub struct MemoryBlobStore {
    inner: RwLock<HashMap<BlobGuid, AlignedBlobBuf>>,
}

impl MemoryBlobStore {
    /// Construct an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of blobs currently stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.read().unwrap().len()
    }

    /// True if no blobs are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.read().unwrap().is_empty()
    }
}

impl BlobStore for MemoryBlobStore {
    fn read_blob(&self, guid: BlobGuid, dst: &mut AlignedBlobBuf) -> Result<()> {
        let g = self.inner.read().unwrap();
        let src = g.get(&guid).ok_or_else(|| {
            Error::BlobStoreIo(io::Error::new(
                io::ErrorKind::NotFound,
                format!("blob {:02x?} not found", &guid[..4]),
            ))
        })?;
        dst.as_mut_slice().copy_from_slice(src.as_slice());
        Ok(())
    }

    fn read_blob_range(&self, guid: BlobGuid, byte_offset: u64, dst: &mut [u8]) -> Result<()> {
        let g = self.inner.read().unwrap();
        let src = g.get(&guid).ok_or_else(|| {
            Error::BlobStoreIo(io::Error::new(
                io::ErrorKind::NotFound,
                format!("blob {:02x?} not found", &guid[..4]),
            ))
        })?;
        let start = byte_offset as usize;
        dst.copy_from_slice(&src.as_slice()[start..start + dst.len()]);
        Ok(())
    }

    fn write_blob(&self, guid: BlobGuid, src: &AlignedBlobBuf) -> Result<()> {
        let mut g = self.inner.write().unwrap();
        g.insert(guid, src.clone());
        Ok(())
    }

    fn write_blobs(&self, writes: &[(BlobGuid, &AlignedBlobBuf)]) -> Result<()> {
        let mut g = self.inner.write().unwrap();
        for (guid, src) in writes {
            g.insert(*guid, (*src).clone());
        }
        Ok(())
    }

    fn delete_blob(&self, guid: BlobGuid) -> Result<()> {
        let mut g = self.inner.write().unwrap();
        g.remove(&guid);
        Ok(())
    }

    fn list_blobs(&self) -> Result<Vec<BlobGuid>> {
        let g = self.inner.read().unwrap();
        Ok(g.keys().copied().collect())
    }

    fn flush(&self) -> Result<()> {
        // RAM is durable as long as the process lives.
        Ok(())
    }

    fn needs_flush(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::PAGE_SIZE;

    fn buf_with(byte_at_100: u8) -> AlignedBlobBuf {
        let mut b = AlignedBlobBuf::zeroed();
        b.as_mut_slice()[100] = byte_at_100;
        b
    }

    #[test]
    fn write_then_read_round_trip() {
        let b = MemoryBlobStore::new();
        let g: BlobGuid = [0xAB; 16];
        b.write_blob(g, &buf_with(42)).unwrap();

        let mut dst = AlignedBlobBuf::zeroed();
        b.read_blob(g, &mut dst).unwrap();
        assert_eq!(dst.as_slice()[100], 42);
        assert!(b.has_blob(g).unwrap());
    }

    #[test]
    fn delete_removes_the_blob() {
        let b = MemoryBlobStore::new();
        let g: BlobGuid = [0xCD; 16];
        b.write_blob(g, &buf_with(7)).unwrap();
        b.delete_blob(g).unwrap();
        assert!(!b.has_blob(g).unwrap());
        let mut dst = AlignedBlobBuf::zeroed();
        assert!(b.read_blob(g, &mut dst).is_err());
    }

    #[test]
    fn write_replaces_existing() {
        let b = MemoryBlobStore::new();
        let g: BlobGuid = [0xEF; 16];
        b.write_blob(g, &buf_with(1)).unwrap();
        b.write_blob(g, &buf_with(99)).unwrap();
        let mut dst = AlignedBlobBuf::zeroed();
        b.read_blob(g, &mut dst).unwrap();
        assert_eq!(dst.as_slice()[100], 99);
    }

    #[test]
    fn list_returns_every_inserted_guid() {
        let b = MemoryBlobStore::new();
        for i in 0..8 {
            let g: BlobGuid = [i as u8; 16];
            b.write_blob(g, &buf_with(i as u8)).unwrap();
        }
        let mut listed = b.list_blobs().unwrap();
        listed.sort();
        assert_eq!(listed.len(), 8);
        for (i, g) in listed.iter().enumerate() {
            assert_eq!(*g, [i as u8; 16]);
        }
    }

    #[test]
    fn flush_is_noop_and_idempotent() {
        let b = MemoryBlobStore::new();
        b.flush().unwrap();
        b.flush().unwrap();
    }

    #[test]
    fn read_into_caller_buffer_does_not_share_storage() {
        let b = MemoryBlobStore::new();
        let g: BlobGuid = [0x11; 16];
        b.write_blob(g, &buf_with(5)).unwrap();

        let mut dst = AlignedBlobBuf::zeroed();
        b.read_blob(g, &mut dst).unwrap();
        dst.as_mut_slice()[100] = 0;

        // Stored buf must still hold 5 — read returned a copy.
        let mut dst2 = AlignedBlobBuf::zeroed();
        b.read_blob(g, &mut dst2).unwrap();
        assert_eq!(dst2.as_slice()[100], 5);
    }

    #[test]
    fn concurrent_readers_do_not_block() {
        use std::sync::Arc;
        use std::thread;

        let b = Arc::new(MemoryBlobStore::new());
        let g: BlobGuid = [0x77; 16];
        b.write_blob(g, &buf_with(123)).unwrap();

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let b = b.clone();
                thread::spawn(move || {
                    for _ in 0..100 {
                        let mut dst = AlignedBlobBuf::zeroed();
                        b.read_blob(g, &mut dst).unwrap();
                        assert_eq!(dst.as_slice()[100], 123);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(b.len(), 1);
        assert_eq!(b.list_blobs().unwrap().len(), 1);
        let _ = PAGE_SIZE;
    }
}
