//! Aligned buffer pool for cold-tier reads (M4-S04).
//!
//! Fixed capacity, fixed buffer size, 4 KiB alignment — the shape both
//! buffered and `O_DIRECT` positional reads accept (S09 decides the
//! default mode; the alignment discipline is paid up front either way).
//! Addresses are stable for the pool's lifetime, so M4-S08 can register
//! the pool with io_uring (fixed buffers) without moving a byte.
//!
//! Custody mirrors [`crate::BufferPool`]: `try_lease() == None` is
//! backpressure, double-release panics, [`AlignedPool::reconcile`] is the
//! leak hook. An [`AlignedBufId`] is plain `Copy` data — it may cross a
//! suspension point; a borrow of the bytes may not (the M0 custody rule).
//!
//! [`AlignedBox`] is the one-off escape hatch for reads larger than a
//! pool buffer (a cold record near the 16 MiB inline bound needs an
//! exact-size window). S08's bounded chunked staging replaces those
//! oversized single reads; the type stays for tests and tooling.

use core::alloc::Layout;

/// Cold-read alignment: 4 KiB — the logical-block ceiling of every NVMe
/// namespace this product targets (S09's O_DIRECT A/B relies on it).
pub const TIER_READ_ALIGN: usize = 4096;

/// Pool buffer handle. Plain data (`Copy`): legal to hold across a
/// suspension, unlike any borrow of the buffer's bytes.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct AlignedBufId(u32);

impl AlignedBufId {
    #[must_use]
    pub fn as_u32(self) -> u32 {
        self.0
    }
}

/// Leak report from [`AlignedPool::reconcile`].
#[derive(Debug, PartialEq, Eq)]
pub struct AlignedLeak {
    pub leased: usize,
}

/// Fixed pool of 4 KiB-aligned read buffers (one contiguous reservation;
/// buffer `i` lives at `base + i * buf_size`).
pub struct AlignedPool {
    base: *mut u8,
    buf_size: usize,
    leased: Box<[bool]>,
    leased_count: usize,
}

impl AlignedPool {
    /// Allocates `count` zeroed buffers of `buf_size` bytes each.
    ///
    /// # Panics
    /// Panics on config violations (`count == 0`, `buf_size` not a
    /// multiple of [`TIER_READ_ALIGN`]) and on allocation failure — the
    /// pool is created at cell boot with bounded, configured sizes, so
    /// failure there is fail-fast, not an operating condition.
    pub fn new(count: usize, buf_size: usize) -> AlignedPool {
        assert!(count > 0, "empty pool");
        assert!(buf_size > 0, "empty buffers");
        assert!(
            buf_size.is_multiple_of(TIER_READ_ALIGN),
            "buf_size must be a multiple of the alignment"
        );
        let layout = Layout::from_size_align(count * buf_size, TIER_READ_ALIGN)
            .expect("pool layout within isize::MAX");
        // SAFETY: layout has non-zero size (asserted above); the result is
        // checked for null before use; zeroed so a never-filled buffer
        // reads as initialized bytes, never uninit.
        let base = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!base.is_null(), "aligned pool allocation failed");
        AlignedPool {
            base,
            buf_size,
            leased: vec![false; count].into_boxed_slice(),
            leased_count: 0,
        }
    }

    /// Buffer size in bytes (every buffer identical).
    #[must_use]
    pub fn buf_size(&self) -> usize {
        self.buf_size
    }

    /// Total buffers.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.leased.len()
    }

    /// Buffers currently leased out.
    #[must_use]
    pub fn leased(&self) -> usize {
        self.leased_count
    }

    /// Attributed bytes (L5): the whole reservation, leased or not.
    #[must_use]
    pub fn reserved_bytes(&self) -> u64 {
        (self.leased.len() * self.buf_size) as u64
    }

    /// Leases one buffer. `None` when the pool is dry — backpressure,
    /// never an error (the caller parks or degrades).
    pub fn try_lease(&mut self) -> Option<AlignedBufId> {
        let free = self.leased.iter().position(|leased| !leased)?;
        self.leased[free] = true;
        self.leased_count += 1;
        Some(AlignedBufId(free as u32))
    }

    /// Returns a leased buffer.
    ///
    /// # Panics
    /// Panics on double-release or a never-leased id — lifecycle bugs
    /// that must fail loudly (the M0 buffer-pool contract).
    pub fn release(&mut self, id: AlignedBufId) {
        let slot = &mut self.leased[id.0 as usize];
        assert!(*slot, "release of a non-leased aligned buffer {}", id.0);
        *slot = false;
        self.leased_count -= 1;
    }

    /// Borrows a buffer's bytes.
    #[must_use]
    pub fn bytes(&self, id: AlignedBufId) -> &[u8] {
        assert!((id.0 as usize) < self.leased.len(), "aligned buffer id out of range");
        let offset = id.0 as usize * self.buf_size;
        // SAFETY: offset is in bounds by the always-on id check above; the
        // allocation lives for the pool's lifetime; `&self` provides
        // aliasing discipline for the returned borrow.
        unsafe { core::slice::from_raw_parts(self.base.add(offset), self.buf_size) }
    }

    /// Mutable variant of [`bytes`](Self::bytes).
    #[must_use]
    pub fn bytes_mut(&mut self, id: AlignedBufId) -> &mut [u8] {
        assert!((id.0 as usize) < self.leased.len(), "aligned buffer id out of range");
        let offset = id.0 as usize * self.buf_size;
        // SAFETY: as `bytes`, and `&mut self` makes the borrow exclusive.
        unsafe { core::slice::from_raw_parts_mut(self.base.add(offset), self.buf_size) }
    }

    /// Leak hook: every lease must be back (run after op storms).
    ///
    /// # Errors
    /// The number of still-leased buffers.
    pub fn reconcile(&self) -> Result<(), AlignedLeak> {
        if self.leased_count == 0 { Ok(()) } else { Err(AlignedLeak { leased: self.leased_count }) }
    }
}

impl Drop for AlignedPool {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.leased.len() * self.buf_size, TIER_READ_ALIGN)
            .expect("validated at construction");
        // SAFETY: base/layout are exactly the live allocation made in
        // `new`, owned by the pool for its whole lifetime.
        unsafe { std::alloc::dealloc(self.base, layout) };
    }
}

impl core::fmt::Debug for AlignedPool {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AlignedPool")
            .field("capacity", &self.capacity())
            .field("buf_size", &self.buf_size)
            .field("leased", &self.leased_count)
            .finish()
    }
}

/// One exact-size aligned allocation (oversized cold reads — see the
/// module doc). Zeroed at birth.
pub struct AlignedBox {
    ptr: *mut u8,
    len: usize,
}

impl AlignedBox {
    /// # Panics
    /// Panics unless `len` is a non-zero multiple of
    /// [`TIER_READ_ALIGN`], and on allocation failure.
    #[must_use]
    pub fn new(len: usize) -> AlignedBox {
        assert!(len > 0, "empty aligned box");
        assert!(len.is_multiple_of(TIER_READ_ALIGN), "len must be a multiple of the alignment");
        let layout =
            Layout::from_size_align(len, TIER_READ_ALIGN).expect("box layout within isize::MAX");
        // SAFETY: non-zero size (asserted); null-checked; zeroed.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "aligned box allocation failed");
        AlignedBox { ptr, len }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        false // len > 0 by construction
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: ptr/len are the live allocation made in `new`; `&self`
        // provides aliasing discipline.
        unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
    }

    #[must_use]
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as `bytes`, and `&mut self` makes the borrow exclusive.
        unsafe { core::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for AlignedBox {
    fn drop(&mut self) {
        let layout =
            Layout::from_size_align(self.len, TIER_READ_ALIGN).expect("validated at construction");
        // SAFETY: ptr/layout are exactly the live allocation made in `new`.
        unsafe { std::alloc::dealloc(self.ptr, layout) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_write_read_release_round_trip() {
        let mut pool = AlignedPool::new(4, TIER_READ_ALIGN);
        let id = pool.try_lease().expect("fresh pool");
        assert_eq!(pool.bytes(id).as_ptr() as usize % TIER_READ_ALIGN, 0, "aligned");
        assert!(pool.bytes(id).iter().all(|&b| b == 0), "zeroed at birth");
        pool.bytes_mut(id).fill(0xAB);
        assert!(pool.bytes(id).iter().all(|&b| b == 0xAB));
        pool.release(id);
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    fn exhaustion_is_backpressure_not_error() {
        let mut pool = AlignedPool::new(2, TIER_READ_ALIGN);
        let a = pool.try_lease().expect("one");
        let _b = pool.try_lease().expect("two");
        assert!(pool.try_lease().is_none(), "dry pool refuses");
        pool.release(a);
        assert!(pool.try_lease().is_some(), "released buffer re-leases");
    }

    #[test]
    fn every_buffer_is_aligned_and_disjoint() {
        let mut pool = AlignedPool::new(3, 2 * TIER_READ_ALIGN);
        let ids: Vec<_> = (0..3).map(|_| pool.try_lease().expect("fits")).collect();
        for (i, &id) in ids.iter().enumerate() {
            assert_eq!(pool.bytes(id).as_ptr() as usize % TIER_READ_ALIGN, 0);
            pool.bytes_mut(id).fill(i as u8 + 1);
        }
        for (i, &id) in ids.iter().enumerate() {
            assert!(pool.bytes(id).iter().all(|&b| b == i as u8 + 1), "buffers are disjoint");
        }
        assert_eq!(pool.reconcile(), Err(AlignedLeak { leased: 3 }));
    }

    #[test]
    #[should_panic(expected = "release of a non-leased")]
    fn double_release_panics() {
        let mut pool = AlignedPool::new(1, TIER_READ_ALIGN);
        let id = pool.try_lease().expect("fresh pool");
        pool.release(id);
        pool.release(id);
    }

    #[test]
    fn aligned_box_round_trip() {
        let mut oversized = AlignedBox::new(4 * TIER_READ_ALIGN);
        assert_eq!(oversized.bytes().as_ptr() as usize % TIER_READ_ALIGN, 0);
        assert!(oversized.bytes().iter().all(|&b| b == 0), "zeroed at birth");
        oversized.bytes_mut()[TIER_READ_ALIGN] = 0x5A;
        assert_eq!(oversized.bytes()[TIER_READ_ALIGN], 0x5A);
        assert_eq!(oversized.len(), 4 * TIER_READ_ALIGN);
    }
}
