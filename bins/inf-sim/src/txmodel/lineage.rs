use super::Rng;
use std::collections::{BTreeMap, BTreeSet};

pub type Txid = u64;
/// One key per cell: key `c` lives on cell `c`.
pub type Cell = usize;

/// The D3 rules the 2026-09-10 review reopened (ADR-0116 A1/A2) and
/// the per-record rule its fix validation reopened again (A8).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Rules {
    /// A record logged by a command that touched a published-undecided
    /// key carries the key's pending txids as dependencies; recovery
    /// applies it only when every dependency committed (A1). `false`
    /// is D3 as written: successors are plain, and a plain write
    /// releases the pin.
    pub successors_inherit_dependencies: bool,
    /// The pinned image is the decision-qualified one — the image
    /// before the key's first pending write, held until the key's
    /// whole pending set is durably decided — and `ckpt-begin` stays
    /// at or before that run's first record (A2). `false` is D3 as
    /// written: the immediate predecessor of the latest pending
    /// record, `ckpt-begin` at the oldest undecided prepare.
    pub pin_decision_qualified_image: bool,
    /// An `always` ack waits for the record's own durability and for
    /// every dependency's durable decision (A1). `false` acks on the
    /// record's own durability alone.
    pub ack_waits_for_dependencies: bool,
    /// A transaction's dependencies are one set — the union of the
    /// pending sets of every key it acquires, read or write — gathered
    /// at the grants, carried by every prepare, and given whole to
    /// every key it publishes (A8). `false` is A1 as written: each
    /// record carries its own key's pending set, a read leg carries
    /// nothing, and recovery qualifies records one by one.
    pub dependencies_are_transaction_wide: bool,
    /// The decision waits for every read participant's durable
    /// watermark to cover the newest record the leg observed, as D3
    /// already makes it wait for every write participant's prepare
    /// (A8). `false` is D3 as written: only prepares gate the decision,
    /// so it can outlive a plain write its read leg saw on another log.
    pub decision_waits_for_read_watermarks: bool,
}

impl Rules {
    pub fn chosen() -> Rules {
        Rules {
            successors_inherit_dependencies: true,
            pin_decision_qualified_image: true,
            ack_waits_for_dependencies: true,
            dependencies_are_transaction_wide: true,
            decision_waits_for_read_watermarks: true,
        }
    }

    pub fn withdrawn() -> Rules {
        Rules {
            successors_inherit_dependencies: false,
            pin_decision_qualified_image: false,
            ack_waits_for_dependencies: false,
            dependencies_are_transaction_wide: false,
            decision_waits_for_read_watermarks: false,
        }
    }

    /// A1–A3 as written before A8 — the rules the fix validation
    /// review ran its partial-overlap history against.
    pub fn per_record() -> Rules {
        Rules {
            dependencies_are_transaction_wide: false,
            decision_waits_for_read_watermarks: false,
            ..Rules::chosen()
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

/// A cross-cell transaction: its coordinator's log carries its
/// decision; its legs are the cells it reads or writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Txn {
    pub id: Txid,
    pub coordinator: Cell,
    pub legs: Vec<(Cell, Access)>,
}

impl Txn {
    pub fn new(id: Txid, coordinator: Cell, legs: &[(Cell, Access)]) -> Txn {
        Txn { id, coordinator, legs: legs.to_vec() }
    }

    fn writes(&self) -> impl Iterator<Item = Cell> + '_ {
        self.legs.iter().filter(|(_, a)| *a == Access::Write).map(|(c, _)| *c)
    }

    fn touches(&self, c: Cell) -> bool {
        self.legs.iter().any(|(l, _)| *l == c)
    }

    fn access(&self, c: Cell) -> Option<Access> {
        self.legs.iter().find(|(l, _)| *l == c).map(|(_, a)| *a)
    }
}

/// The cells and transactions of a history. Transactions arrive in
/// list order: a later one queues behind every earlier one it shares a
/// cell with (FIFO by arrival, D2.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shape {
    pub cells: usize,
    pub txns: Vec<Txn>,
}

use Access::{Read as R, Write as W};

impl Shape {
    /// The batch-24 shape: two cells, T1 then T2 write both keys.
    pub fn full_overlap() -> Shape {
        Shape {
            cells: 2,
            txns: vec![Txn::new(1, 0, &[(0, W), (1, W)]), Txn::new(2, 1, &[(0, W), (1, W)])],
        }
    }

    /// The fix validation's F01 shape: T1 writes `a, b`; T2 writes
    /// `b, c` — T2's `c` leg has no dependency of its own.
    pub fn partial_overlap() -> Shape {
        Shape {
            cells: 3,
            txns: vec![Txn::new(1, 0, &[(0, W), (1, W)]), Txn::new(2, 2, &[(1, W), (2, W)])],
        }
    }

    /// T2 reads T1's `a` and writes `c`: its only record is on a key T1
    /// never touched.
    pub fn asymmetric_read() -> Shape {
        Shape {
            cells: 3,
            txns: vec![Txn::new(1, 0, &[(0, W), (1, W)]), Txn::new(2, 2, &[(0, R), (2, W)])],
        }
    }

    /// T1 reads cell 0 and writes cell 1; the value it reads is a plain
    /// write that is not yet durable.
    pub fn read_watermark() -> Shape {
        Shape { cells: 2, txns: vec![Txn::new(1, 1, &[(0, R), (1, W)])] }
    }

    /// A chain of partial overlaps: T3 depends on T1 only through T2.
    pub fn transitive() -> Shape {
        Shape {
            cells: 4,
            txns: vec![
                Txn::new(1, 0, &[(0, W), (1, W)]),
                Txn::new(2, 2, &[(1, W), (2, W)]),
                Txn::new(3, 3, &[(2, W), (3, W)]),
            ],
        }
    }

    /// Three or four cells, two or three transactions with one or two
    /// write legs and at most one read leg each, spanning at least two
    /// cells; half of the later transactions overlap the previous one
    /// on exactly one cell (the F01 shape).
    pub fn random(seed: u64) -> Shape {
        let mut rng = Rng::new(seed ^ 0x0053_4841_5045);
        let cells = 3 + rng.below(2);
        let count = 2 + rng.below(2);
        let mut txns: Vec<Txn> = Vec::with_capacity(count);
        for id in 1..=count as Txid {
            let legs = loop {
                let mut legs: Vec<(Cell, Access)> = Vec::new();
                let overlap = txns.last().filter(|_| rng.below(2) == 0).map(|prev| {
                    let (c, _) = prev.legs[rng.below(prev.legs.len())];
                    c
                });
                if let Some(c) = overlap {
                    legs.push((c, W));
                }
                for _ in 0..1 + rng.below(2) {
                    let c = rng.below(cells);
                    if !legs.iter().any(|(l, _)| *l == c) {
                        legs.push((c, W));
                    }
                }
                if rng.below(2) == 0 {
                    let c = rng.below(cells);
                    if !legs.iter().any(|(l, _)| *l == c) {
                        legs.push((c, R));
                    }
                }
                if legs.len() >= 2 {
                    break legs;
                }
            };
            txns.push(Txn::new(id, rng.below(cells), &legs));
        }
        Shape { cells, txns }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Rec {
    Prepare { txid: Txid, value: u32, deps: Vec<Txid> },
    Plain { op: usize, value: u32, deps: Vec<Txid> },
    Decision { txid: Txid },
}

/// One key's pending run under the chosen rule: the qualified image,
/// every txid the run depends on, and the run's first record.
#[derive(Clone, Debug)]
struct Run {
    base: u32,
    pending: BTreeSet<Txid>,
    first_record: usize,
}

#[derive(Clone, Debug, Default)]
struct CellLog {
    records: Vec<Rec>,
    /// Records below this index survive the crash.
    durable: usize,
    ram: u32,
    run: Option<Run>,
    /// D3 as written: (writer txid, immediate predecessor).
    pinned: Option<(Txid, u32)>,
    ckpt_image: u32,
    ckpt_begin: usize,
    /// The executed operations the live value depends on — ground
    /// truth for the oracle, independent of the rules' bookkeeping.
    provenance: BTreeSet<usize>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Transaction `t`'s leg on cell `c` executes under its intents: a
    /// write leg logs the prepare, a read leg observes. The first leg
    /// acquires every intent of the transaction (its rounds).
    Prepare(Txid, Cell),
    /// The coordinator decides `t` in memory once every leg ran;
    /// `UnlockOp{Commit}` publishes on every written cell and releases
    /// the intents.
    Publish(Txid),
    /// A plain `INCR` of cell `c`'s key — the successor.
    Incr(Cell),
    /// An everysec timer on cell `c`.
    Fsync(Cell),
    /// A checkpoint on cell `c` (its cut is durable first).
    Checkpoint(Cell),
    Crash,
}

/// An operation in execution order, for the serial oracle.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Tx(Txid),
    Incr { cell: Cell, id: usize },
}

fn tx_value(t: Txid) -> u32 {
    100 * t as u32
}

fn base_value(c: Cell) -> u32 {
    10 + c as u32
}

fn durable_decisions(cells: &[CellLog]) -> BTreeSet<Txid> {
    cells
        .iter()
        .flat_map(|c| c.records.iter().take(c.durable))
        .filter_map(|r| match r {
            Rec::Decision { txid } => Some(*txid),
            _ => None,
        })
        .collect()
}

/// The dependencies a record logged on `cell` now inherits from its key.
fn inherited(rules: Rules, cell: &CellLog) -> Vec<Txid> {
    if !rules.successors_inherit_dependencies {
        return Vec::new();
    }
    match (&cell.run, cell.pinned) {
        (Some(run), _) => run.pending.iter().copied().collect(),
        (None, Some((writer, _))) => vec![writer],
        (None, None) => Vec::new(),
    }
}

fn conditional(rec: &Rec, decided: &BTreeSet<Txid>) -> bool {
    match rec {
        Rec::Prepare { txid, deps, .. } => {
            !decided.contains(txid) || deps.iter().any(|d| !decided.contains(d))
        }
        Rec::Plain { deps, .. } => deps.iter().any(|d| !decided.contains(d)),
        Rec::Decision { .. } => false,
    }
}

/// Runs `steps` over `shape` (every cell booted from a checkpoint
/// holding `10 + c`). Returns the recovered value per cell, or the
/// violation.
pub fn run(rules: Rules, shape: &Shape, steps: &[Step]) -> Result<Vec<u32>, String> {
    let mut m = Model::new(rules, shape);
    for step in steps {
        match *step {
            Step::Prepare(t, c) => m.prepare(t, c),
            Step::Publish(t) => m.publish(t),
            Step::Incr(c) => m.incr(c),
            Step::Fsync(c) => m.cells[c].durable = m.cells[c].records.len(),
            Step::Checkpoint(c) => m.checkpoint(c),
            Step::Crash => break,
        }
        m.settle();
    }
    m.verdict()
}

/// A transaction between its grants and its publication.
#[derive(Clone, Debug, Default)]
struct Active {
    /// A8: the union of the pending sets of every key it acquired.
    closure: BTreeSet<Txid>,
    /// The operations whose values it observed (the oracle's truth).
    observed: BTreeSet<usize>,
    /// Per read leg: the log length the leg observed — the watermark
    /// its decision waits for (A8).
    read_watermarks: BTreeMap<Cell, usize>,
    done: BTreeSet<Cell>,
}

struct Model<'a> {
    rules: Rules,
    shape: &'a Shape,
    cells: Vec<CellLog>,
    active: BTreeMap<Txid, Active>,
    published: BTreeSet<Txid>,
    /// Published: the read watermarks the decision still waits for.
    read_watermarks: BTreeMap<Txid, BTreeMap<Cell, usize>>,
    decision_logged: BTreeSet<Txid>,
    /// Executed operations in order, each with what it observed.
    ops: Vec<(Op, BTreeSet<usize>)>,
}

impl<'a> Model<'a> {
    fn new(rules: Rules, shape: &'a Shape) -> Model<'a> {
        let cells = (0..shape.cells)
            .map(|c| CellLog {
                ram: base_value(c),
                ckpt_image: base_value(c),
                ..CellLog::default()
            })
            .collect();
        Model {
            rules,
            shape,
            cells,
            active: BTreeMap::new(),
            published: BTreeSet::new(),
            read_watermarks: BTreeMap::new(),
            decision_logged: BTreeSet::new(),
            ops: Vec::new(),
        }
    }

    fn txn(&self, t: Txid) -> &'a Txn {
        self.shape.txns.iter().find(|x| x.id == t).expect("known txid")
    }

    /// An intent on cell `c` is held by an acquired, unpublished
    /// transaction other than `except`.
    fn held(&self, c: Cell, except: Option<Txid>) -> bool {
        self.active.keys().any(|t| Some(*t) != except && self.txn(*t).touches(c))
    }

    /// FIFO by arrival: every earlier transaction sharing a cell with
    /// `t` has published.
    fn arrived(&self, t: Txid) -> bool {
        let me = self.txn(t);
        self.shape
            .txns
            .iter()
            .take_while(|x| x.id != t)
            .filter(|x| x.legs.iter().any(|(c, _)| me.touches(*c)))
            .all(|x| self.published.contains(&x.id))
    }

    /// The transaction's rounds: every intent at once, the closure and
    /// the observed set gathered from the keys as granted.
    fn acquire(&mut self, t: Txid) -> bool {
        if self.active.contains_key(&t) {
            return true;
        }
        let txn = self.txn(t);
        if !self.arrived(t) || txn.legs.iter().any(|(c, _)| self.held(*c, Some(t))) {
            return false;
        }
        let mut a = Active::default();
        for (c, access) in &txn.legs {
            a.closure.extend(inherited(self.rules, &self.cells[*c]));
            a.observed.extend(self.cells[*c].provenance.iter().copied());
            if *access == Access::Read {
                a.read_watermarks.insert(*c, self.cells[*c].records.len());
            }
        }
        self.active.insert(t, a);
        true
    }

    fn prepare(&mut self, t: Txid, c: Cell) {
        let Some(access) = self.txn(t).access(c) else { return };
        if self.published.contains(&t) || !self.acquire(t) {
            return;
        }
        let active = self.active.get_mut(&t).expect("acquired");
        if !active.done.insert(c) {
            return;
        }
        if access == Access::Write {
            let deps = if self.rules.dependencies_are_transaction_wide {
                active.closure.iter().copied().collect()
            } else {
                inherited(self.rules, &self.cells[c])
            };
            self.cells[c].records.push(Rec::Prepare { txid: t, value: tx_value(t), deps });
        }
    }

    /// The in-memory decision: publish on every written cell, release
    /// the intents, start (or extend) each key's pending run.
    fn publish(&mut self, t: Txid) {
        let txn = self.txn(t);
        let ready = self.active.get(&t).is_some_and(|a| a.done.len() == txn.legs.len());
        if self.published.contains(&t) || !ready {
            return;
        }
        let active = self.active.remove(&t).expect("acquired");
        let op = self.ops.len();
        self.published.insert(t);
        self.read_watermarks.insert(t, active.read_watermarks.clone());
        self.ops.push((Op::Tx(t), active.observed.clone()));
        for c in txn.writes() {
            let rules = self.rules;
            let cell = &mut self.cells[c];
            let (pos, deps) = cell
                .records
                .iter()
                .enumerate()
                .find_map(|(i, r)| match r {
                    Rec::Prepare { txid, deps, .. } if *txid == t => Some((i, deps.clone())),
                    _ => None,
                })
                .expect("prepared leg");
            let old = std::mem::replace(&mut cell.ram, tx_value(t));
            let run = cell.run.get_or_insert(Run {
                base: old,
                pending: BTreeSet::new(),
                first_record: pos,
            });
            run.pending.insert(t);
            if rules.dependencies_are_transaction_wide {
                run.pending.extend(active.closure.iter().copied());
            } else {
                run.pending.extend(deps);
            }
            cell.pinned = Some((t, old));
            cell.provenance = active.observed.clone();
            cell.provenance.insert(op);
        }
    }

    fn incr(&mut self, c: Cell) {
        if self.held(c, None) {
            return;
        }
        let id = self.ops.iter().filter(|(o, _)| matches!(o, Op::Incr { .. })).count() + 1;
        let op = self.ops.len();
        let cell = &mut self.cells[c];
        self.ops.push((Op::Incr { cell: c, id }, cell.provenance.clone()));
        let deps = inherited(self.rules, cell);
        cell.ram += 1;
        cell.records.push(Rec::Plain { op: id, value: cell.ram, deps });
        cell.provenance.insert(op);
        if !self.rules.successors_inherit_dependencies {
            // D3 as written: a later plain write releases the pin.
            cell.pinned = None;
            cell.run = None;
        }
    }

    /// The cut is durable first; the image streams the pinned image
    /// of a pending key; `ckpt-begin` covers every conditional record
    /// and (A2) every pending run from its first record.
    fn checkpoint(&mut self, c: Cell) {
        let decided = durable_decisions(&self.cells);
        let qualified = self.rules.pin_decision_qualified_image;
        let cell = &mut self.cells[c];
        cell.durable = cell.records.len();
        let streamed = if qualified {
            cell.run.as_ref().map(|run| run.base)
        } else {
            cell.pinned.map(|(_, old)| old)
        };
        cell.ckpt_image = streamed.unwrap_or(cell.ram);
        let first_conditional = if qualified {
            cell.records.iter().position(|r| conditional(r, &decided))
        } else {
            cell.records
                .iter()
                .position(|r| matches!(r, Rec::Prepare { txid, .. } if !decided.contains(txid)))
        };
        let run_origin = if qualified { cell.run.as_ref().map(|r| r.first_record) } else { None };
        cell.ckpt_begin = [Some(cell.records.len()), first_conditional, run_origin]
            .into_iter()
            .flatten()
            .min()
            .expect("a candidate");
    }

    /// The decision is appended to the coordinator's log once every
    /// written participant's prepare is durable (D3, unchanged); a
    /// durable decision releases the pins that depend on nothing else.
    fn settle(&mut self) {
        let shape = self.shape;
        for txn in &shape.txns {
            let t = txn.id;
            if !self.published.contains(&t) || self.decision_logged.contains(&t) {
                continue;
            }
            let prepares_durable = txn.writes().all(|c| {
                self.cells[c]
                    .records
                    .iter()
                    .take(self.cells[c].durable)
                    .any(|r| matches!(r, Rec::Prepare { txid, .. } if *txid == t))
            });
            let reads_covered = !self.rules.decision_waits_for_read_watermarks
                || self.read_watermarks[&t].iter().all(|(c, len)| self.cells[*c].durable >= *len);
            if prepares_durable && reads_covered {
                self.cells[txn.coordinator].records.push(Rec::Decision { txid: t });
                self.decision_logged.insert(t);
            }
        }
        let decided = durable_decisions(&self.cells);
        for cell in &mut self.cells {
            if cell.run.as_ref().is_some_and(|run| run.pending.is_subset(&decided)) {
                cell.run = None;
            }
            if cell.pinned.is_some_and(|(writer, _)| decided.contains(&writer)) {
                cell.pinned = None;
            }
        }
    }

    /// Checkpoint image + durable tail; a tagged or dependent record
    /// applies iff its txid and every dependency committed.
    fn recover(&self, decided: &BTreeSet<Txid>) -> Vec<u32> {
        self.cells
            .iter()
            .map(|cell| {
                let tail = &cell.records[cell.ckpt_begin.min(cell.durable)..cell.durable];
                tail.iter().fold(cell.ckpt_image, |state, rec| match rec {
                    Rec::Prepare { value, .. } | Rec::Plain { value, .. }
                        if !conditional(rec, decided) =>
                    {
                        *value
                    }
                    _ => state,
                })
            })
            .collect()
    }

    /// The dependencies the coordinator of `t` knows: what its legs'
    /// prepares carry.
    fn tx_deps(&self, t: Txid) -> Vec<Txid> {
        self.cells
            .iter()
            .flat_map(|cell| cell.records.iter())
            .filter_map(|r| match r {
                Rec::Prepare { txid, deps, .. } if *txid == t => Some(deps.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// `always` acks at the crash: a durable decision or plain record
    /// and, under A1, every dependency durably decided.
    fn acked(&self, decided: &BTreeSet<Txid>) -> Vec<Op> {
        let mut acked = Vec::new();
        for (c, cell) in self.cells.iter().enumerate() {
            for rec in &cell.records[..cell.durable] {
                let (op, deps) = match rec {
                    Rec::Decision { txid } => (Op::Tx(*txid), self.tx_deps(*txid)),
                    Rec::Plain { op, deps, .. } => (Op::Incr { cell: c, id: *op }, deps.clone()),
                    Rec::Prepare { .. } => continue,
                };
                if !self.rules.ack_waits_for_dependencies
                    || deps.iter().all(|d| decided.contains(d))
                {
                    acked.push(op);
                }
            }
        }
        acked
    }

    /// The oracle: some subset of the executed operations, replayed in
    /// execution order with every transaction whole, must produce the
    /// recovered state; that subset must keep every operation an
    /// operation in it observed; and one such subset must contain
    /// every ack.
    fn verdict(&self) -> Result<Vec<u32>, String> {
        let decided = durable_decisions(&self.cells);
        let out = self.recover(&decided);
        let acked = self.acked(&decided);
        let ops: Vec<Op> = self.ops.iter().map(|(o, _)| *o).collect();
        let matching: Vec<Vec<usize>> = (0..1u32 << ops.len())
            .map(|mask| (0..ops.len()).filter(|i| mask & (1 << i) != 0).collect::<Vec<_>>())
            .filter(|subset| replay(self.shape, subset.iter().map(|i| ops[*i])) == out)
            .collect();
        if matching.is_empty() {
            return Err(format!(
                "SERIALIZABILITY VIOLATION after crash: recovered {out:?} is no serial subset of \
                     {ops:?} (decisions durable: {decided:?})"
            ));
        }
        let unobserved = |subset: &Vec<usize>| {
            subset.iter().find_map(|i| {
                self.ops[*i].1.iter().find(|d| !subset.contains(d)).map(|d| (ops[*i], ops[*d]))
            })
        };
        let closed: Vec<&Vec<usize>> =
            matching.iter().filter(|s| unobserved(s).is_none()).collect();
        if closed.is_empty() {
            let (kept, lost) = unobserved(&matching[0]).expect("not closed");
            return Err(format!(
                "DEPENDENCY VIOLATION after crash: recovered {out:?} matches only subsets of \
                     {ops:?} that keep {kept:?} without {lost:?} it observed (decisions durable: \
                     {decided:?})"
            ));
        }
        let contains = |s: &&Vec<usize>, a: &Op| s.iter().any(|i| ops[*i] == *a);
        if !closed.iter().any(|s| acked.iter().all(|a| contains(s, a))) {
            return Err(format!(
                "ACKED WRITE LOST after crash: {acked:?} acked under `always` but recovered \
                     {out:?} needs a subset without one of them (decisions durable: {decided:?})"
            ));
        }
        Ok(out)
    }
}

fn replay(shape: &Shape, ops: impl Iterator<Item = Op>) -> Vec<u32> {
    let mut state: Vec<u32> = (0..shape.cells).map(base_value).collect();
    for op in ops {
        match op {
            Op::Tx(t) => {
                let txn = shape.txns.iter().find(|x| x.id == t).expect("known txid");
                for c in txn.writes() {
                    state[c] = tx_value(t);
                }
            }
            Op::Incr { cell, .. } => state[cell] += 1,
        }
    }
    state
}

/// F01: T1 publishes; `INCR` on cell 1 reads the published 100; cell
/// 1's timer fsyncs; the crash beats T1's decision. (`full_overlap`)
pub const SUCCESSOR: [Step; 6] = [
    Step::Prepare(1, 0),
    Step::Prepare(1, 1),
    Step::Publish(1),
    Step::Incr(1),
    Step::Fsync(1),
    Step::Crash,
];

/// F02: T1 then T2 publish over both keys; cell 0 checkpoints; the
/// crash beats both decisions. (`full_overlap`)
pub const CHAIN: [Step; 8] = [
    Step::Prepare(1, 0),
    Step::Prepare(1, 1),
    Step::Publish(1),
    Step::Prepare(2, 0),
    Step::Prepare(2, 1),
    Step::Publish(2),
    Step::Checkpoint(0),
    Step::Crash,
];

/// T1 then T2 publish; both cells fsync (both decisions logged, T1's
/// on cell 0, T2's on cell 1); cell 1 fsyncs again — T2's decision is
/// durable, T1's is not; crash. (`full_overlap`)
pub const DEPENDENT_ACK: [Step; 10] = [
    Step::Prepare(1, 0),
    Step::Prepare(1, 1),
    Step::Publish(1),
    Step::Prepare(2, 0),
    Step::Prepare(2, 1),
    Step::Publish(2),
    Step::Fsync(0),
    Step::Fsync(1),
    Step::Fsync(1),
    Step::Crash,
];

/// The fix validation's F01 history (`partial_overlap`): T1 publishes
/// `a, b`; T2 publishes `b, c`; every prepare is durable; T2's
/// decision (cell 2) is durable and T1's (cell 0) is not; crash.
pub const PARTIAL_OVERLAP: [Step; 11] = [
    Step::Prepare(1, 0),
    Step::Prepare(1, 1),
    Step::Publish(1),
    Step::Prepare(2, 1),
    Step::Prepare(2, 2),
    Step::Publish(2),
    Step::Fsync(0),
    Step::Fsync(1),
    Step::Fsync(2),
    Step::Fsync(2),
    Step::Crash,
];

/// The same durability pattern over `asymmetric_read`: T2's only
/// record is on `c`, but it read T1's `a`.
pub const ASYMMETRIC_READ: [Step; 11] = [
    Step::Prepare(1, 0),
    Step::Prepare(1, 1),
    Step::Publish(1),
    Step::Prepare(2, 0),
    Step::Prepare(2, 2),
    Step::Publish(2),
    Step::Fsync(0),
    Step::Fsync(1),
    Step::Fsync(2),
    Step::Fsync(2),
    Step::Crash,
];

/// `read_watermark`: a plain `INCR` on cell 0, then T1 reads it and
/// writes cell 1; cell 1 fsyncs twice (prepare, then decision); cell 0
/// never does; crash.
pub const READ_WATERMARK: [Step; 7] = [
    Step::Incr(0),
    Step::Prepare(1, 0),
    Step::Prepare(1, 1),
    Step::Publish(1),
    Step::Fsync(1),
    Step::Fsync(1),
    Step::Crash,
];

/// The `transitive` chain: T1, T2, T3 publish in turn; every prepare
/// is durable; T2's and T3's decisions are durable, T1's is not.
pub const TRANSITIVE: [Step; 16] = [
    Step::Prepare(1, 0),
    Step::Prepare(1, 1),
    Step::Publish(1),
    Step::Prepare(2, 1),
    Step::Prepare(2, 2),
    Step::Publish(2),
    Step::Prepare(3, 2),
    Step::Prepare(3, 3),
    Step::Publish(3),
    Step::Fsync(0),
    Step::Fsync(1),
    Step::Fsync(2),
    Step::Fsync(3),
    Step::Fsync(2),
    Step::Fsync(3),
    Step::Crash,
];

/// The oracle enumerates subsets of the executed operations, so a
/// history carries at most this many successors.
pub const MAX_INCRS: usize = 6;

/// A seeded history that runs every transaction of the shape to its
/// publication — legs in a random order with successors, fsyncs and
/// checkpoints between — then fsyncs a random subset of cells, so the
/// decisions become durable asymmetrically, then a random tail.
pub fn random_program(shape: &Shape, seed: u64) -> Vec<Step> {
    let mut rng = Rng::new(seed);
    let mut out = Vec::new();
    let mut incrs = 0;
    let noise = |rng: &mut Rng, out: &mut Vec<Step>, incrs: &mut usize| match rng.below(6) {
        0 => out.push(Step::Fsync(rng.below(shape.cells))),
        1 => out.push(Step::Checkpoint(rng.below(shape.cells))),
        2 if *incrs < MAX_INCRS => {
            *incrs += 1;
            out.push(Step::Incr(rng.below(shape.cells)));
        }
        _ => {}
    };
    for txn in &shape.txns {
        let mut legs = txn.legs.clone();
        for i in (1..legs.len()).rev() {
            legs.swap(i, rng.below(i + 1));
        }
        for (c, _) in legs {
            noise(&mut rng, &mut out, &mut incrs);
            out.push(Step::Prepare(txn.id, c));
        }
        out.push(Step::Publish(txn.id));
        for c in 0..shape.cells {
            if rng.below(2) == 0 {
                out.push(Step::Fsync(c));
            }
        }
    }
    for _ in 0..rng.below(6) {
        noise(&mut rng, &mut out, &mut incrs);
    }
    out.push(Step::Crash);
    out
}

/// A seeded interleaving of the shape's legs, publications, successors,
/// fsyncs and checkpoints.
pub fn random_steps(shape: &Shape, seed: u64, len: usize) -> Vec<Step> {
    let mut rng = Rng::new(seed);
    let mut choices: Vec<Step> = Vec::new();
    for txn in &shape.txns {
        choices.extend(txn.legs.iter().map(|(c, _)| Step::Prepare(txn.id, *c)));
    }
    choices.extend(shape.txns.iter().map(|t| Step::Publish(t.id)));
    choices.extend((0..shape.cells).map(Step::Fsync));
    choices.extend((0..shape.cells).map(Step::Checkpoint));
    let fixed = choices.len();
    choices.extend((0..shape.cells).map(Step::Incr));
    let mut out = Vec::with_capacity(len + 1);
    let mut incrs = 0;
    for _ in 0..len {
        let n = if incrs < MAX_INCRS { choices.len() } else { fixed };
        let step = choices[rng.below(n)];
        if matches!(step, Step::Incr(_)) {
            incrs += 1;
        }
        out.push(step);
    }
    out.push(Step::Crash);
    out
}
