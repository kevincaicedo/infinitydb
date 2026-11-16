//! The simulated network + [`SimDriver`]: per-cell in-memory connections
//! behind the real `BackendDriver` contract. Deterministic by construction:
//! `BTreeMap` iteration order, seeded chunk sizes, no wall clock, no real
//! syscalls.

use core::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::rc::Rc;

use inf_alloc::{BufferPool, LeaseKind};
use inf_foundation::FileOffset;
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::{Clock, Nanos, VirtualClock};
use inf_log::fs::sim::BarrierKind;
use inf_runtime::{
    AcceptFailure, BackendDriver, Capabilities, Completion, CompletionResult, CompletionToken,
    IoClass, IoOp, RawFd, StableBytes, StableBytesMut, SubmitStats, TokenClass, Wait, WriteBarrier,
    classify_accept_errno,
};
use inf_server::SimDisk;

use crate::driver::Violations;

/// How a cell's net hands a connection's pending bytes to a reap.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Delivery {
    /// A seeded chunk a reap, every scenario's default: frames span reaps,
    /// so the parser's accumulator is exercised on every run.
    Chunked,
    /// The whole pending bytes, up to one receive buffer, a reap: commands
    /// of one size sent at one step reach the server at one reap.
    Whole,
}

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
    /// ADR-0124's teeth: the stop request is a process death instead of
    /// a drain (the pre-fix `SIGTERM`), so the clean-stop rule — every
    /// acked `everysec` op required — must go red.
    StopKill,
    /// One accept-path error (`EMFILE`) on the listener token while
    /// connections are queued: an accept failure is a counter, never
    /// connection housekeeping, and the first client (`ConnKey {0, 0}`)
    /// keeps being served. The arm parks as the real backends park an
    /// exhaustion failure: the queued connections stay in the backlog until
    /// the plane's retry timer re-arms it or a `Close` returns an fd, so here
    /// a close may let them in before the retry does. `AcceptBroken` is the
    /// plant on which only the retry lets them in.
    AcceptError,
    /// The same accept-path error, of the `Broken` class (`EINVAL`): by the
    /// driver contract only an `AcceptArm` resumes a broken-parked arm,
    /// never a `Close`, so the plane's retry timer is the queued clients'
    /// one way in.
    AcceptBroken,
    /// One `EIO` on the next read of every tier file, armed on the sim
    /// disk before the cold re-read sweep (`m4-tiered` phase 7; review
    /// 2026-08-30, F-L04-02 / ADR-0119 D2): a device error under a cold
    /// read must reach the client as the typed `ERR cold read failed`
    /// reply and the `tiering_cold_read_errors` counter — one reply per
    /// fault, never a nil or a stale value (the L06-02 fold). The arm
    /// lives on the disk, not the net; `m4-tiered` also runs it on every
    /// seed ≡ 7 (mod 8) without the flag.
    TierReadEio,
    /// One tier-file write answers `EIO` on each cell (the sim disk's
    /// per-file fault, armed on the first tier file of the tier-budget
    /// shape's first namespace once it exists): the round turns the failed
    /// write unsent again and offers it as a retry group, charged like any
    /// group — the retry arm of the tier class's accounting oracle.
    TierWriteEio,
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
    /// The client is gone (a reset, [`CellNet::client_reset`]): the net
    /// posts a receive error where a FIN would post the EOF, and answers
    /// every later `Send` `ECONNRESET`.
    client_reset: bool,
    /// The net posted this fd's end of stream: no chunk and no receive arm
    /// may follow it.
    eof_posted: bool,
    /// The plane disarmed this fd's receive at least once.
    disarmed: bool,
    /// The plane re-armed this fd's receive after a disarm while the client
    /// had already sent its FIN.
    rearmed_after_fin: bool,
    /// `Send`s the net received for this fd after it posted the fd's EOF.
    sends_after_eof: u64,
    /// Reaps that posted a chunk of this fd and its EOF together.
    shared_reaps: u64,
    /// The EOF was posted after `rearmed_after_fin`.
    ended_after_rearm: bool,
    /// The net posted a chunk of this fd after its EOF (`net-data-after-eof`).
    data_after_eof: bool,
}

/// A parked accept arm's class: the two failures the driver contract parks
/// on. A transient failure delivers nothing and never parks, so it has no
/// variant here (ADR-0118 D1).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum AcceptPark {
    /// Descriptor or memory exhaustion: an `AcceptArm`, or any `Close` that
    /// returns an fd, resumes it.
    Exhausted,
    /// A broken listener: only an `AcceptArm` resumes it.
    Broken,
}

impl AcceptPark {
    /// The park the contract's table gives `errno`; `None` for a transient
    /// failure, which never parks.
    #[must_use]
    pub fn of(errno: i32) -> Option<AcceptPark> {
        match classify_accept_errno(errno) {
            AcceptFailure::Exhausted => Some(AcceptPark::Exhausted),
            AcceptFailure::Broken => Some(AcceptPark::Broken),
            AcceptFailure::Transient => None,
        }
    }

    /// Whether a `Close` that returns an fd resumes this park: an exhaustion
    /// park only, as both real backends do (ADR-0118 D2).
    #[cfg(not(inf_canary_accept_close_resumes_any))]
    fn resumes_on_close(self) -> bool {
        match self {
            AcceptPark::Exhausted => true,
            AcceptPark::Broken => false,
        }
    }

    /// Canary: a `Close` resumes a park of either class, so a broken
    /// listener comes back with no retry.
    #[cfg(inf_canary_accept_close_resumes_any)]
    fn resumes_on_close(self) -> bool {
        true
    }
}

impl Plant {
    /// Whether this plant is the accept-retry oracle's leg: a listener that
    /// only the plane's retry resumes. It alone builds the oracle's watch;
    /// the match names every plant, so a plant added is classified here.
    #[must_use]
    pub fn is_accept_retry_leg(self) -> bool {
        match self {
            Plant::AcceptBroken => true,
            Plant::None
            | Plant::LostWakeup
            | Plant::FsyncLies
            | Plant::StopKill
            | Plant::AcceptError
            | Plant::TierReadEio
            | Plant::TierWriteEio => false,
        }
    }

    /// The errno an accept plant delivers, `None` for every other plant.
    fn accept_errno(self) -> Option<i32> {
        match self {
            Plant::AcceptError => Some(libc::EMFILE),
            Plant::AcceptBroken => Some(libc::EINVAL),
            Plant::None
            | Plant::LostWakeup
            | Plant::FsyncLies
            | Plant::StopKill
            | Plant::TierReadEio
            | Plant::TierWriteEio => None,
        }
    }
}

/// One cell's network endpoint: the listener plus every connection accepted
/// by this cell. The harness holds the same handle to play the client side.
#[derive(Debug)]
pub struct CellNet {
    cell: u16,
    accept_armed: bool,
    accept_token: Option<CompletionToken>,
    /// Parked after the plant fired, with the park's class: no accepts until
    /// an `AcceptArm`, or a `Close` when the class is `Exhausted`.
    accept_parked: Option<AcceptPark>,
    /// Parked arms resumed, by an `AcceptArm` or a `Close`.
    accept_resumes: u64,
    /// Every `AcceptArm` op this driver received, the first arm included:
    /// the driver's side of the plane's accept retry.
    accept_arms: u64,
    backlog: VecDeque<RawFd>,
    conns: BTreeMap<RawFd, SimConn>,
    next_fd: RawFd,
    rng: SplitMix64,
    plant: Plant,
    plant_fired: bool,
    /// What a reap hands each connection's server side
    /// ([`CellNet::deliver_whole`]).
    delivery: Delivery,
    /// The close legs' capability: a closed client's last chunk and its EOF
    /// are posted in one reap ([`CellNet::set_eof_with_data`]).
    eof_with_data: bool,
    /// `Send`s answered `ECONNRESET` because their client was reset.
    reset_send_errors: u64,
    /// The net's own invariants that a reap broke, as their tokens:
    /// `net-data-after-eof` and `recv-armed-after-eof`.
    violations: Vec<String>,
}

/// Synthetic listener fd for a cell (never a real fd).
pub fn listener_fd(cell: u16) -> RawFd {
    1_000_000 + i32::from(cell)
}

impl CellNet {
    /// True once the requested plant has fired (batch 34: a positive
    /// control that never fires is a vacuous run, not a green one).
    #[must_use]
    pub fn plant_fired(&self) -> bool {
        self.plant_fired
    }

    /// How many times a parked accept arm was resumed (F-L11-02: the
    /// plane's retry timer and the driver's close-resume are the only
    /// two paths; the count is bounded by wall time, never a spin).
    #[must_use]
    pub fn accept_resumes(&self) -> u64 {
        self.accept_resumes
    }

    /// The parked arm's class, `None` while the arm is up.
    #[must_use]
    pub fn accept_parked(&self) -> Option<AcceptPark> {
        self.accept_parked
    }

    /// Every `AcceptArm` op the driver received, the first arm included.
    #[must_use]
    pub fn accept_arms(&self) -> u64 {
        self.accept_arms
    }

    pub fn new(cell: u16, seed: u64, plant: Plant) -> Rc<RefCell<CellNet>> {
        Rc::new(RefCell::new(CellNet {
            cell,
            accept_armed: false,
            accept_token: None,
            accept_parked: None,
            accept_resumes: 0,
            accept_arms: 0,
            backlog: VecDeque::new(),
            conns: BTreeMap::new(),
            next_fd: 0,
            rng: SplitMix64::new(seed ^ 0xD15C_0000 ^ u64::from(cell)),
            plant,
            plant_fired: false,
            delivery: Delivery::Chunked,
            eof_with_data: false,
            reset_send_errors: 0,
            violations: Vec::new(),
        }))
    }

    /// Deliver every connection's pending bytes whole, one receive buffer
    /// a reap, from now on: commands of one size sent at one step reach
    /// the server at one reap, the burst a stopped-then-resumed process
    /// reads. The seeded chunking stays every other scenario's (no draw is
    /// taken here, so their traces are unchanged). Returns the delivery it
    /// replaced, which a harness phase that sends one burst whole restores
    /// at the burst's end ([`Self::restore_delivery`]).
    pub fn deliver_whole(&mut self) -> Delivery {
        core::mem::replace(&mut self.delivery, Delivery::Whole)
    }

    /// Deliver as `delivery` from now on: what [`Self::deliver_whole`]
    /// replaced, at the end of the burst it was set for.
    pub fn restore_delivery(&mut self, delivery: Delivery) {
        self.delivery = delivery;
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

    /// Client side: give up on a refused connection — nothing queued
    /// toward the server will ever be read (the server closed without
    /// arming a receive), so it leaves the progress accounting.
    pub fn client_abandon(&mut self, fd: RawFd) {
        if let Some(conn) = self.conns.get_mut(&fd) {
            conn.to_server.clear();
            conn.client_closed = true;
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

    /// Client→server bytes of the one connection `fd` that the server has
    /// not taken yet, `None` for an fd this net never opened: a client's
    /// own share of [`pending_bytes`](Self::pending_bytes), which counts
    /// every connection of the cell.
    pub fn unread(&self, fd: RawFd) -> Option<usize> {
        self.conns.get(&fd).map(|c| c.to_server.len())
    }

    /// Client side: the client is gone, its socket reset. What it sent stays
    /// queued for the server, it reads nothing more, and once the server has
    /// taken every byte the net posts the receive error a reset raises
    /// (`ECONNRESET`) where a FIN would post the EOF. With no receive armed
    /// (after its EOF) the server learns it from its next `Send`, which
    /// answers `ECONNRESET` too.
    pub fn client_reset(&mut self, fd: RawFd) {
        if let Some(conn) = self.conns.get_mut(&fd) {
            conn.client_closed = true;
            conn.client_reset = true;
            conn.suppressed = false;
            conn.to_client.clear();
        }
    }

    /// From now on, a closed client's last chunk, up to one receive buffer,
    /// and its EOF are posted in one reap, data first: the reap a multishot
    /// receive gives bytes and a FIN that reached the kernel together. Off
    /// in every scenario but the close legs, it takes no draw.
    pub fn set_eof_with_data(&mut self, on: bool) {
        self.eof_with_data = on;
    }

    /// Whether the net posted `fd`'s end of stream.
    #[must_use]
    pub fn eof_posted(&self, fd: RawFd) -> bool {
        self.conns.get(&fd).is_some_and(|c| c.eof_posted)
    }

    /// Whether the plane disarmed `fd`'s receive and has not re-armed it.
    #[must_use]
    pub fn recv_disarmed(&self, fd: RawFd) -> bool {
        self.conns.get(&fd).is_some_and(|c| c.disarmed && !c.recv_armed)
    }

    /// `Send`s the net received for `fd` after it posted the fd's EOF.
    #[must_use]
    pub fn sends_after_eof(&self, fd: RawFd) -> u64 {
        self.conns.get(&fd).map_or(0, |c| c.sends_after_eof)
    }

    /// Reaps that posted a chunk and the EOF of one fd, over every fd.
    #[must_use]
    pub fn eof_shared_reaps(&self) -> u64 {
        self.conns.values().map(|c| c.shared_reaps).sum()
    }

    /// EOFs posted for an fd re-armed after a disarm while its client had
    /// sent its FIN, over every fd.
    #[must_use]
    pub fn eof_after_rearm(&self) -> u64 {
        self.conns.values().filter(|c| c.ended_after_rearm).count() as u64
    }

    /// `Send`s answered `ECONNRESET` because their client was reset.
    #[must_use]
    pub fn reset_send_errors(&self) -> u64 {
        self.reset_send_errors
    }

    /// The net's broken invariants since the last call, as their tokens.
    pub fn take_violations(&mut self) -> Vec<String> {
        for (fd, conn) in &mut self.conns {
            if core::mem::take(&mut conn.data_after_eof) {
                self.violations
                    .push(format!("net-data-after-eof: fd {fd} was posted a chunk after its EOF"));
            }
        }
        core::mem::take(&mut self.violations)
    }
}

/// Moves every net's broken invariants into `violations`. A harness that
/// judges a run drains its nets where it reads its own violations, so a
/// refused re-arm or a chunk posted after an EOF is never dropped with them.
pub(crate) fn drain_violations(nets: &[Rc<RefCell<CellNet>>], violations: &mut Violations) {
    for net in nets {
        for broken in net.borrow_mut().take_violations() {
            violations.push(broken);
        }
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
    /// Barrier ops the disk holds ([`SimDisk::hold_syncs`],
    /// [`SimDisk::hold_barriers`]), each whole with the barrier kind read
    /// once at its hold, submission order: each is submitted at the first
    /// driver entry after its hold lifts, ahead of that entry's own ops, and
    /// is observed and drawn only then.
    held_syncs: Vec<(BarrierKind, IoOp)>,
    /// Tier reads the disk holds ([`SimDisk::hold_reads`]), submission
    /// order: each is submitted at the first driver entry after its hold
    /// lifts, ahead of that entry's own ops.
    held_reads: Vec<IoOp>,
    /// The fds of the writes and syncs the last `submit_and_reap` carried
    /// from the loop: the device's reading of a park's own submit. One
    /// list, cleared at each submit, its capacity the largest submit's.
    submitted_io: Vec<RawFd>,
    ops: Vec<IoOp>,
    stats: SubmitStats,
    /// Bytes/ops observed per class (ADR-0088 D8).
    observed: ObservedIo,
    /// The wait the loop handed the last `submit_and_reap`. The driver never
    /// blocks on it, since the scheduler owns virtual time, so this is what
    /// a park asked for: its timeout, read by the oracles.
    wait: Option<Wait>,
    /// Each entry's tier writes by namespace directory, when a leg's clause
    /// reads them ([`SimDriver::tallying_tier_dirs`]).
    tier_dirs: Option<TierDirTally>,
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
        offset: FileOffset,
        data: StableBytes,
    },
    Write {
        offset: FileOffset,
        data: StableBytes,
        linked: Option<CompletionToken>,
    },
    /// A tier read under the bandwidth model (ADR-0088 D8): the buffer is
    /// filled at its due time, never before.
    Read {
        offset: FileOffset,
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
    /// Tier reads the driver completed, a value or an error, counted as it
    /// hands each completion to the loop: the device's own completion
    /// marks, which a harness reads between steps against its replies.
    pub read_completions: u64,
    /// The most tier-class bytes and ops one driver entry carried — what
    /// one MAINTAIN pass submitted for the tier flush, read from the ops'
    /// token class alone (the attainable oracle's disk-side clause).
    pub tier_pass_bytes_max: u64,
    pub tier_pass_ops_max: u64,
}

/// One namespace directory's tier writes in one driver entry: the writes
/// whose file has an OS name under `ns-<ns>`, and their bytes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TierDirWrites {
    pub ns: u32,
    pub writes: u64,
    pub bytes: u64,
}

/// The tier writes of each driver entry by namespace directory: a turn's
/// shape read from the device's side. A driver op carries an fd and a
/// token, so each write is keyed by the `ns-<id>` component of an OS name
/// of the file its fd is open on, read from the sim disk at the entry;
/// the token's lane, the server's own recycled allocation, is never read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TierDirTally {
    /// Driver entries so far: the entry [`Self::entry`] belongs to.
    pub entries: u64,
    /// The last entry's tier writes, one row a directory, in the order of
    /// each directory's first write.
    pub entry: Vec<TierDirWrites>,
    /// Tier writes over the driver's life whose file has no OS name under
    /// a `ns-<id>` directory: a shape read from no file.
    pub unnamed_writes: u64,
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
    /// The net this driver serves, whose invariants its harness drains.
    pub(crate) fn net(&self) -> &Rc<RefCell<CellNet>> {
        &self.net
    }

    pub fn new(net: Rc<RefCell<CellNet>>) -> SimDriver {
        SimDriver {
            net,
            disk: None,
            clock: None,
            pending_syncs: Vec::new(),
            held_syncs: Vec::new(),
            held_reads: Vec::new(),
            submitted_io: Vec::new(),
            ops: Vec::new(),
            stats: SubmitStats::default(),
            observed: ObservedIo::default(),
            wait: None,
            tier_dirs: None,
        }
    }

    /// The same driver, keeping each entry's tier writes by namespace
    /// directory ([`TierDirTally`]): one pass over the sim disk's OS names
    /// per tier write, so only a leg whose clause reads the tally asks.
    #[must_use]
    pub fn tallying_tier_dirs(self) -> SimDriver {
        SimDriver { tier_dirs: Some(TierDirTally::default()), ..self }
    }

    /// The tally of each entry's tier writes by namespace directory, on a
    /// driver that keeps it.
    pub fn tier_dirs(&self) -> Option<&TierDirTally> {
        self.tier_dirs.as_ref()
    }

    /// The wait the loop handed the last `submit_and_reap`, `None` before
    /// the first: a park's timeout as the loop asked for it.
    pub fn last_wait(&self) -> Option<Wait> {
        self.wait
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

    /// A planted canary's reader: the tier bytes and ops in the driver's own
    /// queue, which every entry drains, read by the spend's step check in
    /// place of the cell loop's queue.
    #[cfg(inf_canary_tier_spend_driver_queue)]
    pub fn driver_queue_tier_io(&self) -> (u64, u64) {
        tier_io_of(&self.ops)
    }

    /// Barrier ops the disk holds now, every kind together: submitted by
    /// the plane and kept from the device until the hold lifts — the
    /// harness's own reading of a held barrier, no product counter.
    pub fn held_syncs(&self) -> usize {
        let held = self.held_barriers();
        held.standalone + held.frames()
    }

    /// The barrier ops the disk holds now, counted by the kind each was held
    /// as: the harness's own reading of which barriers a hold keeps, no
    /// product counter.
    pub fn held_barriers(&self) -> HeldBarriers {
        let mut held = HeldBarriers::default();
        for (kind, _) in &self.held_syncs {
            match kind {
                BarrierKind::Standalone => held.standalone += 1,
                BarrierKind::Linked => held.linked += 1,
                BarrierKind::WriteThrough => held.write_through += 1,
            }
        }
        held
    }

    /// Tier reads the disk holds now ([`SimDisk::hold_reads`]): issued by
    /// the cell's cold engine and kept from the device until the hold lifts
    /// — the harness's own reading of a queued read, no product counter.
    pub fn held_reads(&self) -> usize {
        self.held_reads.len()
    }

    /// Whether a write or a sync on a file `named` accepts is submitted
    /// and not completed: deferred on the device's timeline, or held by
    /// the disk. Reads do not count. The device's own reading, no product
    /// counter.
    pub fn io_in_flight_named(&self, named: fn(&std::path::Path) -> bool) -> bool {
        let Some(disk) = self.disk.as_ref() else { return false };
        let pending = self.pending_syncs.iter().filter(|pending| match pending.kind {
            PendingKind::Fsync | PendingKind::WriteThrough { .. } | PendingKind::Write { .. } => {
                true
            }
            PendingKind::Read { .. } => false,
        });
        let held = self.held_syncs.iter().filter_map(|(_, op)| barrier_fd(op));
        let mut fds = pending.map(|pending| pending.fd).chain(held);
        fds.any(|fd| disk.fd_named(fd, named))
    }

    /// Whether the last `submit_and_reap` carried a write or a sync on a
    /// file `named` accepts, from the loop's own ops; a held sync it
    /// released was in flight already and is not counted.
    pub fn submitted_named(&self, named: fn(&std::path::Path) -> bool) -> bool {
        let Some(disk) = self.disk.as_ref() else { return false };
        self.submitted_io.iter().any(|&fd| disk.fd_named(fd, named))
    }
}

/// Barrier ops held whole by the disk, counted by kind
/// ([`SimDriver::held_barriers`]): one driver's, so one cell's.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct HeldBarriers {
    /// Standalone fdatasyncs.
    pub standalone: usize,
    /// Writes with their linked fdatasync.
    pub linked: usize,
    /// Write-through writes.
    pub write_through: usize,
}

impl HeldBarriers {
    /// The held writes that carry a frame's barrier: a linked sync or a
    /// write-through write.
    #[must_use]
    pub fn frames(self) -> usize {
        self.linked + self.write_through
    }
}

/// The barrier `op` carries to the device, `None` for an op that carries
/// none: the one place an op's barrier is read, so a new op that carries
/// one cannot pass a hold unread.
fn barrier_of(op: &IoOp) -> Option<BarrierKind> {
    match op {
        IoOp::Fdatasync { .. } => Some(BarrierKind::Standalone),
        IoOp::LogWrite { barrier, .. } => match barrier {
            WriteBarrier::LinkedFsync { .. } => Some(BarrierKind::Linked),
            WriteBarrier::WriteThrough => Some(BarrierKind::WriteThrough),
            WriteBarrier::None => None,
        },
        IoOp::AcceptArm { .. }
        | IoOp::RecvArm { .. }
        | IoOp::RecvDisarm { .. }
        | IoOp::Send { .. }
        | IoOp::Close { .. }
        | IoOp::TierRead { .. } => None,
    }
}

/// The file descriptor of a write or a sync, the ops that may carry a
/// barrier.
fn barrier_fd(op: &IoOp) -> Option<RawFd> {
    let (IoOp::LogWrite { fd, .. } | IoOp::Fdatasync { fd, .. }) = op else { return None };
    Some(*fd)
}

/// Whether the disk holds `op`, a barrier of `kind`, now: the armed hold's
/// scope admits the kind, on a file the hold names. With no hold armed the
/// disk reads nothing more.
fn held_now(disk: &SimDisk, kind: BarrierKind, op: &IoOp) -> bool {
    barrier_fd(op).is_some_and(|fd| disk.barrier_held(fd, kind))
}

/// The one hold check of a driver entry, before the op's arm: an op the
/// disk holds goes whole onto the held list with its barrier kind, read
/// here once, so nothing of it is observed or drawn until its hold lifts;
/// any other op is handed back to its arm. An op with no barrier reads the
/// disk not at all.
fn hold_or_pass(
    disk: Option<&SimDisk>,
    held_syncs: &mut Vec<(BarrierKind, IoOp)>,
    op: IoOp,
) -> Option<IoOp> {
    let (Some(disk), Some(kind)) = (disk, barrier_of(&op)) else { return Some(op) };
    if !held_now(disk, kind, &op) {
        return Some(op);
    }
    held_syncs.push((kind, op));
    None
}

/// Moves every held barrier op whose hold has lifted, or whose fd has
/// closed, to the front of `ops`, in submission order, so the driver entry
/// submits it first. Each is read by the kind it was held as. A free
/// function over the fields, so the drain's `net` borrow stays disjoint.
fn release_held_syncs(
    disk: Option<&SimDisk>,
    held_syncs: &mut Vec<(BarrierKind, IoOp)>,
    ops: &mut Vec<IoOp>,
) {
    let Some(disk) = disk else { return };
    let still_held = |(kind, op): &(BarrierKind, IoOp)| held_now(disk, *kind, op);
    if held_syncs.iter().all(still_held) {
        return;
    }
    let mut released = Vec::new();
    let mut kept = Vec::with_capacity(held_syncs.len());
    for held in held_syncs.drain(..) {
        if still_held(&held) {
            kept.push(held);
        } else {
            released.push(held.1);
        }
    }
    *held_syncs = kept;
    ops.splice(0..0, released);
}

/// Moves every held tier read whose hold has lifted to the front of `ops`,
/// in submission order, ahead of the released syncs' turn: a read held
/// before the drain's own ops is submitted first, as the held syncs are.
fn release_held_reads(disk: Option<&SimDisk>, held_reads: &mut Vec<IoOp>, ops: &mut Vec<IoOp>) {
    let Some(disk) = disk else { return };
    let held = |op: &IoOp| match op {
        IoOp::TierRead { fd, .. } => disk.read_held(*fd),
        IoOp::LogWrite { .. }
        | IoOp::Fdatasync { .. }
        | IoOp::AcceptArm { .. }
        | IoOp::RecvArm { .. }
        | IoOp::RecvDisarm { .. }
        | IoOp::Send { .. }
        | IoOp::Close { .. } => false,
    };
    if held_reads.iter().all(&held) {
        return;
    }
    let mut released = Vec::new();
    let mut kept = Vec::with_capacity(held_reads.len());
    for op in held_reads.drain(..) {
        if held(&op) {
            kept.push(op);
        } else {
            released.push(op);
        }
    }
    *held_reads = kept;
    ops.splice(0..0, released);
}

/// The tier-class bytes and ops among `ops`, by each op's token class.
pub(crate) fn tier_io_of(ops: &[IoOp]) -> (u64, u64) {
    let is_tier = |token: CompletionToken| IoClass::of(token.class()) == Some(IoClass::TierFlush);
    let mut tally = (0u64, 0u64);
    for op in ops {
        match op {
            IoOp::LogWrite { data, token, .. } if is_tier(*token) => {
                tally.0 += u64::from(data.len());
                tally.1 += 1;
            }
            IoOp::Fdatasync { token, .. } if is_tier(*token) => tally.1 += 1,
            IoOp::LogWrite { .. }
            | IoOp::Fdatasync { .. }
            | IoOp::AcceptArm { .. }
            | IoOp::RecvArm { .. }
            | IoOp::RecvDisarm { .. }
            | IoOp::Send { .. }
            | IoOp::Close { .. }
            | IoOp::TierRead { .. } => {}
        }
    }
    tally
}

/// One driver entry's tier writes folded into `tally` by the namespace
/// directory each write's file sits in.
fn tally_tier_dirs(disk: Option<&SimDisk>, ops: &[IoOp], tally: &mut TierDirTally) {
    tally.entries += 1;
    tally.entry.clear();
    for op in ops {
        let IoOp::LogWrite { fd, data, token, .. } = op else { continue };
        if IoClass::of(token.class()) != Some(IoClass::TierFlush) {
            continue;
        }
        let names = disk.map(|disk| disk.file_fd_names(*fd)).unwrap_or_default();
        let Some(ns) = names.iter().find_map(|name| ns_dir_of(name)) else {
            tally.unnamed_writes += 1;
            continue;
        };
        let bytes = u64::from(data.len());
        match tally.entry.iter_mut().find(|row| row.ns == ns) {
            Some(row) => {
                row.writes += 1;
                row.bytes += bytes;
            }
            None => tally.entry.push(TierDirWrites { ns, writes: 1, bytes }),
        }
    }
}

/// The `<id>` of the `ns-<id>` directory right under a `shard-N` directory
/// that `path` sits in: how the data directory lays out a namespace's
/// files. `None` for any other path.
fn ns_dir_of(path: &std::path::Path) -> Option<u32> {
    let mut parts = path.components().map(|part| part.as_os_str().to_str());
    parts.find(|part| part.is_some_and(|part| part.starts_with("shard-")))?;
    parts.next()??.strip_prefix("ns-")?.parse().ok()
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
    offset: FileOffset,
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
fn plain_write(
    disk: &SimDisk,
    fd: i32,
    offset: FileOffset,
    data: &StableBytes,
) -> CompletionResult {
    match disk.driver_write_at(fd, offset, stable_slice(data)) {
        Ok(()) => CompletionResult::LogWritten,
        Err(err) => CompletionResult::Error { errno: write_errno(&err), buf: None },
    }
}

/// The errno a failed sim write reports, as a kernel would: an OS error
/// the disk raised passes through (`EBADF` on a closed fd — F-L01-02),
/// the disk's `InvalidInput` (a direct write the filesystem refuses —
/// ADR-0088 D3 as amended, the checkpoint's in-band downgrade signal)
/// is `EINVAL`; everything else (the dead switch) is `EIO`.
fn write_errno(err: &std::io::Error) -> i32 {
    if let Some(errno) = err.raw_os_error() {
        return errno;
    }
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

/// Applies a `RecvArm`. The end of stream ends a receive (ADR-0196 A1.3),
/// so the net refuses an arm for an fd whose EOF it posted, its invariant
/// `recv-armed-after-eof`.
fn arm_recv(fd: RawFd, conn: &mut SimConn, token: CompletionToken, violations: &mut Vec<String>) {
    if conn.eof_posted {
        violations.push(format!("recv-armed-after-eof: fd {fd} was armed after its EOF"));
        return;
    }
    if conn.client_closed && conn.disarmed && !conn.recv_armed {
        conn.rearmed_after_fin = true;
    }
    conn.recv_armed = true;
    conn.recv_token = Some(token);
}

/// Takes a `Send` for a connection the server has not closed: its bytes
/// reach the client, or a reset client answers `ECONNRESET`.
fn take_send(
    conn: &mut SimConn,
    pool: &BufferPool,
    buf: inf_alloc::BufferId,
    len: u32,
    reset_send_errors: &mut u64,
) -> CompletionResult {
    if conn.eof_posted {
        conn.sends_after_eof += 1;
    }
    if conn.client_reset {
        *reset_send_errors += 1;
        return CompletionResult::Error { errno: libc::ECONNRESET, buf: Some(buf) };
    }
    conn.to_client.extend_from_slice(&pool.bytes(buf)[..len as usize]);
    CompletionResult::Sent { buf }
}

/// Posts a closed client's end once the server has taken its bytes: the
/// receive error of a reset, or the EOF, a zero-length receive with a leased
/// buffer (the contract), which a dry pool leaves for a later reap.
fn post_end(
    conn: &mut SimConn,
    token: CompletionToken,
    pool: &mut BufferPool,
    out: &mut Vec<Completion>,
) {
    if conn.client_reset {
        conn.recv_armed = false;
        let result = CompletionResult::Error { errno: libc::ECONNRESET, buf: None };
        out.push(Completion { token, result });
        return;
    }
    let Some(buf) = pool.try_lease(LeaseKind::Recv) else { return };
    conn.recv_armed = false;
    conn.eof_posted = true;
    conn.ended_after_rearm = conn.rearmed_after_fin;
    out.push(Completion { token, result: CompletionResult::Recv { buf, len: 0 } });
}

/// The shared reap of a closed client (`eof_with_data`): its remaining bytes,
/// up to one receive buffer, and, when they were the last, its end in the
/// same reap, data first. A reset client's end is its receive error.
fn post_with_end(
    conn: &mut SimConn,
    token: CompletionToken,
    pool: &mut BufferPool,
    out: &mut Vec<Completion>,
) {
    let Some(buf) = pool.try_lease(LeaseKind::Recv) else { return };
    let chunk = conn.to_server.len().min(pool.buf_size());
    let bytes = pool.bytes_mut(buf);
    for (i, b) in conn.to_server.drain(..chunk).enumerate() {
        bytes[i] = b;
    }
    let data = Completion { token, result: CompletionResult::Recv { buf, len: chunk as u32 } };
    if !conn.to_server.is_empty() {
        out.push(data);
        return;
    }
    // Canary: the end is posted before the chunk it follows.
    #[cfg(inf_canary_net_data_after_eof)]
    post_end(conn, token, pool, out);
    conn.data_after_eof |= conn.eof_posted;
    out.push(data);
    let posted = out.len();
    if !conn.eof_posted {
        post_end(conn, token, pool, out);
    }
    if out.len() > posted {
        conn.shared_reaps += 1;
    }
}

impl BackendDriver for SimDriver {
    fn push(&mut self, op: IoOp) {
        self.ops.push(op);
    }

    fn submit_and_reap(
        &mut self,
        pool: &mut BufferPool,
        wait: Wait,
        out: &mut Vec<Completion>,
    ) -> io::Result<usize> {
        self.wait = Some(wait);
        self.submitted_io.clear();
        for op in &self.ops {
            if let IoOp::LogWrite { fd, .. } | IoOp::Fdatasync { fd, .. } = op {
                self.submitted_io.push(*fd);
            }
        }
        let before = out.len();
        let mut net = self.net.borrow_mut();
        let submitted = self.ops.len() as u64;
        let (tier_bytes, tier_ops) = tier_io_of(&self.ops);
        self.observed.tier_pass_bytes_max = self.observed.tier_pass_bytes_max.max(tier_bytes);
        self.observed.tier_pass_ops_max = self.observed.tier_pass_ops_max.max(tier_ops);
        if let Some(tally) = self.tier_dirs.as_mut() {
            tally_tier_dirs(self.disk.as_ref(), &self.ops, tally);
        }

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
                        let target = stable_mut_slice(&buf);
                        match disk.driver_read_at(fd, offset, target) {
                            Ok(n) if n == target.len() => CompletionResult::TierRead,
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
        release_held_syncs(self.disk.as_ref(), &mut self.held_syncs, &mut ops);
        release_held_reads(self.disk.as_ref(), &mut self.held_reads, &mut ops);
        for op in ops.drain(..) {
            let Some(op) = hold_or_pass(self.disk.as_ref(), &mut self.held_syncs, op) else {
                continue;
            };
            match op {
                IoOp::AcceptArm { token, .. } => {
                    net.accept_arms += 1;
                    if net.accept_parked.take().is_some() {
                        net.accept_resumes += 1;
                    }
                    net.accept_armed = true;
                    net.accept_token = Some(token);
                }
                IoOp::RecvArm { fd, token } => {
                    let CellNet { conns, violations, .. } = &mut *net;
                    if let Some(conn) = conns.get_mut(&fd) {
                        arm_recv(fd, conn, token, violations);
                    }
                }
                IoOp::RecvDisarm { fd } => {
                    if let Some(conn) = net.conns.get_mut(&fd) {
                        conn.recv_armed = false;
                        conn.disarmed = true;
                    }
                }
                IoOp::Send { fd, buf, len, token } => {
                    let CellNet { conns, reset_send_errors, violations, .. } = &mut *net;
                    let result = match conns.get_mut(&fd) {
                        Some(conn) if !conn.server_closed => {
                            take_send(conn, pool, buf, len, reset_send_errors)
                        }
                        // The server pushed this fd's `Close` before the send:
                        // a closing slot holds no descriptor to send from, so
                        // the plane sent bytes it had no socket for.
                        Some(_) => {
                            violations.push(format!(
                                "bytes-after-close: fd {fd} was sent {len} B after its Close"
                            ));
                            CompletionResult::Error { errno: libc::EPIPE, buf: Some(buf) }
                        }
                        None => CompletionResult::Error { errno: libc::EPIPE, buf: Some(buf) },
                    };
                    out.push(Completion { token, result });
                }
                IoOp::Close { fd, token } => {
                    if let Some(conn) = net.conns.get_mut(&fd) {
                        conn.server_closed = true;
                        conn.recv_armed = false;
                    }
                    out.push(Completion { token, result: CompletionResult::Closed });
                    // An fd returned: an exhaustion-parked arm resumes
                    // (the driver contract's own resume path); a broken
                    // listener waits for the consumer's `AcceptArm`.
                    if net.accept_parked.is_some_and(AcceptPark::resumes_on_close) {
                        net.accept_parked = None;
                        net.accept_resumes += 1;
                    }
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
                    // A held read reaches the device only once its hold
                    // lifts: it is observed then, not now.
                    if disk.read_held(fd) {
                        self.held_reads.push(IoOp::TierRead { fd, offset, buf, token });
                        continue;
                    }
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
                    let target = stable_mut_slice(&buf);
                    // The op contract: `TierRead` means the buffer is
                    // FULL; EOF inside the flushed range is corruption.
                    let result = match disk.driver_read_at(fd, offset, target) {
                        Ok(n) if n == target.len() => CompletionResult::TierRead,
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

        // Accept everything queued (multishot semantics) — unless parked.
        if net.accept_armed && net.accept_parked.is_none() {
            let token = net.accept_token.expect("armed implies token");
            // The accept plants: the kernel refused a QUEUED accept. The
            // error rides the listener token, never a connection's, and the
            // arm parks with the backlog intact under the contract's class
            // for the errno: only the plane's re-arm, or for an exhaustion
            // park a `Close`, lets the queued clients in. A transient errno
            // parks nothing, as the contract surfaces none, so a plant of
            // one never fires.
            let planted = net
                .plant
                .accept_errno()
                .and_then(|errno| AcceptPark::of(errno).map(|park| (errno, park)));
            if let Some((errno, park)) = planted
                && !net.plant_fired
                && net.backlog.len() >= 2
            {
                // The failure lands mid-batch: one client is let in first
                // (the live `{0, 0}` the error must not tear down), the rest
                // stay queued.
                let fd = net.backlog.pop_front().expect("two queued");
                out.push(Completion { token, result: CompletionResult::Accepted { fd } });
                net.plant_fired = true;
                net.accept_parked = Some(park);
                out.push(Completion {
                    token,
                    result: CompletionResult::Error { errno, buf: None },
                });
            } else {
                while let Some(fd) = net.backlog.pop_front() {
                    out.push(Completion { token, result: CompletionResult::Accepted { fd } });
                }
            }
        }

        // Deliver one seeded chunk per armed connection per reap (BTreeMap
        // order = deterministic). Chunk boundaries are random so spanning
        // frames exercise the parser's accumulator on every run.
        let fds: Vec<RawFd> = net.conns.keys().copied().collect();
        for fd in fds {
            let CellNet { conns, rng, plant, plant_fired, delivery, eof_with_data, .. } = &mut *net;
            let Some(conn) = conns.get_mut(&fd) else { continue };
            if !conn.recv_armed || conn.server_closed || conn.suppressed {
                continue;
            }
            let token = conn.recv_token.expect("armed implies token");
            if conn.to_server.is_empty() {
                if conn.client_closed {
                    post_end(conn, token, pool, out);
                }
                continue;
            }
            if *eof_with_data && conn.client_closed {
                post_with_end(conn, token, pool, out);
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
            let chunk = match delivery {
                Delivery::Whole => max,
                Delivery::Chunked => 1 + (rng.next_u64() as usize) % max,
            };
            let bytes = pool.bytes_mut(buf);
            for (i, b) in conn.to_server.drain(..chunk).enumerate() {
                bytes[i] = b;
            }
            conn.data_after_eof |= conn.eof_posted;
            out.push(Completion {
                token,
                result: CompletionResult::Recv { buf, len: chunk as u32 },
            });
        }

        let produced = out.len() - before;
        let reads = out[before..].iter().filter(|c| c.token.class() == TokenClass::TierRead);
        self.observed.read_completions += reads.count() as u64;
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

#[cfg(test)]
mod close_capability_tests {
    use super::{CellNet, Plant, SimDriver, listener_fd};
    use inf_alloc::BufferPool;
    use inf_runtime::{
        BackendDriver, Completion, CompletionResult, CompletionToken, IoOp, TokenClass, Wait,
    };
    use std::rc::Rc;

    /// The lengths of the receives a reap posted.
    fn received(out: &[Completion]) -> Vec<u32> {
        out.iter()
            .filter_map(|completion| match completion.result {
                CompletionResult::Recv { len, .. } => Some(len),
                _ => None,
            })
            .collect()
    }

    /// With `eof_with_data` on, a closed client's last chunk and its end of
    /// stream are posted in one reap, the chunk first, and the net's own
    /// invariant `net-data-after-eof` holds.
    #[test]
    fn a_shared_reap_posts_the_chunk_before_the_end_of_stream() {
        let net = CellNet::new(0, 1, Plant::None);
        let mut driver = SimDriver::new(Rc::clone(&net));
        let mut pool = BufferPool::new(4, 1024);
        let fd = net.borrow_mut().connect();
        let accept = CompletionToken::new(TokenClass::Accept, 0, 0);
        driver.push(IoOp::AcceptArm { listener: listener_fd(0), token: accept });
        let mut out = Vec::new();
        driver.submit_and_reap(&mut pool, Wait::Poll, &mut out).expect("reap");
        assert!(matches!(out[..], [Completion { result: CompletionResult::Accepted { .. }, .. }]));
        driver.push(IoOp::RecvArm { fd, token: CompletionToken::new(TokenClass::Recv, 1, 0) });
        let ping = b"*1\r\n$4\r\nPING\r\n";
        {
            let mut net = net.borrow_mut();
            net.set_eof_with_data(true);
            net.client_send(fd, ping);
            net.client_close(fd);
        }
        out.clear();
        driver.submit_and_reap(&mut pool, Wait::Poll, &mut out).expect("reap");
        let violations = net.borrow_mut().take_violations();
        assert!(violations.is_empty(), "{violations:?}");
        assert_eq!(received(&out), [ping.len() as u32, 0], "the chunk, then the end, in one reap");
        assert_eq!(net.borrow().eof_shared_reaps(), 1);
        assert!(net.borrow().eof_posted(fd));
    }
}

#[cfg(test)]
mod tests {
    use super::ns_dir_of;
    use std::path::Path;

    /// A tier file is keyed by the `ns-<id>` directory right under its
    /// shard; a log segment, a namespace-like name outside a shard and a
    /// malformed id key nothing.
    #[test]
    fn a_tier_file_is_keyed_by_its_namespace_directory() {
        assert_eq!(ns_dir_of(Path::new("node/shard-1/ns-16/cold/tier-000000.itier")), Some(16));
        assert_eq!(ns_dir_of(Path::new("/data/shard-0/ns-7/cold/tier-000003.itier")), Some(7));
        assert_eq!(ns_dir_of(Path::new("node/shard-0/seg-000000.ilog")), None);
        assert_eq!(ns_dir_of(Path::new("ns-3/shard-0/seg-000000.ilog")), None);
        assert_eq!(ns_dir_of(Path::new("node/shard-0/ns-x/cold/tier-000000.itier")), None);
        assert_eq!(ns_dir_of(Path::new("node/shard-0")), None);
    }
}
