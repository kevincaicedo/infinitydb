//! The simulated network + [`SimDriver`]: per-cell in-memory connections
//! behind the real `BackendDriver` contract. Deterministic by construction:
//! `BTreeMap` iteration order, seeded chunk sizes, no wall clock, no real
//! syscalls.

use core::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::rc::Rc;

use inf_alloc::{BufferPool, LeaseKind};
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::{Clock, Nanos, VirtualClock};
use inf_runtime::{
    BackendDriver, Capabilities, Completion, CompletionResult, CompletionToken, IoClass, IoOp,
    RawFd, StableBytes, StableBytesMut, SubmitStats, Wait, WriteBarrier,
};
use inf_server::SimDisk;

/// Fault plants (armed per scenario, fire on seeded draws).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum Plant {
    #[default]
    None,
    /// Drop one recv readiness edge: the connection's pending bytes stop
    /// being delivered until *new* bytes arrive (the classic lost wakeup).
    /// Sequential clients never send again before the reply ⇒ stall.
    LostWakeup,
    /// The M2-S19 durability-oracle canary (ADR-0021 D4): every driver
    /// fsync completes `Synced` **without flushing the disk** — the
    /// watermark advances, `always` acks release, and a power cut then
    /// eats acked bytes. Models any path that acks ahead of durable
    /// coverage; the oracle must catch it within 1,000 seeds.
    FsyncLies,
}

#[derive(Debug, Default)]
struct SimConn {
    to_server: VecDeque<u8>,
    to_client: Vec<u8>,
    client_closed: bool,
    server_closed: bool,
    recv_armed: bool,
    recv_token: Option<CompletionToken>,
    /// Lost-wakeup plant fired here: delivery suppressed until new bytes.
    suppressed: bool,
}

/// One cell's network endpoint: the listener plus every connection accepted
/// by this cell. The harness holds the same handle to play the client side.
#[derive(Debug)]
pub struct CellNet {
    cell: u16,
    accept_armed: bool,
    accept_token: Option<CompletionToken>,
    backlog: VecDeque<RawFd>,
    conns: BTreeMap<RawFd, SimConn>,
    next_fd: RawFd,
    rng: SplitMix64,
    plant: Plant,
    plant_fired: bool,
}

/// Synthetic listener fd for a cell (never a real fd).
pub fn listener_fd(cell: u16) -> RawFd {
    1_000_000 + i32::from(cell)
}

impl CellNet {
    pub fn new(cell: u16, seed: u64, plant: Plant) -> Rc<RefCell<CellNet>> {
        Rc::new(RefCell::new(CellNet {
            cell,
            accept_armed: false,
            accept_token: None,
            backlog: VecDeque::new(),
            conns: BTreeMap::new(),
            next_fd: 0,
            rng: SplitMix64::new(seed ^ 0xD15C_0000 ^ u64::from(cell)),
            plant,
            plant_fired: false,
        }))
    }

    /// Client side: open a connection to this cell; returns the fd handle.
    pub fn connect(&mut self) -> RawFd {
        self.next_fd += 1;
        let fd = i32::from(self.cell) * 100_000 + self.next_fd;
        self.conns.insert(fd, SimConn::default());
        self.backlog.push_back(fd);
        fd
    }

    /// Client side: send bytes. New arrivals clear a suppressed-delivery
    /// plant (edge-triggered semantics: the lost wakeup heals only on new
    /// data — which a reply-waiting client never produces).
    pub fn client_send(&mut self, fd: RawFd, bytes: &[u8]) {
        if let Some(conn) = self.conns.get_mut(&fd) {
            conn.to_server.extend(bytes);
            conn.suppressed = false;
        }
    }

    /// Client side: drain reply bytes.
    pub fn client_recv(&mut self, fd: RawFd) -> Vec<u8> {
        match self.conns.get_mut(&fd) {
            Some(conn) => core::mem::take(&mut conn.to_client),
            None => Vec::new(),
        }
    }

    /// Client side: half-close (FIN). The server reaps EOF and closes.
    pub fn client_close(&mut self, fd: RawFd) {
        if let Some(conn) = self.conns.get_mut(&fd) {
            conn.client_closed = true;
            conn.suppressed = false;
        }
    }

    /// True once the server closed its side too (teardown complete).
    pub fn closed(&self, fd: RawFd) -> bool {
        self.conns.get(&fd).is_none_or(|c| c.server_closed)
    }

    /// Total undelivered client→server bytes (progress accounting).
    pub fn pending_bytes(&self) -> usize {
        self.conns.values().map(|c| c.to_server.len()).sum()
    }
}

/// `BackendDriver` over a [`CellNet`]. One per cell. Durable scenarios
/// attach a [`SimDisk`] (M2-S18, ADR-0020 D7): `LogWrite`/`Fdatasync`
/// execute against its volatile/durable layers — a write completes
/// `LogWritten` (page-cache semantics, NOT durable), an fsync flushes
/// and completes `Synced`, and failure semantics mirror the uring
/// contract (a failed write surfaces its linked sync as `ECANCELED`; a
/// dead disk completes ops with `EIO`).
#[derive(Debug)]
pub struct SimDriver {
    net: Rc<RefCell<CellNet>>,
    disk: Option<SimDisk>,
    /// Injected clock (M2.5-S14): armed together with a stall-modeled
    /// disk so fsync completions land later in virtual time. `None`
    /// keeps the legacy instant-fsync driver byte-identical.
    clock: Option<Rc<VirtualClock>>,
    /// Deferred fsync CQEs, due-time order (the disk's serial timeline
    /// keeps due times monotone, so pushes append in order).
    pending_syncs: Vec<PendingSync>,
    ops: Vec<IoOp>,
    stats: SubmitStats,
    /// Bytes/ops observed per class (ADR-0088 D8).
    observed: ObservedIo,
}

/// One deferred op: applied AND completed only once the virtual clock
/// passes `due` — during the service window the bytes stay volatile, so
/// a power cut eats them (the honest-stall invariant). Dir-vs-file
/// routing happens at release time via `driver_fdatasync`, which already
/// branches on dir fds. A write-through barrier (ADR-0086 D8) carries its
/// payload: the bytes reach the disk at `due`, never earlier — a cut
/// inside the service window loses the whole frame, exactly the un-acked
/// shape the oracle must tolerate. A plain write (ADR-0087 D7) lands in
/// the volatile layer at `due`; its linked fsync, if any, is scheduled on
/// the flush timeline only then (`IO_LINK`: the sync starts after the
/// write), so a standalone fdatasync issued *earlier* can run before the
/// write lands — the coverage hole the drain rule exists for.
#[derive(Debug)]
struct PendingSync {
    due: Nanos,
    fd: i32,
    token: CompletionToken,
    kind: PendingKind,
}

#[derive(Debug)]
enum PendingKind {
    Fsync,
    WriteThrough {
        offset: u64,
        data: StableBytes,
    },
    Write {
        offset: u64,
        data: StableBytes,
        linked: Option<CompletionToken>,
    },
    /// A tier read under the bandwidth model (ADR-0088 D8): the buffer is
    /// filled at its due time, never before.
    Read {
        offset: u64,
        buf: StableBytesMut,
    },
}

/// Device bytes and ops the driver observed per class (ADR-0088 D8 —
/// the accounting oracle's other side of `io_budget_bytes_*`). Reads
/// carry no class in their token: they are counted as `reads`, compared
/// to the two cold-read classes together.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ObservedIo {
    pub bytes: [u64; IoClass::COUNT],
    pub ops: [u64; IoClass::COUNT],
    pub read_bytes: u64,
    pub read_ops: u64,
}

impl ObservedIo {
    fn note(&mut self, token: CompletionToken, bytes: u64) {
        if let Some(class) = IoClass::of(token.class()) {
            self.bytes[class.index()] = self.bytes[class.index()].saturating_add(bytes);
            self.ops[class.index()] += 1;
        }
    }
}

impl SimDriver {
    pub fn new(net: Rc<RefCell<CellNet>>) -> SimDriver {
        SimDriver {
            net,
            disk: None,
            clock: None,
            pending_syncs: Vec::new(),
            ops: Vec::new(),
            stats: SubmitStats::default(),
            observed: ObservedIo::default(),
        }
    }

    /// A driver whose file ops execute against `disk` (M2-S18).
    pub fn with_disk(net: Rc<RefCell<CellNet>>, disk: SimDisk) -> SimDriver {
        SimDriver { disk: Some(disk), ..SimDriver::new(net) }
    }

    /// M2.5-S14: a durable driver with the injected clock the stall
    /// device needs. With no stall model armed on `disk` this behaves
    /// exactly like [`Self::with_disk`] (fsyncs complete inline).
    pub fn with_disk_stall(
        net: Rc<RefCell<CellNet>>,
        disk: SimDisk,
        clock: Rc<VirtualClock>,
    ) -> SimDriver {
        SimDriver { disk: Some(disk), clock: Some(clock), ..SimDriver::new(net) }
    }

    /// Draws a deferred completion time for one fsync, `None` on the
    /// legacy inline path (no clock or no stall model armed).
    fn schedule_sync(&self, disk: &SimDisk) -> Option<Nanos> {
        let clock = self.clock.as_ref()?;
        disk.schedule_fsync(clock.now().0).map(Nanos)
    }

    /// Draws a deferred completion time for one write-through barrier
    /// (ADR-0086 D8), `None` on the inline path.
    fn schedule_through(&self, disk: &SimDisk, len: u64) -> Option<Nanos> {
        let clock = self.clock.as_ref()?;
        disk.schedule_write_through(clock.now().0, len).map(Nanos)
    }

    /// Draws a deferred completion time for one plain write (ADR-0087
    /// D7), `None` on the inline path.
    fn schedule_write(&self, disk: &SimDisk, len: u64) -> Option<Nanos> {
        let clock = self.clock.as_ref()?;
        disk.schedule_write(clock.now().0, len).map(Nanos)
    }

    /// Draws a deferred completion time for one tier read (ADR-0088 D8),
    /// `None` on the inline path (no read bandwidth modeled).
    fn schedule_read(&self, disk: &SimDisk, len: u64) -> Option<Nanos> {
        let clock = self.clock.as_ref()?;
        disk.schedule_read(clock.now().0, len).map(Nanos)
    }

    /// The per-class device ledger (ADR-0088 D8 oracle input).
    pub fn observed_io(&self) -> ObservedIo {
        self.observed
    }
}

/// Queue a deferred op in due order. Write-through and plain-write draws
/// do not ride the serial flush timeline, so they may be due before an
/// earlier-queued fsync — insert, never append. A free function over the
/// field so the drain loop's `net` borrow stays disjoint.
fn defer(pending_syncs: &mut Vec<PendingSync>, pending: PendingSync) {
    let at = pending_syncs.partition_point(|p| p.due <= pending.due);
    pending_syncs.insert(at, pending);
}

/// Execute one write-through barrier against the disk (ADR-0086 D8):
/// durable at completion. `Plant::FsyncLies` applies — a `LogWritten`
/// that persisted nothing is the canary the ack-stream oracle must catch.
fn write_through(
    disk: &SimDisk,
    plant: Plant,
    fd: i32,
    offset: u64,
    data: &StableBytes,
) -> CompletionResult {
    let result = if plant == Plant::FsyncLies {
        disk.driver_write_at(fd, offset, stable_slice(data))
    } else {
        disk.driver_write_through(fd, offset, stable_slice(data))
    };
    match result {
        Ok(()) => CompletionResult::LogWritten,
        Err(err) => CompletionResult::Error { errno: write_errno(&err), buf: None },
    }
}

/// Execute one deferred plain write against the disk's volatile layer
/// (ADR-0087 D7): `LogWritten` means reached the file, never durable.
fn plain_write(disk: &SimDisk, fd: i32, offset: u64, data: &StableBytes) -> CompletionResult {
    match disk.driver_write_at(fd, offset, stable_slice(data)) {
        Ok(()) => CompletionResult::LogWritten,
        Err(err) => CompletionResult::Error { errno: write_errno(&err), buf: None },
    }
}

/// The errno a failed sim write reports, as a kernel would: the disk's
/// `InvalidInput` (a direct write the filesystem refuses — ADR-0088 D3 as
/// amended, the checkpoint's in-band downgrade signal) is `EINVAL`;
/// everything else (the dead switch, a bad fd) is `EIO`.
fn write_errno(err: &std::io::Error) -> i32 {
    if err.kind() == std::io::ErrorKind::InvalidInput { libc::EINVAL } else { libc::EIO }
}

/// Audited unsafe (see `SAFETY.md`): a backend driver executing an op
/// reads its `StableBytes` payload.
fn stable_slice(data: &StableBytes) -> &[u8] {
    // SAFETY: the plane holds the staging `FrameLease` until this op's
    // terminal completion (the `StableBytes::new` contract, ADR-0013);
    // we are inside `submit_and_reap`, strictly before that completion
    // is delivered, so the bytes are live, at this address, unmodified.
    unsafe { std::slice::from_raw_parts(data.as_ptr(), data.len() as usize) }
}

/// Audited unsafe (see `SAFETY.md`): a backend driver executing a
/// `TierRead` fills its `StableBytesMut` target (M4-S04). The mut-from-
/// ref shape is the point: `StableBytesMut` is a `Copy` raw-pointer
/// capability whose exclusivity comes from its constructor's contract,
/// not from the borrow — exactly what the uring tier hands the kernel.
#[allow(clippy::mut_from_ref)]
fn stable_mut_slice(buf: &StableBytesMut) -> &mut [u8] {
    // SAFETY: the issuing command holds the aligned-pool lease and does
    // not touch the buffer until this op's terminal completion (the
    // `StableBytesMut::new` contract); we are inside `submit_and_reap`,
    // strictly before that completion is delivered, so the bytes are
    // live, at this address, and unaliased.
    unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr(), buf.len() as usize) }
}

impl BackendDriver for SimDriver {
    fn push(&mut self, op: IoOp) {
        self.ops.push(op);
    }

    fn submit_and_reap(
        &mut self,
        pool: &mut BufferPool,
        _wait: Wait,
        out: &mut Vec<Completion>,
    ) -> io::Result<usize> {
        let before = out.len();
        let mut net = self.net.borrow_mut();
        let submitted = self.ops.len() as u64;

        // M2.5-S14: release fsyncs whose device service time elapsed.
        // The flush is deferred WITH the completion — during the stall
        // window the bytes are page-cache volatile, so a power cut must
        // eat them and the watermark must not have advanced. Deferring
        // only the CQE while flushing early would defeat the oracle.
        if !self.pending_syncs.is_empty() {
            let now = self.clock.as_ref().expect("pending syncs imply a stall clock").now();
            while self.pending_syncs.first().is_some_and(|sync| sync.due <= now) {
                let PendingSync { fd, token, kind, .. } = self.pending_syncs.remove(0);
                let disk = self.disk.as_ref().expect("pending syncs imply a disk");
                let result = match kind {
                    PendingKind::WriteThrough { offset, data } => {
                        write_through(disk, net.plant, fd, offset, &data)
                    }
                    PendingKind::Write { offset, data, linked } => {
                        let result = plain_write(disk, fd, offset, &data);
                        // The linked sync starts only now (IO_LINK) —
                        // and only if the write succeeded (a failed write
                        // cancels its chain, ADR-0013).
                        if let Some(sync) = linked {
                            if matches!(result, CompletionResult::LogWritten) {
                                let due = disk.schedule_fsync(now.0).map(Nanos).unwrap_or(now);
                                let pending =
                                    PendingSync { due, fd, token: sync, kind: PendingKind::Fsync };
                                defer(&mut self.pending_syncs, pending);
                            } else {
                                out.push(Completion {
                                    token: sync,
                                    result: CompletionResult::Error {
                                        errno: libc::ECANCELED,
                                        buf: None,
                                    },
                                });
                            }
                        }
                        result
                    }
                    PendingKind::Read { offset, buf } => {
                        let dest = stable_mut_slice(&buf);
                        match disk.driver_read_at(fd, offset, dest) {
                            Ok(n) if n == dest.len() => CompletionResult::TierRead,
                            Ok(_) | Err(_) => {
                                CompletionResult::Error { errno: libc::EIO, buf: None }
                            }
                        }
                    }
                    // Plant::FsyncLies (ADR-0021 D4) holds here too: Synced
                    // without the flush — the canary the oracle must catch.
                    PendingKind::Fsync if net.plant == Plant::FsyncLies => CompletionResult::Synced,
                    PendingKind::Fsync => match disk.driver_fdatasync(fd) {
                        Ok(()) => CompletionResult::Synced,
                        Err(_) => CompletionResult::Error { errno: libc::EIO, buf: None },
                    },
                };
                out.push(Completion { token, result });
            }
        }

        // Reused backing storage: the drain below may push deferred
        // syncs, which needs `self` free of the `ops` borrow.
        let mut ops = core::mem::take(&mut self.ops);
        for op in ops.drain(..) {
            match op {
                IoOp::AcceptArm { token, .. } => {
                    net.accept_armed = true;
                    net.accept_token = Some(token);
                }
                IoOp::RecvArm { fd, token } => {
                    if let Some(conn) = net.conns.get_mut(&fd) {
                        conn.recv_armed = true;
                        conn.recv_token = Some(token);
                    }
                }
                IoOp::RecvDisarm { fd } => {
                    if let Some(conn) = net.conns.get_mut(&fd) {
                        conn.recv_armed = false;
                    }
                }
                IoOp::Send { fd, buf, len, token } => {
                    let result = match net.conns.get_mut(&fd) {
                        Some(conn) if !conn.server_closed => {
                            conn.to_client.extend_from_slice(&pool.bytes(buf)[..len as usize]);
                            CompletionResult::Sent { buf }
                        }
                        _ => CompletionResult::Error { errno: libc::EPIPE, buf: Some(buf) },
                    };
                    out.push(Completion { token, result });
                }
                IoOp::Close { fd, token } => {
                    if let Some(conn) = net.conns.get_mut(&fd) {
                        conn.server_closed = true;
                        conn.recv_armed = false;
                    }
                    out.push(Completion { token, result: CompletionResult::Closed });
                }
                // The simulated disk (M2-S18, ADR-0020 D7). Completion
                // order is submission order — the group-commit ledger
                // already tolerates cross-fd reordering (ADR-0013);
                // *survival* reordering is the disk model's job.
                IoOp::LogWrite { fd, offset, data, token, barrier } => {
                    let disk = self
                        .disk
                        .as_ref()
                        .expect("durable sim scenarios construct SimDriver::with_disk (M2-S18)");
                    let len = u64::from(data.len());
                    self.observed.note(token, len);
                    if let Some(sync) = barrier.fsync_token() {
                        self.observed.note(sync, 0);
                    }
                    if matches!(barrier, WriteBarrier::WriteThrough) {
                        // Write-through (ADR-0086 D8): the write IS the
                        // barrier. Under the stall model it lands AND
                        // completes at its drawn time — nothing reaches
                        // the disk during the service window.
                        if let Some(due) = self.schedule_through(disk, len) {
                            let kind = PendingKind::WriteThrough { offset, data };
                            defer(&mut self.pending_syncs, PendingSync { due, fd, token, kind });
                            continue;
                        }
                        let result = write_through(disk, net.plant, fd, offset, &data);
                        out.push(Completion { token, result });
                        continue;
                    }
                    let fsync_token = barrier.fsync_token();
                    // Plain write under the stall model (ADR-0087 D7): it
                    // lands at its own drawn time, independent of every
                    // other op; its linked sync is scheduled at landing.
                    if let Some(due) = self.schedule_write(disk, len) {
                        let kind = PendingKind::Write { offset, data, linked: fsync_token };
                        defer(&mut self.pending_syncs, PendingSync { due, fd, token, kind });
                        continue;
                    }
                    match disk.driver_write_at(fd, offset, stable_slice(&data)) {
                        Ok(()) => {
                            out.push(Completion { token, result: CompletionResult::LogWritten });
                            if let Some(sync) = fsync_token {
                                // Stall device (M2.5-S14): the linked
                                // fsync defers to its drawn completion
                                // time — flush AND CQE together, above.
                                if let Some(due) = self.schedule_sync(disk) {
                                    defer(
                                        &mut self.pending_syncs,
                                        PendingSync {
                                            due,
                                            fd,
                                            token: sync,
                                            kind: PendingKind::Fsync,
                                        },
                                    );
                                    continue;
                                }
                                // Plant::FsyncLies (ADR-0021 D4): report
                                // Synced without flushing — the canary the
                                // durability oracle must catch.
                                let result = if net.plant == Plant::FsyncLies {
                                    CompletionResult::Synced
                                } else {
                                    match disk.driver_fdatasync(fd) {
                                        Ok(()) => CompletionResult::Synced,
                                        Err(_) => {
                                            CompletionResult::Error { errno: libc::EIO, buf: None }
                                        }
                                    }
                                };
                                out.push(Completion { token: sync, result });
                            }
                        }
                        Err(err) => {
                            // Uring linked-chain contract: the failed write
                            // cancels its linked sync — `Synced` can never
                            // cover a failed prefix (ADR-0013).
                            out.push(Completion {
                                token,
                                result: CompletionResult::Error {
                                    errno: write_errno(&err),
                                    buf: None,
                                },
                            });
                            if let Some(sync) = fsync_token {
                                out.push(Completion {
                                    token: sync,
                                    result: CompletionResult::Error {
                                        errno: libc::ECANCELED,
                                        buf: None,
                                    },
                                });
                            }
                        }
                    }
                }
                IoOp::TierRead { fd, offset, buf, token } => {
                    let disk = self
                        .disk
                        .as_ref()
                        .expect("tiered sim scenarios construct SimDriver::with_disk (M4-S04)");
                    self.observed.read_bytes += u64::from(buf.len());
                    self.observed.read_ops += 1;
                    // Bandwidth model (ADR-0088 D8): the read lands at its
                    // drawn time; with no read rate it completes inline
                    // exactly as before.
                    if let Some(due) = self.schedule_read(disk, u64::from(buf.len())) {
                        let kind = PendingKind::Read { offset, buf };
                        defer(&mut self.pending_syncs, PendingSync { due, fd, token, kind });
                        continue;
                    }
                    let dest = stable_mut_slice(&buf);
                    // The op contract: `TierRead` means the buffer is
                    // FULL; EOF inside the flushed range is corruption.
                    let result = match disk.driver_read_at(fd, offset, dest) {
                        Ok(n) if n == dest.len() => CompletionResult::TierRead,
                        Ok(_) | Err(_) => CompletionResult::Error { errno: libc::EIO, buf: None },
                    };
                    out.push(Completion { token, result });
                }
                IoOp::Fdatasync { fd, token } => {
                    let disk = self
                        .disk
                        .as_ref()
                        .expect("durable sim scenarios construct SimDriver::with_disk (M2-S18)");
                    self.observed.note(token, 0);
                    // Stall device (M2.5-S14): standalone fsyncs (barrier
                    // dirs, everysec ticks) defer exactly like linked ones.
                    if let Some(due) = self.schedule_sync(disk) {
                        defer(
                            &mut self.pending_syncs,
                            PendingSync { due, fd, token, kind: PendingKind::Fsync },
                        );
                        continue;
                    }
                    let result = if net.plant == Plant::FsyncLies {
                        CompletionResult::Synced // the lying-fsync canary
                    } else {
                        match disk.driver_fdatasync(fd) {
                            Ok(()) => CompletionResult::Synced,
                            Err(_) => CompletionResult::Error { errno: libc::EIO, buf: None },
                        }
                    };
                    out.push(Completion { token, result });
                }
            }
        }
        self.ops = ops;

        // Accept everything queued (multishot semantics).
        if net.accept_armed {
            let token = net.accept_token.expect("armed implies token");
            while let Some(fd) = net.backlog.pop_front() {
                out.push(Completion { token, result: CompletionResult::Accepted { fd } });
            }
        }

        // Deliver one seeded chunk per armed connection per reap (BTreeMap
        // order = deterministic). Chunk boundaries are random so spanning
        // frames exercise the parser's accumulator on every run.
        let fds: Vec<RawFd> = net.conns.keys().copied().collect();
        for fd in fds {
            let CellNet { conns, rng, plant, plant_fired, .. } = &mut *net;
            let Some(conn) = conns.get_mut(&fd) else { continue };
            if !conn.recv_armed || conn.server_closed || conn.suppressed {
                continue;
            }
            let token = conn.recv_token.expect("armed implies token");
            if conn.to_server.is_empty() {
                if conn.client_closed {
                    // EOF: zero-length recv with a leased buffer (contract).
                    if let Some(buf) = pool.try_lease(LeaseKind::Recv) {
                        conn.recv_armed = false;
                        out.push(Completion {
                            token,
                            result: CompletionResult::Recv { buf, len: 0 },
                        });
                    }
                }
                continue;
            }
            // The lost-wakeup plant: one seeded readiness edge vanishes.
            if *plant == Plant::LostWakeup && !*plant_fired && rng.next_u64() % 256 == 0 {
                conn.suppressed = true;
                *plant_fired = true;
                continue;
            }
            let Some(buf) = pool.try_lease(LeaseKind::Recv) else { continue };
            let max = conn.to_server.len().min(pool.buf_size());
            let chunk = 1 + (rng.next_u64() as usize) % max;
            let bytes = pool.bytes_mut(buf);
            for (i, b) in conn.to_server.drain(..chunk).enumerate() {
                bytes[i] = b;
            }
            out.push(Completion {
                token,
                result: CompletionResult::Recv { buf, len: chunk as u32 },
            });
        }

        let produced = out.len() - before;
        self.stats = SubmitStats { syscalls: 1, sqes: submitted, cqes: produced as u64 };
        Ok(produced)
    }

    fn register_pool(&mut self, _pool: &mut BufferPool) -> io::Result<()> {
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            backend: "sim",
            multishot_accept: true,
            multishot_recv: true,
            provided_buffers: false,
            fixed_buffers: false,
            single_issuer: true,
            defer_taskrun: false,
            performance_tier: false, // gate tooling must reject sim numbers
        }
    }

    fn submit_stats(&self) -> SubmitStats {
        self.stats
    }
}
