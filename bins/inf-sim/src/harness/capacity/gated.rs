//! `m0-capacity`'s gated legs: an owner's `always` verdicts for a peer
//! (ADR-0177 D3, D6), each published only once the owner's fsync watermark
//! covers it. Two cells, X = 0 the origin of every write and Y = 1 the owner
//! of every key, durable, on the small-credit mesh (`d` = 8), with the
//! simulated device holding every barrier while the writes are sent:
//!
//! - `gated-hold`: 8 clients of X each send one `SET` of a key of one length
//!   in a flat `always` namespace, in one step, delivered whole; 7 reach Y,
//!   whose fills park 7 verdicts for X.
//! - `gated-cold`, the overtake: Y has a tiered `always` namespace with a
//!   key made cold before the holds, and the flat one. With the device also
//!   holding Y's tier reads, a client of X `APPEND`s to the cold key, so Y's
//!   pump opens its slot and suspends on the held read; then another client
//!   of X `SET`s a key of the flat namespace, which only the pump-active term
//!   of the divert parks behind it.
//! - `gated-error`: a client of X `INCR`s a key Y holds as text, its error a
//!   reply read under the hold; then a `SET` in the flat namespace.
//!
//! Each clause a token: `frame-barrier-unheld`, `writes-unstaged`,
//! `verdicts-unparked`, `gated-unpolled`, `cold-read-unheld` and
//! `overtook-held-apply` are engagement (`VACUOUS`); `acked-under-hold` (no
//! write's ack is read before the lift), `error-held` (the error is),
//! `reply-stalled-after-lift` and `reply-wrong-after-lift` (every reply after
//! it, exact), `parked-bytes-kept` and `parked-refused` (the deques' end
//! state) are the oracles. They read the leg's own clients, its holds and the
//! census rows, never the product's verdict. An overtake stops Y's cell at
//! its gated deque's `second-open-slot` check before any clause reads it.

use std::path::Path;

use inf_foundation::hash64;

use super::small_credit::{
    GENERAL_WINDOW, Run, SETTLE_STEPS, flat_bytes, padded_names_on, tiered_create,
};
use super::{CapacityReport, CloseLeg, X, Y};
use crate::driver::Violations;
use crate::durable::{MiniClient, STALL_STEPS, TraceObserver, is_tier_file};

/// The node's cells.
const CELLS: u16 = 2;
/// The flat `always` namespace of every gated leg.
const FLAT: &[u8] = b"gh";
/// The tiered `always` namespace of `gated-cold`.
const COLD: &[u8] = b"gc";
/// The writes of `gated-hold`.
const SETS: usize = 8;
/// A cold-fill key's value, and the `SET`s pipelined a round trip.
const COLD_VALUE_LEN: usize = 400;
const COLD_SET_BATCH: usize = 500;
/// Keys written at most before the window gives its oldest pages back: five
/// times the namespace's 3 MiB.
const COLD_KEYS_MAX: usize = 40_000;

/// Holds every barrier on every file.
fn any_file(_: &Path) -> bool {
    true
}

/// One leg's phases: its node, its violations and its readings.
type Phases = fn(&mut Run, &mut Violations, &mut Vec<(&'static str, u64)>) -> Result<(), String>;

/// `gated-hold`.
pub(super) fn gated_hold_leg(seed: u64) -> CapacityReport {
    gated_leg(seed, CloseLeg::GatedHold, hold)
}

/// `gated-cold`.
pub(super) fn gated_cold_leg(seed: u64) -> CapacityReport {
    gated_leg(seed, CloseLeg::GatedCold, cold)
}

/// `gated-error`.
pub(super) fn gated_error_leg(seed: u64) -> CapacityReport {
    gated_leg(seed, CloseLeg::GatedError, error)
}

/// Runs one gated leg; the node drops before the observer seals, so every
/// cell's census verdict is in the report.
fn gated_leg(seed: u64, leg: CloseLeg, phases: Phases) -> CapacityReport {
    let observer = TraceObserver::default();
    let mut violations = Violations::default();
    let mut readings = Vec::new();
    let steps = match Run::boot(seed, CELLS, &observer) {
        Ok(mut run) => {
            let ran = phases(&mut run, &mut violations, &mut readings);
            if let Err(what) = ran {
                violations.push(format!("{}: {what}", leg.name()));
            }
            run.steps
        }
        Err(what) => {
            violations.push(format!("{}: {what}", leg.name()));
            0
        }
    };
    let sealed = observer.seal();
    readings.push(("cap_rows_checked", sealed.census.rows_checked));
    readings.push(("cap_rows_crossed", sealed.census.rows_crossed));
    violations.extend(sealed.census.violations);
    CapacityReport {
        leg,
        trace_hash: hash64(&observer.trace_bytes(), seed),
        state_hash: sealed.state_hash,
        violations,
        scheduler_steps: steps,
        readings,
    }
}

/// The census row `row`'s high water on Y.
fn high_water(run: &Run, row: &str) -> u64 {
    run.node.plane(Y).cap_census().row(row).map_or(0, |row| u64::from(row.high_water))
}

/// Y's appended records.
fn appended(run: &Run) -> u64 {
    run.node.plane(Y).durable_stats().map_or(0, |stats| stats.records_appended)
}

/// `CREATE` of the flat `always` namespace, from a client of X.
fn create_flat(run: &mut Run) -> Result<(), String> {
    let create: &[&[u8]] = &[b"INF.NS", b"CREATE", FLAT, b"MODE", b"durable", b"FSYNC", b"always"];
    match run.call(X, create)?.as_slice() {
        b"+OK\r\n" => Ok(()),
        other => Err(format!("CREATE answered {:?}", String::from_utf8_lossy(other))),
    }
}

/// Reads what reached `clients` into `replies`, one reply each at most.
fn read_into(run: &mut Run, clients: &mut [MiniClient], replies: &mut [Option<Vec<u8>>]) {
    for (client, reply) in clients.iter_mut().zip(replies.iter_mut()) {
        if reply.is_none() {
            *reply = client.recv(&mut run.node);
        }
    }
}

/// The hold's engagement once the burst is in: the device held a barrier of
/// either class (a frame's on Buffered seeds, a standalone fdatasync on
/// Direct ones), Y appended the batch, Y's gated deque for X parked its
/// verdicts (`verdicts` of them, or over is red), and with no ack read Y
/// parks a watermark wait.
fn engaged(run: &Run, before: u64, verdicts: u64, violations: &mut Violations) {
    let driver = run.node.driver(Y);
    if driver.held_syncs() == 0 {
        let held = driver.held_barriers();
        violations.push(format!("VACUOUS frame-barrier-unheld: no barrier held ({held:?})"));
    }
    let staged = appended(run) - before;
    if staged < verdicts {
        violations.push(format!("VACUOUS writes-unstaged: {staged} of {verdicts} appended on Y"));
    }
    let parked = high_water(run, "gated");
    if parked < verdicts {
        violations.push(format!("VACUOUS verdicts-unparked: Y's gated row held {parked}"));
    } else if parked > verdicts {
        violations.push(format!(
            "verdicts-unparked: Y's gated row held {parked}, over the {verdicts} X has un-replied"
        ));
    }
    if run.node.plane(Y).ack_waiters_for_sim() == 0 {
        violations.push("VACUOUS gated-unpolled: Y parked no watermark wait".into());
    }
}

/// Steps until a client of `clients` reads a reply, or Y has appended `want`
/// records past `before` and parks a watermark wait, at most `STALL_STEPS`,
/// then settles. Returns what each client read under the hold.
fn step_held(
    run: &mut Run,
    clients: &mut [MiniClient],
    before: u64,
    want: u64,
) -> Result<Vec<Option<Vec<u8>>>, String> {
    let mut early = vec![None; clients.len()];
    for _ in 0..STALL_STEPS {
        read_into(run, clients, &mut early);
        let staged = appended(run) - before >= want;
        if early.iter().any(Option::is_some)
            || (staged && run.node.plane(Y).ack_waiters_for_sim() > 0)
        {
            break;
        }
        run.step().map_err(|err| format!("step failed: {err}"))?;
    }
    for _ in 0..SETTLE_STEPS {
        run.step().map_err(|err| format!("step failed: {err}"))?;
        read_into(run, clients, &mut early);
    }
    Ok(early)
}

/// After the barrier lift: every client reads its reply, exact, the deques
/// end empty and no op was refused.
fn lifted(
    run: &mut Run,
    clients: &mut [MiniClient],
    mut replies: Vec<Option<Vec<u8>>>,
    want: &[&[u8]],
    violations: &mut Violations,
) -> Result<(), String> {
    run.disk.hold_barriers(None);
    for _ in 0..STALL_STEPS {
        read_into(run, clients, &mut replies);
        if replies.iter().all(Option::is_some) {
            break;
        }
        run.step().map_err(|err| format!("step failed: {err}"))?;
    }
    for (index, (reply, want)) in replies.iter().zip(want).enumerate() {
        match reply {
            Some(reply) if reply == want => {}
            Some(reply) => violations.push(format!(
                "reply-wrong-after-lift: client {index} read {:?}, want {:?}",
                String::from_utf8_lossy(reply),
                String::from_utf8_lossy(want)
            )),
            None => violations.push(format!(
                "reply-stalled-after-lift: client {index} unanswered {STALL_STEPS} steps after \
                 the lift"
            )),
        }
    }
    for _ in 0..SETTLE_STEPS {
        run.step().map_err(|err| format!("step failed: {err}"))?;
    }
    let quiet = run.tripwire(Y, "fabric_parked_bytes")?;
    if quiet != 0 {
        violations.push(format!("parked-bytes-kept: Y's fabric_parked_bytes reads {quiet}"));
    }
    for cell in [X, Y] {
        let refused = run.tripwire(cell, "fabric_parked_refused")?;
        if refused != 0 {
            violations.push(format!("parked-refused: cell {cell} refused {refused} op(s)"));
        }
    }
    Ok(())
}

/// No client read a reply under the hold.
fn none_acked(early: &[Option<Vec<u8>>], violations: &mut Violations) {
    for (index, reply) in early.iter().enumerate() {
        if let Some(reply) = reply {
            violations.push(format!(
                "acked-under-hold: client {index} read {:?} before Y's fsync",
                String::from_utf8_lossy(reply)
            ));
        }
    }
}

/// The 8 `SET`s of `gated-hold` from clients of X, sent at one step and
/// delivered whole; returns the clients.
fn send_sets(run: &mut Run) -> Result<Vec<MiniClient>, String> {
    let mut clients = Vec::new();
    for _ in 0..SETS {
        clients.push(run.client_in(X, FLAT)?);
    }
    let delivery = run.node.nets[X].borrow_mut().deliver_whole();
    for (client, name) in clients.iter_mut().zip(padded_names_on(CELLS, Y, "gh", SETS)) {
        client.send(&mut run.node, &[b"SET", &name, b"1"]);
    }
    run.node.nets[X].borrow_mut().restore_delivery(delivery);
    Ok(clients)
}

/// `gated-hold`.
fn hold(
    run: &mut Run,
    violations: &mut Violations,
    readings: &mut Vec<(&'static str, u64)>,
) -> Result<(), String> {
    create_flat(run)?;
    run.disk.hold_barriers(Some(any_file));
    let before = appended(run);
    let mut clients = send_sets(run)?;
    let early = step_held(run, &mut clients, before, GENERAL_WINDOW)?;
    readings.push(("gated_high_water", high_water(run, "gated")));
    if early.iter().all(Option::is_none) {
        engaged(run, before, GENERAL_WINDOW, violations);
    }
    none_acked(&early, violations);
    lifted(run, &mut clients, early, &[&b"+OK\r\n"[..]; SETS], violations)
}

/// `gated-cold`.
fn cold(
    run: &mut Run,
    violations: &mut Violations,
    readings: &mut Vec<(&'static str, u64)>,
) -> Result<(), String> {
    let created = run.call(Y, &tiered_create(COLD))?;
    if created != b"+OK\r\n" {
        return Err(format!("CREATE answered {:?}", String::from_utf8_lossy(&created)));
    }
    let key = fill_cold(run)?;
    create_flat(run)?;
    let mut clients = [run.client_in(X, COLD)?, run.client_in(X, FLAT)?];
    let mut early = vec![None; clients.len()];
    run.disk.hold_barriers(Some(any_file));
    run.disk.hold_reads(Some(is_tier_file));
    clients[0].send(&mut run.node, &[b"APPEND", &key, b"+"]);
    for _ in 0..STALL_STEPS {
        read_into(run, &mut clients, &mut early);
        if early.iter().any(Option::is_some) || run.node.driver(Y).held_reads() > 0 {
            break;
        }
        run.step().map_err(|err| format!("step failed: {err}"))?;
    }
    if run.node.driver(Y).held_reads() == 0 {
        violations.push("VACUOUS cold-read-unheld: Y held no tier read for the APPEND".into());
    }
    let name = padded_names_on(CELLS, Y, "gh", 1).remove(0);
    let set: [&[u8]; 3] = [b"SET", &name, b"1"];
    clients[1].send(&mut run.node, &set);
    for _ in 0..SETTLE_STEPS {
        run.step().map_err(|err| format!("step failed: {err}"))?;
        read_into(run, &mut clients, &mut early);
    }
    let parked = run.tripwire(Y, "fabric_parked_bytes")?;
    readings.push(("fabric_parked_bytes_held", parked));
    if parked != flat_bytes(&set) {
        violations.push(format!(
            "VACUOUS overtook-held-apply: Y parked {parked} B under the held read, not the \
             SET's {} B",
            flat_bytes(&set)
        ));
    }
    run.disk.hold_reads(None);
    for _ in 0..SETTLE_STEPS {
        run.step().map_err(|err| format!("step failed: {err}"))?;
        read_into(run, &mut clients, &mut early);
    }
    none_acked(&early, violations);
    let appended = format!(":{}\r\n", COLD_VALUE_LEN + 1).into_bytes();
    lifted(run, &mut clients, early, &[appended.as_slice(), &b"+OK\r\n"[..]], violations)
}

/// Writes keys Y owns in the tiered namespace, from a client of Y, until
/// Y's window gave its oldest pages back; returns the oldest written, now
/// cold. A write parked past its tail-stall timeout answers the typed
/// `STALLED` verdict unapplied (ADR-0053 D4), and is sent again.
fn fill_cold(run: &mut Run) -> Result<Vec<u8>, String> {
    let router = inf_store::SlotRouter::new_contiguous(CELLS);
    let mut keys = (0..).map(|i: usize| format!("c:{i:05}").into_bytes()).filter(move |name| {
        usize::from(router.cell_of(inf_store::SlotRouter::slot_of(name)).0) == Y
    });
    let mut writer = run.client_in(Y, COLD)?;
    let value = vec![b'c'; COLD_VALUE_LEN];
    let (mut written, mut stalled) = (0, 0);
    let mut oldest = None;
    let mut batch: Vec<Vec<u8>> = Vec::new();
    loop {
        if written + stalled >= COLD_KEYS_MAX {
            return Err(format!("{written} keys ({stalled} stalled) made no cold working set"));
        }
        while batch.len() < COLD_SET_BATCH {
            batch.push(keys.next().ok_or("no cold key")?);
        }
        for name in &batch {
            writer.send(&mut run.node, &[b"SET", name, &value]);
        }
        let mut retry = Vec::new();
        let mut answered = 0;
        for _ in 0..STALL_STEPS {
            while let Some(reply) = writer.recv(&mut run.node) {
                let name = batch.get(answered).ok_or("a cold fill reply past its batch")?;
                if reply == b"+OK\r\n" {
                    oldest.get_or_insert_with(|| name.clone());
                    written += 1;
                } else if reply.starts_with(b"-STALLED ") {
                    retry.push(name.clone());
                    stalled += 1;
                } else {
                    let reply = String::from_utf8_lossy(&reply);
                    return Err(format!("a cold fill SET read {reply:?}"));
                }
                answered += 1;
            }
            if answered == batch.len() {
                break;
            }
            run.step().map_err(|err| format!("step failed: {err}"))?;
        }
        if answered < batch.len() {
            return Err(format!("{answered} of {} cold fill SETs answered", batch.len()));
        }
        batch = retry;
        let tiering = run.call_on(&mut writer, &[b"INFO", b"tiering"])?;
        let field = |name: &str| info_field(&tiering, name);
        if field("tiering_flush_confirmed_bytes") > 3 << 20
            && field("tiering_region_decommit_pages") > 0
        {
            writer.close(&mut run.node);
            return oldest.ok_or_else(|| "no cold fill SET applied".to_owned());
        }
    }
}

/// One field of an `INFO` section's text, as a number (0 when absent).
fn info_field(info: &[u8], name: &str) -> u64 {
    String::from_utf8_lossy(info)
        .lines()
        .find_map(|line| line.strip_prefix(name).and_then(|rest| rest.strip_prefix(':')))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

/// `gated-error`.
fn error(
    run: &mut Run,
    violations: &mut Violations,
    readings: &mut Vec<(&'static str, u64)>,
) -> Result<(), String> {
    create_flat(run)?;
    let text = padded_names_on(CELLS, Y, "ge", 1).remove(0);
    let mut owner = run.client_in(Y, FLAT)?;
    let set = run.call_on(&mut owner, &[b"SET", &text, b"x"])?;
    if set != b"+OK\r\n" {
        return Err(format!("the owner's SET read {:?}", String::from_utf8_lossy(&set)));
    }
    owner.close(&mut run.node);
    let mut incr = run.client_in(X, FLAT)?;
    let mut setter = [run.client_in(X, FLAT)?];
    run.disk.hold_barriers(Some(any_file));
    incr.send(&mut run.node, &[b"INCR", &text]);
    let mut error = None;
    for _ in 0..STALL_STEPS {
        error = incr.recv(&mut run.node);
        if error.is_some() {
            break;
        }
        run.step().map_err(|err| format!("step failed: {err}"))?;
    }
    match error.as_deref() {
        Some(b"-ERR value is not an integer or out of range\r\n") => {}
        Some(other) => violations.push(format!(
            "error-held: the INCR read {:?} under the hold",
            String::from_utf8_lossy(other)
        )),
        None => violations.push("error-held: the INCR's error waited for the hold".into()),
    }
    let before = appended(run);
    let name = padded_names_on(CELLS, Y, "gh", 1).remove(0);
    setter[0].send(&mut run.node, &[b"SET", &name, b"1"]);
    let early = step_held(run, &mut setter, before, 1)?;
    readings.push(("gated_high_water", high_water(run, "gated")));
    none_acked(&early, violations);
    lifted(run, &mut setter, early, &[&b"+OK\r\n"[..]], violations)?;
    incr.close(&mut run.node);
    Ok(())
}
