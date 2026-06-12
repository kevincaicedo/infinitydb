//! Fixed-capacity wire buffer pool with a typed lease state machine.
//!
//! Ports the Vortex buffer-lifecycle discipline (salvage map §24): every
//! buffer provably returns to the pool on terminal completion. The state
//! machine makes double-lease and double-release panics (programming errors,
//! never recoverable), and `reconcile()` is the leak-test hook used by the
//! 1M-cycle lifecycle test (M0-S04 AC).
//!
//! Invariants the backend drivers rely on:
//! - Buffer addresses are **stable** for the pool's lifetime (boxed slices,
//!   never reallocated) — io_uring fixed-buffer registration requires this.
//! - The pool never grows; exhaustion surfaces as `try_lease() == None`,
//!   which is the backpressure signal (master plan §5.1 "bounded everything").

use core::fmt;

/// Index of a buffer inside its pool. `Copy` because it crosses the driver
/// boundary as plain data; the lease state machine — not move semantics —
/// is what enforces single ownership (stale copies panic on misuse).
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct BufferId(u32);

impl BufferId {
    #[inline]
    pub fn as_u32(self) -> u32 {
        self.0
    }

    #[inline]
    fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum LeaseKind {
    Recv,
    Send,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum SlotState {
    Free,
    /// Driver→kernel custody (e.g. io_uring provided-buffer ring): not in
    /// use by any consumer, but not free either. Accounted separately so
    /// `reconcile()` keeps meaning "no consumer-visible leak".
    Staged,
    Leased(LeaseKind),
}

/// Lease accounting did not return to zero — a buffer leaked.
#[derive(Debug, PartialEq, Eq)]
pub struct LeaseLeak {
    pub leaked: usize,
}

impl fmt::Display for LeaseLeak {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} buffer lease(s) never returned to the pool", self.leaked)
    }
}

impl std::error::Error for LeaseLeak {}

pub struct BufferPool {
    /// Boxed slices: stable addresses for the pool's lifetime.
    storage: Vec<Box<[u8]>>,
    state: Vec<SlotState>,
    free: Vec<u32>,
    buf_size: usize,
    leased: usize,
    staged: usize,
    recv_leases: u64,
    send_leases: u64,
}

impl BufferPool {
    /// A pool of `count` buffers of `buf_size` bytes each.
    pub fn new(count: usize, buf_size: usize) -> BufferPool {
        assert!(count > 0 && buf_size > 0, "pool dimensions must be non-zero");
        assert!(count <= u32::MAX as usize, "buffer count exceeds id space");
        BufferPool {
            storage: (0..count).map(|_| vec![0u8; buf_size].into_boxed_slice()).collect(),
            state: vec![SlotState::Free; count],
            free: (0..count as u32).rev().collect(),
            buf_size,
            leased: 0,
            staged: 0,
            recv_leases: 0,
            send_leases: 0,
        }
    }

    /// Lease a buffer. `None` means the pool is dry — the caller must
    /// backpressure (stop arming recvs / stop serializing responses).
    #[inline]
    pub fn try_lease(&mut self, kind: LeaseKind) -> Option<BufferId> {
        let id = self.free.pop()?;
        self.state[id as usize] = SlotState::Leased(kind);
        self.leased += 1;
        match kind {
            LeaseKind::Recv => self.recv_leases += 1,
            LeaseKind::Send => self.send_leases += 1,
        }
        Some(BufferId(id))
    }

    /// Return a leased buffer.
    ///
    /// # Panics
    /// Panics on double-release or on an id that was never leased — both are
    /// lifecycle bugs that must fail loudly (Vortex terminal-completion proof).
    #[inline]
    pub fn release(&mut self, id: BufferId) {
        let slot = &mut self.state[id.index()];
        assert!(
            matches!(slot, SlotState::Leased(_)),
            "buffer {} released while not leased (double release?)",
            id.0
        );
        *slot = SlotState::Free;
        self.free.push(id.0);
        self.leased -= 1;
    }

    /// Stage a free buffer into driver→kernel custody (provided-buffer
    /// ring). Staged buffers count in neither `leased()` nor `available()`;
    /// they return via [`promote_staged`](Self::promote_staged) (kernel
    /// handed it to the consumer — becomes a `Recv` lease) or
    /// [`unstage`](Self::unstage) (kernel rejected it / group teardown).
    #[inline]
    pub fn try_stage(&mut self) -> Option<BufferId> {
        let id = self.free.pop()?;
        self.state[id as usize] = SlotState::Staged;
        self.staged += 1;
        Some(BufferId(id))
    }

    /// The kernel delivered a staged buffer to the consumer: it becomes an
    /// ordinary `Recv` lease (the consumer must `release` it).
    ///
    /// # Panics
    /// Panics if the buffer is not staged — a driver lifecycle bug.
    #[inline]
    pub fn promote_staged(&mut self, id: BufferId) {
        let slot = &mut self.state[id.index()];
        assert!(matches!(slot, SlotState::Staged), "buffer {} promoted while not staged", id.0);
        *slot = SlotState::Leased(LeaseKind::Recv);
        self.staged -= 1;
        self.leased += 1;
        self.recv_leases += 1;
    }

    /// Return a staged buffer to the free list without a consumer lease.
    ///
    /// # Panics
    /// Panics if the buffer is not staged — a driver lifecycle bug.
    #[inline]
    pub fn unstage(&mut self, id: BufferId) {
        let slot = &mut self.state[id.index()];
        assert!(matches!(slot, SlotState::Staged), "buffer {} unstaged while not staged", id.0);
        *slot = SlotState::Free;
        self.free.push(id.0);
        self.staged -= 1;
    }

    #[inline]
    pub fn bytes(&self, id: BufferId) -> &[u8] {
        &self.storage[id.index()]
    }

    #[inline]
    pub fn bytes_mut(&mut self, id: BufferId) -> &mut [u8] {
        &mut self.storage[id.index()]
    }

    #[inline]
    pub fn buf_size(&self) -> usize {
        self.buf_size
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.storage.len()
    }

    /// Consumer-owned leases (excludes staged buffers).
    #[inline]
    pub fn leased(&self) -> usize {
        self.leased
    }

    /// Buffers in driver→kernel custody (provided-buffer ring).
    #[inline]
    pub fn staged(&self) -> usize {
        self.staged
    }

    /// Buffers immediately available to lease or stage.
    #[inline]
    pub fn available(&self) -> usize {
        self.free.len()
    }

    /// Total bytes reserved by this pool (memory attribution: `wire_buffers_bytes`).
    #[inline]
    pub fn reserved_bytes(&self) -> usize {
        self.capacity() * self.buf_size
    }

    /// Leak check: no consumer-visible lease may remain (test/shutdown
    /// hook). Staged buffers are driver custody, accounted via `staged()`,
    /// and are not leaks — a live provided-buffer ring legitimately holds
    /// them for the driver's lifetime.
    pub fn reconcile(&self) -> Result<(), LeaseLeak> {
        if self.leased == 0 { Ok(()) } else { Err(LeaseLeak { leaked: self.leased }) }
    }

    /// Lifetime lease counts `(recv, send)` — debug counters for lifecycle tests.
    pub fn lease_counts(&self) -> (u64, u64) {
        (self.recv_leases, self.send_leases)
    }
}

impl fmt::Debug for BufferPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "BufferPool {{ capacity: {}, buf_size: {}, leased: {} }}",
            self.capacity(),
            self.buf_size,
            self.leased
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_release_storm_reconciles() {
        let mut pool = BufferPool::new(8, 64);
        let mut held = Vec::new();
        // Deterministic pseudo-random storm.
        let mut x: u64 = 0x9E3779B97F4A7C15;
        for _ in 0..100_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            if x & 1 == 0 || held.is_empty() {
                if let Some(id) = pool.try_lease(LeaseKind::Recv) {
                    held.push(id);
                }
            } else {
                let idx = (x as usize >> 1) % held.len();
                pool.release(held.swap_remove(idx));
            }
        }
        for id in held.drain(..) {
            pool.release(id);
        }
        assert_eq!(pool.reconcile(), Ok(()));
        assert_eq!(pool.leased(), 0);
    }

    #[test]
    fn exhaustion_is_backpressure_not_growth() {
        let mut pool = BufferPool::new(2, 16);
        let a = pool.try_lease(LeaseKind::Send).expect("first");
        let _b = pool.try_lease(LeaseKind::Send).expect("second");
        assert_eq!(pool.try_lease(LeaseKind::Send), None);
        pool.release(a);
        assert!(pool.try_lease(LeaseKind::Recv).is_some());
    }

    #[test]
    fn staging_lifecycle_promote_and_unstage() {
        let mut pool = BufferPool::new(2, 16);
        let a = pool.try_stage().expect("stage a");
        let b = pool.try_stage().expect("stage b");
        assert_eq!(pool.try_stage(), None);
        assert_eq!((pool.staged(), pool.leased(), pool.available()), (2, 0, 0));
        assert_eq!(pool.reconcile(), Ok(()), "staged buffers are not consumer leaks");

        // Kernel delivers `a` to the consumer: ordinary Recv lease now.
        pool.promote_staged(a);
        assert_eq!((pool.staged(), pool.leased()), (1, 1));
        assert_eq!(pool.lease_counts().0, 1, "promotion counts as a recv lease");
        pool.release(a);

        // Kernel rejects `b`: back to free without a lease.
        pool.unstage(b);
        assert_eq!((pool.staged(), pool.leased(), pool.available()), (0, 0, 2));
        assert_eq!(pool.reconcile(), Ok(()));
    }

    #[test]
    #[should_panic(expected = "promoted while not staged")]
    fn promote_unstaged_panics() {
        let mut pool = BufferPool::new(1, 16);
        let id = pool.try_lease(LeaseKind::Recv).expect("lease");
        pool.promote_staged(id);
    }

    #[test]
    #[should_panic(expected = "unstaged while not staged")]
    fn unstage_free_panics() {
        let mut pool = BufferPool::new(1, 16);
        let id = pool.try_stage().expect("stage");
        pool.unstage(id);
        pool.unstage(id);
    }

    #[test]
    #[should_panic(expected = "double release")]
    fn double_release_panics() {
        let mut pool = BufferPool::new(1, 16);
        let id = pool.try_lease(LeaseKind::Recv).expect("lease");
        pool.release(id);
        pool.release(id);
    }

    #[test]
    fn reconcile_detects_leak() {
        let mut pool = BufferPool::new(2, 16);
        let _leaked = pool.try_lease(LeaseKind::Recv);
        assert_eq!(pool.reconcile(), Err(LeaseLeak { leaked: 1 }));
    }

    #[test]
    fn buffers_are_distinct_and_sized() {
        let mut pool = BufferPool::new(2, 32);
        let a = pool.try_lease(LeaseKind::Recv).expect("a");
        let b = pool.try_lease(LeaseKind::Recv).expect("b");
        assert_ne!(a, b);
        pool.bytes_mut(a)[0] = 0xAA;
        pool.bytes_mut(b)[0] = 0xBB;
        assert_eq!(pool.bytes(a)[0], 0xAA);
        assert_eq!(pool.bytes(b)[0], 0xBB);
        assert_eq!(pool.bytes(a).len(), 32);
    }
}
