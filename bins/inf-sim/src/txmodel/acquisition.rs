use super::Rng;
use std::collections::{BTreeMap, VecDeque};

mod checks;
mod histories;

pub use histories::DEP_PHASES;
pub use histories::PHASES;
pub use histories::cancellation_history;
pub use histories::consecutive_dependents_history;
pub use histories::dependent_cancellation_history;
pub use histories::exec_failure_history;
pub use histories::msetnx_history;
use histories::name;
pub use histories::native_failure_history;
pub use histories::ordered_rename_history;
pub use histories::read_pair_history;
pub use histories::refused_then_read_history;
pub use histories::rename_history;
pub use histories::reply_order_history;
pub use histories::storm;
pub use histories::storm_with;
pub use histories::two_key_history;
pub use histories::watch_interval_history;

// ---- shared data definitions (behaviour lives in the child modules) ----------

// ---- shared data definitions (behaviour lives in the child modules) ----------

// ---- shared data definitions (behaviour lives in the child modules) ----------

pub type Key = u32;
pub type Cell = usize;

/// The cross-partition acquisition policy.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Acquisition {
    /// Master plan §6.3 before ADR-0112: `LockOp` fanned to every owner
    /// at once, a free key granted on arrival, waiters kept sorted by
    /// txid ("a short backward scan"). Sorting revokes nothing.
    WithdrawnTxidSorted,
    /// ADR-0116 D2: one `LockOp` round per distinct owner in canonical
    /// order (partition id ascending), each round awaited before the
    /// next; per-key queues FIFO by arrival at the owner.
    Canonical,
    /// Dragonfly's rule: parallel fan; an owner refuses to schedule a
    /// txid below a conflicting queue tail; the coordinator cancels,
    /// takes a larger txid and retries, bounded and counted.
    Reschedule { max_retries: u32 },
}

/// The last phase in which disconnect or timeout still aborts.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CancelUntil {
    /// ADR-0116 A4: the coordinator's in-memory decision is the
    /// irreversible point — `UnlockOp{Commit}` may already have
    /// published on an owner.
    Decision,
    /// M6-S08 as written: cancellation aborts until the decision is
    /// durable, so an abort can chase a commit that already published.
    DurableDecision,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Rules {
    pub acquisition: Acquisition,
    /// Every read — singleton or transactional — queues behind a held
    /// intent (ADR-0116 D2). `false` is the read-uncommitted variant:
    /// a pure read bypasses the queues.
    pub reads_wait: bool,
    /// A WATCH-only key's R intent is held from validation through the
    /// decision (ADR-0114 D1). `false` is validate-and-release.
    pub hold_watch_intents: bool,
    /// A participant's leg writes into a private set published only by
    /// `UnlockOp{Commit}` (ADR-0116 D5/A4). `false` is the batch-22
    /// model as written: the leg writes live values at execution.
    pub stage_privately: bool,
    pub cancel_until: CancelUntil,
    /// A command whose value or condition crosses owners (`RENAME`,
    /// `MSETNX`) runs as coordinator stages — gather, combine, apply —
    /// under the held intents (ADR-0116 A6). `false` is D2.5 as
    /// written: fixed arguments make the per-owner legs independent,
    /// so each owner runs its half of the command alone.
    pub stage_dependents: bool,
    /// A plain read inside the transaction sees the private write set
    /// first (ADR-0116 D5/A6: every leg runs its slice against its
    /// private set). `false` is the batch-22 model as written: every
    /// read returns the published value, so a `GET` queued after the
    /// transaction's own write — or after a dependent command's apply
    /// — answers the pre-transaction state.
    pub read_your_writes: bool,
}

impl Rules {
    pub fn chosen() -> Rules {
        Rules {
            acquisition: Acquisition::Canonical,
            reads_wait: true,
            hold_watch_intents: true,
            stage_privately: true,
            cancel_until: CancelUntil::Decision,
            stage_dependents: true,
            read_your_writes: true,
        }
    }

    pub fn withdrawn() -> Rules {
        Rules {
            acquisition: Acquisition::WithdrawnTxidSorted,
            reads_wait: false,
            hold_watch_intents: false,
            stage_privately: false,
            cancel_until: CancelUntil::DurableDecision,
            stage_dependents: false,
            read_your_writes: false,
        }
    }
}

/// Where a disconnect or timeout lands (ADR-0116 A4's phase table,
/// per stage since A6).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CancelPhase {
    /// Admitted, no `LockOp` sent yet.
    Admit,
    /// The `LockOp` for round `n` is in flight.
    Acquire(usize),
    /// Every round granted; stage `s`'s `ExecOp`s fanned.
    Execute(usize),
    /// The `n`-th `SubResult` of stage `s` arrived with legs still
    /// outstanding.
    Executed(usize, usize),
    /// Every leg of stage `s` is in and the coordinator combines its
    /// dependent command's gather results before the apply stage (A6).
    Combine(usize),
    /// Decided in memory; the cancellation lands at a seeded point
    /// while `UnlockOp{outcome}` is in flight — before, between or
    /// after the owners' publications, or after the durable decision.
    Decided,
    /// The decision record is durable.
    Durable,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Cancel {
    Disconnect,
    Timeout,
}

impl Cancel {
    fn reason(self, after_decision: bool) -> &'static str {
        match (self, after_decision) {
            (Cancel::Disconnect, false) => "disconnect",
            (Cancel::Timeout, false) => "timeout",
            (Cancel::Disconnect, true) => "disconnect after the decision",
            (Cancel::Timeout, true) => "timeout after the decision",
        }
    }
}

/// A queued command whose effect spans owners (ADR-0116 A6): its
/// value or condition is a function of more than one owner's state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dependent {
    /// `RENAME src dst` across owners: the destination takes the
    /// source's value as the transaction sees it; the source is
    /// deleted. Absent source ⇒ `no such key`.
    Move { src: Key, dst: Key },
    /// `MSETNX k…` across owners: every key is set iff none exists.
    SetIfNoneExist { keys: Vec<Key> },
}

impl Dependent {
    pub fn keys(&self) -> Vec<Key> {
        match self {
            Dependent::Move { src, dst } => vec![*src, *dst],
            Dependent::SetIfNoneExist { keys } => keys.clone(),
        }
    }

    fn label(&self) -> String {
        match self {
            Dependent::Move { src, dst } => format!("RENAME {src}→{dst}"),
            Dependent::SetIfNoneExist { keys } => format!("MSETNX {keys:?}"),
        }
    }
}

/// One queued command (ADR-0116 A6): the program is ordered and the
/// coordinator compiles it into stages — a stage is the maximal run
/// of plain commands up to and including one dependent command,
/// whose apply half is the stage(s) that follow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cmd {
    /// `SET k`: a plain write of this transaction's value.
    Set(Key),
    /// `GET k`: a read — the private set first (D5, read-your-writes).
    Get(Key),
    /// A dependent multi-owner command.
    Dep(Dependent),
}

impl Cmd {
    fn keys(&self) -> Vec<Key> {
        match self {
            Cmd::Set(k) | Cmd::Get(k) => vec![*k],
            Cmd::Dep(dep) => dep.keys(),
        }
    }

    fn writes(&self) -> bool {
        !matches!(self, Cmd::Get(_))
    }

    fn label(&self) -> String {
        match self {
            Cmd::Set(k) => format!("SET {k}"),
            Cmd::Get(k) => format!("GET {k}"),
            Cmd::Dep(dep) => dep.label(),
        }
    }
}

/// One element of the transaction's reply array, in queue order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Ok,
    /// `GET`: the writer seen (`None` = absent).
    Value(Option<usize>),
    /// `MSETNX`: 1 set, 0 condition failed.
    Int(u8),
    /// `EXEC` embeds the failed command's error.
    Err(&'static str),
}

impl Reply {
    fn text(&self) -> String {
        match self {
            Reply::Ok => "OK".to_string(),
            Reply::Value(v) => name(*v),
            Reply::Int(n) => n.to_string(),
            Reply::Err(e) => (*e).to_string(),
        }
    }

    /// The failure a reply carries, if any (`INF.TX` aborts on it).
    fn failure(&self) -> Option<&'static str> {
        match self {
            Reply::Err(e) => Some(e),
            Reply::Int(0) => Some("condition failed"),
            _ => None,
        }
    }

    /// Of two owners' halves of one command (the withdrawn independent
    /// split), the failure wins.
    fn worse(self, other: Reply) -> Reply {
        if self.failure().is_some() { self } else { other }
    }
}

/// One execute stage: the plain commands running in the owners' legs
/// and the dependent command whose gather rides their tail.
#[derive(Clone, Debug, Default)]
struct Stage {
    cmds: Vec<usize>,
    dep: Option<usize>,
}

/// The stage compiler (A6): stage assignment is monotone in queue
/// order; a dependent command closes its stage.
fn compile(program: &[Cmd]) -> Vec<Stage> {
    let mut stages = Vec::new();
    let mut cur = Stage::default();
    for (i, cmd) in program.iter().enumerate() {
        match cmd {
            Cmd::Dep(_) => {
                cur.dep = Some(i);
                stages.push(std::mem::take(&mut cur));
            }
            Cmd::Set(_) | Cmd::Get(_) => cur.cmds.push(i),
        }
    }
    if !cur.cmds.is_empty() {
        stages.push(cur);
    }
    stages
}

/// The stages a program compiles to, as `(index, closes with a
/// dependent command)` — the storm's cancel-phase menu.
pub fn stages_of(program: &[Cmd]) -> Vec<(usize, bool)> {
    compile(program).iter().enumerate().map(|(i, s)| (i, s.dep.is_some())).collect()
}

/// One transaction. `program` is the queue in order (A6); when it is
/// empty the legacy fields compile to one: the reads, then the plain
/// writes, then the dependent command. Writes take W intents, reads
/// and WATCH-only keys take R intents (a key both read/watched and
/// written takes W).
#[derive(Clone, Debug, Default)]
pub struct TxnSpec {
    pub coordinator: Cell,
    pub program: Vec<Cmd>,
    pub writes: Vec<Key>,
    pub reads: Vec<Key>,
    pub watches: Vec<Key>,
    /// A dependent multi-owner command queued after the plain writes.
    pub dependent: Option<Dependent>,
    /// The owner whose apply leg cannot reserve the command's
    /// publication resources (a destination refusal — OOM, over-frame).
    pub refuses_at: Option<Cell>,
    /// Native `INF.TX`: a leg's failed condition aborts the whole
    /// transaction. `false` is Redis `EXEC`: the failure is embedded
    /// in that leg's reply and the decision is still `Commit`.
    pub native: bool,
    /// The owner whose first plain write fails its condition (`NX`,
    /// `IF REV`, …) and stages nothing for that command.
    pub fails_at: Option<Cell>,
    /// A disconnect or timeout delivered to the coordinator the moment
    /// the transaction enters this phase.
    pub cancel: Option<(CancelPhase, Cancel)>,
}

/// A client's sequential program: the next transaction is issued only
/// after the previous one replied (two `GET`s by one client are two
/// singleton transactions in order).
#[derive(Clone, Debug, Default)]
pub struct Client {
    pub program: Vec<TxnSpec>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Intent {
    R,
    W,
}

#[derive(Copy, Clone, Debug)]
struct Entry {
    txn: usize,
    txid: u64,
    intent: Intent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Committed,
    Aborted(&'static str),
    Stuck,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    Acquiring,
    /// Reschedule: cancels in flight for the old txid.
    Cancelling,
    Executing,
    Terminal(Outcome),
}

#[derive(Clone, Debug)]
struct Txn {
    spec: TxnSpec,
    txid: u64,
    phase: Phase,
    /// Distinct owners in canonical order.
    owners: Vec<Cell>,
    /// Every key the transaction writes: plain writes ∪ the dependent
    /// commands' keys.
    writes_all: Vec<Key>,
    /// The queue in order, its compiled stages and the one in flight
    /// (A6).
    program: Vec<Cmd>,
    stages: Vec<Stage>,
    si: usize,
    /// The execute step of the stage in flight.
    stage: ExecStage,
    /// `ExecOp`s fanned for the step in flight.
    fan: usize,
    /// Gather results of the stage's dependent command: a conditioned
    /// key exists somewhere; the source's value; an owner refused.
    probe_any: bool,
    probe_value: Option<usize>,
    probe_refused: bool,
    /// The stage's dependent command failed (its reason), whole.
    dep_failed: Option<&'static str>,
    /// The first plain write on `fails_at` failed its condition (and
    /// which command it was).
    cond_failed: bool,
    failed_set: Option<usize>,
    /// The command whose failure aborted a native transaction — set
    /// only on that abort, so a cancellation racing a failed leg is
    /// not mistaken for the command's own abort.
    failed_idx: Option<usize>,
    /// Replies by program index; the array in queue order once decided.
    replies: BTreeMap<usize, Reply>,
    reply: Option<Vec<Reply>>,
    /// Published values of every key the program names when execution
    /// began — the serial oracle's pre-state (intents are held from
    /// here).
    pre: BTreeMap<Key, Option<usize>>,
    /// Canonical: the round in flight. Parallel fans and cancels:
    /// replies outstanding.
    cursor: usize,
    refused: bool,
    retries: u32,
    granted: Vec<bool>,
    /// Per owner: the leg reported a failed condition.
    failed: Vec<bool>,
    /// Per owner: the leg's staged writes were published.
    published: Vec<bool>,
    /// The coordinator decided `Commit` in memory.
    decided_commit: bool,
    /// The decision record is durable.
    durable: bool,
    /// Read-uncommitted variant: a pure read that takes no intent.
    bypass: bool,
    /// Registration snapshot per watched key (the owner's mutation
    /// count when the connection watched).
    reg: BTreeMap<Key, u64>,
    /// Every read: the key, the value seen, whether it came from the
    /// transaction's own private set.
    reads_seen: Vec<(Key, Option<usize>, bool)>,
    commit_seq: Option<u64>,
}

#[derive(Clone, Debug)]
enum Msg {
    Lock {
        txn: usize,
        owner: Cell,
    },
    Granted {
        txn: usize,
        owner: Cell,
    },
    WatchFail {
        txn: usize,
        owner: Cell,
    },
    Refused {
        txn: usize,
        owner: Cell,
    },
    Cancel {
        txn: usize,
        txid: u64,
        owner: Cell,
    },
    Exec {
        txn: usize,
        owner: Cell,
        stage: ExecStage,
    },
    SubResult(SubResult),
    /// `UnlockOp{outcome}`: publish or discard the staged leg, then
    /// release every intent.
    Unlock {
        txn: usize,
        owner: Cell,
        commit: bool,
    },
    /// The coordinator's decision record reached the disk.
    DecisionDurable {
        txn: usize,
    },
    /// A disconnect or timeout planted for `CancelPhase::Decided`.
    CancelAt {
        txn: usize,
        kind: Cancel,
    },
}

/// Which step of the stage in flight an `ExecOp` carries (ADR-0116
/// A6).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ExecStage {
    /// Every owner's plain leg of the stage. Chosen: the gather half
    /// of the stage's dependent command rides its tail. Withdrawn: the
    /// owner runs its whole half of the command here, alone.
    Legs,
    /// Move: the destination's reserve-then-stage put of the shipped
    /// value.
    ApplyDest,
    /// Move: the source's delete, after the destination staged.
    ApplySrc,
    /// Set-if-none-exist: stage the sets reserved in the gather.
    Apply,
}

/// A leg's result back to the coordinator.
#[derive(Clone, Debug)]
struct SubResult {
    txn: usize,
    owner: Cell,
    /// The leg's first plain write failed its condition.
    failed: bool,
    probe: Probe,
    dep_failed: Option<&'static str>,
    /// The leg's replies by program index, in queue order.
    replies: Vec<(usize, Reply)>,
    /// The withdrawn independent split: this owner's half of the
    /// dependent command replied.
    dep_reply: Option<Reply>,
}

/// What a leg reports back for a dependent command's gather half.
#[derive(Copy, Clone, Debug, Default)]
struct Probe {
    /// A key the command conditions on exists in the owner's view.
    present: bool,
    /// The source's value as the owner sees it (its private set first).
    value: Option<usize>,
    /// The owner could not reserve the command's publication resources.
    refused: bool,
}

/// The serial oracle's verdict: the program run alone against the
/// pre-state.
struct Serial {
    replies: Vec<Reply>,
    state: BTreeMap<Key, Option<usize>>,
    abort: Option<&'static str>,
}

/// Endpoint of the coordinator-side messages that carry no owner.
pub const COORD: Cell = usize::MAX;
pub const DISK: Cell = usize::MAX - 1;

impl Msg {
    fn endpoint(&self) -> (usize, Cell) {
        match *self {
            Msg::Lock { txn, owner }
            | Msg::Granted { txn, owner }
            | Msg::WatchFail { txn, owner }
            | Msg::Refused { txn, owner }
            | Msg::Cancel { txn, owner, .. }
            | Msg::Exec { txn, owner, .. }
            | Msg::Unlock { txn, owner, .. } => (txn, owner),
            Msg::SubResult(SubResult { txn, .. }) | Msg::CancelAt { txn, .. } => (txn, COORD),
            Msg::DecisionDurable { txn } => (txn, DISK),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Owner {
    queues: BTreeMap<Key, VecDeque<Entry>>,
    /// Published values: the last writer, if any.
    values: BTreeMap<Key, Option<usize>>,
    /// Private write sets awaiting `UnlockOp`, per transaction: the
    /// key and its new writer (`None` = deleted).
    staged: BTreeMap<usize, Vec<(Key, Option<usize>)>>,
}

/// What one run observed.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub committed: usize,
    pub aborted: usize,
    pub retries: u32,
    pub stuck: Vec<usize>,
    pub violations: Vec<String>,
    pub max_intents: usize,
    pub steps: usize,
}

/// The model: cells, transactions, messages in flight. Delivery order
/// is a seeded pick among pending messages, optionally preceded by a
/// script of `(txn, owner)` endpoints delivered first (the pinned
/// histories).
pub struct Model {
    rules: Rules,
    cells: usize,
    txns: Vec<Txn>,
    owners: Vec<Owner>,
    clients: Vec<(Client, usize)>,
    client_of: BTreeMap<usize, usize>,
    pending: Vec<Msg>,
    script: VecDeque<(usize, Cell)>,
    next_txid: u64,
    next_commit: u64,
    /// Ground-truth mutation count per key (published writes).
    mutations: BTreeMap<Key, u64>,
    /// Per key, the committed writers in decision order (`None` = a
    /// committed delete).
    committed_writes: BTreeMap<Key, Vec<(u64, Option<usize>)>>,
    rng: Rng,
    pub report: Report,
}

impl Model {
    pub fn new(rules: Rules, cells: usize, seed: u64) -> Model {
        assert!(cells > 0);
        Model {
            rules,
            cells,
            txns: Vec::new(),
            owners: vec![Owner::default(); cells],
            clients: Vec::new(),
            client_of: BTreeMap::new(),
            pending: Vec::new(),
            script: VecDeque::new(),
            next_txid: 1,
            next_commit: 1,
            mutations: BTreeMap::new(),
            committed_writes: BTreeMap::new(),
            rng: Rng::new(seed),
            report: Report::default(),
        }
    }

    pub fn owner_of(&self, key: Key) -> Cell {
        key as usize % self.cells
    }

    /// Deliver these endpoints' messages first, in this order.
    pub fn script(&mut self, order: &[(usize, Cell)]) {
        self.script.extend(order.iter().copied());
    }

    /// Register a transaction (WATCH registrations are taken now —
    /// the connection watched before `MULTI`). Returns its index.
    pub fn submit(&mut self, mut spec: TxnSpec) -> usize {
        if spec.program.is_empty() {
            for keys in [&mut spec.writes, &mut spec.reads] {
                keys.sort_unstable();
                keys.dedup();
            }
            spec.program = spec
                .reads
                .iter()
                .map(|k| Cmd::Get(*k))
                .chain(spec.writes.iter().map(|k| Cmd::Set(*k)))
                .chain(spec.dependent.iter().cloned().map(Cmd::Dep))
                .collect();
        }
        spec.watches.sort_unstable();
        spec.watches.dedup();
        let (mut reads, mut writes, mut writes_all) = (Vec::new(), Vec::new(), Vec::new());
        for cmd in &spec.program {
            match cmd {
                Cmd::Get(k) => reads.push(*k),
                Cmd::Set(k) => {
                    writes.push(*k);
                    writes_all.push(*k);
                }
                Cmd::Dep(dep) => {
                    let keys = dep.keys();
                    let mut dep_owners: Vec<Cell> =
                        keys.iter().map(|k| self.owner_of(*k)).collect();
                    dep_owners.sort_unstable();
                    dep_owners.dedup();
                    assert!(dep_owners.len() >= 2, "a dependent command spans owners");
                    writes_all.extend(keys);
                }
            }
        }
        for keys in [&mut reads, &mut writes, &mut writes_all] {
            keys.sort_unstable();
            keys.dedup();
        }
        spec.reads = reads;
        spec.writes = writes;
        let reg = spec.watches.iter().map(|k| (*k, *self.mutations.get(k).unwrap_or(&0))).collect();
        let mut owners: Vec<Cell> = writes_all
            .iter()
            .chain(&spec.reads)
            .chain(&spec.watches)
            .map(|k| self.owner_of(*k))
            .collect();
        owners.sort_unstable();
        owners.dedup();
        let bypass = !self.rules.reads_wait
            && writes_all.is_empty()
            && spec.watches.is_empty()
            && !spec.reads.is_empty();
        let idx = self.txns.len();
        let n = owners.len();
        let program = spec.program.clone();
        let stages = compile(&program);
        self.txns.push(Txn {
            spec,
            txid: 0,
            phase: Phase::Acquiring,
            owners,
            writes_all,
            program,
            stages,
            si: 0,
            stage: ExecStage::Legs,
            fan: 0,
            probe_any: false,
            probe_value: None,
            probe_refused: false,
            dep_failed: None,
            cond_failed: false,
            failed_set: None,
            failed_idx: None,
            replies: BTreeMap::new(),
            reply: None,
            pre: BTreeMap::new(),
            cursor: 0,
            refused: false,
            retries: 0,
            granted: vec![false; n],
            failed: vec![false; n],
            published: vec![false; n],
            decided_commit: false,
            durable: false,
            bypass,
            reg,
            reads_seen: Vec::new(),
            commit_seq: None,
        });
        self.start(idx);
        idx
    }

    /// Register a client program; its first transaction is issued now.
    pub fn client(&mut self, client: Client) {
        let id = self.clients.len();
        self.clients.push((client, 0));
        self.issue_next(id);
    }

    fn issue_next(&mut self, client: usize) {
        let (prog, next) = &mut self.clients[client];
        let Some(spec) = prog.program.get(*next).cloned() else { return };
        *next += 1;
        let t = self.submit(spec);
        self.client_of.insert(t, client);
    }

    fn start(&mut self, txn: usize) {
        self.txns[txn].txid = self.next_txid;
        self.next_txid += 1;
        let t = &mut self.txns[txn];
        t.cursor = 0;
        t.refused = false;
        for g in &mut t.granted {
            *g = false;
        }
        t.phase = Phase::Acquiring;
        let first_attempt = t.retries == 0;
        if first_attempt {
            self.enter(txn, CancelPhase::Admit);
            if matches!(self.txns[txn].phase, Phase::Terminal(_)) {
                return;
            }
        }
        if self.txns[txn].owners.is_empty() {
            self.decide(txn);
            return;
        }
        let bypass = self.txns[txn].bypass;
        let parallel = bypass || !matches!(self.rules.acquisition, Acquisition::Canonical);
        if bypass {
            self.txns[txn].phase = Phase::Executing;
        }
        if parallel {
            let owners = self.txns[txn].owners.clone();
            self.txns[txn].cursor = owners.len();
            self.txns[txn].fan = owners.len();
            for owner in owners {
                self.pending.push(Msg::Lock { txn, owner });
            }
            // A parallel fan has no later round: every `Acquire(n)`
            // is this one.
            for round in 0..self.txns[txn].owners.len() {
                self.enter(txn, CancelPhase::Acquire(round));
            }
        } else {
            let owner = self.txns[txn].owners[0];
            self.pending.push(Msg::Lock { txn, owner });
            self.enter(txn, CancelPhase::Acquire(0));
        }
    }

    /// The transaction enters `phase`: a cancellation planted there
    /// lands now.
    fn enter(&mut self, txn: usize, phase: CancelPhase) {
        if let Some((at, kind)) = self.txns[txn].spec.cancel
            && at == phase
        {
            self.cancel(txn, kind);
        }
    }

    /// Disconnect or timeout at the coordinator (ADR-0116 A4).
    fn cancel(&mut self, txn: usize, kind: Cancel) {
        match self.txns[txn].phase {
            Phase::Acquiring | Phase::Cancelling | Phase::Executing => {
                self.abort(txn, kind.reason(false));
            }
            Phase::Terminal(Outcome::Committed)
                if !self.txns[txn].durable
                    && self.rules.cancel_until == CancelUntil::DurableDecision =>
            {
                // M6-S08 as written: the coordinator future runs to
                // abort — the `UnlockOp{Commit}` still staged in an
                // unflushed outbound batch leaves as `Abort`; owners
                // whose batch already flushed have published.
                self.txns[txn].phase = Phase::Terminal(Outcome::Aborted(kind.reason(true)));
                self.report.committed -= 1;
                self.report.aborted += 1;
                for msg in &mut self.pending {
                    if let Msg::Unlock { txn: t, commit, .. } = msg
                        && *t == txn
                    {
                        *commit = false;
                    }
                }
            }
            // The outcome completes; the client may never learn it.
            Phase::Terminal(_) => {}
        }
    }

    /// Run to quiescence: no pending message. Transactions still
    /// non-terminal are stuck (a wait-for cycle).
    pub fn run(&mut self, max_steps: usize) -> &Report {
        while let Some(msg) = self.pick() {
            self.report.steps += 1;
            if self.report.steps > max_steps {
                break;
            }
            self.process(msg);
            let held = self.held_intents();
            self.report.max_intents = self.report.max_intents.max(held);
        }
        for (i, t) in self.txns.iter().enumerate() {
            if !matches!(t.phase, Phase::Terminal(_)) {
                self.report.stuck.push(i);
            }
        }
        if !self.report.stuck.is_empty() {
            let waits = self.wait_for();
            self.report.violations.push(format!(
                "ACQUISITION DEADLOCK: {} transaction(s) stuck with no message in flight — {waits}",
                self.report.stuck.len()
            ));
            return &self.report;
        }
        let residue = self.held_intents();
        if residue != 0 {
            self.report.violations.push(format!(
                "INTENT LEAK: {residue} intent(s) held after every transaction reached a terminal \
                     state"
            ));
        }
        let staged: usize = self.owners.iter().map(|o| o.staged.len()).sum();
        if staged != 0 {
            self.report.violations.push(format!(
                "STAGING LEAK: {staged} private write set(s) held after every transaction reached \
                     a terminal state"
            ));
        }
        self.check_publication();
        self.check_programs();
        &self.report
    }

    fn held_intents(&self) -> usize {
        self.owners.iter().map(|o| o.queues.values().map(VecDeque::len).sum::<usize>()).sum()
    }

    fn wait_for(&self) -> String {
        let mut out = Vec::new();
        for &i in &self.report.stuck {
            let t = &self.txns[i];
            for (oi, &owner) in t.owners.iter().enumerate() {
                if t.granted[oi] {
                    continue;
                }
                for (key, q) in &self.owners[owner].queues {
                    if let Some(pos) = q.iter().position(|e| e.txn == i) {
                        let ahead: Vec<String> =
                            q.iter().take(pos).map(|e| format!("T{}", e.txn + 1)).collect();
                        if !ahead.is_empty() {
                            out.push(format!(
                                "T{} waits on key {key}@cell{owner} behind {}",
                                i + 1,
                                ahead.join(",")
                            ));
                        }
                    }
                }
            }
        }
        out.join("; ")
    }

    /// Delivery is a seeded pick among the pending messages, subject
    /// to the fabric's per-pair order: two messages from one
    /// coordinator to one owner for one transaction ride one SPSC
    /// ring (M0-S05) and arrive in the order they were sent, so an
    /// `UnlockOp` never overtakes the `LockOp` or `ExecOp` it cancels.
    /// Everything else — other pairs, the coordinator's inbound
    /// results, the disk — reorders freely.
    fn pick(&mut self) -> Option<Msg> {
        if self.pending.is_empty() {
            return None;
        }
        if let Some(want) = self.script.front().copied()
            && let Some(pos) = self.pending.iter().position(|m| m.endpoint() == want)
        {
            self.script.pop_front();
            return Some(self.pending.remove(pos));
        }
        // The scripted endpoint (if any) is not in flight yet: deliver
        // something else and keep waiting for it.
        let eligible: Vec<usize> = (0..self.pending.len())
            .filter(|&i| {
                let ep = self.pending[i].endpoint();
                ep.1 >= DISK || !self.pending[..i].iter().any(|m| m.endpoint() == ep)
            })
            .collect();
        let pos = eligible[self.rng.below(eligible.len())];
        Some(self.pending.remove(pos))
    }

    fn intents_at(&self, txn: usize, owner: Cell) -> Vec<(Key, Intent)> {
        let spec = &self.txns[txn].spec;
        let mut out: BTreeMap<Key, Intent> = BTreeMap::new();
        for k in spec.reads.iter().chain(&spec.watches) {
            if self.owner_of(*k) == owner {
                out.insert(*k, Intent::R);
            }
        }
        for k in &self.txns[txn].writes_all {
            if self.owner_of(*k) == owner {
                out.insert(*k, Intent::W);
            }
        }
        out.into_iter().collect()
    }

    fn process(&mut self, msg: Msg) {
        match msg {
            Msg::Lock { txn, owner } => self.owner_lock(txn, owner),
            Msg::Granted { txn, .. } => self.coord_granted(txn),
            Msg::WatchFail { txn, .. } => self.abort(txn, "watch dirty at validation"),
            Msg::Refused { txn, .. } => self.coord_refused(txn),
            Msg::Cancel { txn, txid, owner } => {
                self.remove_entries(txn, Some(txid), owner);
                if self.txns[txn].phase == Phase::Cancelling {
                    self.txns[txn].cursor -= 1;
                    if self.txns[txn].cursor == 0 {
                        self.start(txn);
                    }
                }
            }
            Msg::Exec { txn, owner, stage } => self.owner_exec(txn, owner, stage),
            Msg::SubResult(res) => self.coord_subresult(res),
            Msg::Unlock { txn, owner, commit } => self.owner_unlock(txn, owner, commit),
            Msg::DecisionDurable { txn } => {
                self.txns[txn].durable = true;
                self.enter(txn, CancelPhase::Durable);
            }
            Msg::CancelAt { txn, kind } => self.cancel(txn, kind),
        }
    }

    fn owner_lock(&mut self, txn: usize, owner: Cell) {
        let intents = self.intents_at(txn, owner);
        let txid = self.txns[txn].txid;
        if self.txns[txn].bypass {
            // Read-uncommitted variant: the read is the whole visit.
            let program = self.txns[txn].program.clone();
            let mut replies = Vec::new();
            for (i, cmd) in program.iter().enumerate() {
                if let Cmd::Get(key) = cmd
                    && self.owner_of(*key) == owner
                {
                    let seen = self.observe_read(txn, owner, *key);
                    replies.push((i, Reply::Value(seen)));
                }
            }
            self.pending.push(Msg::SubResult(SubResult {
                txn,
                owner,
                failed: false,
                probe: Probe::default(),
                dep_failed: None,
                replies,
                dep_reply: None,
            }));
            return;
        }
        if let Acquisition::Reschedule { .. } = self.rules.acquisition {
            let conflict = intents.iter().any(|(key, _)| {
                self.owners[owner]
                    .queues
                    .get(key)
                    .and_then(VecDeque::back)
                    .is_some_and(|e| e.txid > txid)
            });
            if conflict {
                self.pending.push(Msg::Refused { txn, owner });
                return;
            }
        }
        for (key, intent) in intents {
            let q = self.owners[owner].queues.entry(key).or_default();
            let entry = Entry { txn, txid, intent };
            match self.rules.acquisition {
                Acquisition::WithdrawnTxidSorted if !q.is_empty() => {
                    // The holder keeps the grant; waiters sort by txid.
                    let mut pos = q.len();
                    while pos > 1 && q[pos - 1].txid > txid {
                        pos -= 1;
                    }
                    q.insert(pos, entry);
                }
                _ => q.push_back(entry),
            }
        }
        self.try_grant(owner, txn);
    }

    /// W is eligible at the head; R behind a contiguous R prefix.
    fn eligible(&self, owner: Cell, txn: usize) -> bool {
        self.owners[owner].queues.values().all(|q| match q.iter().position(|e| e.txn == txn) {
            None | Some(0) => true,
            Some(pos) => {
                q[pos].intent == Intent::R && q.iter().take(pos).all(|e| e.intent == Intent::R)
            }
        })
    }

    fn try_grant(&mut self, owner: Cell, txn: usize) {
        let Some(oi) = self.txns[txn].owners.iter().position(|o| *o == owner) else {
            return;
        };
        if self.txns[txn].granted[oi]
            || self.txns[txn].phase != Phase::Acquiring
            || !self.eligible(owner, txn)
        {
            return;
        }
        self.txns[txn].granted[oi] = true;
        // Validation is the owner's, inside the grant activation.
        let watched: Vec<Key> = self.txns[txn]
            .spec
            .watches
            .iter()
            .copied()
            .filter(|k| self.owner_of(*k) == owner)
            .collect();
        for key in &watched {
            if self.mutations.get(key).copied().unwrap_or(0) != self.txns[txn].reg[key] {
                self.pending.push(Msg::WatchFail { txn, owner });
                return;
            }
        }
        if !self.rules.hold_watch_intents {
            // Validate-and-release: WATCH-only R intents leave now.
            let spec = self.txns[txn].spec.clone();
            for key in watched {
                if spec.writes.contains(&key) || spec.reads.contains(&key) {
                    continue;
                }
                if let Some(q) = self.owners[owner].queues.get_mut(&key) {
                    q.retain(|e| e.txn != txn);
                    if q.is_empty() {
                        self.owners[owner].queues.remove(&key);
                    }
                }
                self.wake(owner, key);
            }
        }
        self.pending.push(Msg::Granted { txn, owner });
    }

    fn coord_granted(&mut self, txn: usize) {
        if self.txns[txn].phase != Phase::Acquiring {
            return;
        }
        match self.rules.acquisition {
            Acquisition::Canonical => {
                self.txns[txn].cursor += 1;
                let cursor = self.txns[txn].cursor;
                if cursor < self.txns[txn].owners.len() {
                    let owner = self.txns[txn].owners[cursor];
                    self.pending.push(Msg::Lock { txn, owner });
                    self.enter(txn, CancelPhase::Acquire(cursor));
                } else {
                    self.exec_fan(txn);
                }
            }
            _ => self.parallel_reply(txn),
        }
    }

    fn coord_refused(&mut self, txn: usize) {
        if self.txns[txn].phase != Phase::Acquiring {
            return;
        }
        self.txns[txn].refused = true;
        self.parallel_reply(txn);
    }

    fn parallel_reply(&mut self, txn: usize) {
        self.txns[txn].cursor -= 1;
        if self.txns[txn].cursor != 0 {
            return;
        }
        if !self.txns[txn].refused {
            self.exec_fan(txn);
            return;
        }
        let Acquisition::Reschedule { max_retries } = self.rules.acquisition else {
            unreachable!("only the reschedule policy refuses")
        };
        self.txns[txn].retries += 1;
        self.report.retries += 1;
        if self.txns[txn].retries > max_retries {
            self.abort(txn, "reschedule budget");
            return;
        }
        let old = self.txns[txn].txid;
        let owners = self.txns[txn].owners.clone();
        self.txns[txn].phase = Phase::Cancelling;
        self.txns[txn].cursor = owners.len();
        for owner in owners {
            self.pending.push(Msg::Cancel { txn, txid: old, owner });
        }
    }

    /// Every intent granted: snapshot the pre-state and fan the first
    /// stage (a program with no stage decides at once).
    fn exec_fan(&mut self, txn: usize) {
        self.txns[txn].phase = Phase::Executing;
        let mut keys: Vec<Key> = self.txns[txn].program.iter().flat_map(Cmd::keys).collect();
        keys.sort_unstable();
        keys.dedup();
        self.txns[txn].pre = keys.iter().map(|k| (*k, self.published(*k))).collect();
        self.txns[txn].si = 0;
        if self.txns[txn].stages.is_empty() {
            self.decide(txn);
            return;
        }
        self.fan_stage(txn);
    }

    /// Fan the stage in flight to every owner with a plain command in
    /// it and to its dependent command's owners (the gather).
    fn fan_stage(&mut self, txn: usize) {
        let si = self.txns[txn].si;
        let owners = self.stage_owners(txn, si);
        let t = &mut self.txns[txn];
        t.stage = ExecStage::Legs;
        t.fan = owners.len();
        t.cursor = owners.len();
        t.probe_any = false;
        t.probe_value = None;
        t.probe_refused = false;
        for owner in owners {
            self.pending.push(Msg::Exec { txn, owner, stage: ExecStage::Legs });
        }
        self.enter(txn, CancelPhase::Execute(si));
    }

    fn stage_owners(&self, txn: usize, si: usize) -> Vec<Cell> {
        let t = &self.txns[txn];
        let stage = &t.stages[si];
        let mut owners: Vec<Cell> = stage
            .cmds
            .iter()
            .chain(stage.dep.iter())
            .flat_map(|i| t.program[*i].keys())
            .map(|k| self.owner_of(k))
            .collect();
        owners.sort_unstable();
        owners.dedup();
        owners
    }

    /// The dependent command closing stage `si`, if any.
    fn stage_dep(&self, txn: usize, si: usize) -> Option<(usize, Dependent)> {
        let t = &self.txns[txn];
        t.stages[si].dep.map(|i| match &t.program[i] {
            Cmd::Dep(dep) => (i, dep.clone()),
            cmd => unreachable!("a stage closes with a dependent command, not {cmd:?}"),
        })
    }

    /// The stage after the one in flight, or the decision.
    fn next_stage(&mut self, txn: usize) {
        let t = &mut self.txns[txn];
        t.si += 1;
        t.dep_failed = None;
        if t.si < t.stages.len() {
            self.fan_stage(txn);
        } else {
            self.decide(txn);
        }
    }

    /// A read at the owner: the private set first under
    /// read-your-writes, else the published value. Records what was
    /// seen for the decision's stale-read check.
    fn observe_read(&mut self, txn: usize, owner: Cell, key: Key) -> Option<usize> {
        let staged = self.owners[owner]
            .staged
            .get(&txn)
            .and_then(|s| s.iter().rev().find(|(k, _)| *k == key).map(|(_, v)| *v));
        let (seen, own) = match staged {
            Some(v) if self.rules.read_your_writes => (v, true),
            _ => (self.owners[owner].values.get(&key).copied().flatten(), false),
        };
        if !own
            && let Some(w) = seen
            && w != txn
            && self.txns[w].commit_seq.is_none()
        {
            self.report.violations.push(format!(
                "READ UNCOMMITTED: T{} read key {key} written by T{} before T{}'s decision",
                txn + 1,
                w + 1,
                w + 1
            ));
        }
        self.txns[txn].reads_seen.push((key, seen, own));
        seen
    }

    fn owner_exec(&mut self, txn: usize, owner: Cell, stage: ExecStage) {
        if stage == ExecStage::Legs {
            self.owner_legs(txn, owner);
            return;
        }
        let si = self.txns[txn].si;
        let (_, dep) = self.stage_dep(txn, si).expect("an apply step has its command");
        let mut probe = Probe::default();
        match (stage, dep) {
            (ExecStage::ApplyDest, Dependent::Move { dst, .. }) => {
                if self.txns[txn].spec.refuses_at == Some(owner) {
                    probe.refused = true;
                } else {
                    let value = self.txns[txn].probe_value;
                    self.stage_write(txn, owner, dst, value);
                }
            }
            (ExecStage::ApplySrc, Dependent::Move { src, .. }) => {
                self.stage_write(txn, owner, src, None);
            }
            (ExecStage::Apply, Dependent::SetIfNoneExist { keys }) => {
                for key in keys {
                    if self.owner_of(key) == owner {
                        self.stage_write(txn, owner, key, Some(txn));
                    }
                }
            }
            (stage, dep) => unreachable!("step {stage:?} without its command: {dep:?}"),
        }
        self.pending.push(Msg::SubResult(SubResult {
            txn,
            owner,
            failed: false,
            probe,
            dep_failed: None,
            replies: Vec::new(),
            dep_reply: None,
        }));
    }

    /// The plain leg of the stage in flight: this owner's slice of the
    /// program in queue order — reads against the private set, writes
    /// staged privately (chosen) or written live (withdrawn), the
    /// first write on `fails_at` failing its condition — then the
    /// stage's dependent command: its gather half (chosen) or its
    /// whole owner-local half (withdrawn).
    fn owner_legs(&mut self, txn: usize, owner: Cell) {
        let si = self.txns[txn].si;
        let stage = self.txns[txn].stages[si].clone();
        let program = self.txns[txn].program.clone();
        let fails_here = self.txns[txn].spec.fails_at == Some(owner);
        if self.rules.stage_privately {
            self.owners[owner].staged.entry(txn).or_default();
        }
        let mut replies = Vec::new();
        let mut failed = false;
        for i in stage.cmds {
            match &program[i] {
                Cmd::Get(key) if self.owner_of(*key) == owner => {
                    let seen = self.observe_read(txn, owner, *key);
                    replies.push((i, Reply::Value(seen)));
                }
                Cmd::Set(key) if self.owner_of(*key) == owner => {
                    if fails_here && !self.txns[txn].cond_failed {
                        self.txns[txn].cond_failed = true;
                        failed = true;
                        replies.push((i, Reply::Err("condition failed")));
                    } else {
                        self.stage_write(txn, owner, *key, Some(txn));
                        replies.push((i, Reply::Ok));
                    }
                }
                _ => {}
            }
        }
        if !self.rules.stage_privately && !failed {
            let oi = self.txns[txn].owners.iter().position(|o| *o == owner).expect("owner");
            self.txns[txn].published[oi] = true;
        }
        let mut probe = Probe::default();
        let mut dep_failed = None;
        let mut dep_reply = None;
        if let Some((_, dep)) = self.stage_dep(txn, si) {
            if self.rules.stage_dependents {
                probe = self.gather(txn, owner, &dep);
            } else {
                (dep_failed, dep_reply) = self.independent_half(txn, owner, &dep);
            }
        }
        self.pending.push(Msg::SubResult(SubResult {
            txn,
            owner,
            failed,
            probe,
            dep_failed,
            replies,
            dep_reply,
        }));
    }

    /// A key as the owner's leg sees it: the transaction's private
    /// set first (read-your-writes), then the published value.
    fn view(&self, txn: usize, owner: Cell, key: Key) -> Option<usize> {
        let staged = self.owners[owner]
            .staged
            .get(&txn)
            .and_then(|s| s.iter().rev().find(|(k, _)| *k == key).map(|(_, v)| *v));
        match staged {
            Some(v) => v,
            None => self.owners[owner].values.get(&key).copied().flatten(),
        }
    }

    /// The gather half (A6): report the condition and the value the
    /// coordinator combines; reserve the condition family's resources.
    fn gather(&self, txn: usize, owner: Cell, dep: &Dependent) -> Probe {
        let mut probe = Probe::default();
        match dep {
            Dependent::Move { src, .. } if self.owner_of(*src) == owner => {
                probe.value = self.view(txn, owner, *src);
                probe.present = probe.value.is_some();
            }
            Dependent::Move { .. } => {}
            Dependent::SetIfNoneExist { keys } => {
                let mine = keys.iter().copied().filter(|k| self.owner_of(*k) == owner);
                probe.present = mine.clone().any(|k| self.view(txn, owner, k).is_some());
                probe.refused = mine.count() > 0 && self.txns[txn].spec.refuses_at == Some(owner);
            }
        }
        probe
    }

    /// D2.5 as written: the owner runs its half of the command with the
    /// arguments it has — the other owner's private set is invisible,
    /// so a source's staged value is read as published (pre-transaction)
    /// and the source's delete does not wait for the destination.
    /// Returns the half's failure and its reply.
    fn independent_half(
        &mut self,
        txn: usize,
        owner: Cell,
        dep: &Dependent,
    ) -> (Option<&'static str>, Option<Reply>) {
        match dep {
            Dependent::Move { src, dst } => {
                if self.owner_of(*src) == owner {
                    self.stage_write(txn, owner, *src, None);
                }
                if self.owner_of(*dst) != owner {
                    return (None, None);
                }
                if self.txns[txn].spec.refuses_at == Some(owner) {
                    return (Some("destination refused"), Some(Reply::Err("destination refused")));
                }
                let Some(value) =
                    self.owners[self.owner_of(*src)].values.get(src).copied().flatten()
                else {
                    return (Some("no such key"), Some(Reply::Err("no such key")));
                };
                self.stage_write(txn, owner, *dst, Some(value));
                (None, Some(Reply::Ok))
            }
            Dependent::SetIfNoneExist { keys } => {
                let mine: Vec<Key> =
                    keys.iter().copied().filter(|k| self.owner_of(*k) == owner).collect();
                if mine.is_empty() {
                    return (None, None);
                }
                if self.txns[txn].spec.refuses_at == Some(owner) {
                    return (Some("refused"), Some(Reply::Err("refused")));
                }
                if mine.iter().any(|k| self.view(txn, owner, *k).is_some()) {
                    return (Some("condition failed"), Some(Reply::Int(0)));
                }
                for key in mine {
                    self.stage_write(txn, owner, key, Some(txn));
                }
                (None, Some(Reply::Int(1)))
            }
        }
    }

    /// Into the private set (chosen) or straight to the published
    /// value (the withdrawn live-write rule).
    fn stage_write(&mut self, txn: usize, owner: Cell, key: Key, value: Option<usize>) {
        if self.rules.stage_privately {
            self.owners[owner].staged.entry(txn).or_default().push((key, value));
        } else {
            self.publish(txn, owner, &[(key, value)]);
        }
    }

    fn publish(&mut self, txn: usize, owner: Cell, entries: &[(Key, Option<usize>)]) {
        for (key, value) in entries {
            self.owners[owner].values.insert(*key, *value);
            *self.mutations.entry(*key).or_insert(0) += 1;
        }
        let oi = self.txns[txn].owners.iter().position(|o| *o == owner).expect("a participant");
        self.txns[txn].published[oi] = true;
    }

    fn coord_subresult(&mut self, res: SubResult) {
        let SubResult { txn, owner, failed, probe, dep_failed, replies, dep_reply } = res;
        let si = self.txns[txn].si;
        let dep_idx = self.txns[txn].stages.get(si).and_then(|s| s.dep);
        let t = &mut self.txns[txn];
        if failed {
            let oi = t.owners.iter().position(|o| *o == owner).expect("owner");
            t.failed[oi] = true;
            if t.failed_set.is_none() {
                t.failed_set = replies.iter().find(|(_, r)| r.failure().is_some()).map(|(i, _)| *i);
            }
        }
        for (i, r) in replies {
            t.replies.insert(i, r);
        }
        if let Some(r) = dep_reply {
            let idx = dep_idx.expect("a half of the stage's dependent command");
            let merged = match t.replies.remove(&idx) {
                Some(prev) => prev.worse(r),
                None => r,
            };
            t.replies.insert(idx, merged);
        }
        if probe.present {
            t.probe_any = true;
            t.probe_value = probe.value;
        }
        t.probe_refused |= probe.refused;
        if dep_failed.is_some() {
            t.dep_failed = dep_failed;
        }
        t.cursor -= 1;
        let stage = t.stage;
        if t.cursor != 0 {
            if stage == ExecStage::Legs {
                let arrived = self.txns[txn].fan - self.txns[txn].cursor;
                self.enter(txn, CancelPhase::Executed(si, arrived));
            }
            return;
        }
        if self.txns[txn].phase != Phase::Executing {
            // Cancelled while the legs were executing: the abort's
            // unlocks are already in flight.
            return;
        }
        let native = self.txns[txn].spec.native;
        match stage {
            ExecStage::Legs => {
                let leg_failed = self.txns[txn].failed.iter().any(|f| *f);
                let dep_failed = self.txns[txn].dep_failed;
                if native && leg_failed {
                    // Queue order: the plain write failed before the
                    // stage's dependent command ran.
                    self.txns[txn].failed_idx = self.txns[txn].failed_set;
                    self.abort(txn, "condition failed");
                } else if native && let Some(reason) = dep_failed {
                    self.txns[txn].failed_idx = dep_idx;
                    self.abort(txn, reason);
                } else if dep_idx.is_some() && self.rules.stage_dependents {
                    self.combine(txn);
                } else {
                    self.next_stage(txn);
                }
            }
            ExecStage::ApplyDest => {
                if self.txns[txn].probe_refused {
                    self.dependent_failed(txn, "destination refused");
                    return;
                }
                let Some((_, Dependent::Move { src, .. })) = self.stage_dep(txn, si) else {
                    unreachable!("ApplyDest is the move family's")
                };
                let owner = self.owner_of(src);
                self.txns[txn].stage = ExecStage::ApplySrc;
                self.txns[txn].cursor = 1;
                self.txns[txn].fan = 1;
                self.pending.push(Msg::Exec { txn, owner, stage: ExecStage::ApplySrc });
            }
            ExecStage::ApplySrc => {
                let idx = dep_idx.expect("the move family's stage");
                self.txns[txn].replies.insert(idx, Reply::Ok);
                self.next_stage(txn);
            }
            ExecStage::Apply => {
                let idx = dep_idx.expect("the condition family's stage");
                self.txns[txn].replies.insert(idx, Reply::Int(1));
                self.next_stage(txn);
            }
        }
    }

    /// The combine step (A6): the coordinator turns the gather results
    /// into the verdict and the apply stage(s); a cancellation planted
    /// here aborts with every owner's private set discarded.
    fn combine(&mut self, txn: usize) {
        let si = self.txns[txn].si;
        self.enter(txn, CancelPhase::Combine(si));
        if self.txns[txn].phase != Phase::Executing {
            return;
        }
        let (_, dep) = self.stage_dep(txn, si).expect("a dependent command");
        match dep {
            Dependent::Move { dst, .. } => {
                if !self.txns[txn].probe_any {
                    self.dependent_failed(txn, "no such key");
                    return;
                }
                let owner = self.owner_of(dst);
                self.txns[txn].stage = ExecStage::ApplyDest;
                self.txns[txn].cursor = 1;
                self.txns[txn].fan = 1;
                self.pending.push(Msg::Exec { txn, owner, stage: ExecStage::ApplyDest });
            }
            Dependent::SetIfNoneExist { keys } => {
                if self.txns[txn].probe_refused {
                    self.dependent_failed(txn, "refused");
                    return;
                }
                if self.txns[txn].probe_any {
                    self.dependent_failed(txn, "condition failed");
                    return;
                }
                let mut owners: Vec<Cell> = keys.iter().map(|k| self.owner_of(*k)).collect();
                owners.sort_unstable();
                owners.dedup();
                self.txns[txn].stage = ExecStage::Apply;
                self.txns[txn].cursor = owners.len();
                self.txns[txn].fan = owners.len();
                for owner in owners {
                    self.pending.push(Msg::Exec { txn, owner, stage: ExecStage::Apply });
                }
            }
        }
    }

    /// The dependent command fails whole with nothing of it staged:
    /// `INF.TX` aborts, `EXEC` embeds the error (0 for a failed
    /// `MSETNX` condition) and continues with the next stage.
    fn dependent_failed(&mut self, txn: usize, reason: &'static str) {
        let si = self.txns[txn].si;
        let (idx, dep) = self.stage_dep(txn, si).expect("a dependent command");
        let reply = match (dep, reason) {
            (Dependent::SetIfNoneExist { .. }, "condition failed") => Reply::Int(0),
            _ => Reply::Err(reason),
        };
        let native = {
            let t = &mut self.txns[txn];
            t.dep_failed = Some(reason);
            t.replies.insert(idx, reply);
            if t.spec.native {
                t.failed_idx = Some(idx);
            }
            t.spec.native
        };
        if native {
            self.abort(txn, reason);
        } else {
            self.next_stage(txn);
        }
    }

    fn decide(&mut self, txn: usize) {
        // Ground truth: a mutation of a watched key between the
        // registration and this decision must abort the EXEC. Under
        // the withdrawn live-write rule the transaction's own write of
        // a watched key is already counted and is not foreign.
        let own = &self.txns[txn].writes_all;
        let own_live = !self.rules.stage_privately;
        let dirty: Vec<Key> = self.txns[txn]
            .reg
            .iter()
            .filter(|(k, reg)| {
                self.mutations.get(*k).copied().unwrap_or(0)
                    != **reg + u64::from(own_live && own.contains(k))
            })
            .map(|(k, _)| *k)
            .collect();
        if let Some(key) = dirty.first() {
            self.report.violations.push(format!(
                "WATCH INTERVAL VIOLATION: T{} committed after key {key} was mutated between the \
                     owner's validation and the decision",
                txn + 1
            ));
        }
        let seq = self.next_commit;
        self.next_commit += 1;
        self.txns[txn].commit_seq = Some(seq);
        self.txns[txn].decided_commit = true;
        let t = &self.txns[txn];
        let reply: Vec<Reply> = (0..t.program.len())
            .map(|i| t.replies.get(&i).cloned().expect("every queued command replied"))
            .collect();
        let committed: Vec<(Key, Option<usize>)> = if self.rules.stage_privately {
            // What the owners will publish: their private sets.
            t.owners
                .iter()
                .filter_map(|o| self.owners[*o].staged.get(&txn))
                .flat_map(|s| s.iter().copied())
                .collect()
        } else {
            t.writes_all
                .iter()
                .copied()
                .filter(|k| {
                    let oi = t.owners.iter().position(|o| *o == self.owner_of(*k)).expect("owner");
                    !t.failed[oi]
                })
                .map(|k| (k, Some(txn)))
                .collect()
        };
        self.txns[txn].reply = Some(reply);
        for (key, value) in committed {
            self.committed_writes.entry(key).or_default().push((seq, value));
        }
        // A bypass read has no exclusion interval to pin its point to;
        // its uncommitted-read check above is the meaningful one. A
        // read answered from the private set is the program's own
        // business — the serial oracle checks it.
        if !self.txns[txn].bypass {
            for (key, seen, own) in self.txns[txn].reads_seen.clone() {
                if own {
                    continue;
                }
                let last = self
                    .committed_writes
                    .get(&key)
                    .and_then(|v| v.iter().rev().find(|(s, _)| *s < seq))
                    .and_then(|(_, w)| *w);
                if last != seen {
                    self.report.violations.push(format!(
                        "STALE READ: T{} read key {key} = {} but the last committed writer before \
                             it was {}",
                        txn + 1,
                        name(seen),
                        name(last)
                    ));
                }
            }
        }
        self.txns[txn].phase = Phase::Terminal(Outcome::Committed);
        self.report.committed += 1;
        self.finish(txn, true);
        self.pending.push(Msg::DecisionDurable { txn });
        if let Some((CancelPhase::Decided, kind)) = self.txns[txn].spec.cancel {
            self.pending.push(Msg::CancelAt { txn, kind });
        }
    }

    fn abort(&mut self, txn: usize, reason: &'static str) {
        if matches!(self.txns[txn].phase, Phase::Terminal(_)) {
            return;
        }
        self.txns[txn].phase = Phase::Terminal(Outcome::Aborted(reason));
        self.report.aborted += 1;
        self.finish(txn, false);
    }

    /// Every terminal path carries the outcome to every owner, which
    /// publishes or discards its leg and releases every intent.
    fn finish(&mut self, txn: usize, commit: bool) {
        for owner in self.txns[txn].owners.clone() {
            self.pending.push(Msg::Unlock { txn, owner, commit });
        }
        if let Some(client) = self.client_of.get(&txn).copied() {
            self.issue_next(client);
        }
    }

    fn owner_unlock(&mut self, txn: usize, owner: Cell, commit: bool) {
        if let Some(entries) = self.owners[owner].staged.remove(&txn)
            && commit
        {
            self.publish(txn, owner, &entries);
        }
        self.remove_entries(txn, None, owner);
    }

    fn remove_entries(&mut self, txn: usize, txid: Option<u64>, owner: Cell) {
        let keys: Vec<Key> = self.owners[owner].queues.keys().copied().collect();
        for key in keys {
            let q = self.owners[owner].queues.get_mut(&key).expect("key listed");
            let before = q.len();
            q.retain(|e| !(e.txn == txn && txid.is_none_or(|id| e.txid == id)));
            if q.is_empty() {
                self.owners[owner].queues.remove(&key);
                continue;
            }
            if q.len() != before {
                self.wake(owner, key);
            }
        }
    }

    fn wake(&mut self, owner: Cell, key: Key) {
        let heads: Vec<usize> = self.owners[owner]
            .queues
            .get(&key)
            .map(|q| q.iter().map(|e| e.txn).collect())
            .unwrap_or_default();
        for t in heads {
            self.try_grant(owner, t);
        }
    }

    pub fn outcome(&self, txn: usize) -> Outcome {
        match &self.txns[txn].phase {
            Phase::Terminal(o) => o.clone(),
            _ => Outcome::Stuck,
        }
    }

    /// The published writer of `key`, if any.
    pub fn published(&self, key: Key) -> Option<usize> {
        self.owners[self.owner_of(key)].values.get(&key).copied().flatten()
    }

    /// A committed transaction's reply array in queue order (`None`
    /// until it decided, or if it aborted).
    pub fn reply(&self, txn: usize) -> Option<Vec<Reply>> {
        self.txns[txn].reply.clone()
    }
}

/// The phases before the decision: a cancellation there aborts.
pub fn before_decision(phase: CancelPhase) -> bool {
    !matches!(phase, CancelPhase::Decided | CancelPhase::Durable)
}
