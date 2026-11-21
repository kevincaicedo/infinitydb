//! A connection's task waits on the durability watermark once, for the
//! smallest sequence its pending replies need (ADR-0177 D5): the pending
//! entry carries its sequence, so a pipeline of `always` writes held on
//! their fsync is one wait, not one per write. The test reads the cell's
//! watermark waits through the plane between scheduler steps; the device's
//! held frame barrier, the durable plane's appended records and the
//! client's bytes are its engagement.

use core::num::NonZeroU32;
use std::path::Path;

use super::park::{Quiet, QuietBoot, STEPS_MAX};
use super::{DurableScenario, MiniClient, encode};

/// The writes one connection pipelines: one pump pass stages what one
/// receive buffer carried (1,024 B on the simulated node), so the burst is
/// the most writes of one width that fit one buffer. 31 `SET`s of 33 B take
/// 1,023 B; a 32nd would take a second receive, and its pass.
const WRITES: usize = 31;

/// One write's bytes on the wire: `SET ack:NNN 1` as a RESP array.
const SET_BYTES: usize = 33;

/// Steps the cell runs once the burst is appended, so every wait its pump
/// takes for the burst is parked before the count is read.
const SETTLE_STEPS: u32 = 16;

/// Holds every barrier on every file.
fn any_file(_: &Path) -> bool {
    true
}

/// One connection sends 31 `always` writes at one step, delivered whole,
/// while the device holds every barrier: the pump stages all 31 in one pass,
/// and the cell then parks exactly one watermark wait for them, its front's.
/// Once the hold lifts, all 31 are acknowledged and no
/// wait is left parked: no entry is stranded behind the front. Engagement,
/// each else its `VACUOUS` line: the device held a frame barrier
/// (`frame-barrier-unheld`), all 31 were appended (`writes-unstaged`), and no
/// ack was read under the hold (`acked-under-hold`).
#[test]
fn a_task_waits_on_the_watermark_once_for_its_staged_writes() {
    let mut quiet = Quiet::boot(1, DurableScenario::SPIN_ITERS, 0);
    let mut setup = MiniClient::connect(&mut quiet.node, 0);
    let create: &[&[u8]] =
        &[b"INF.NS", b"CREATE", b"acks", b"MODE", b"durable", b"FSYNC", b"always"];
    assert_eq!(quiet.call(&mut setup, create, "the always CREATE"), b"+OK\r\n");
    let mut writer = MiniClient::connect(&mut quiet.node, 0);
    assert_eq!(quiet.call(&mut writer, &[b"INF.NS", b"USE", b"acks"], "USE"), b"+OK\r\n");
    let appended = |quiet: &Quiet| {
        quiet.node.plane(0).durable_stats().map_or(0, |stats| stats.records_appended)
    };
    let before = appended(&quiet);
    quiet.disk.hold_barriers(Some(any_file));
    let delivery = quiet.node.nets[0].borrow_mut().deliver_whole();
    for n in 0..WRITES {
        let key = format!("ack:{n:03}");
        let set: [&[u8]; 3] = [b"SET", key.as_bytes(), b"1"];
        assert_eq!(encode(&set).len(), SET_BYTES, "the burst's writes share one width");
        writer.send(&mut quiet.node, &set);
    }
    for _ in 0..STEPS_MAX {
        if appended(&quiet) >= before + WRITES as u64 {
            break;
        }
        quiet.step();
    }
    for _ in 0..SETTLE_STEPS {
        quiet.step();
    }
    quiet.node.nets[0].borrow_mut().restore_delivery(delivery);
    let held = quiet.node.driver(0).held_barriers();
    let unheld = format!("VACUOUS frame-barrier-unheld: no frame barrier was held ({held:?})");
    assert!(held.frames() > 0, "{unheld}");
    let staged = appended(&quiet) - before;
    let unstaged = format!("VACUOUS writes-unstaged: {staged} of {WRITES} writes were appended");
    assert!(staged >= WRITES as u64, "{unstaged}");
    let acked = writer.recv(&mut quiet.node);
    assert_eq!(acked, None, "VACUOUS acked-under-hold: an ack was read before the hold lifted");
    let waits = quiet.node.plane(0).ack_waiters_for_sim();
    assert_eq!(
        waits, 1,
        "wait-per-write: {waits} watermark waits are parked for one connection's {WRITES} \
         always writes, where its front's one is"
    );
    quiet.disk.hold_barriers(None);
    let mut acked = 0;
    for _ in 0..STEPS_MAX {
        quiet.step();
        while let Some(reply) = writer.recv(&mut quiet.node) {
            assert_eq!(reply.as_slice(), b"+OK\r\n", "write {acked}'s ack");
            acked += 1;
        }
        if acked == WRITES {
            break;
        }
    }
    assert_eq!(acked, WRITES, "stranded: {acked} of {WRITES} writes acknowledged after the hold");
    let left = quiet.node.plane(0).ack_waiters_for_sim();
    assert_eq!(left, 0, "{left} watermark waits are left parked after every ack");
}

/// A budget of `b` wakes between two LOG steps, as the durable config carries it.
fn wakes(b: usize) -> NonZeroU32 {
    NonZeroU32::new(u32::try_from(b).expect("a budget fits a u32")).expect("a non-zero budget")
}

/// The `always` namespace every writer of the wake rows uses, created on a setup connection,
/// and `count` writers on it, each its own connection.
fn always_writers(quiet: &mut Quiet, count: usize) -> Vec<MiniClient> {
    let mut setup = MiniClient::connect(&mut quiet.node, 0);
    let create: &[&[u8]] =
        &[b"INF.NS", b"CREATE", b"acks", b"MODE", b"durable", b"FSYNC", b"always"];
    assert_eq!(quiet.call(&mut setup, create, "the always CREATE"), b"+OK\r\n");
    (0..count)
        .map(|_| {
            let mut writer = MiniClient::connect(&mut quiet.node, 0);
            assert_eq!(quiet.call(&mut writer, &[b"INF.NS", b"USE", b"acks"], "USE"), b"+OK\r\n");
            writer
        })
        .collect()
}

/// The cell's durable stats, read between scheduler steps.
fn stats(quiet: &Quiet) -> inf_server::DurableStats {
    quiet.node.plane(0).durable_stats().expect("the cell's durable plane")
}

/// One `always` SET from each of `writers`, delivered whole at the next step, then steps
/// until the cell has appended `appended` records in all, and `SETTLE_STEPS` more.
fn send_sets(quiet: &mut Quiet, writers: &mut [MiniClient], first_key: usize, appended: u64) {
    let delivery = quiet.node.nets[0].borrow_mut().deliver_whole();
    for (n, writer) in (first_key..).zip(writers.iter_mut()) {
        let key = format!("ack:{n:03}");
        writer.send(&mut quiet.node, &[b"SET", key.as_bytes(), b"1"]);
    }
    for _ in 0..STEPS_MAX {
        if stats(quiet).records_appended >= appended {
            break;
        }
        quiet.step();
    }
    for _ in 0..SETTLE_STEPS {
        quiet.step();
    }
    quiet.node.nets[0].borrow_mut().restore_delivery(delivery);
}

/// Acks read from `writers` now, each checked `+OK`.
fn acks(quiet: &mut Quiet, writers: &mut [MiniClient]) -> usize {
    let mut acked = 0;
    for writer in writers {
        while let Some(reply) = writer.recv(&mut quiet.node) {
            assert_eq!(reply.as_slice(), b"+OK\r\n", "an always write's ack");
            acked += 1;
        }
    }
    acked
}

/// `k` writers, each one `always` SET held behind the device's barriers: the node booted at
/// `boot`, the writes appended, no ack read and every wait queued before the caller lifts.
fn held_writers(boot: QuietBoot, k: usize) -> (Quiet, Vec<MiniClient>) {
    let mut quiet = Quiet::boot_with(boot);
    let mut writers = always_writers(&mut quiet, k);
    let before = stats(&quiet).records_appended;
    quiet.disk.hold_barriers(Some(any_file));
    send_sets(&mut quiet, &mut writers, 0, before + k as u64);
    assert_eq!(acks(&mut quiet, &mut writers), 0, "an ack before its barrier completed");
    assert_eq!(quiet.node.plane(0).ack_waiters_for_sim(), k, "each writer's wait is queued");
    (quiet, writers)
}

/// The cut leg's writers: three, so the executor's drop removes entries that sift the rest.
const CUT_WRITERS: usize = 3;

/// A task dropped with its watermark wait queued leaves the heap in its executor's drop, the
/// one product path that ends a queued wait (ADR-0177 D2, D5): three writers' `always` writes
/// queued under a held barrier, the second writer closed by a FIN and the third by a reset,
/// then the node dropped as the simulator's cut drops it, a read-only view of the gate's count
/// held. Neither close ends a wait: a FIN keeps the slot flushing the replies it owes
/// (ADR-0196 A1.1) and a reset drains its queue (ADR-0123 A2), so all three stay queued.
/// Oracle: the view reads 0 after the drop (`waits-left-at-the-cut`). Engagement, each else
/// its `VACUOUS` line, read first: three waits queued before the closes (`waits-unqueued`),
/// the FIN's end of stream posted and the reset's connection closed (`close-unreached`), and
/// all three still queued after both (`wait-left-at-a-close`).
#[test]
fn a_task_its_executor_drops_with_its_wait_queued_leaves_the_heap() {
    let mut quiet = Quiet::boot(1, DurableScenario::SPIN_ITERS, 0);
    let mut writers = always_writers(&mut quiet, CUT_WRITERS);
    let before = stats(&quiet).records_appended;
    quiet.disk.hold_barriers(Some(any_file));
    send_sets(&mut quiet, &mut writers, 0, before + CUT_WRITERS as u64);
    let queued = quiet.node.plane(0).ack_waiters_for_sim();
    assert_eq!(
        queued, CUT_WRITERS,
        "VACUOUS waits-unqueued: {queued} of {CUT_WRITERS} waits queued before the closes"
    );
    let reset_fd = writers.pop().expect("the third writer").fd;
    let fin = writers.pop().expect("the second writer");
    let fin_fd = fin.fd;
    fin.close(&mut quiet.node);
    quiet.node.nets[0].borrow_mut().client_reset(reset_fd);
    let reached = |quiet: &Quiet| {
        let net = quiet.node.nets[0].borrow();
        (net.eof_posted(fin_fd), net.closed(reset_fd))
    };
    for _ in 0..STEPS_MAX {
        if reached(&quiet) == (true, true) {
            break;
        }
        quiet.step();
    }
    for _ in 0..SETTLE_STEPS {
        quiet.step();
    }
    let (eof_posted, closed) = reached(&quiet);
    let fin_closed = quiet.node.nets[0].borrow().closed(fin_fd);
    assert!(eof_posted, "VACUOUS close-unreached: the FIN's end of stream was never posted");
    assert!(closed, "VACUOUS close-unreached: the reset connection was never closed");
    let after = quiet.node.plane(0).ack_waiters_for_sim();
    assert_eq!(
        after, CUT_WRITERS,
        "VACUOUS wait-left-at-a-close: {after} of {CUT_WRITERS} waits queued after the closes"
    );
    let view = quiet.node.plane(0).ack_waiting_view_for_sim().expect("the cell's durable plane");
    let Quiet { node, .. } = quiet;
    drop(node);
    let left = view.waiting();
    eprintln!(
        "cut leg: {queued} queued before the closes; FIN end of stream {eof_posted}, its slot \
         closed {fin_closed}; reset closed {closed}; {after} queued after; {left} after the drop"
    );
    assert_eq!(
        left, 0,
        "waits-left-at-the-cut: {left} waits queued on the gate after the node's drop, where \
         its executor's drop removes each"
    );
}

/// A Direct seed of the shipped scenario with three frames in flight and no fill or group
/// hold (ADR-0087 D7): its lift lands two covering completions in one turn.
const DIRECT_K3_SEED: u64 = 5;

/// Steps from the lift until R, the first turn whose `fsync_groups` rose past `groups`:
/// answers R, in turns from the lift (the lift's own is 1), and the covering completions R
/// reaped. A step that skips the cell runs no turn.
fn step_to_reap(quiet: &mut Quiet, groups: u64) -> (u32, u64) {
    let mut turns = 0;
    for _ in 0..STEPS_MAX {
        if quiet.step().is_none() {
            continue;
        }
        turns += 1;
        let rise = stats(quiet).fsync_groups - groups;
        if rise > 0 {
            return (turns, rise);
        }
    }
    panic!("no covering completion in {STEPS_MAX} steps after the lift");
}

/// Several covering syncs reaped in one turn wake at most the budget between two LOG steps
/// (ADR-0177 A2.2): at a budget of 2, three write-through frames of three writers each held,
/// then lifted. R reaps two covering completions or more: the first sync's batch takes the
/// turn's two wakes, a later one finds none left, and the LOG step wakes two more, so
/// `waiting()` after R is `k - 2b`, 5, at any count past one; then all nine are acknowledged.
/// Engagement, each else its `VACUOUS` line, read first: three write-through barriers held
/// before the lift (`three-frames-unheld`), and two covering completions or more in R, the
/// least count at which a full budget a sync reads apart (`covering-completions-unengaged`).
/// Oracle: `waiting()` after R (`wakes-past-the-budget`).
#[test]
fn several_covering_syncs_in_one_turn_wake_at_most_the_budget() {
    const K: usize = 9;
    const B: usize = 2;
    let spin_iters = DurableScenario::SPIN_ITERS;
    let boot =
        QuietBoot { seed: DIRECT_K3_SEED, cells: 1, spin_iters, watched: 0, wakes: wakes(B) };
    let mut quiet = Quiet::boot_with(boot);
    let mut writers = always_writers(&mut quiet, K);
    let before = stats(&quiet).records_appended;
    quiet.disk.hold_barriers(Some(any_file));
    for frame in 0..3 {
        let group = &mut writers[3 * frame..3 * frame + 3];
        send_sets(&mut quiet, group, 3 * frame, before + 3 * (frame as u64 + 1));
    }
    let held = quiet.node.driver(0).held_barriers();
    assert_eq!(
        held.write_through, 3,
        "VACUOUS three-frames-unheld: {held:?} held, where three write-through frames are"
    );
    assert_eq!(acks(&mut quiet, &mut writers), 0, "an ack before its barrier completed");
    assert_eq!(quiet.node.plane(0).ack_waiters_for_sim(), K, "each writer's wait is queued");
    let groups = stats(&quiet).fsync_groups;
    quiet.disk.hold_barriers(None);
    let (reap, covering) = step_to_reap(&mut quiet, groups);
    eprintln!("several syncs, seed {DIRECT_K3_SEED}: {held:?} held, R turn {reap}, c {covering}");
    assert!(
        covering >= 2,
        "VACUOUS covering-completions-unengaged: R reaped {covering} covering completions, \
         where two or more are"
    );
    let left = quiet.node.plane(0).ack_waiters_for_sim();
    assert_eq!(
        left,
        K - 2 * B,
        "wakes-past-the-budget: {left} waits left after R, where its first sync's batch and \
         the LOG step's woke {B} each"
    );
    let mut acked = acks(&mut quiet, &mut writers);
    for _ in 0..STEPS_MAX {
        if acked == K {
            break;
        }
        quiet.step();
        acked += acks(&mut quiet, &mut writers);
    }
    assert_eq!(acked, K, "stranded: {acked} of {K} writes acknowledged");
    assert_eq!(quiet.node.plane(0).ack_waiters_for_sim(), 0, "a wait left after every ack");
}

/// EG12's schedule for one sync covering `k` waits at a budget of `b` (ADR-0177 A2.2), from
/// the loop's step order: R, the turn that reaps the covering completion, wakes the reap's
/// batch and the LOG step's; each later turn runs the batch the turn before it woke, writes
/// its acks, and its LOG step wakes the next; a written ack is read the turn after.
#[derive(Clone, Copy, Debug)]
struct Schedule {
    k: usize,
    b: usize,
}

impl Schedule {
    /// The batches the sync's waits take: `⌈k / b⌉`.
    fn batches(self) -> usize {
        self.k.div_ceil(self.b)
    }

    /// `waiting()` after turn R + j: two batches woken at R, one each turn after.
    fn waiting_after(self, j: usize) -> usize {
        self.k.saturating_sub((j + 2) * self.b)
    }

    /// The acks the client holds after turn R + n: a batch's acks read a turn after it ran.
    fn acks_after(self, n: usize) -> usize {
        self.k.min(n * self.b)
    }

    /// LOG steps that leave the gate owing: each but the last batch's, the reap's batch none,
    /// and when `b` divides `k` the last batch ends on its budget with no peek, so its LOG
    /// step leaves the debt one step more.
    fn deferred(self) -> u64 {
        let batches = self.batches();
        let steps =
            if self.k.is_multiple_of(self.b) { batches - 1 } else { batches.saturating_sub(2) };
        u64::try_from(steps).expect("a step count fits a u64")
    }
}

/// One turn's reads, between scheduler steps.
#[derive(Clone, Copy, Debug)]
struct TurnRead {
    parked: bool,
    owes: bool,
    waiting: usize,
    /// The acks the client holds after the turn.
    acks: usize,
}

/// What the bound row read at one point.
struct BoundRead {
    schedule: Schedule,
    /// The failed clauses, each line naming its clause.
    failed: Vec<String>,
    /// Covering completions over the run, the turns read owing, and the LOG steps that left
    /// wakes owed.
    syncs: u64,
    owing_turns: usize,
    deferred: u64,
    /// Parks within `SETTLE_STEPS` after the run: the instrument's control.
    control_parks: usize,
}

/// The bound row at `schedule`'s (k, b): `k` writers under one held barrier on `Quiet`'s seed
/// at `spin_iters: 0`, where an idle cell parks at once; the lift, then each turn read to R
/// and `⌈k / b⌉ + 2` turns past it. The clauses are collected, never asserted here.
fn bound_point(schedule: Schedule) -> BoundRead {
    let Schedule { k, b } = schedule;
    let boot = QuietBoot { seed: 0xC0FFEE, cells: 1, spin_iters: 0, watched: 0, wakes: wakes(b) };
    let (mut quiet, mut writers) = held_writers(boot, k);
    let start = stats(&quiet);
    let mut owed = quiet.node.plane(0).ack_wakes_owed_for_sim();
    quiet.disk.hold_barriers(None);
    let (mut reads, mut acked, mut reap) = (Vec::new(), 0, None);
    for _ in 0..STEPS_MAX {
        let Some(parked) = quiet.step() else { continue };
        acked += acks(&mut quiet, &mut writers);
        let plane = quiet.node.plane(0);
        let (owes, waiting) = (plane.ack_wakes_owed_for_sim(), plane.ack_waiters_for_sim());
        let groups = stats(&quiet).fsync_groups - start.fsync_groups;
        reads.push(TurnRead { parked, owes, waiting, acks: acked });
        let turn = reads.len();
        if reap.is_none() && groups > 0 {
            reap = Some(turn);
        }
        if reap.is_some_and(|reap| turn >= reap + schedule.batches() + 2) {
            break;
        }
    }
    let mut failed = Vec::new();
    for (turn, read) in (1..).zip(&reads) {
        if owed && read.parked {
            failed.push(format!("parked-owing: ({k}, {b}) turn {turn} parked owing a wake"));
        }
        owed = read.owes;
        let (waiting, held) = match reap {
            Some(reap) if turn >= reap => {
                let j = turn - reap;
                (schedule.waiting_after(j), schedule.acks_after(j))
            }
            Some(_) | None => (k, 0),
        };
        if (read.waiting, read.acks) != (waiting, held) {
            let (got, acks) = (read.waiting, read.acks);
            failed.push(format!(
                "wakes-schedule: ({k}, {b}) turn {turn} read {got} queued and {acks} acks, \
                 where EG12 has {waiting} and {held}"
            ));
        }
    }
    let end = stats(&quiet);
    let reap_turn = reap.unwrap_or(0);
    let column =
        |read: fn(&TurnRead) -> String| -> Vec<String> { reads.iter().map(read).collect() };
    eprintln!(
        "bound row ({k}, {b}): R turn {reap_turn}, D {}; by turn from the lift, queued {:?}, \
         acks {:?}, owes {:?}, parked {:?}; deferred {}",
        reap_turn.saturating_sub(1),
        column(|read| read.waiting.to_string()),
        column(|read| read.acks.to_string()),
        column(|read| u8::from(read.owes).to_string()),
        column(|read| u8::from(read.parked).to_string()),
        end.ack_wakes_deferred - start.ack_wakes_deferred,
    );
    let control_parks = (0..SETTLE_STEPS).filter(|_| quiet.step() == Some(true)).count();
    BoundRead {
        schedule,
        failed,
        syncs: end.fsync_groups - start.fsync_groups,
        owing_turns: reads.iter().filter(|read| read.owes).count(),
        deferred: end.ack_wakes_deferred - start.ack_wakes_deferred,
        control_parks,
    }
}

/// More covered acks than one budget leave a batch a turn, and a cell owing a wake never
/// parks (ADR-0177 A2.2; EG11, EG12): the bound row at (9, 2) and (9, 4), `b` not dividing
/// `k`, and at (8, 4), `b` dividing it, each against EG12's one function of (k, b). Oracle:
/// after every turn from the lift, `waiting()` and the acks the client holds as EG12 has them
/// (`wakes-schedule`), and no turn parked while the turn before it left the gate owing
/// (`parked-owing`), by the park instrument and `owes()` between steps; every point's failed
/// clauses in one assertion, read before any engagement. Engagement, else its `VACUOUS`
/// line: one sync covered all k (`one-sync-unread`), a turn read owing and the LOG steps that
/// left wakes owed as EG12 counts them (`owing-turn-unread`). Control: the cell parks once
/// the last ack is read, so the instrument reads a park in this run. R is the device's, and
/// judges nothing: D, its turns from the lift, is printed.
#[test]
fn acks_past_the_budget_follow_the_loops_schedule_and_an_owing_cell_never_parks() {
    let points = [Schedule { k: 9, b: 2 }, Schedule { k: 9, b: 4 }, Schedule { k: 8, b: 4 }];
    let reads: Vec<BoundRead> = points.into_iter().map(bound_point).collect();
    let failed: Vec<&str> =
        reads.iter().flat_map(|read| read.failed.iter().map(String::as_str)).collect();
    assert!(failed.is_empty(), "{} clauses failed: {}", failed.len(), failed.join("; "));
    for read in &reads {
        let Schedule { k, b } = read.schedule;
        assert_eq!(
            read.syncs, 1,
            "VACUOUS one-sync-unread: ({k}, {b}) took {} covering syncs, where one is",
            read.syncs
        );
        let expected = read.schedule.deferred();
        assert!(
            read.owing_turns > 0 && read.deferred == expected,
            "VACUOUS owing-turn-unread: ({k}, {b}) read {} turns owing and {} LOG steps that \
             left wakes owed, where EG12 counts {expected}",
            read.owing_turns,
            read.deferred
        );
        assert!(read.control_parks > 0, "control: ({k}, {b}) never parked after the last ack");
    }
}
