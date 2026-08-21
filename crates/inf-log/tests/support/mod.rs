//! Shared harness for the M2-S05/S06 integration tests: a deterministic
//! scripted `BackendDriver` (real file I/O or recorded, per-op completion
//! delays, write/fsync fault injection) and a `CellPlane` that performs the
//! reactor LOG-step choreography exactly as ADR-0013 D2 specifies — the
//! shape `inf-server`'s cell adopts at M2-S08.
//!
//! These are **test-harness edges** (inf-log dev-depends on inf-runtime /
//! inf-alloc); the normal-dependency DAG is unchanged.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::rc::Rc;

use inf_alloc::BufferPool;
use inf_foundation::time::Nanos;
use inf_log::fs::{SegmentFs, StdSegmentFs};
use inf_log::{
    FsyncClass, FsyncTicket, GroupCommit, Lsn, MutationEffect, NsId, SegmentConfig, SegmentRotor,
    StagedAt, StagingConfig, StagingRing, SyncReason, create_cell_dirs,
};
use inf_runtime::{
    BackendDriver, Capabilities, CellPlane, Completion, CompletionResult, CompletionToken, IoOp,
    LoopCx, RawFd, StableBytes, SubmitStats, TokenClass, Wait, WatermarkGate, WriteBarrier,
};

/// Fresh per-test directory under the system temp dir (the pattern the S02
/// tests established; no tempfile dependency).
pub fn test_dir(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("inf-log-s05-{tag}-{}", std::process::id()));
    if root.exists() {
        std::fs::remove_dir_all(&root).expect("clear stale test dir");
    }
    root
}

// ---- ticket ↔ token packing (plane-side detail; inf-log never sees tokens) --

pub fn fsync_token(ticket: FsyncTicket) -> CompletionToken {
    let raw = ticket.as_u64();
    assert!(raw < 1 << 56, "ticket fits slot+gen");
    CompletionToken::new(TokenClass::Fsync, (raw & 0xFF_FFFF) as u32, (raw >> 24) as u32)
}

pub fn token_ticket(token: CompletionToken) -> FsyncTicket {
    FsyncTicket::from_u64(u64::from(token.slot()) | (u64::from(token.generation()) << 24))
}

pub fn write_token(seq: u64) -> CompletionToken {
    CompletionToken::new(TokenClass::LogWrite, (seq & 0xFF_FFFF) as u32, (seq >> 24) as u32)
}

// ---- ScriptedDriver ---------------------------------------------------------

/// Whether the driver performs the real syscalls (identity/replay tests) or
/// only records them (virtual-time tests where 86 400 real fdatasyncs would
/// measure the disk, not the policy).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum IoMode {
    Real,
    Recorded,
}

enum PendingKind {
    Write {
        fd: RawFd,
        offset: u64,
        data: StableBytes,
        written: u32,
        token: CompletionToken,
        fsync_token: Option<CompletionToken>,
    },
    Fsync {
        fd: RawFd,
        token: CompletionToken,
    },
}

struct Pending {
    kind: PendingKind,
    delay: u32,
}

/// Deterministic completion-shaped driver over the M2-S05 file-op contract:
/// short writes withhold the chained sync until every byte landed, failed
/// writes cancel it (`Error{ECANCELED}`) — the same observable semantics the
/// uring tier implements with `IOSQE_IO_LINK` + supersession.
pub struct ScriptedDriver {
    mode: IoMode,
    queued: Vec<IoOp>,
    inflight: VecDeque<Pending>,
    /// Per-op completion delays (in submit calls), popped as ops arrive;
    /// empty ⇒ complete on the next submit. Seeded by schedule tests.
    pub delays: VecDeque<u32>,
    pub fail_next_write: Option<i32>,
    pub short_next_write: Option<u32>,
    pub fail_next_fsync: Option<i32>,
    pub log_writes_submitted: u64,
    pub fsyncs_submitted: u64,
    stats: SubmitStats,
    /// File references taken at push, mirroring the uring contract (the
    /// kernel refs the file at SQE submission): a delayed schedule may
    /// execute an op after the app dropped its fd — e.g. a standalone
    /// fdatasync racing a deferred-seal handle drop (M2-S22, the
    /// one-in-flight group-commit gate made this reachable). Held for the
    /// driver's lifetime; test-scale op counts make that cheap.
    held: Vec<File>,
}

impl ScriptedDriver {
    pub fn new(mode: IoMode) -> ScriptedDriver {
        ScriptedDriver {
            mode,
            queued: Vec::new(),
            inflight: VecDeque::new(),
            delays: VecDeque::new(),
            fail_next_write: None,
            short_next_write: None,
            fail_next_fsync: None,
            log_writes_submitted: 0,
            fsyncs_submitted: 0,
            stats: SubmitStats::default(),
            held: Vec::new(),
        }
    }

    fn next_delay(&mut self) -> u32 {
        self.delays.pop_front().unwrap_or(0)
    }

    fn write_chunk(&self, fd: RawFd, offset: u64, data: StableBytes, written: u32, chunk: u32) {
        if self.mode == IoMode::Recorded {
            return;
        }
        // SAFETY: the fd stays open for the rotor's lifetime (the plane
        // outlives the loop run) and `data` upholds the StableBytes
        // contract — the FrameLease is held until this op's terminal
        // completion. ManuallyDrop keeps us from closing a borrowed fd.
        let file = ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
        // SAFETY: `written + chunk ≤ data.len()` by construction below.
        let slice = unsafe {
            std::slice::from_raw_parts(data.as_ptr().add(written as usize), chunk as usize)
        };
        file.write_all_at(slice, offset + u64::from(written)).expect("test pwrite");
    }

    fn sync_fd(&self, fd: RawFd) {
        if self.mode == IoMode::Recorded {
            return;
        }
        // SAFETY: as in `write_chunk` — borrowed fd, never closed here.
        let file = ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
        file.sync_data().expect("test fdatasync");
    }

    /// Duplicate `fd` and keep the dup alive for the driver's lifetime —
    /// the scripted analog of the kernel's file reference at submission.
    fn hold(&mut self, fd: RawFd) -> RawFd {
        // SAFETY: dup on a live fd; returns a fresh owned fd or -1 (asserted).
        let dup = unsafe { libc::dup(fd) };
        assert!(dup >= 0, "dup({fd}) failed: {}", io::Error::last_os_error());
        // SAFETY: `dup` is owned by no one else; the `File` in `held`
        // closes it exactly once when the driver drops.
        self.held.push(unsafe { File::from_raw_fd(dup) });
        dup
    }
}

impl BackendDriver for ScriptedDriver {
    fn push(&mut self, op: IoOp) {
        // Take the file reference the kernel would take at SQE submission
        // (see `held`): the op keeps executing on the dup'd fd even if the
        // app closes its own copy before a delayed schedule runs the op.
        let op = match op {
            IoOp::LogWrite { fd, offset, data, token, barrier }
                if self.mode != IoMode::Recorded =>
            {
                IoOp::LogWrite { fd: self.hold(fd), offset, data, token, barrier }
            }
            IoOp::Fdatasync { fd, token } if self.mode != IoMode::Recorded => {
                IoOp::Fdatasync { fd: self.hold(fd), token }
            }
            other => other,
        };
        self.queued.push(op);
    }

    fn submit_and_reap(
        &mut self,
        _pool: &mut BufferPool,
        _wait: Wait,
        out: &mut Vec<Completion>,
    ) -> io::Result<usize> {
        let before = out.len();
        self.stats = SubmitStats { syscalls: 1, sqes: self.queued.len() as u64, cqes: 0 };
        for op in std::mem::take(&mut self.queued) {
            let delay = self.next_delay();
            let kind = match op {
                IoOp::LogWrite { fd, offset, data, token, barrier } => {
                    self.log_writes_submitted += 1;
                    // This harness drives buffered segments only: a
                    // write-through frame would be a plane bug here.
                    assert!(
                        !matches!(barrier, WriteBarrier::WriteThrough),
                        "scripted driver models FLUSH-class barriers only"
                    );
                    let fsync_token = barrier.fsync_token();
                    if fsync_token.is_some() {
                        self.fsyncs_submitted += 1;
                    }
                    PendingKind::Write { fd, offset, data, written: 0, token, fsync_token }
                }
                IoOp::Fdatasync { fd, token } => {
                    self.fsyncs_submitted += 1;
                    PendingKind::Fsync { fd, token }
                }
                other => panic!("no non-file ops in this harness: {other:?}"),
            };
            self.inflight.push_back(Pending { kind, delay });
        }

        // Execute everything due this round, preserving submission order.
        let mut still = VecDeque::new();
        while let Some(mut p) = self.inflight.pop_front() {
            if p.delay > 0 {
                p.delay -= 1;
                still.push_back(p);
                continue;
            }
            match p.kind {
                PendingKind::Write { fd, offset, data, written, token, fsync_token } => {
                    if let Some(errno) = self.fail_next_write.take() {
                        out.push(Completion {
                            token,
                            result: CompletionResult::Error { errno, buf: None },
                        });
                        if let Some(ft) = fsync_token {
                            // Contract: a failed write cancels the chained sync.
                            out.push(Completion {
                                token: ft,
                                result: CompletionResult::Error {
                                    errno: libc::ECANCELED,
                                    buf: None,
                                },
                            });
                        }
                        continue;
                    }
                    let remaining = data.len() - written;
                    let chunk = match self.short_next_write.take() {
                        Some(k) => k.min(remaining),
                        None => remaining,
                    };
                    self.write_chunk(fd, offset, data, written, chunk);
                    let written = written + chunk;
                    if written < data.len() {
                        // Short write: remainder stays in flight; the
                        // chained sync is withheld until every byte landed
                        // (the uring tier's supersession, observably).
                        still.push_back(Pending {
                            kind: PendingKind::Write {
                                fd,
                                offset,
                                data,
                                written,
                                token,
                                fsync_token,
                            },
                            delay: 0,
                        });
                        continue;
                    }
                    out.push(Completion { token, result: CompletionResult::LogWritten });
                    if let Some(ft) = fsync_token {
                        let delay = self.next_delay();
                        still.push_back(Pending {
                            kind: PendingKind::Fsync { fd, token: ft },
                            delay,
                        });
                    }
                }
                PendingKind::Fsync { fd, token } => {
                    if let Some(errno) = self.fail_next_fsync.take() {
                        out.push(Completion {
                            token,
                            result: CompletionResult::Error { errno, buf: None },
                        });
                        continue;
                    }
                    self.sync_fd(fd);
                    out.push(Completion { token, result: CompletionResult::Synced });
                }
            }
        }
        self.inflight = still;
        let produced = out.len() - before;
        self.stats.cqes = produced as u64;
        Ok(produced)
    }

    fn register_pool(&mut self, _pool: &mut BufferPool) -> io::Result<()> {
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            backend: "scripted",
            multishot_accept: false,
            multishot_recv: false,
            provided_buffers: false,
            fixed_buffers: false,
            single_issuer: false,
            defer_taskrun: false,
            performance_tier: false,
        }
    }

    fn submit_stats(&self) -> SubmitStats {
        self.stats
    }
}

// ---- DurablePlane -----------------------------------------------------------

pub const EVERYSEC_TIMER_KEY: u64 = 0xE5EC;
const NS: NsId = NsId(7);

/// One queued mutation for the plane's workload script.
pub struct Job {
    pub class: FsyncClass,
    pub seq: u64,
}

pub fn job_key(seq: u64) -> Vec<u8> {
    format!("key:{seq:012}").into_bytes()
}

pub fn job_value(seq: u64) -> Vec<u8> {
    format!("value-{seq:012}-{}", "x".repeat((seq % 23) as usize)).into_bytes()
}

/// The M2-S05/S06 cell plane: EXECUTE stages effects (spawning gated ack
/// futures for `always`), LOG runs the ADR-0013 choreography, REAP routes
/// write/fsync completions back into the lease + ledger + gate.
pub struct DurablePlane {
    pub staging: StagingRing,
    pub rotor: SegmentRotor<StdSegmentFs>,
    pub commit: GroupCommit<<StdSegmentFs as SegmentFs>::File>,
    pub gate: WatermarkGate,
    pub in_flight: Option<inf_log::FrameLease>,
    /// Workload script: staged up to `jobs_per_iter` per EXECUTE.
    pub jobs: VecDeque<Job>,
    pub jobs_per_iter: usize,
    /// `always` records staged since the last seal, resolved to LSNs there.
    staged_always: Vec<(StagedAt, u64)>,
    /// Everything staged, for replay verification: (seq, resolved LSN).
    pub staged_log: Vec<(u64, Option<Lsn>)>,
    pending_lsn_resolve: Vec<(StagedAt, usize)>,
    /// Acks in completion order: (record LSN, watermark observed at ack).
    pub acks: Rc<RefCell<Vec<(Lsn, u64)>>>,
    /// (reason, submission time) per fsync — the everysec timing assert.
    pub fsync_submits: Vec<(SyncReason, Nanos)>,
    /// Terminal I/O errors observed (fail-stop territory; tests assert).
    pub io_errors: Vec<(TokenClass, i32)>,
    pub everysec: bool,
    timer_armed: bool,
    write_seq: u64,
    /// LogWrite ops queued in the CURRENT iteration (tripwire: ≤ 1).
    pub writes_this_iter: u32,
    pub cell_failed: bool,
}

impl DurablePlane {
    pub fn new(dir: &std::path::Path, staging: StagingConfig, segment: SegmentConfig) -> Self {
        let fs = StdSegmentFs;
        let dirs = create_cell_dirs(&fs, dir).expect("cell dirs");
        let rotor = SegmentRotor::create_fresh(fs, dirs.log, segment).expect("rotor");
        DurablePlane {
            staging: StagingRing::new(staging),
            rotor,
            commit: GroupCommit::new(),
            gate: WatermarkGate::new(),
            in_flight: None,
            jobs: VecDeque::new(),
            jobs_per_iter: 8,
            staged_always: Vec::new(),
            staged_log: Vec::new(),
            pending_lsn_resolve: Vec::new(),
            acks: Rc::new(RefCell::new(Vec::new())),
            fsync_submits: Vec::new(),
            io_errors: Vec::new(),
            everysec: true,
            timer_armed: false,
            write_seq: 0,
            writes_this_iter: 0,
            cell_failed: false,
        }
    }

    pub fn push_jobs(&mut self, n: u64, class: FsyncClass) {
        let base = self.staged_log.len() as u64 + self.jobs.len() as u64;
        for i in 0..n {
            self.jobs.push_back(Job { class, seq: base + i });
        }
    }

    /// Total records staged into frames so far. (Shared support: not every
    /// test binary calls every helper.)
    #[allow(dead_code)]
    pub fn staged_records(&self) -> usize {
        self.staged_log.len()
    }
}

impl CellPlane for DurablePlane {
    fn on_completion(&mut self, cx: &mut LoopCx<'_>, c: Completion) {
        match (c.token.class(), c.result) {
            (TokenClass::LogWrite, CompletionResult::LogWritten) => {
                self.commit.note_frame_written();
                let lease = self.in_flight.take().expect("LogWritten with no in-flight lease");
                self.staging.release(lease);
            }
            (TokenClass::Fsync, CompletionResult::Synced) => {
                if let Some(end) = self.commit.on_fsync_complete(token_ticket(c.token), cx.now) {
                    self.gate.advance(end.to_u64());
                }
            }
            (
                class @ (TokenClass::LogWrite | TokenClass::Fsync),
                CompletionResult::Error { errno, .. },
            ) => {
                // §8.4: fail-stop territory. The harness records and freezes.
                if class == TokenClass::Fsync {
                    self.commit.on_fsync_error(token_ticket(c.token));
                } else {
                    // The write's lease is terminal — release so teardown
                    // asserts stay meaningful; the cell is failed regardless.
                    if let Some(lease) = self.in_flight.take() {
                        self.staging.release(lease);
                    }
                }
                self.io_errors.push((class, errno));
                self.cell_failed = true;
            }
            (class, result) => panic!("unexpected completion {class:?}/{result:?}"),
        }
    }

    fn on_timer(&mut self, cx: &mut LoopCx<'_>, key: u64) {
        assert_eq!(key, EVERYSEC_TIMER_KEY);
        self.commit.note_everysec_tick();
        cx.timers.insert(cx.now + Nanos::from_secs(1), EVERYSEC_TIMER_KEY);
    }

    fn parse_execute(&mut self, cx: &mut LoopCx<'_>) {
        self.writes_this_iter = 0;
        if self.everysec && !self.timer_armed {
            cx.timers.insert(cx.now + Nanos::from_secs(1), EVERYSEC_TIMER_KEY);
            self.timer_armed = true;
        }
        if self.cell_failed {
            return;
        }
        let mut staged = 0;
        while staged < self.jobs_per_iter {
            let Some(job) = self.jobs.front() else { break };
            let key = job_key(job.seq);
            let value = job_value(job.seq);
            let effect = MutationEffect::StringSet { ns: NS, key: &key, value: &value };
            match self.staging.stage(&effect) {
                Ok(at) => {
                    let job = self.jobs.pop_front().expect("checked front");
                    self.commit.note_staged(job.class);
                    let idx = self.staged_log.len();
                    self.staged_log.push((job.seq, None));
                    self.pending_lsn_resolve.push((at, idx));
                    if job.class == FsyncClass::Always {
                        self.staged_always.push((at, job.seq));
                    }
                    staged += 1;
                }
                // Typed backpressure: stop staging this iteration (the
                // read-rearm stop, in harness form).
                Err(_full) => break,
            }
        }
        cx.charge(inf_runtime::GroupClass::Foreground, staged as u32);
    }

    fn maintain(&mut self, cx: &mut LoopCx<'_>) {
        if !self.cell_failed {
            self.rotor.maintain(cx.now.as_millis()).expect("maintain");
        }
    }

    fn seal_log(&mut self, cx: &mut LoopCx<'_>) {
        if self.cell_failed {
            return;
        }
        if self.staging.can_seal() {
            let frame_len = self.staging.pending_frame_len();
            let (slot, seal) =
                self.rotor.begin_frame_deferred(frame_len, cx.now.as_millis()).expect("reserve");
            if let Some(handoff) = seal {
                let fd = handoff.raw_fd().expect("std tier has fds");
                let ticket = self.commit.register_seal_fsync(handoff, cx.now);
                self.fsync_submits.push((SyncReason::Seal, cx.now));
                cx.push(IoOp::Fdatasync { fd, token: fsync_token(ticket) });
            }
            let end = slot.base().advance(slot.len());
            let covered = self.commit.watermark().map_or(0, |lsn| lsn.to_u64());
            let lease = self.staging.seal(slot.first_record_lsn(), covered, slot.layout());
            // Records have LSNs now: resolve the replay log + spawn gated
            // ack futures for `always` records (S06).
            for (at, idx) in self.pending_lsn_resolve.drain(..) {
                self.staged_log[idx].1 = Some(lease.lsn_of(at));
            }
            for (at, _seq) in self.staged_always.drain(..) {
                let lsn = lease.lsn_of(at);
                let gate = self.gate.clone();
                let acks = Rc::clone(&self.acks);
                cx.executor.spawn_local(async move {
                    gate.waiter(lsn.to_u64()).await;
                    let watermark = gate.watermark();
                    assert!(
                        watermark >= lsn.to_u64(),
                        "S06 oracle: ack for {lsn} before the watermark covered it"
                    );
                    acks.borrow_mut().push((lsn, watermark));
                });
            }
            self.commit.note_frame_queued(end, lease.frame_len());
            let barrier = if self.commit.frame_fsync_due() {
                let ticket = self.commit.register_linked_fsync(cx.now);
                self.fsync_submits.push((SyncReason::Linked, cx.now));
                WriteBarrier::LinkedFsync { fsync_token: fsync_token(ticket) }
            } else {
                WriteBarrier::None
            };
            let offset = u64::from(slot.base().offset);
            let fd = self.rotor.active_raw_fd().expect("std tier has fds");
            self.rotor.commit_frame_queued(slot);
            self.write_seq += 1;
            self.writes_this_iter += 1;
            let bytes = self.staging.leased_frame(&lease);
            // SAFETY: the FrameLease is held in `self.in_flight` until the
            // LogWritten completion releases it (ADR-0013 D1) — the sealed
            // buffer neither moves nor resets while this op is in flight.
            let data = unsafe { StableBytes::new(bytes) };
            self.in_flight = Some(lease);
            cx.push(IoOp::LogWrite {
                fd,
                offset,
                data,
                token: write_token(self.write_seq),
                barrier,
            });
        } else if self.commit.standalone_fsync_due() {
            let ticket = self.commit.register_standalone_fsync(cx.now);
            self.fsync_submits.push((SyncReason::Standalone, cx.now));
            let fd = self.rotor.active_raw_fd().expect("std tier has fds");
            cx.push(IoOp::Fdatasync { fd, token: fsync_token(ticket) });
        }
    }

    fn respond(&mut self, _cx: &mut LoopCx<'_>) {}

    fn fabric_out(&mut self, _cx: &mut LoopCx<'_>) -> bool {
        !self.jobs.is_empty()
            || !self.staging.is_empty()
            || self.in_flight.is_some()
            || self.commit.pending_fsyncs() > 0
    }
}
