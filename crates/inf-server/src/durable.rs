//! Per-cell durable-namespace machinery (M2-S08): the staging ring, the
//! segment rotor, the group-commit ledger, and the **seq-keyed ack gate**,
//! driven from the plane's EXECUTE/LOG/REAP/timer steps exactly per the
//! ADR-0013 D2 choreography (the `inf-log/tests/support` `DurablePlane` is
//! the reference implementation this productionizes).
//!
//! ## Why the ack gate is keyed by sequence number, not LSN
//!
//! An `always` response future must register its wait *at dispatch*, but a
//! record's LSN exists only after LOG seals the frame. Every staged record
//! therefore gets a monotone **durable seq** synchronously; frames are
//! FIFO, so seq order equals LSN order, and when the fsync watermark covers
//! a frame's exclusive-end LSN it covers every seq staged into it — the
//! gate advances to that frame's last seq. The S06 oracle ("no ack before
//! the watermark covers its LSN") holds structurally: `ack_gate` only
//! advances from `GroupCommit::on_fsync_complete`, which only fires on
//! `Synced` completions (ADR-0013 D3).

use std::collections::VecDeque;
use std::path::PathBuf;

use inf_foundation::time::Nanos;
use inf_log::fs::{SegmentFs, StdSegmentFs};
use inf_log::{
    FsyncClass, FsyncTicket, GroupCommit, MutationEffect, NsId, RecordView, SegmentConfig,
    SegmentRotor, StagingConfig, StagingRing,
};
use inf_runtime::{CompletionToken, IoOp, LoopCx, TokenClass, WaitList, WatermarkGate};
use inf_store::{Keyspace, WallAnchor};

use crate::ckpt::{CkptCell, CkptPhase, CkptStats, SCAN_CHUNK_ENTRIES, ckpt_token};
use crate::log_bytes;

/// Timer-wheel key for the everysec tick (plane-armed, injected clock).
pub(crate) const EVERYSEC_TIMER_KEY: u64 = 0xE5EC_0001;

/// Durable-path configuration one cell receives from the node assembly.
/// Absent config means a memory-only cell: durable DDL is refused with a
/// documented error and none of this module's code runs (M2-S09's zero-cost
/// branch is `Option::is_none`).
#[derive(Clone, Debug)]
pub struct DurableConfig {
    /// Node data directory; cell `k` owns `<data_dir>/shard-k/`.
    pub data_dir: PathBuf,
    pub staging: StagingConfig,
    pub segment: SegmentConfig,
    /// Fuzzy-checkpoint policy (M2-S10, ADR-0016).
    pub ckpt: inf_log::CkptConfig,
}

/// Cumulative durable counters flushed into `NodeInfo` by MAINTAIN (the
/// S21 vocabulary, born cell-local — no atomics).
#[derive(Copy, Clone, Debug, Default)]
pub struct DurableStats {
    pub records_appended: u64,
    pub acks_gated: u64,
    pub pending_log_bytes: u64,
    pub last_durable_lsn: u64,
    pub watermark_lag_lsn: u64,
    pub fsyncs_completed: u64,
}

/// One cell's durable plane state (plane-owned; `inf-store` never sees it).
pub(crate) struct DurableCell {
    pub staging: StagingRing,
    pub rotor: SegmentRotor<StdSegmentFs>,
    pub commit: GroupCommit<<StdSegmentFs as SegmentFs>::File>,
    /// Ack gate keyed by durable seq (see module docs).
    pub ack_gate: WatermarkGate,
    /// Wakes pump futures parked on staging backpressure (`StagingFull`)
    /// once the in-flight frame releases.
    pub drained: WaitList<()>,
    /// Fuzzy-checkpoint driver (M2-S10, ADR-0016).
    pub ckpt: CkptCell,
    in_flight: Option<inf_log::FrameLease>,
    /// Last durable seq assigned (0 = none; the gate starts at 0).
    last_seq: u64,
    /// Frames queued but not yet durable: (exclusive-end LSN, last seq).
    frame_seqs: VecDeque<(u64, u64)>,
    write_seq: u64,
    records_appended: u64,
    acks_gated: u64,
    /// §8.4: a terminal log-I/O error freezes the cell's durable plane
    /// (checked before fail-stop so tests can observe the frozen state).
    pub failed: bool,
}

impl DurableCell {
    pub fn new(
        staging: StagingConfig,
        rotor: SegmentRotor<StdSegmentFs>,
        ckpt: CkptCell,
    ) -> DurableCell {
        DurableCell {
            staging: StagingRing::new(staging),
            rotor,
            commit: GroupCommit::new(),
            ack_gate: WatermarkGate::new(),
            drained: WaitList::new(),
            ckpt,
            in_flight: None,
            last_seq: 0,
            frame_seqs: VecDeque::new(),
            write_seq: 0,
            records_appended: 0,
            acks_gated: 0,
            failed: false,
        }
    }

    /// Admission check for one command's worth of effects (the caller's
    /// conservative byte estimate). `false` = park on [`Self::drained`].
    pub fn would_fit(&self, bytes: usize) -> bool {
        self.staging.would_fit(bytes)
    }

    /// Stage one effect. Admission was checked via [`Self::would_fit`]
    /// with a conservative estimate, so refusal here is an invariant
    /// violation, not backpressure. Returns the record's durable seq.
    pub fn stage(&mut self, effect: &MutationEffect<'_>, class: FsyncClass) -> u64 {
        assert!(!self.failed, "staging into a failed durable cell");
        let _at = self.staging.stage(effect).expect("admission pre-checked by would_fit");
        self.commit.note_staged(class);
        self.records_appended += 1;
        self.last_seq += 1;
        self.last_seq
    }

    /// Registers one gated `always` ack (counter only; the waiter itself
    /// is `ack_gate.waiter(seq)` at the dispatch site).
    pub fn note_gated_ack(&mut self) {
        self.acks_gated += 1;
    }

    /// MAINTAIN slice: keep the next segment preallocated (rotation stays
    /// a pointer swap — S02).
    pub fn maintain(&mut self, now_ms: u64) {
        if self.failed {
            return;
        }
        if let Err(err) = self.rotor.maintain(now_ms) {
            // ENOSPC discipline (S02): surfaced before any write needs the
            // space; `space_exhausted()` gates admission at the command
            // layer. Other I/O errors are fail-stop territory.
            if !self.rotor.space_exhausted() {
                self.fail_stop("segment maintain", &err.to_string());
            }
        }
    }

    /// The LOG step (ADR-0013 D2): seal at most one frame into one
    /// positional write (+ linked fdatasync when a sync is due), or issue
    /// a standalone fdatasync for a frameless everysec tick.
    pub fn seal_log(&mut self, cx: &mut LoopCx<'_>) {
        if self.failed {
            return;
        }
        if self.staging.can_seal() {
            let frame_len = self.staging.pending_frame_len();
            let deferred = match self.rotor.begin_frame_deferred(frame_len, cx.now.as_millis()) {
                Ok(deferred) => deferred,
                Err(err) => {
                    if self.rotor.space_exhausted() {
                        // Typed refusal already gates new admissions; the
                        // staged frame waits for space (maintain retries).
                        return;
                    }
                    self.fail_stop("frame reserve", &err.to_string());
                }
            };
            let (slot, seal) = deferred;
            if let Some(handoff) = seal {
                let fd = handoff.raw_fd().expect("std segment tier has fds");
                let ticket = self.commit.register_seal_fsync(handoff, cx.now);
                cx.push(IoOp::Fdatasync { fd, token: fsync_token(ticket) });
            }
            let end = slot.base().advance(frame_len);
            let lease = self.staging.seal(slot.first_record_lsn());
            // A pending ckpt-begin marker rides this frame: its LSN is now
            // real (ADR-0016 D3).
            self.ckpt.on_frame_sealed(&lease);
            self.frame_seqs.push_back((end.to_u64(), self.last_seq));
            self.commit.note_frame_queued(end, frame_len);
            let fsync = self
                .commit
                .frame_fsync_due()
                .then(|| fsync_token(self.commit.register_linked_fsync(cx.now)));
            let offset = u64::from(slot.base().offset);
            let fd = self.rotor.active_raw_fd().expect("std segment tier has fds");
            self.rotor.commit_frame_queued(slot);
            self.write_seq += 1;
            let data = log_bytes::sealed_frame(&self.staging, &lease);
            self.in_flight = Some(lease);
            cx.push(IoOp::LogWrite {
                fd,
                offset,
                data,
                token: write_token(self.write_seq),
                fsync_token: fsync,
            });
        } else if self.commit.standalone_fsync_due() {
            let ticket = self.commit.register_standalone_fsync(cx.now);
            let fd = self.rotor.active_raw_fd().expect("std segment tier has fds");
            cx.push(IoOp::Fdatasync { fd, token: fsync_token(ticket) });
        }
    }

    /// REAP: the frame write's terminal completion — release the lease
    /// (the `StableBytes` custody point) and wake staging-parked pumps.
    pub fn on_log_written(&mut self) {
        self.commit.note_frame_written();
        let lease = self.in_flight.take().expect("LogWritten with no in-flight lease");
        self.staging.release(lease);
        self.drained.wake_all(());
    }

    /// REAP: an fsync completed — advance the durability watermark and
    /// wake every ack whose frame it covers (FIFO by seq == by LSN).
    pub fn on_synced(&mut self, token: CompletionToken, now: Nanos) {
        if let Some(end) = self.commit.on_fsync_complete(token_ticket(token), now) {
            let watermark = end.to_u64();
            let mut last_covered = None;
            while self.frame_seqs.front().is_some_and(|&(end_lsn, _)| end_lsn <= watermark) {
                last_covered = self.frame_seqs.pop_front().map(|(_, seq)| seq);
            }
            if let Some(seq) = last_covered {
                self.ack_gate.advance(seq);
            }
        }
    }

    /// Timer: the everysec tick (idle ticks are free — counted, no I/O).
    pub fn on_everysec_tick(&mut self, cx: &mut LoopCx<'_>) {
        self.commit.note_everysec_tick();
        cx.timers.insert(cx.now + Nanos::from_secs(1), EVERYSEC_TIMER_KEY);
    }

    /// §8.4 fail-stop: a terminal error on the durable path. The watermark
    /// freezes (no ack for the affected batch can ever fire) and the
    /// process exits via panic — fsync failure is never caught-and-continued
    /// (the fsyncgate rule; S17 formalizes exit codes).
    pub fn fail_stop(&mut self, what: &str, detail: &str) -> ! {
        self.failed = true;
        panic!("durable-path {what} failed (fail-stop, §8.4): {detail}");
    }

    /// Terminal error routed from REAP (write or fsync token).
    pub fn on_log_error(&mut self, token: CompletionToken, errno: i32) -> ! {
        if token.class() == TokenClass::Fsync {
            let _ = self.commit.on_fsync_error(token_ticket(token));
        }
        self.fail_stop("I/O", &format!("errno {errno} on {:?}", token.class()))
    }

    /// One fuzzy-checkpoint slice (M2-S10, ADR-0016 D5): runs under the
    /// `GroupClass::Checkpoint` deficit — `budget_units` convert at
    /// 1 unit ≈ 1 KiB streamed, hard-capped by `CkptConfig::slice_bytes`.
    /// Returns the units to charge. All data writes ride the driver; the
    /// only blocking ops are file create/rename/dir-fsync metadata (the
    /// rotor-prealloc class).
    pub fn ckpt_slice(
        &mut self,
        ks: &mut Keyspace,
        cx: &mut LoopCx<'_>,
        budget_units: u32,
        anchor: WallAnchor,
    ) -> u32 {
        if self.failed {
            return 0;
        }
        // Idle: trigger check → stage the begin marker (one record; its
        // frame seals at this iteration's LOG step).
        if matches!(self.ckpt.phase, CkptPhase::Idle) {
            let total = self.staging.stats().append_bytes;
            if self.ckpt.should_begin(total) {
                let id = self.ckpt.pending_id();
                let effect = MutationEffect::CkptBegin { ckpt_id: id };
                if self.staging.would_fit(effect.encoded_len()) {
                    let at = self.staging.stage(&effect).expect("admission pre-checked");
                    self.commit.note_staged(FsyncClass::Everysec);
                    self.records_appended += 1;
                    self.last_seq += 1;
                    self.ckpt.requested = false;
                    self.ckpt.phase = CkptPhase::AwaitBeginLsn { id, at };
                }
            }
            return 0;
        }
        // Begin LSN resolves at LOG (`on_frame_sealed`); nothing to do yet.
        if matches!(self.ckpt.phase, CkptPhase::AwaitBeginLsn { .. }) {
            return 0;
        }
        // Begun: create the .ick.new file + queue the header write.
        if let CkptPhase::Begun { id, begin_lsn } = self.ckpt.phase {
            let ns_ids = ks.durable_ns_ids();
            if let Err(err) = self.ckpt.open_stream(id, begin_lsn, ns_ids) {
                self.ckpt.abort("create", &err.to_string());
                return 1;
            }
            let CkptPhase::Stream(st) = &mut self.ckpt.phase else { unreachable!("just opened") };
            let lease = st.in_flight.as_ref().expect("header staged by open_stream");
            st.write_seq += 1;
            cx.push(IoOp::LogWrite {
                fd: st.fd,
                offset: lease.offset(),
                data: log_bytes::ckpt_block(&st.stream, lease),
                token: ckpt_token(TokenClass::CkptWrite, st.write_seq),
                fsync_token: None,
            });
            return 1;
        }
        let cfg = self.ckpt.cfg;
        // Publish once the completion fdatasync landed (rename+dir-fsync).
        if matches!(&self.ckpt.phase, CkptPhase::Stream(st) if st.sync_done) {
            let CkptPhase::Stream(st) = std::mem::replace(&mut self.ckpt.phase, CkptPhase::Idle)
            else {
                unreachable!("matched above")
            };
            let total = self.staging.stats().append_bytes;
            let unix_now = anchor.unix_from_internal(cx.now);
            if let Err(err) = self.ckpt.publish(st, unix_now, total) {
                self.ckpt.abort("publish", &err.to_string());
            }
            return 1;
        }
        let CkptPhase::Stream(st) = &mut self.ckpt.phase else { return 0 };
        // One deref, split fields (borrow splitting doesn't cross `Box`).
        let crate::ckpt::Streaming {
            stream,
            fd,
            ns_ids,
            ns_idx,
            cursor,
            walk_done,
            footer_staged,
            sync_issued,
            in_flight,
            write_seq,
            ..
        } = &mut **st;
        // One section in flight max: wait for its completion.
        if in_flight.is_some() {
            return 0;
        }
        // Footer written+released → the completion barrier.
        if *footer_staged {
            if !*sync_issued {
                *sync_issued = true;
                *write_seq += 1;
                cx.push(IoOp::Fdatasync {
                    fd: *fd,
                    token: ckpt_token(TokenClass::CkptSync, *write_seq),
                });
            }
            return 0;
        }
        // Fill: pull post-images under the byte budget (the walker — the
        // resize-stable SCAN cursor, ADR-0016 D2).
        let mut emitted: u32 = 0;
        if !*walk_done {
            let slice_cap = cfg.slice_bytes.min(budget_units.saturating_mul(1024)).max(1);
            while emitted < slice_cap && !*walk_done && !stream.section_full() {
                let Some(&ns_raw) = ns_ids.get(*ns_idx) else {
                    *walk_done = true;
                    break;
                };
                let ns = NsId(ns_raw);
                match ks.ns_store_mut(ns) {
                    // Dropped mid-walk: its records replay as skips anyway.
                    None => {
                        *ns_idx += 1;
                        *cursor = 0;
                    }
                    Some(store) => {
                        let bytes = &mut emitted;
                        let next = store.scan_post_images(
                            *cursor,
                            SCAN_CHUNK_ENTRIES,
                            cx.now,
                            |key, value, expire_ms| {
                                let rec = RecordView::StringPostImage { ns, key, value };
                                *bytes = bytes.saturating_add(rec.encoded_len() as u32);
                                stream.stage_record(&rec);
                                if let Some(ms) = expire_ms {
                                    let at_unix_ms =
                                        anchor.unix_from_internal(Nanos::from_millis(ms));
                                    let rec = RecordView::ExpireAt { ns, at_unix_ms, key };
                                    *bytes = bytes.saturating_add(rec.encoded_len() as u32);
                                    stream.stage_record(&rec);
                                }
                            },
                        );
                        if next == 0 {
                            *ns_idx += 1;
                            *cursor = 0;
                        } else {
                            *cursor = next;
                        }
                        if *ns_idx == ns_ids.len() {
                            *walk_done = true;
                        }
                    }
                }
            }
        }
        // Queue at most one block per slice: a full (or final partial)
        // section, or — once everything drained — the footer.
        if stream.section_full() || (*walk_done && stream.can_seal()) {
            let lease = stream.seal_section();
            *write_seq += 1;
            cx.push(IoOp::LogWrite {
                fd: *fd,
                offset: lease.offset(),
                data: log_bytes::ckpt_block(stream, &lease),
                token: ckpt_token(TokenClass::CkptWrite, *write_seq),
                fsync_token: None,
            });
            *in_flight = Some(lease);
        } else if *walk_done {
            let lease = stream.finish();
            *write_seq += 1;
            cx.push(IoOp::LogWrite {
                fd: *fd,
                offset: lease.offset(),
                data: log_bytes::ckpt_block(stream, &lease),
                token: ckpt_token(TokenClass::CkptWrite, *write_seq),
                fsync_token: None,
            });
            *in_flight = Some(lease);
            *footer_staged = true;
        }
        emitted.div_ceil(1024).max(1)
    }

    /// REAP: a checkpoint section/header/footer write completed — release
    /// the lease (the `StableBytes` custody point).
    pub fn on_ckpt_written(&mut self) {
        let CkptPhase::Stream(st) = &mut self.ckpt.phase else {
            panic!("CkptWrite completion with no checkpoint streaming")
        };
        let lease = st.in_flight.take().expect("CkptWrite with no in-flight lease");
        st.stream.release(lease);
    }

    /// REAP: the checkpoint's completion fdatasync landed — publish next
    /// MAINTAIN slice.
    pub fn on_ckpt_synced(&mut self) {
        let CkptPhase::Stream(st) = &mut self.ckpt.phase else {
            panic!("CkptSync completion with no checkpoint streaming")
        };
        st.sync_done = true;
    }

    /// REAP: a checkpoint op failed — abort the checkpoint, never the
    /// process (the old checkpoint and the whole log stay valid; the
    /// milestone's "checkpoints abort cleanly" rule).
    pub fn on_ckpt_error(&mut self, errno: i32) {
        self.ckpt.abort("I/O", &format!("errno {errno}"));
    }

    /// Manual trigger latch (`INF.CKPT` rides the control handle — S20).
    pub fn request_ckpt(&mut self) {
        self.ckpt.requested = true;
    }

    /// Checkpoint gauges for the MAINTAIN stats flush.
    pub fn ckpt_stats(&self) -> CkptStats {
        self.ckpt.stats()
    }

    /// Counters for the MAINTAIN stats flush (S21 vocabulary).
    pub fn stats(&self) -> DurableStats {
        let durable = self.commit.watermark().map_or(0, |l| l.to_u64());
        let queued = self.commit.queued_up_to().map_or(0, |l| l.to_u64());
        DurableStats {
            records_appended: self.records_appended,
            acks_gated: self.acks_gated,
            pending_log_bytes: self.commit.pending_log_bytes(),
            last_durable_lsn: durable,
            watermark_lag_lsn: queued.saturating_sub(durable),
            fsyncs_completed: self.commit.stats().fsyncs_completed,
        }
    }

    /// Admission gate for durable writes when preallocation failed
    /// (ENOSPC — degrade loudly, never corrupt).
    pub fn space_exhausted(&self) -> bool {
        self.rotor.space_exhausted()
    }
}

// ---- ticket ↔ token packing (plane-side detail; inf-log never sees tokens) --

fn fsync_token(ticket: FsyncTicket) -> CompletionToken {
    let raw = ticket.as_u64();
    assert!(raw < 1 << 56, "ticket fits slot+gen");
    CompletionToken::new(TokenClass::Fsync, (raw & 0xFF_FFFF) as u32, (raw >> 24) as u32)
}

fn token_ticket(token: CompletionToken) -> FsyncTicket {
    FsyncTicket::from_u64(u64::from(token.slot()) | (u64::from(token.generation()) << 24))
}

fn write_token(seq: u64) -> CompletionToken {
    CompletionToken::new(TokenClass::LogWrite, (seq & 0xFF_FFFF) as u32, (seq >> 24) as u32)
}
