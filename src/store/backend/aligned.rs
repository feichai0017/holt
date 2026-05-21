//! `AlignedBlobBuf` — heap-allocated, 4 KB-aligned 512 KB buffer.
//!
//! All blob I/O in holt flows through this type so that:
//!
//! 1. Buffers can be handed directly to `O_DIRECT` files without a
//!    bounce copy — the kernel rejects unaligned submissions.
//! 2. Buffers can be registered with `io_uring`'s
//!    `register_buffers` for SQE-fast-path submission.
//! 3. `MemoryBackend` keeps an identical layout, so swapping
//!    backends never changes the on-the-wire shape of a blob.

use std::alloc::{alloc, alloc_zeroed, dealloc, Layout};
use std::ptr::NonNull;
#[cfg(any(test, all(target_os = "linux", feature = "io-uring")))]
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
#[cfg(any(test, all(target_os = "linux", feature = "io-uring")))]
use std::sync::Arc;

use crate::layout::PAGE_SIZE;

/// Buffer alignment in bytes. Matches the smallest NVMe physical
/// block, satisfies `O_DIRECT`'s alignment requirement on Linux,
/// and is a multiple of the page size on every supported arch.
pub const BUF_ALIGN: usize = 4096;

/// Fixed-buffer index exposed to `io_uring`'s `*_FIXED`
/// opcodes. The kernel ABI stores this index as `u16`, so the
/// allocator refuses larger pools.
pub(crate) type FixedBufferIndex = u16;

/// A process-local pool of `PAGE_SIZE` frames whose addresses stay
/// stable for the pool's lifetime.
///
/// Persistent Linux backends register this pool with their
/// `io_uring` instance once at open time. Individual
/// [`AlignedBlobBuf`] values then lease one fixed slot and return
/// it to the pool on drop. The pool itself owns the backing slab,
/// so every registered pointer remains valid until the backend
/// unregisters buffers and the final lease is dropped.
#[cfg(any(test, all(target_os = "linux", feature = "io-uring")))]
#[derive(Clone, Debug)]
pub(crate) struct BlobBufPool {
    inner: Arc<BlobBufPoolInner>,
}

#[cfg(any(test, all(target_os = "linux", feature = "io-uring")))]
#[derive(Debug)]
struct BlobBufPoolInner {
    ptr: NonNull<u8>,
    slots: usize,
    head: AtomicU64,
    next: Box<[AtomicU32]>,
}

#[cfg(any(test, all(target_os = "linux", feature = "io-uring")))]
const EMPTY_FIXED_SLOT: u32 = u32::MAX;

/// A heap-allocated, 4 KB-aligned, `PAGE_SIZE`-byte buffer.
///
/// One per logical blob in flight. Cheap to construct (single
/// `alloc`), cheap to clone (single `memcpy`). `Send + Sync` — the
/// raw pointer is the sole owner of its allocation.
pub struct AlignedBlobBuf {
    ptr: NonNull<u8>,
    owner: BlobBufOwner,
}

enum BlobBufOwner {
    Heap,
    #[cfg(any(test, all(target_os = "linux", feature = "io-uring")))]
    Pool {
        pool: Arc<BlobBufPoolInner>,
        index: FixedBufferIndex,
    },
}

impl AlignedBlobBuf {
    /// Allocate a zero-filled buffer.
    #[must_use]
    pub fn zeroed() -> Self {
        let layout = Self::layout();
        let raw = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
        Self {
            ptr,
            owner: BlobBufOwner::Heap,
        }
    }

    /// Allocate an uninitialized buffer. Caller must fill before
    /// reading (typical use: io_uring read fills it from disk).
    #[must_use]
    pub fn uninit() -> Self {
        let layout = Self::layout();
        let raw = unsafe { alloc(layout) };
        let ptr = NonNull::new(raw).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
        Self {
            ptr,
            owner: BlobBufOwner::Heap,
        }
    }

    /// Allocate an uninitialized frame from `pool`. Returns `None`
    /// when every fixed slot is currently leased; callers should
    /// fall back to [`Self::uninit`] in that case.
    #[cfg(any(test, all(target_os = "linux", feature = "io-uring")))]
    #[must_use]
    pub(crate) fn pooled_uninit(pool: &BlobBufPool) -> Option<Self> {
        let index = pool.inner.alloc_slot()?;
        let ptr = pool.inner.ptr_for_index(index);
        Some(Self {
            ptr,
            owner: BlobBufOwner::Pool {
                pool: Arc::clone(&pool.inner),
                index,
            },
        })
    }

    /// Allocate a zero-filled frame from `pool`, falling back to
    /// `None` when the pool is exhausted.
    #[cfg(any(test, all(target_os = "linux", feature = "io-uring")))]
    #[must_use]
    pub(crate) fn pooled_zeroed(pool: &BlobBufPool) -> Option<Self> {
        let mut out = Self::pooled_uninit(pool)?;
        out.as_mut_slice().fill(0);
        Some(out)
    }

    /// `io_uring` fixed-buffer slot index when this buffer comes
    /// from a registered pool.
    #[must_use]
    pub(crate) fn fixed_buffer_index(&self) -> Option<FixedBufferIndex> {
        match &self.owner {
            BlobBufOwner::Heap => None,
            #[cfg(any(test, all(target_os = "linux", feature = "io-uring")))]
            BlobBufOwner::Pool { index, .. } => Some(*index),
        }
    }

    /// Allocate a zero-filled buffer from the same pool when this
    /// buffer is pooled; otherwise allocate a normal heap buffer.
    #[must_use]
    pub(crate) fn zeroed_like(&self) -> Self {
        match &self.owner {
            BlobBufOwner::Heap => Self::zeroed(),
            #[cfg(any(test, all(target_os = "linux", feature = "io-uring")))]
            BlobBufOwner::Pool { pool, .. } => {
                let wrapper = BlobBufPool {
                    inner: Arc::clone(pool),
                };
                Self::pooled_zeroed(&wrapper).unwrap_or_else(Self::zeroed)
            }
        }
    }

    /// Raw pointer for FFI / io_uring `iovec`. Always non-null,
    /// always 4 KB-aligned, always `PAGE_SIZE` bytes valid.
    #[must_use]
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    /// Mutable raw pointer. Same invariants as [`Self::as_ptr`].
    #[must_use]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// Read-only view as a slice of `PAGE_SIZE` bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), PAGE_SIZE as usize) }
    }

    /// Mutable view as a slice of `PAGE_SIZE` bytes.
    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), PAGE_SIZE as usize) }
    }

    /// Length in bytes — always `PAGE_SIZE` (512 KB).
    #[must_use]
    pub const fn len(&self) -> usize {
        PAGE_SIZE as usize
    }

    /// `true` if `len() == 0` — always false; here for clippy.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    fn layout() -> Layout {
        Layout::from_size_align(PAGE_SIZE as usize, BUF_ALIGN)
            .expect("PAGE_SIZE/BUF_ALIGN both > 0 and BUF_ALIGN is a power of two")
    }
}

impl Drop for AlignedBlobBuf {
    fn drop(&mut self) {
        match &self.owner {
            BlobBufOwner::Heap => unsafe { dealloc(self.ptr.as_ptr(), Self::layout()) },
            #[cfg(any(test, all(target_os = "linux", feature = "io-uring")))]
            BlobBufOwner::Pool { pool, index } => pool.free_slot(*index),
        }
    }
}

impl Default for AlignedBlobBuf {
    fn default() -> Self {
        Self::zeroed()
    }
}

impl Clone for AlignedBlobBuf {
    fn clone(&self) -> Self {
        let mut out = match &self.owner {
            BlobBufOwner::Heap => Self::uninit(),
            #[cfg(any(test, all(target_os = "linux", feature = "io-uring")))]
            BlobBufOwner::Pool { pool, .. } => {
                let wrapper = BlobBufPool {
                    inner: Arc::clone(pool),
                };
                Self::pooled_uninit(&wrapper).unwrap_or_else(Self::uninit)
            }
        };
        out.as_mut_slice().copy_from_slice(self.as_slice());
        out
    }
}

impl std::fmt::Debug for AlignedBlobBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "AlignedBlobBuf({:p}, {} B, fixed={:?})",
            self.ptr.as_ptr(),
            PAGE_SIZE,
            self.fixed_buffer_index(),
        )
    }
}

// SAFETY: AlignedBlobBuf owns its allocation exclusively (no
// aliasing) and exposes Rust's normal &/&mut borrow rules through
// as_slice / as_mut_slice. Sending the owning struct across threads
// is therefore sound.
unsafe impl Send for AlignedBlobBuf {}
unsafe impl Sync for AlignedBlobBuf {}

#[cfg(any(test, all(target_os = "linux", feature = "io-uring")))]
impl BlobBufPool {
    /// Allocate `slots` fixed frames. Returns `None` for `0` slots
    /// or for a pool larger than the `io_uring` `u16` fixed-buffer
    /// index space.
    #[must_use]
    pub(crate) fn new(slots: usize) -> Option<Self> {
        if slots == 0 || slots > usize::from(FixedBufferIndex::MAX) + 1 {
            return None;
        }
        let size = (PAGE_SIZE as usize).checked_mul(slots)?;
        let layout = Layout::from_size_align(size, BUF_ALIGN).ok()?;
        let raw = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
        let next = (0..slots)
            .map(|idx| {
                let next = idx.saturating_add(1);
                let next = if next < slots {
                    next as u32
                } else {
                    EMPTY_FIXED_SLOT
                };
                AtomicU32::new(next)
            })
            .collect();
        Some(Self {
            inner: Arc::new(BlobBufPoolInner {
                ptr,
                slots,
                head: AtomicU64::new(pack_free_head(0, 0)),
                next,
            }),
        })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn iovecs(&self) -> Vec<libc::iovec> {
        (0..self.inner.slots)
            .map(|idx| libc::iovec {
                iov_base: self
                    .inner
                    .ptr_for_index(idx as FixedBufferIndex)
                    .as_ptr()
                    .cast(),
                iov_len: PAGE_SIZE as usize,
            })
            .collect()
    }
}

#[cfg(any(test, all(target_os = "linux", feature = "io-uring")))]
impl BlobBufPoolInner {
    fn alloc_slot(&self) -> Option<FixedBufferIndex> {
        loop {
            let head = self.head.load(Ordering::Acquire);
            let (tag, index) = unpack_free_head(head);
            if index == EMPTY_FIXED_SLOT {
                return None;
            }
            debug_assert!((index as usize) < self.slots);
            let next = self.next[index as usize].load(Ordering::Relaxed);
            let new_head = pack_free_head(tag.wrapping_add(1), next);
            if self
                .head
                .compare_exchange_weak(head, new_head, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(index as FixedBufferIndex);
            }
            std::hint::spin_loop();
        }
    }

    fn free_slot(&self, index: FixedBufferIndex) {
        debug_assert!((index as usize) < self.slots);
        let index = u32::from(index);
        loop {
            let head = self.head.load(Ordering::Acquire);
            let (tag, old_head) = unpack_free_head(head);
            self.next[index as usize].store(old_head, Ordering::Relaxed);
            let new_head = pack_free_head(tag.wrapping_add(1), index);
            if self
                .head
                .compare_exchange_weak(head, new_head, Ordering::Release, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
            std::hint::spin_loop();
        }
    }

    fn ptr_for_index(&self, index: FixedBufferIndex) -> NonNull<u8> {
        debug_assert!((index as usize) < self.slots);
        let offset = (index as usize) * PAGE_SIZE as usize;
        unsafe { NonNull::new_unchecked(self.ptr.as_ptr().add(offset)) }
    }
}

#[cfg(any(test, all(target_os = "linux", feature = "io-uring")))]
const fn pack_free_head(tag: u32, index: u32) -> u64 {
    ((tag as u64) << 32) | index as u64
}

#[cfg(any(test, all(target_os = "linux", feature = "io-uring")))]
const fn unpack_free_head(head: u64) -> (u32, u32) {
    ((head >> 32) as u32, head as u32)
}

#[cfg(any(test, all(target_os = "linux", feature = "io-uring")))]
impl Drop for BlobBufPoolInner {
    fn drop(&mut self) {
        let size = (PAGE_SIZE as usize)
            .checked_mul(self.slots)
            .expect("pool size was checked at construction");
        let layout = Layout::from_size_align(size, BUF_ALIGN)
            .expect("pool layout was checked at construction");
        unsafe { dealloc(self.ptr.as_ptr(), layout) };
    }
}

// SAFETY: BlobBufPoolInner owns one slab. Slot leasing is protected
// by the tagged atomic free-list; each live AlignedBlobBuf has
// exclusive ownership of its slot, so Send/Sync match the
// heap-backed buffer contract.
#[cfg(any(test, all(target_os = "linux", feature = "io-uring")))]
unsafe impl Send for BlobBufPoolInner {}
#[cfg(any(test, all(target_os = "linux", feature = "io-uring")))]
unsafe impl Sync for BlobBufPoolInner {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zeroed_is_zeroed() {
        let b = AlignedBlobBuf::zeroed();
        assert_eq!(b.len(), PAGE_SIZE as usize);
        assert!(b.as_slice().iter().all(|&x| x == 0));
    }

    #[test]
    fn pointer_is_4k_aligned() {
        for _ in 0..16 {
            let b = AlignedBlobBuf::zeroed();
            assert_eq!(b.as_ptr() as usize % BUF_ALIGN, 0);
        }
    }

    #[test]
    fn clone_is_independent_memcpy() {
        let mut a = AlignedBlobBuf::zeroed();
        a.as_mut_slice()[100] = 0xAB;
        let mut b = a.clone();
        assert_eq!(b.as_slice()[100], 0xAB);
        b.as_mut_slice()[100] = 0xCD;
        assert_eq!(a.as_slice()[100], 0xAB, "clone must not alias source");
        assert_eq!(b.as_slice()[100], 0xCD);
    }

    #[test]
    fn pooled_buffer_returns_fixed_index() {
        let pool = BlobBufPool::new(2).unwrap();
        let a = AlignedBlobBuf::pooled_zeroed(&pool).unwrap();
        let b = AlignedBlobBuf::pooled_zeroed(&pool).unwrap();
        assert_eq!(a.len(), PAGE_SIZE as usize);
        assert!(a.fixed_buffer_index().is_some());
        assert!(b.fixed_buffer_index().is_some());
        assert_ne!(a.fixed_buffer_index(), b.fixed_buffer_index());
        assert!(AlignedBlobBuf::pooled_uninit(&pool).is_none());
        drop(a);
        assert!(AlignedBlobBuf::pooled_uninit(&pool).is_some());
    }

    #[test]
    fn pooled_buffer_free_list_survives_concurrent_churn() {
        let pool = BlobBufPool::new(8).unwrap();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let pool = pool.clone();
                scope.spawn(move || {
                    for _ in 0..1000 {
                        let mut b = loop {
                            if let Some(b) = AlignedBlobBuf::pooled_uninit(&pool) {
                                break b;
                            }
                            std::hint::spin_loop();
                        };
                        b.as_mut_slice()[0] = 0x7B;
                    }
                });
            }
        });
        let leased: Vec<_> = (0..8)
            .map(|_| AlignedBlobBuf::pooled_uninit(&pool).unwrap())
            .collect();
        assert!(AlignedBlobBuf::pooled_uninit(&pool).is_none());
        drop(leased);
        assert!(AlignedBlobBuf::pooled_uninit(&pool).is_some());
    }

    #[test]
    fn uninit_is_writable() {
        let mut b = AlignedBlobBuf::uninit();
        b.as_mut_slice().fill(0x42);
        assert!(b.as_slice().iter().all(|&x| x == 0x42));
    }

    #[test]
    fn default_equals_zeroed() {
        let b = AlignedBlobBuf::default();
        assert!(b.as_slice().iter().all(|&x| x == 0));
    }
}
