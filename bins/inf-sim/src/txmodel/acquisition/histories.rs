//! The named histories (F2, F4/F5, ADR-0116 A1–A10) and the phase tables
//! the model's tests and sweeps replay.

use super::*;

pub(super) fn name(txn: Option<usize>) -> String {
    txn.map_or("absent".to_string(), |w| format!("T{}", w + 1))
}

/// The F2 two-key history: T1 (lower txid) reaches X's owner first,
/// T2 reaches Y's owner first, then each reaches the other.
pub fn two_key_history(rules: Rules) -> Report {
    let mut m = Model::new(rules, 2, 7);
    let t1 = m.submit(TxnSpec { coordinator: 0, writes: vec![0, 1], ..Default::default() });
    let t2 = m.submit(TxnSpec { coordinator: 1, writes: vec![0, 1], ..Default::default() });
    // Parallel fans: X@0 (T1), Y@1 (T2), Y@1 (T1), X@0 (T2).
    m.script(&[(t1, 0), (t2, 1), (t1, 1), (t2, 0)]);
    m.run(10_000).clone()
}

/// A seeded storm: `txns` transactions over `keys` keys across `cells`
/// cells, ordered programs of 1–4 `GET`/`SET`/WATCH steps, random
/// delivery; with `faults`, one in four is native, one in four fails
/// its first plain write, one in four carries a cross-owner `RENAME`
/// or `MSETNX` at a random queue position (one in four of those a
/// second one, one in four with a refusing owner) and one in four is
/// cancelled at a random phase of a random stage.
pub fn storm(rules: Rules, cells: usize, keys: u32, txns: usize, seed: u64) -> Report {
    storm_with(rules, cells, keys, txns, seed, false)
}

pub fn storm_with(
    rules: Rules,
    cells: usize,
    keys: u32,
    txns: usize,
    seed: u64,
    faults: bool,
) -> Report {
    let mut rng = Rng::new(seed ^ 0xA5A5);
    let mut m = Model::new(rules, cells, seed);
    for _ in 0..txns {
        let n = 1 + rng.below(4);
        let mut spec = TxnSpec { coordinator: rng.below(cells), ..Default::default() };
        for _ in 0..n {
            let k = rng.below(keys as usize) as Key;
            match rng.below(4) {
                0 => spec.program.push(Cmd::Get(k)),
                1 => spec.watches.push(k),
                _ => spec.program.push(Cmd::Set(k)),
            }
        }
        if faults {
            storm_faults(&mut rng, &mut spec, cells, keys);
        }
        m.submit(spec);
    }
    m.run(200_000).clone()
}

fn storm_faults(rng: &mut Rng, spec: &mut TxnSpec, cells: usize, keys: u32) {
    spec.native = rng.below(4) == 0;
    if rng.below(4) == 0 {
        spec.fails_at = spec.program.iter().find_map(|c| match c {
            Cmd::Set(k) => Some(*k as usize % cells),
            _ => None,
        });
    }
    let deps = match (rng.below(4), rng.below(4)) {
        (0, 0) => 2,
        (0, _) => 1,
        _ => 0,
    };
    for _ in 0..deps {
        if cells < 2 {
            break;
        }
        let a = rng.below(keys as usize) as Key;
        let b = rng.below(keys as usize) as Key;
        if a as usize % cells == b as usize % cells {
            continue;
        }
        let dep = if rng.below(2) == 0 {
            Dependent::Move { source: a, target: b }
        } else {
            Dependent::SetIfNoneExist { keys: vec![a, b] }
        };
        if rng.below(4) == 0 {
            spec.refuses_at = Some(match dep {
                Dependent::Move { target, .. } => target as usize % cells,
                Dependent::SetIfNoneExist { .. } => [a, b][rng.below(2)] as usize % cells,
            });
        }
        let at = rng.below(spec.program.len() + 1);
        spec.program.insert(at, Cmd::Dep(dep));
    }
    if rng.below(4) == 0 {
        let stages = stages_of(&spec.program);
        let s = if stages.is_empty() { 0 } else { stages[rng.below(stages.len())].0 };
        let combines: Vec<usize> = stages.iter().filter(|(_, d)| *d).map(|(i, _)| *i).collect();
        let phase = match rng.below(7) {
            0 => CancelPhase::Admit,
            1 => CancelPhase::Acquire(rng.below(3)),
            2 => CancelPhase::Execute(s),
            3 => CancelPhase::Executed(s, 1),
            4 => CancelPhase::Decided,
            5 if !combines.is_empty() => CancelPhase::Combine(combines[rng.below(combines.len())]),
            _ => CancelPhase::Durable,
        };
        let kind = if rng.below(2) == 0 { Cancel::Disconnect } else { Cancel::Timeout };
        spec.cancel = Some((phase, kind));
    }
}

/// Two clients: T writes `a@0` and `b@1`; a reader issues `GET a`
/// then `GET b` — under the read-uncommitted variant a read can
/// observe T's write before T's decision.
pub fn read_pair_history(rules: Rules, seed: u64) -> Report {
    let mut m = Model::new(rules, 2, seed);
    m.submit(TxnSpec { coordinator: 0, writes: vec![0, 1], ..Default::default() });
    m.client(Client {
        program: vec![
            TxnSpec { coordinator: 0, reads: vec![0], ..Default::default() },
            TxnSpec { coordinator: 1, reads: vec![1], ..Default::default() },
        ],
    });
    m.run(10_000).clone()
}

/// The WATCH-only interval history (ADR-0114 D1): T1 writes `a@0`
/// and watches `b@1`; W writes `b`. Under validate-and-release W
/// lands between T1's validation on cell 1 and T1's decision.
pub fn watch_interval_history(rules: Rules) -> Report {
    let mut m = Model::new(rules, 2, 11);
    let t1 = m.submit(TxnSpec {
        coordinator: 0,
        writes: vec![0],
        watches: vec![1],
        ..Default::default()
    });
    let w = m.submit(TxnSpec { coordinator: 1, writes: vec![1], ..Default::default() });
    // T1 acquires a@0, validates b@1 (clean); then W's lock, exec,
    // decision and unlock (publication) on b@1; then T1's exec and
    // decision.
    m.script(&[(t1, 0), (t1, 0), (t1, 1), (t1, 1), (w, 1), (w, 1), (w, 1), (w, COORD), (w, 1)]);
    m.run(10_000).clone()
}

/// A native `INF.TX` writing `a@0` and `b@1` whose leg on cell 1 fails
/// its condition after cell 0's leg succeeded: nothing may be
/// published (ADR-0116 D5/A4). Returns the report and T's outcome.
pub fn native_failure_history(rules: Rules) -> (Report, Outcome, Option<usize>) {
    let mut m = Model::new(rules, 2, 5);
    let t = m.submit(TxnSpec {
        coordinator: 0,
        writes: vec![0, 1],
        native: true,
        fails_at: Some(1),
        ..Default::default()
    });
    m.run(10_000);
    (m.report.clone(), m.outcome(t), m.published(0))
}

/// The same history as Redis `EXEC`: the failed command's error is
/// embedded, the rest commits.
pub fn exec_failure_history(rules: Rules) -> (Report, Outcome, [Option<usize>; 2]) {
    let mut m = Model::new(rules, 2, 5);
    let t = m.submit(TxnSpec {
        coordinator: 0,
        writes: vec![0, 1],
        fails_at: Some(1),
        ..Default::default()
    });
    m.run(10_000);
    (m.report.clone(), m.outcome(t), [m.published(0), m.published(1)])
}

/// T writes `a@0` and `b@1`; a disconnect or timeout lands the moment
/// T enters `phase`. Returns the report, T's outcome and whether each
/// key was published.
pub fn cancellation_history(
    rules: Rules,
    phase: CancelPhase,
    kind: Cancel,
    seed: u64,
) -> (Report, Outcome, [bool; 2]) {
    let mut m = Model::new(rules, 2, seed);
    let t = m.submit(TxnSpec {
        coordinator: 0,
        writes: vec![0, 1],
        cancel: Some((phase, kind)),
        ..Default::default()
    });
    m.run(10_000);
    let published = [m.published(0) == Some(t), m.published(1) == Some(t)];
    (m.report.clone(), m.outcome(t), published)
}

/// F04's first history: `SET a; RENAME a b` with `a@0`, `b@1` — the
/// destination needs the value staged on the other owner; with
/// `refuses`, the destination cannot reserve (ADR-0110's refused put).
/// Returns the report, T's outcome and the published writer of each
/// key.
pub fn rename_history(
    rules: Rules,
    native: bool,
    refuses: bool,
) -> (Report, Outcome, [Option<usize>; 2]) {
    let mut m = Model::new(rules, 2, 13);
    let t = m.submit(TxnSpec {
        coordinator: 0,
        writes: vec![0],
        dependent: Some(Dependent::Move { source: 0, target: 1 }),
        native,
        refuses_at: refuses.then_some(1),
        ..Default::default()
    });
    m.run(10_000);
    (m.report.clone(), m.outcome(t), [m.published(0), m.published(1)])
}

/// F04's second history: `MSETNX a b` with `a@0` absent and `b@1`
/// written by an earlier committed transaction — one condition over
/// both owners. Returns the report, T's outcome and the published
/// writer of each key (the earlier writer is index 0).
pub fn msetnx_history(rules: Rules, native: bool) -> (Report, Outcome, [Option<usize>; 2]) {
    let mut m = Model::new(rules, 2, 17);
    m.client(Client {
        program: vec![
            TxnSpec { coordinator: 1, writes: vec![1], ..Default::default() },
            TxnSpec {
                coordinator: 0,
                dependent: Some(Dependent::SetIfNoneExist { keys: vec![0, 1] }),
                native,
                ..Default::default()
            },
        ],
    });
    m.run(10_000);
    (m.report.clone(), m.outcome(1), [m.published(0), m.published(1)])
}

/// What an ordered history returns: the report, T's outcome, its reply
/// array (committed only) and the published writer of each key.
pub type Ordered<const N: usize> = (Report, Outcome, Option<Vec<Reply>>, [Option<usize>; N]);

fn ordered<const N: usize>(m: &Model, t: usize) -> Ordered<N> {
    let published: [Option<usize>; N] = std::array::from_fn(|k| m.published(k as Key));
    (m.report.clone(), m.outcome(t), m.reply(t), published)
}

/// The review's complete F04 history: `EXEC { SET a; RENAME a b;
/// GET b }` with `a@0`, `b@1` — the `GET` is queued after the
/// dependent command and must answer the value its apply staged on
/// the other owner, in queue order.
pub fn ordered_rename_history(rules: Rules, native: bool) -> Ordered<2> {
    let mut m = Model::new(rules, 2, 19);
    let t = m.submit(TxnSpec {
        coordinator: 0,
        program: vec![Cmd::Set(0), Cmd::Dep(Dependent::Move { source: 0, target: 1 }), Cmd::Get(1)],
        native,
        ..Default::default()
    });
    m.run(10_000);
    ordered(&m, t)
}

/// Two dependent commands in one queue over three owners: `MSETNX a b;
/// RENAME b c; GET c; GET b` with `a@0`, `b@1`, `c@2` — the second
/// command's gather must see the first's apply in `b`'s private set.
pub fn consecutive_dependents_history(rules: Rules, native: bool) -> Ordered<3> {
    let mut m = Model::new(rules, 3, 23);
    let t = m.submit(TxnSpec {
        coordinator: 0,
        program: vec![
            Cmd::Dep(Dependent::SetIfNoneExist { keys: vec![0, 1] }),
            Cmd::Dep(Dependent::Move { source: 1, target: 2 }),
            Cmd::Get(2),
            Cmd::Get(1),
        ],
        native,
        ..Default::default()
    });
    m.run(10_000);
    ordered(&m, t)
}

/// A refused command followed by reads: `SET a; RENAME a b; GET a;
/// GET b` where `b`'s owner cannot reserve — `EXEC` embeds the refusal
/// and the reads see the `SET` kept and `b` untouched; `INF.TX`
/// aborts with nothing published.
pub fn refused_then_read_history(rules: Rules, native: bool) -> Ordered<2> {
    let mut m = Model::new(rules, 2, 29);
    let t = m.submit(TxnSpec {
        coordinator: 0,
        program: vec![
            Cmd::Set(0),
            Cmd::Dep(Dependent::Move { source: 0, target: 1 }),
            Cmd::Get(0),
            Cmd::Get(1),
        ],
        native,
        refuses_at: Some(1),
        ..Default::default()
    });
    m.run(10_000);
    ordered(&m, t)
}

/// Reply order across owners with a failed condition in the middle:
/// `SET a; SET b; GET b; GET a` with `a@0`, `b@1`, `b`'s `SET` failing
/// its condition (`fails`) — the replies come back in queue order
/// whatever the legs' arrival order, and each `GET` answers its own
/// leg's private set.
pub fn reply_order_history(rules: Rules, fails: bool) -> Ordered<2> {
    let mut m = Model::new(rules, 2, 31);
    let t = m.submit(TxnSpec {
        coordinator: 1,
        program: vec![Cmd::Set(0), Cmd::Set(1), Cmd::Get(1), Cmd::Get(0)],
        fails_at: fails.then_some(1),
        ..Default::default()
    });
    m.run(10_000);
    ordered(&m, t)
}

/// `SET a; RENAME a b; GET b` with a disconnect or timeout landing the
/// moment T enters `phase` — `Combine(0)` and the second stage
/// included. Returns the report, T's outcome and whether the serial
/// outcome (`a` absent, `b` = T) holds.
pub fn dependent_cancellation_history(
    rules: Rules,
    phase: CancelPhase,
    kind: Cancel,
    seed: u64,
) -> (Report, Outcome, bool) {
    let mut m = Model::new(rules, 2, seed);
    let t = m.submit(TxnSpec {
        coordinator: 0,
        program: vec![Cmd::Set(0), Cmd::Dep(Dependent::Move { source: 0, target: 1 }), Cmd::Get(1)],
        cancel: Some((phase, kind)),
        ..Default::default()
    });
    m.run(10_000);
    let moved = m.published(0).is_none() && m.published(1) == Some(t);
    (m.report.clone(), m.outcome(t), moved)
}

/// Every phase a cancellation can land in when the transaction carries
/// a dependent command followed by a second stage.
pub const DEP_PHASES: [CancelPhase; 9] = [
    CancelPhase::Admit,
    CancelPhase::Acquire(0),
    CancelPhase::Acquire(1),
    CancelPhase::Execute(0),
    CancelPhase::Executed(0, 1),
    CancelPhase::Combine(0),
    CancelPhase::Execute(1),
    CancelPhase::Decided,
    CancelPhase::Durable,
];

/// Every phase a cancellation can land in, for the two-owner history.
pub const PHASES: [CancelPhase; 7] = [
    CancelPhase::Admit,
    CancelPhase::Acquire(0),
    CancelPhase::Acquire(1),
    CancelPhase::Execute(0),
    CancelPhase::Executed(0, 1),
    CancelPhase::Decided,
    CancelPhase::Durable,
];
