//! The M4.6-S20 contract model (ADR-0116): cross-partition intent
//! acquisition, the reads-versus-intents rule, WATCH history, and the
//! durable prepare → decision → checkpoint rule, as executable adversarial
//! histories. No engine code runs here — a design story produces an ADR
//! and a model, not code (M4.6 §3); M6-S04/S06/S09/S16/S17 turn each
//! history into a DST scenario against the real cells.
//!
//! Every safety test runs the chosen rules by default and the withdrawn
//! rules under `INF_TXMODEL_VARIANT=withdrawn` — the red the review
//! harness records. The `withdrawn_*` twins pin those reds as positive
//! controls, so the model cannot pass vacuously.
//!
//! Eight models, one withdrawn rule set each:
//!
//! - [`acquisition`]: master plan §6.3 as written — a parallel lock fan,
//!   grants on arrival, waiters sorted by txid — deadlocks on the F2
//!   two-key history; canonical acquisition (ADR-0116 D2) cannot.
//!   Dragonfly's reschedule rule completes too, with counted retries.
//!   Readers that bypass intents observe uncommitted writes; a WATCH-only
//!   R intent released after validation lets a mutation land before the
//!   decision. (ADR-0116 A4, the 2026-09-10 review's F09:) a leg that
//!   writes live values leaves an aborted transaction's write published;
//!   a cancellation honoured past the in-memory decision publishes half a
//!   commit or answers a false abort; private staging and the A4 phase
//!   table never do, at any of the seven phases a cancellation can land.
//!   (ADR-0116 A6, F04:) D2.5's "fixed arguments make per-owner legs
//!   independent" renames from the source's pre-transaction value and
//!   publishes half an `MSETNX`; coordinator stages (gather → combine →
//!   apply) under the held intents give the serial outcome and abort
//!   cleanly at the combine step.
//! - [`watch`]: endpoint version equality (`Version | MISSING`) accepts
//!   the F3 absent → present → absent history and a delete/recreate;
//!   the owner's registration (ADR-0116 D4) agrees with the history
//!   oracle on every history, including eviction and owner restart.
//!   (ADR-0116 A5, F07:) re-registering on a repeated `WATCH` launders
//!   the mutation between the two; the additive rule agrees with the
//!   sticky-window oracle and with Redis 8.0.5's verdicts on every line
//!   of `seeds/watch-redis-oracle.txt`.
//! - [`durable`]: a decision that does not wait for durable prepares, or
//!   a checkpoint that streams a published record whose decision is not
//!   yet durable, leaves half a transaction after a crash; the ADR-0116
//!   D3 rules never do.
//! - [`lineage`] (ADR-0116 A1/A2, the 2026-09-10 review's F01/F02): a
//!   plain successor of a published-undecided record that outlives its
//!   dropped source, and a checkpoint that streams the immediate
//!   predecessor when that predecessor is itself undecided, both recover
//!   a state no serial history produces; inherited dependencies, the
//!   decision-qualified pinned image and dependency-gated `always` acks
//!   never do. (ADR-0116 A8, the fix validation's F01 reopening:)
//!   per-record dependencies recover half of a transaction whose legs
//!   inherited different sets — partial overlap, a read leg on one owner
//!   and a write on another, a chain two hops from its dropped root — and
//!   a decision gated on prepares alone outlives a plain write its read
//!   leg observed on another log; one transaction-wide closure gathered
//!   at the grants, carried by every prepare, and read-watermark gating
//!   of the decision recover every transaction whole.
//! - [`identity`] (ADR-0116 A3, F03): resuming `local_seq` above the
//!   coordinator's replayed maximum reissues a txid a remote participant's
//!   durable prepare still carries; a durable reservation carried by the
//!   checkpoint never does.
//! - [`revision`] (ADR-0116 A7, F05): a `RecordRevision` whose incarnation
//!   changes only at creation repeats after the u24 version wraps, after a
//!   checkpoint drops a dead incarnation, and after the u32 counter wraps;
//!   re-incarnation on wrap, a durable checkpoint-carried reservation and
//!   a typed refusal at exhaustion never repeat a durable token.
//! - [`credits`] (ADR-0116 A9, F06): D2.3's `Queued` reply plus a grant
//!   callback returns one credit twice and overflows the pair's reply
//!   headroom; keeping the request's credit until the grant, hop by hop,
//!   deadlocks a holder behind its own contenders on a saturated pair;
//!   one deferred terminal reply per `LockOp` and every hop's credit
//!   reserved at Admit complete every storm and drain the pair.
//! - [`retention`] (ADR-0116 A10, F08): "pinned bytes ≤ retention cap ×
//!   frame bound" is false — a 24 B tombstone pins a 1 MiB image or a
//!   cold extent; charging the live image at the grant against separate
//!   pinned budgets, returned or converted at publication and released
//!   with the pin, conserves the counter, holds the bound and leaks
//!   nothing.

/// The rules under test: `chosen` by default, `withdrawn` under the env
/// hook the review harness uses to record the red.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Variant {
    Chosen,
    Withdrawn,
}

impl Variant {
    /// `INF_TXMODEL_VARIANT=withdrawn` selects the withdrawn rules for
    /// every safety test (the harness's red); anything else is chosen.
    pub fn from_env() -> Variant {
        match std::env::var("INF_TXMODEL_VARIANT") {
            Ok(v) if v == "withdrawn" => Variant::Withdrawn,
            _ => Variant::Chosen,
        }
    }
}

/// xorshift64* — the model's only entropy; seeded, so every failure is a
/// replayable seed.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n.max(1) as u64) as usize
    }
}

// ---------------------------------------------------------------------
// Model 1 — intent acquisition, staging, decision, cancellation
// ---------------------------------------------------------------------

pub mod acquisition {
    use super::Rng;
    use std::collections::{BTreeMap, VecDeque};

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
            let reg =
                spec.watches.iter().map(|k| (*k, *self.mutations.get(k).unwrap_or(&0))).collect();
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
                    "INTENT LEAK: {residue} intent(s) held after every transaction reached a terminal state"
                ));
            }
            let staged: usize = self.owners.iter().map(|o| o.staged.len()).sum();
            if staged != 0 {
                self.report.violations.push(format!(
                    "STAGING LEAK: {staged} private write set(s) held after every transaction reached a terminal state"
                ));
            }
            self.check_publication();
            self.check_programs();
            &self.report
        }

        /// An aborted transaction published nothing; a committed one
        /// published every non-failed leg.
        fn check_publication(&mut self) {
            for i in 0..self.txns.len() {
                let t = &self.txns[i];
                let Phase::Terminal(outcome) = &t.phase else { continue };
                let legs: Vec<(usize, Cell)> = t
                    .owners
                    .iter()
                    .enumerate()
                    .map(|(oi, o)| (oi, *o))
                    .filter(|(_, o)| t.writes_all.iter().any(|k| self.owner_of(*k) == *o))
                    .collect();
                let published: Vec<Cell> =
                    legs.iter().filter(|(oi, _)| t.published[*oi]).map(|(_, o)| *o).collect();
                let unpublished: Vec<Cell> = legs
                    .iter()
                    .filter(|(oi, _)| !t.published[*oi] && !t.failed[*oi])
                    .map(|(_, o)| *o)
                    .collect();
                let msg = match outcome {
                    Outcome::Committed if !unpublished.is_empty() => Some(format!(
                        "PARTIAL PUBLICATION: T{} decided Commit but cell(s) {unpublished:?} never published its leg",
                        i + 1
                    )),
                    Outcome::Aborted(reason) if t.decided_commit && !published.is_empty() => {
                        if unpublished.is_empty() {
                            Some(format!(
                                "FALSE ABORT: T{} replied aborted ({reason}) but every owner published its leg",
                                i + 1
                            ))
                        } else {
                            Some(format!(
                                "PARTIAL PUBLICATION: T{} ({reason}) published on cell(s) {published:?} and discarded on cell(s) {unpublished:?}",
                                i + 1
                            ))
                        }
                    }
                    Outcome::Aborted(reason) if !published.is_empty() => {
                        let key = t
                            .writes_all
                            .iter()
                            .find(|k| self.owner_of(**k) == published[0])
                            .copied()
                            .expect("a written key on the published leg");
                        Some(format!(
                            "STAGING VIOLATION: T{} aborted ({reason}) but its write to key {key}@cell{} is published",
                            i + 1,
                            published[0]
                        ))
                    }
                    _ => None,
                };
                if let Some(msg) = msg {
                    self.report.violations.push(msg);
                }
            }
        }

        /// The serial oracle (M6-S07's AC): every terminal transaction's
        /// reply array and published state equal the program run alone
        /// against the pre-state, in queue order.
        fn check_programs(&mut self) {
            for i in 0..self.txns.len() {
                let t = &self.txns[i];
                let Phase::Terminal(outcome) = t.phase.clone() else { continue };
                if t.bypass || t.program.is_empty() {
                    continue;
                }
                let serial = self.serial_run(i);
                match outcome {
                    Outcome::Committed => self.check_committed(i, &serial),
                    Outcome::Aborted(reason) => self.check_aborted(i, reason, &serial),
                    Outcome::Stuck => {}
                }
            }
        }

        fn check_committed(&mut self, i: usize, serial: &Serial) {
            let t = &self.txns[i];
            if let Some(reason) = serial.abort {
                self.report.violations.push(format!(
                    "PROGRAM ORACLE: T{} committed where the serial history aborts ({reason})",
                    i + 1
                ));
                return;
            }
            let got = t.reply.clone().expect("a committed transaction replied");
            let mut out = Vec::new();
            for (idx, (g, w)) in got.iter().zip(&serial.replies).enumerate() {
                if g != w {
                    out.push(format!(
                        "REPLY MISMATCH: T{}'s {} replied {} where the serial history replies {}",
                        i + 1,
                        t.program[idx].label(),
                        g.text(),
                        w.text()
                    ));
                }
            }
            let seq = t.commit_seq.expect("committed");
            for key in t.writes_all.clone() {
                let later = self
                    .committed_writes
                    .get(&key)
                    .is_some_and(|v| v.iter().any(|(s, _)| *s > seq));
                if later {
                    continue;
                }
                let want = serial.state.get(&key).copied().flatten();
                let got = self.published(key);
                if got == want {
                    continue;
                }
                let writer = t
                    .program
                    .iter()
                    .rposition(|c| c.writes() && c.keys().contains(&key))
                    .expect("a written key has a writing command");
                let kind = match t.program[writer] {
                    Cmd::Dep(_) => "DEPENDENT COMMAND",
                    _ => "PLAIN COMMAND",
                };
                out.push(format!(
                    "{kind}: T{}'s {} left key {key}@cell{} = {} where the serial history gives {}",
                    i + 1,
                    t.program[writer].label(),
                    self.owner_of(key),
                    name(got),
                    name(want)
                ));
            }
            self.report.violations.extend(out);
        }

        fn check_aborted(&mut self, i: usize, reason: &'static str, serial: &Serial) {
            let t = &self.txns[i];
            // Only a command's own failure is the oracle's business —
            // cancellations, dirty watches and refusals are not.
            let Some(idx) = t.failed_idx else { return };
            let label = t.program[idx].label();
            let msg = match serial.abort {
                None => format!(
                    "DEPENDENT LEG: T{} aborted ({reason}) but the serial history commits its {label}",
                    i + 1
                ),
                Some(want) if want != reason => format!(
                    "PROGRAM ORACLE: T{} aborted ({reason}) where the serial history aborts ({want})",
                    i + 1
                ),
                Some(_) => return,
            };
            self.report.violations.push(msg);
        }

        /// The program run alone against the pre-state with the same
        /// planted refusal and condition failure: replies in queue order,
        /// the state afterwards, and where `INF.TX` aborts.
        fn serial_run(&self, txn: usize) -> Serial {
            let t = &self.txns[txn];
            let spec = &t.spec;
            let mut state = t.pre.clone();
            let mut replies = Vec::new();
            let mut cond_used = false;
            let mut abort = None;
            for cmd in &t.program {
                let reply = match cmd {
                    Cmd::Get(k) => Reply::Value(state.get(k).copied().flatten()),
                    Cmd::Set(k) => {
                        if spec.fails_at == Some(self.owner_of(*k)) && !cond_used {
                            cond_used = true;
                            Reply::Err("condition failed")
                        } else {
                            state.insert(*k, Some(txn));
                            Reply::Ok
                        }
                    }
                    Cmd::Dep(Dependent::Move { src, dst }) => {
                        let value = state.get(src).copied().flatten();
                        if value.is_none() {
                            Reply::Err("no such key")
                        } else if spec.refuses_at == Some(self.owner_of(*dst)) {
                            Reply::Err("destination refused")
                        } else {
                            state.insert(*dst, value);
                            state.insert(*src, None);
                            Reply::Ok
                        }
                    }
                    Cmd::Dep(Dependent::SetIfNoneExist { keys }) => {
                        if keys.iter().any(|k| spec.refuses_at == Some(self.owner_of(*k))) {
                            Reply::Err("refused")
                        } else if keys.iter().any(|k| state.get(k).copied().flatten().is_some()) {
                            Reply::Int(0)
                        } else {
                            for k in keys {
                                state.insert(*k, Some(txn));
                            }
                            Reply::Int(1)
                        }
                    }
                };
                let failure = reply.failure();
                replies.push(reply);
                if spec.native
                    && let Some(reason) = failure
                {
                    abort = Some(reason);
                    break;
                }
            }
            Serial { replies, state, abort }
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
                    probe.refused =
                        mine.count() > 0 && self.txns[txn].spec.refuses_at == Some(owner);
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
                        return (
                            Some("destination refused"),
                            Some(Reply::Err("destination refused")),
                        );
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
                    t.failed_set =
                        replies.iter().find(|(_, r)| r.failure().is_some()).map(|(i, _)| *i);
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
                    "WATCH INTERVAL VIOLATION: T{} committed after key {key} was mutated between the owner's validation and the decision",
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
                        let oi =
                            t.owners.iter().position(|o| *o == self.owner_of(*k)).expect("owner");
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
                            "STALE READ: T{} read key {key} = {} but the last committed writer before it was {}",
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

    fn name(txn: Option<usize>) -> String {
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
                Dependent::Move { src: a, dst: b }
            } else {
                Dependent::SetIfNoneExist { keys: vec![a, b] }
            };
            if rng.below(4) == 0 {
                spec.refuses_at = Some(match dep {
                    Dependent::Move { dst, .. } => dst as usize % cells,
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
                5 if !combines.is_empty() => {
                    CancelPhase::Combine(combines[rng.below(combines.len())])
                }
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
            dependent: Some(Dependent::Move { src: 0, dst: 1 }),
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
            program: vec![Cmd::Set(0), Cmd::Dep(Dependent::Move { src: 0, dst: 1 }), Cmd::Get(1)],
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
                Cmd::Dep(Dependent::Move { src: 1, dst: 2 }),
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
                Cmd::Dep(Dependent::Move { src: 0, dst: 1 }),
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
            program: vec![Cmd::Set(0), Cmd::Dep(Dependent::Move { src: 0, dst: 1 }), Cmd::Get(1)],
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

    /// The phases before the decision: a cancellation there aborts.
    pub fn before_decision(phase: CancelPhase) -> bool {
        !matches!(phase, CancelPhase::Decided | CancelPhase::Durable)
    }
}

// ---------------------------------------------------------------------
// Model 2 — WATCH history on one owner
// ---------------------------------------------------------------------

pub mod watch {
    use super::Rng;

    /// Two keys on one owner: enough for a repeated WATCH of one key and a
    /// WATCH of the other after the first changed.
    pub type Key = usize;
    pub const KEYS: usize = 2;

    /// How WATCH remembers what it saw.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Representation {
        /// M6-S03 as written: `{Version(v) | MISSING}` read at WATCH and
        /// compared at EXEC.
        EndpointEquality,
        /// ADR-0116 D4: the owner keeps a bounded registration whose dirty
        /// state every mutation sets synchronously; EXEC asks the owner.
        OwnerRegistration,
    }

    /// What a `WATCH` of a key the connection already watches does.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum RepeatedWatch {
        /// The batch-22 model as written: a fresh registration — a new
        /// observation, a clean dirty bit, eviction and restart forgotten.
        Reregister,
        /// ADR-0116 A5: a connection-local no-op — the first registration,
        /// its dirty state and its fate stand until EXEC/DISCARD/UNWATCH/
        /// RESET/disconnect release every registration together.
        Additive,
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct Rules {
        pub representation: Representation,
        pub repeated_watch: RepeatedWatch,
    }

    impl Rules {
        pub fn chosen() -> Rules {
            Rules {
                representation: Representation::OwnerRegistration,
                repeated_watch: RepeatedWatch::Additive,
            }
        }

        pub fn withdrawn() -> Rules {
            Rules {
                representation: Representation::EndpointEquality,
                repeated_watch: RepeatedWatch::Reregister,
            }
        }
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Event {
        Watch(Key),
        Set(Key),
        Del(Key),
        Expire(Key),
        /// `FLUSHDB`: every present key is deleted.
        Flush,
        /// The registration table evicts this key's entry (capacity).
        Evict(Key),
        /// The owner restarts: registrations and intents are lost.
        Restart,
        /// `UNWATCH`/`DISCARD`/`RESET`/disconnect: the lifecycle reset that
        /// releases every registration the connection holds.
        Unwatch,
        Exec,
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Verdict {
        Commit,
        Abort,
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct Record {
        present: bool,
        /// u24 record version: bumps per mutation; a recreated record
        /// starts at 0 again (interfaces-m0 record header).
        version: u32,
    }

    /// The connection's token for one key and the owner's entry behind it.
    #[derive(Clone, Copy, Debug)]
    struct Registration {
        observed: Option<u32>,
        dirty: bool,
        evicted: bool,
        /// The owner restarted: the token is unknown there.
        lost: bool,
    }

    /// The sticky-window oracle: the connection is watching from its first
    /// `Watch` until `Exec`/`Unwatch`; anything inside that window that a
    /// registration cannot vouch for — a mutation of a watched key, an
    /// eviction, an owner restart — is a violation no later `Watch` can
    /// undo. Deliberately not the per-registration bits the model keeps.
    #[derive(Clone, Copy, Debug, Default)]
    struct Window {
        watched: [bool; KEYS],
        violated: bool,
    }

    impl Window {
        fn open(&self) -> bool {
            self.watched.iter().any(|w| *w)
        }
    }

    /// Run `history` (must end with `Exec`) and return the verdict the
    /// rules give and the sticky-window oracle's verdict.
    pub fn run(rules: Rules, history: &[Event]) -> (Verdict, Verdict) {
        let mut recs = [Record::default(); KEYS];
        let mut regs: [Option<Registration>; KEYS] = [None; KEYS];
        let mut window = Window::default();
        for ev in history {
            match *ev {
                Event::Watch(k) => {
                    if regs[k].is_none() || rules.repeated_watch == RepeatedWatch::Reregister {
                        regs[k] = Some(Registration {
                            observed: recs[k].present.then_some(recs[k].version),
                            dirty: false,
                            evicted: false,
                            lost: false,
                        });
                    }
                    window.watched[k] = true;
                }
                Event::Set(k) => {
                    if recs[k].present {
                        recs[k].version = (recs[k].version + 1) & 0x00FF_FFFF;
                    } else {
                        recs[k] = Record { present: true, version: 0 };
                    }
                    mutate(&mut regs, &mut window, k);
                }
                Event::Del(k) | Event::Expire(k) => {
                    if recs[k].present {
                        recs[k] = Record::default();
                        mutate(&mut regs, &mut window, k);
                    }
                }
                Event::Flush => {
                    let present: Vec<Key> = (0..KEYS).filter(|k| recs[*k].present).collect();
                    for k in present {
                        recs[k] = Record::default();
                        mutate(&mut regs, &mut window, k);
                    }
                }
                Event::Evict(k) => {
                    if let Some(reg) = &mut regs[k] {
                        reg.evicted = true;
                    }
                    window.violated |= window.watched[k];
                }
                Event::Restart => {
                    for reg in regs.iter_mut().flatten() {
                        reg.lost = true;
                    }
                    window.violated |= window.open();
                }
                Event::Unwatch => {
                    regs = [None; KEYS];
                    window = Window::default();
                }
                Event::Exec => {
                    let oracle = if window.violated { Verdict::Abort } else { Verdict::Commit };
                    let clean = |k: usize, reg: &Registration| match rules.representation {
                        Representation::EndpointEquality => {
                            reg.observed == recs[k].present.then_some(recs[k].version)
                        }
                        Representation::OwnerRegistration => {
                            !(reg.dirty || reg.evicted || reg.lost)
                        }
                    };
                    let verdict =
                        if regs.iter().enumerate().all(|(k, r)| r.is_none_or(|r| clean(k, &r))) {
                            Verdict::Commit
                        } else {
                            Verdict::Abort
                        };
                    return (verdict, oracle);
                }
            }
        }
        panic!("history must end with Exec");
    }

    /// The owner marks the key's registration dirty synchronously; the
    /// window records the violation.
    fn mutate(regs: &mut [Option<Registration>; KEYS], window: &mut Window, k: Key) {
        if let Some(reg) = &mut regs[k] {
            reg.dirty = true;
        }
        window.violated |= window.watched[k];
    }

    /// The F3 history: WATCH an absent key, another client creates and
    /// deletes it, EXEC sees `MISSING` again.
    pub const ABSENT_PRESENT_ABSENT: [Event; 4] =
        [Event::Watch(0), Event::Set(0), Event::Del(0), Event::Exec];
    /// Delete/recreate: the recreated record's initial version equals the
    /// observed one.
    pub const DELETE_RECREATE: [Event; 5] =
        [Event::Set(0), Event::Watch(0), Event::Del(0), Event::Set(0), Event::Exec];
    /// The review's F07 history: a repeated WATCH must not launder the
    /// mutation between the two.
    pub const REPEATED_WATCH: [Event; 4] =
        [Event::Watch(0), Event::Set(0), Event::Watch(0), Event::Exec];
    /// Watching another key after the first watched key changed.
    pub const ADDITIONAL_KEY_AFTER_CHANGE: [Event; 4] =
        [Event::Watch(0), Event::Set(0), Event::Watch(1), Event::Exec];
    /// A repeated WATCH after an eviction or an owner restart is a no-op:
    /// the first token is the one EXEC presents, and it is unknown.
    pub const REPEATED_WATCH_AFTER_EVICTION: [Event; 5] =
        [Event::Set(0), Event::Watch(0), Event::Evict(0), Event::Watch(0), Event::Exec];
    pub const REPEATED_WATCH_AFTER_RESTART: [Event; 5] =
        [Event::Set(0), Event::Watch(0), Event::Restart, Event::Watch(0), Event::Exec];
    /// `UNWATCH` is the real reset: the WATCH after it starts clean.
    pub const RESET_THEN_WATCH: [Event; 5] =
        [Event::Watch(0), Event::Set(0), Event::Unwatch, Event::Watch(0), Event::Exec];

    /// A seeded random history of `len` events over both keys ending in
    /// `Exec`.
    pub fn random_history(seed: u64, len: usize) -> Vec<Event> {
        let mut rng = Rng::new(seed);
        let mut out = Vec::with_capacity(len + 1);
        for _ in 0..len {
            let k = rng.below(KEYS);
            out.push(match rng.below(11) {
                0 | 1 => Event::Watch(k),
                2 | 3 => Event::Set(k),
                4 => Event::Del(k),
                5 => Event::Expire(k),
                6 => Event::Flush,
                7 => Event::Evict(k),
                8 => Event::Restart,
                9 => Event::Unwatch,
                _ => Event::Set(k),
            });
        }
        out.push(Event::Exec);
        out
    }

    /// One line of `seeds/watch-redis-oracle.txt`: the history and Redis's
    /// own verdict (`scripts/txmodel-watch-redis-oracle.py`).
    pub fn parse_fixture_line(line: &str) -> Option<(Vec<Event>, Verdict)> {
        let (history, verdict) = line.split_once('\t')?;
        let verdict = match verdict.trim() {
            "abort" => Verdict::Abort,
            "commit" => Verdict::Commit,
            _ => return None,
        };
        let key = |tok: &str| tok[1..].parse::<usize>().ok().filter(|k| *k < KEYS);
        let events = history
            .split_whitespace()
            .map(|tok| match tok.as_bytes()[0] {
                b'W' => key(tok).map(Event::Watch),
                b'S' => key(tok).map(Event::Set),
                b'D' => key(tok).map(Event::Del),
                b'F' => Some(Event::Flush),
                b'U' => Some(Event::Unwatch),
                b'X' => Some(Event::Exec),
                _ => None,
            })
            .collect::<Option<Vec<Event>>>()?;
        Some((events, verdict))
    }

    /// The fixture, compiled in so the test needs no working directory.
    pub const REDIS_FIXTURE: &str = include_str!("../seeds/watch-redis-oracle.txt");
}

// ---------------------------------------------------------------------
// Model 3 — durable prepare, decision, checkpoint, crash
// ---------------------------------------------------------------------

pub mod durable {
    use super::Rng;
    use std::collections::BTreeMap;

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct Rules {
        /// The decision record is appended only after every participant's
        /// prepare is durable (ADR-0116 D3).
        pub decision_waits_for_durable_prepares: bool,
        /// A checkpoint streams the pinned predecessor of a published
        /// record whose decision is not yet durable (ADR-0116 D3).
        pub checkpoint_streams_predecessor: bool,
    }

    impl Rules {
        pub fn chosen() -> Rules {
            Rules {
                decision_waits_for_durable_prepares: true,
                checkpoint_streams_predecessor: true,
            }
        }

        pub fn withdrawn() -> Rules {
            Rules {
                decision_waits_for_durable_prepares: false,
                checkpoint_streams_predecessor: false,
            }
        }
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum Rec {
        Prepare { txid: u64, key: u32, value: u32 },
        Decision { txid: u64 },
    }

    #[derive(Clone, Debug, Default)]
    struct CellLog {
        records: Vec<Rec>,
        /// Records below this index survive the crash.
        durable: usize,
        ram: BTreeMap<u32, u32>,
        /// Predecessor images pinned for published-undecided records.
        pinned: BTreeMap<u32, Option<u32>>,
        ckpt_image: BTreeMap<u32, u32>,
        ckpt_begin: usize,
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Step {
        /// Participant `c` executes, logs its prepare and publishes.
        Prepare(usize),
        /// The coordinator (cell 0) decides once both prepared.
        Decide,
        /// An everysec timer on cell `c`.
        Fsync(usize),
        /// A checkpoint on cell `c`.
        Checkpoint(usize),
        Crash,
    }

    /// Two participants (cell 0 also coordinates) run one transaction
    /// `T` writing key 0 on cell 0 and key 1 on cell 1 over pre-images
    /// `10`/`11` (each cell booted from a checkpoint holding its
    /// pre-image); `steps` interleave everysec-style fsyncs and
    /// checkpoints with the protocol until the crash. Returns the
    /// recovered values per key, or a description of the partial commit.
    pub fn run(rules: Rules, steps: &[Step]) -> Result<[u32; 2], String> {
        let txid = 7;
        let mut cells = [CellLog::default(), CellLog::default()];
        for (c, cell) in cells.iter_mut().enumerate() {
            cell.ram.insert(c as u32, 10 + c as u32);
            cell.ckpt_image.insert(c as u32, 10 + c as u32);
        }
        let mut prepared = [false, false];
        let mut decided = false;
        let mut decision_pending = false;
        for step in steps {
            match *step {
                Step::Prepare(c) if !prepared[c] => {
                    prepared[c] = true;
                    let cell = &mut cells[c];
                    cell.records.push(Rec::Prepare { txid, key: c as u32, value: 100 });
                    // Publication (intents released) — the pre-image stays
                    // pinned until the decision is durable.
                    let old = cell.ram.insert(c as u32, 100);
                    cell.pinned.insert(c as u32, old);
                }
                Step::Prepare(_) => {}
                Step::Decide if prepared.iter().all(|p| *p) && !decided => {
                    decided = true;
                    decision_pending = true;
                }
                Step::Decide => {}
                Step::Fsync(c) => cells[c].durable = cells[c].records.len(),
                Step::Checkpoint(c) => {
                    let decision_durable = decision_durable(&cells, txid);
                    let cell = &mut cells[c];
                    let mut begin = cell.records.len();
                    let mut image = BTreeMap::new();
                    for (k, v) in &cell.ram {
                        let pending = cell.pinned.contains_key(k) && !decision_durable;
                        if pending && rules.checkpoint_streams_predecessor {
                            if let Some(Some(old)) = cell.pinned.get(k) {
                                image.insert(*k, *old);
                            }
                        } else {
                            image.insert(*k, *v);
                        }
                    }
                    cell.ckpt_image = image;
                    // The tail must still carry an undecided prepare.
                    if !decision_durable
                        && let Some(pos) =
                            cell.records.iter().position(|r| matches!(r, Rec::Prepare { .. }))
                    {
                        begin = begin.min(pos);
                    }
                    cell.ckpt_begin = begin;
                }
                Step::Crash => break,
            }
            if decision_pending {
                let prepares_durable = cells.iter().all(|c| {
                    c.records.iter().take(c.durable).any(|r| matches!(r, Rec::Prepare { .. }))
                });
                if prepares_durable || !rules.decision_waits_for_durable_prepares {
                    cells[0].records.push(Rec::Decision { txid });
                    decision_pending = false;
                }
            }
            if decision_durable(&cells, txid) {
                for cell in &mut cells {
                    cell.pinned.clear();
                }
            }
        }
        // Recovery: every cell replays checkpoint + durable tail, parks
        // prepares, and resolves them against the coordinator's log.
        let decision = decision_durable(&cells, txid);
        let mut out = [0u32; 2];
        for (c, cell) in cells.iter().enumerate() {
            let mut state = cell.ckpt_image.clone();
            let tail = &cell.records[cell.ckpt_begin.min(cell.durable)..cell.durable];
            for rec in tail {
                if let Rec::Prepare { key, value, .. } = rec
                    && decision
                {
                    state.insert(*key, *value);
                }
            }
            out[c] = *state.get(&(c as u32)).unwrap_or(&0);
        }
        let all = out.iter().all(|v| *v == 100);
        let none = out.iter().all(|v| *v != 100);
        if all || none {
            Ok(out)
        } else {
            Err(format!(
                "PARTIAL COMMIT after crash: key0 = {}, key1 = {} (decision durable: {decision})",
                out[0], out[1]
            ))
        }
    }

    fn decision_durable(cells: &[CellLog; 2], txid: u64) -> bool {
        cells[0].records.iter().take(cells[0].durable).any(|r| *r == Rec::Decision { txid })
    }

    /// The everysec history the review named: both participants publish,
    /// the coordinator decides, cell 1 checkpoints its published record,
    /// the crash arrives before any fsync.
    pub const EVERYSEC_HALF_CHECKPOINT: [Step; 5] =
        [Step::Prepare(0), Step::Prepare(1), Step::Decide, Step::Checkpoint(1), Step::Crash];

    /// A seeded interleaving of protocol steps, fsyncs and checkpoints.
    pub fn random_steps(seed: u64, len: usize) -> Vec<Step> {
        let mut rng = Rng::new(seed);
        let mut out = Vec::with_capacity(len + 1);
        for _ in 0..len {
            out.push(match rng.below(7) {
                0 => Step::Prepare(0),
                1 => Step::Prepare(1),
                2 => Step::Decide,
                3 => Step::Fsync(0),
                4 => Step::Fsync(1),
                5 => Step::Checkpoint(0),
                _ => Step::Checkpoint(1),
            });
        }
        out.push(Step::Crash);
        out
    }
}

// ---------------------------------------------------------------------
// Model 4 — pending lineage: successors, chains, the qualified image
// ---------------------------------------------------------------------

pub mod lineage {
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
            let run_origin =
                if qualified { cell.run.as_ref().map(|r| r.first_record) } else { None };
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
                    || self.read_watermarks[&t]
                        .iter()
                        .all(|(c, len)| self.cells[*c].durable >= *len);
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
                        Rec::Plain { op, deps, .. } => {
                            (Op::Incr { cell: c, id: *op }, deps.clone())
                        }
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
                    "SERIALIZABILITY VIOLATION after crash: recovered {out:?} is no serial subset of {ops:?} (decisions durable: {decided:?})"
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
                    "DEPENDENCY VIOLATION after crash: recovered {out:?} matches only subsets of {ops:?} that keep {kept:?} without {lost:?} it observed (decisions durable: {decided:?})"
                ));
            }
            let contains = |s: &&Vec<usize>, a: &Op| s.iter().any(|i| ops[*i] == *a);
            if !closed.iter().any(|s| acked.iter().all(|a| contains(s, a))) {
                return Err(format!(
                    "ACKED WRITE LOST after crash: {acked:?} acked under `always` but recovered {out:?} needs a subset without one of them (decisions durable: {decided:?})"
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
}

// ---------------------------------------------------------------------
// Model 5 — txid identity across boots
// ---------------------------------------------------------------------

pub mod identity {
    use super::Rng;
    use std::collections::BTreeSet;

    /// The D9 rule the 2026-09-10 review reopened (ADR-0116 A3).
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct Rules {
        /// A boot resumes above the last durable `txid-reserve` high-water,
        /// which is fsynced before any txid in its range leaves the cell
        /// (A3). `false` is D9 as written: above the highest txid the
        /// cell's own log replayed.
        pub resume_from_durable_reservation: bool,
        /// The checkpoint carries the reservation high-water, so truncating
        /// the tail never loses it (A3). `false`: the reservation records
        /// are truncated with the tail.
        pub checkpoint_carries_reservation: bool,
    }

    impl Rules {
        pub fn chosen() -> Rules {
            Rules { resume_from_durable_reservation: true, checkpoint_carries_reservation: true }
        }

        pub fn withdrawn() -> Rules {
            Rules { resume_from_durable_reservation: false, checkpoint_carries_reservation: false }
        }
    }

    /// Reservation chunk — small so the model crosses chunk boundaries.
    pub const CHUNK: u64 = 16;

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum Rec {
        Reserve { upto: u64 },
        Tag { seq: u64 },
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Step {
        /// The coordinator issues the next txid to a transaction.
        Issue,
        /// The coordinator logs its own leg's prepare for the last txid.
        LocalPrepare,
        /// A remote participant's durable prepare carries the last txid.
        RemoteSurvives,
        Fsync,
        /// The tail is truncated behind a checkpoint.
        Checkpoint,
        /// Non-durable local records are lost; the cell reboots.
        Crash,
    }

    /// One coordinator cell (cell 0) issuing txids against remote
    /// participants whose durable prepares outlive the coordinator's own
    /// records. Returns the number of txids issued, or the reissue.
    pub fn run(rules: Rules, steps: &[Step]) -> Result<u64, String> {
        let mut log: Vec<Rec> = Vec::new();
        let mut durable = 0usize;
        let mut next = 1u64;
        let mut reserved_upto = 0u64;
        let mut ckpt_reserved = 0u64;
        let mut last_issued = None;
        let mut remote = BTreeSet::new();
        let mut issued = 0u64;
        for step in steps {
            match *step {
                Step::Issue => {
                    if rules.resume_from_durable_reservation && next > reserved_upto {
                        reserved_upto = next + CHUNK - 1;
                        log.push(Rec::Reserve { upto: reserved_upto });
                        // Durable before any txid of the range leaves the cell.
                        durable = log.len();
                    }
                    let seq = next;
                    next += 1;
                    issued += 1;
                    if remote.contains(&seq) {
                        return Err(format!(
                            "TXID REISSUE: (0, {seq}) issued again while a remote participant's durable prepare still carries it"
                        ));
                    }
                    last_issued = Some(seq);
                }
                Step::LocalPrepare => {
                    if let Some(seq) = last_issued {
                        log.push(Rec::Tag { seq });
                    }
                }
                Step::RemoteSurvives => {
                    if let Some(seq) = last_issued {
                        remote.insert(seq);
                    }
                }
                Step::Fsync => durable = log.len(),
                Step::Checkpoint => {
                    if rules.checkpoint_carries_reservation {
                        ckpt_reserved = ckpt_reserved.max(reserved_upto);
                    }
                    log.clear();
                    durable = 0;
                }
                Step::Crash => {
                    log.truncate(durable);
                    let floor = replay_floor(rules, &log, ckpt_reserved);
                    next = floor + 1;
                    reserved_upto = floor;
                    last_issued = None;
                }
            }
        }
        Ok(issued)
    }

    fn replay_floor(rules: Rules, log: &[Rec], ckpt_reserved: u64) -> u64 {
        let mut replayed_max = 0;
        let mut reserved_max = ckpt_reserved;
        for rec in log {
            match rec {
                Rec::Tag { seq } => replayed_max = replayed_max.max(*seq),
                Rec::Reserve { upto } => reserved_max = reserved_max.max(*upto),
            }
        }
        if rules.resume_from_durable_reservation {
            replayed_max.max(reserved_max)
        } else {
            replayed_max
        }
    }

    /// F03: forty transactions persisted locally; txid 41 issued, a remote
    /// prepare survives, the coordinator crashes before logging anything
    /// for 41, reboots, and issues again.
    pub fn remote_prepare_history() -> Vec<Step> {
        let mut steps = Vec::new();
        for _ in 0..40 {
            steps.push(Step::Issue);
            steps.push(Step::LocalPrepare);
        }
        steps.extend([Step::Fsync, Step::Issue, Step::RemoteSurvives, Step::Crash, Step::Issue]);
        steps
    }

    /// A seeded history of issues, local/remote prepares, fsyncs,
    /// checkpoints and crashes.
    pub fn random_steps(seed: u64, len: usize) -> Vec<Step> {
        let mut rng = Rng::new(seed);
        (0..len)
            .map(|_| match rng.below(9) {
                0..=2 => Step::Issue,
                3 => Step::LocalPrepare,
                4 => Step::RemoteSurvives,
                5 => Step::Fsync,
                6 => Step::Checkpoint,
                _ => Step::Crash,
            })
            .collect()
    }
}

// ---------------------------------------------------------------------
// Model 6 — RecordRevision non-repetition (ADR-0116 A7)
// ---------------------------------------------------------------------

pub mod revision {
    use super::Rng;
    use std::collections::BTreeSet;

    /// Field widths, shrunk so every wrap and the exhaustion are reachable
    /// in a short history. The real widths are u24 and u32 (ADR-0114 D6);
    /// the arithmetic in the ADR scales these.
    pub const VERSION_BITS: u32 = 3;
    pub const INCARNATION_BITS: u32 = 4;
    /// Reservation chunk — small so the model crosses chunk boundaries.
    pub const CHUNK: u64 = 4;

    /// The D8 rule the 2026-09-10 review reopened (ADR-0116 A7).
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct Rules {
        /// The mutation that would wrap the version re-incarnates the
        /// record instead (A7). `false` is D8 as written: the version
        /// wraps and the incarnation stands.
        pub reincarnate_on_wrap: bool,
        /// The incarnation counter resumes from a durable, checkpoint-
        /// carried reservation (A7, A3's shape). `false`: from the highest
        /// incarnation the replayed records carry.
        pub resume_from_durable_reservation: bool,
        /// An exhausted counter refuses the create (`INF REVEXHAUSTED`).
        /// `false`: the counter wraps to zero.
        pub refuse_at_exhaustion: bool,
    }

    impl Rules {
        pub fn chosen() -> Rules {
            Rules {
                reincarnate_on_wrap: true,
                resume_from_durable_reservation: true,
                refuse_at_exhaustion: true,
            }
        }

        pub fn withdrawn() -> Rules {
            Rules {
                reincarnate_on_wrap: false,
                resume_from_durable_reservation: false,
                refuse_at_exhaustion: false,
            }
        }
    }

    /// `RecordRevision` — the `IF REV` token a client retains.
    #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub struct Token {
        pub incarnation: u64,
        pub version: u64,
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Step {
        /// `SET` of the key: a create when absent, an update when present.
        Create,
        /// A mutation of the present key (no-op when absent).
        Update,
        Delete,
        Fsync,
        /// The tail is truncated behind a checkpoint image of the key.
        Checkpoint,
        /// Non-durable records are lost; the cell reboots.
        Crash,
    }

    #[derive(Copy, Clone, Debug)]
    enum Rec {
        Reserve { upto: u64 },
        Live(Token),
        Tombstone,
    }

    #[derive(Clone, Debug, Default)]
    pub struct Stats {
        pub issued: u64,
        pub reincarnations: u64,
        pub refused: u64,
        pub boots: u64,
    }

    /// One key on one cell, one history. A token returned to a client is
    /// a durable fact once its record is; the violation is a token issued
    /// twice for distinct durable mutations. (A token acked under
    /// `everysec` for a write the crash window lost may be reissued — the
    /// same window as the write, disclosed in the ADR.)
    pub fn run(rules: Rules, steps: &[Step]) -> Result<Stats, String> {
        let mut state = State { next_inc: 1, ..State::default() };
        for step in steps {
            match step {
                Step::Create | Step::Update => state.write(rules, *step)?,
                Step::Delete => {
                    if state.live.take().is_some() {
                        state.log.push(Rec::Tombstone);
                    }
                }
                Step::Fsync => state.fsync(),
                Step::Checkpoint => {
                    state.fsync();
                    state.ckpt = Some((state.live, state.reserved_upto));
                    state.log.clear();
                    state.durable = 0;
                }
                Step::Crash => state.crash(rules),
            }
        }
        Ok(state.stats)
    }

    #[derive(Default)]
    struct State {
        log: Vec<Rec>,
        durable: usize,
        ckpt: Option<(Option<Token>, u64)>,
        live: Option<Token>,
        next_inc: u64,
        reserved_upto: u64,
        counter_wrapped: bool,
        issued_durable: BTreeSet<Token>,
        stats: Stats,
    }

    impl State {
        fn write(&mut self, rules: Rules, step: Step) -> Result<(), String> {
            let ver_max = 1u64 << VERSION_BITS;
            let next = match self.live {
                None if step == Step::Update => return Ok(()),
                Some(t) if t.version + 1 < ver_max => {
                    Token { incarnation: t.incarnation, version: t.version + 1 }
                }
                Some(t) if !rules.reincarnate_on_wrap => {
                    // The withdrawn rule wraps without changing incarnation.
                    let wrapped = Token { incarnation: t.incarnation, version: 0 };
                    if self.issued_durable.contains(&wrapped) {
                        return Err(format!(
                            "REVISION REPEAT: ({}, 0) issued again after {ver_max} mutations of a record that never left — the u{VERSION_BITS} version wrapped under incarnation {}",
                            t.incarnation, t.incarnation
                        ));
                    }
                    wrapped
                }
                _ => {
                    let Some(fresh) = self.reincarnate(rules)? else { return Ok(()) };
                    fresh
                }
            };
            self.stats.issued += 1;
            self.live = Some(next);
            self.log.push(Rec::Live(next));
            Ok(())
        }

        fn reincarnate(&mut self, rules: Rules) -> Result<Option<Token>, String> {
            let inc_max = 1u64 << INCARNATION_BITS;
            if rules.resume_from_durable_reservation && self.next_inc > self.reserved_upto {
                let upto = (self.reserved_upto + CHUNK).min(inc_max);
                self.log.push(Rec::Reserve { upto });
                self.fsync();
                self.reserved_upto = upto;
            }
            if self.next_inc >= inc_max {
                if rules.refuse_at_exhaustion {
                    self.stats.refused += 1;
                    return Ok(None);
                }
                self.next_inc = 0;
                self.counter_wrapped = true;
            }
            let incarnation = self.next_inc;
            self.next_inc += 1;
            if self.live.is_some() {
                self.stats.reincarnations += 1;
            }
            let fresh = Token { incarnation, version: 0 };
            if self.issued_durable.contains(&fresh) {
                let why = if self.counter_wrapped {
                    format!("the u{INCARNATION_BITS} incarnation counter wrapped")
                } else {
                    "the counter resumed below a dead incarnation after a restart".to_string()
                };
                return Err(format!(
                    "REVISION REPEAT: ({incarnation}, 0) issued again for a new incarnation — {why}"
                ));
            }
            Ok(Some(fresh))
        }

        fn fsync(&mut self) {
            for rec in &self.log[self.durable..] {
                if let Rec::Live(t) = rec {
                    self.issued_durable.insert(*t);
                }
            }
            self.durable = self.log.len();
        }

        fn crash(&mut self, rules: Rules) {
            self.stats.boots += 1;
            self.log.truncate(self.durable);
            let (image, ckpt_reserved) = self.ckpt.unwrap_or((None, 0));
            self.live = image;
            let mut replayed_max = image.map_or(0, |t| t.incarnation);
            let mut reservation = ckpt_reserved;
            for rec in &self.log {
                match rec {
                    Rec::Reserve { upto } => reservation = reservation.max(*upto),
                    Rec::Live(t) => {
                        self.live = Some(*t);
                        replayed_max = replayed_max.max(t.incarnation);
                    }
                    Rec::Tombstone => self.live = None,
                }
            }
            if rules.resume_from_durable_reservation {
                // Burn the unused part of the durable reservation on boot.
                self.next_inc = reservation + 1;
                self.reserved_upto = reservation;
            } else {
                self.next_inc = replayed_max + 1;
            }
        }
    }

    /// F05's history: one record, never deleted, updated past the version
    /// width.
    pub fn wrap_history() -> Vec<Step> {
        let mut steps = vec![Step::Create, Step::Fsync];
        steps.extend(std::iter::repeat_n([Step::Update, Step::Fsync], 1 << VERSION_BITS).flatten());
        steps
    }

    /// Two lives of the key, both durable, the second dead and its
    /// tombstone folded into a checkpoint image: the replayed records
    /// carry no incarnation at all.
    pub fn dead_incarnation_history() -> Vec<Step> {
        vec![
            Step::Create,
            Step::Fsync,
            Step::Delete,
            Step::Fsync,
            Step::Create,
            Step::Fsync,
            Step::Delete,
            Step::Checkpoint,
            Step::Crash,
            Step::Create,
            Step::Fsync,
        ]
    }

    /// Creates and deletes past the incarnation width.
    pub fn exhaustion_history() -> Vec<Step> {
        std::iter::repeat_n(
            [Step::Create, Step::Fsync, Step::Delete, Step::Fsync],
            1 << INCARNATION_BITS,
        )
        .flatten()
        .chain([Step::Create, Step::Fsync])
        .collect()
    }

    pub fn random_steps(seed: u64, len: usize) -> Vec<Step> {
        let mut rng = Rng::new(seed ^ 0x5EED_0007);
        (0..len)
            .map(|_| match rng.below(16) {
                0..=2 => Step::Create,
                3..=8 => Step::Update,
                9..=10 => Step::Delete,
                11..=12 => Step::Fsync,
                13 => Step::Checkpoint,
                _ => Step::Crash,
            })
            .collect()
    }
}

// ---------------------------------------------------------------------
// Model 7 — fabric credits for queued grants (ADR-0116 A9, review F06)
// ---------------------------------------------------------------------

pub mod credits {
    use super::Rng;
    use std::collections::{BTreeMap, BTreeSet, VecDeque};

    pub type Txn = usize;

    /// D2.3's queued grant against M0-S09's credit contract (ADR-0116 A9).
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct Rules {
        /// A `LockOp` has exactly one reply, sent when its entries are
        /// eligible — or when validation fails, the owner refuses, or an
        /// abort removes the parked entries; the parked `LockOp` is the
        /// queue entry (A9). `false` is D2.3 as written: `Queued` replies
        /// at once and the grant callback is a second reply to the same
        /// request.
        pub grant_is_the_deferred_terminal_reply: bool,
        /// At Admit a transaction reserves every credit it will send toward
        /// the owner — `LockOp`, its `ExecOp`s, `UnlockOp`, the
        /// decision-durable notification — and is refused typed when the
        /// pair's pool is short (A9). `false` is the mesh as-is: each hop
        /// takes its credit when sent and waits when there is none.
        pub credits_reserved_at_admit: bool,
    }

    impl Rules {
        pub fn chosen() -> Rules {
            Rules { grant_is_the_deferred_terminal_reply: true, credits_reserved_at_admit: true }
        }

        pub fn withdrawn() -> Rules {
            Rules { grant_is_the_deferred_terminal_reply: false, credits_reserved_at_admit: false }
        }

        /// The review's named alternative: keep the request's credit until
        /// the grant, and take every other hop's credit when it is sent.
        pub fn deferred_hop_by_hop() -> Rules {
            Rules { grant_is_the_deferred_terminal_reply: true, credits_reserved_at_admit: false }
        }
    }

    /// One coordinator, one owner, one directed ring each way.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct Config {
        /// `MeshConfig::data_credits` for the pair; each ring holds twice
        /// as many slots (the reply-headroom invariant).
        pub data_credits: u32,
        /// Keys on the owner; every transaction locks one.
        pub keys: usize,
        /// Transactions offered, in arrival order.
        pub txns: usize,
        /// Scheduler steps a sent `LockOp` waits before the coordinator's
        /// timer aborts the transaction.
        pub timeout: usize,
    }

    /// The hops one transaction sends toward one owner: `LockOp`, one
    /// `ExecOp` (a class-1/2 program), `UnlockOp`, the notification.
    pub const HOPS_PER_OWNER: u32 = 4;

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum Msg {
        Lock(Txn),
        Exec(Txn),
        Unlock {
            txn: Txn,
            commit: bool,
        },
        Notify(Txn),
        /// Withdrawn: the nonterminal first reply to a parked `LockOp`.
        Queued(Txn),
        /// The grant: the deferred terminal reply (chosen) or the second
        /// reply to the same request (withdrawn).
        Granted(Txn),
        /// Chosen: the parked `LockOp`'s terminal reply once an abort
        /// removed its entries.
        Aborted(Txn),
        SubResult(Txn),
        UnlockAck(Txn),
        NotifyAck(Txn),
    }

    impl Msg {
        fn txn(&self) -> Txn {
            match *self {
                Msg::Lock(t)
                | Msg::Exec(t)
                | Msg::Unlock { txn: t, .. }
                | Msg::Notify(t)
                | Msg::Queued(t)
                | Msg::Granted(t)
                | Msg::Aborted(t)
                | Msg::SubResult(t)
                | Msg::UnlockAck(t)
                | Msg::NotifyAck(t) => t,
            }
        }

        fn name(&self) -> &'static str {
            match self {
                Msg::Lock(_) => "LockOp",
                Msg::Exec(_) => "ExecOp",
                Msg::Unlock { .. } => "UnlockOp",
                Msg::Notify(_) => "the decision-durable notification",
                _ => "a reply",
            }
        }
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum State {
        /// The `LockOp` is in flight or parked; `since` is the send step.
        Locking {
            since: usize,
        },
        Executing,
        Unlocking,
        Notifying,
        /// The timer fired: `UnlockOp{Abort}` is in flight or pending.
        Aborting,
        Committed,
        Aborted,
        Refused,
    }

    impl State {
        fn terminal(self) -> bool {
            matches!(self, State::Committed | State::Aborted | State::Refused)
        }
    }

    #[derive(Clone, Debug)]
    struct Coordinator {
        state: State,
        /// Requests sent so far — the reserved credits already spent.
        spent: u32,
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Action {
        /// The next transaction arrives at the coordinator.
        Arrive,
        DeliverToOwner,
        DeliverToCoord,
        /// The coordinator's timer fires for a parked transaction.
        Timeout(Txn),
        /// Deliver and send until nothing moves, firing the timers of
        /// whatever is still parked when nothing else can happen.
        Settle,
    }

    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct Stats {
        pub committed: usize,
        pub aborted: usize,
        pub refused: usize,
        pub max_parked: usize,
        pub max_ring: usize,
    }

    struct Model {
        rules: Rules,
        cfg: Config,
        credits: u32,
        to_owner: VecDeque<Msg>,
        to_coord: VecDeque<Msg>,
        txns: Vec<Coordinator>,
        /// Requests waiting to be sent, in the order they were queued (hop
        /// by hop: the head waits for a credit — the mesh backpressures the
        /// originating connection, it never reorders).
        outbox: VecDeque<Msg>,
        arrived: usize,
        step: usize,
        /// The owner: one FIFO per key, and the `LockOp`s parked behind a
        /// head.
        queues: Vec<VecDeque<Txn>>,
        key_of: BTreeMap<Txn, usize>,
        parked: BTreeSet<Txn>,
        stats: Stats,
    }

    /// The scripted history. Returns the stats, or the violation.
    pub fn run_script(rules: Rules, cfg: Config, actions: &[Action]) -> Result<Stats, String> {
        let mut m = Model::new(rules, cfg);
        for action in actions {
            m.act(*action)?;
        }
        m.settle()?;
        m.finish()
    }

    /// A seeded interleaving of arrivals, deliveries and timers until
    /// every transaction is terminal.
    pub fn run(rules: Rules, cfg: Config, seed: u64) -> Result<Stats, String> {
        let mut rng = Rng::new(seed);
        let mut m = Model::new(rules, cfg);
        let bound = 200 * cfg.txns.max(1);
        loop {
            let enabled = m.enabled();
            if enabled.is_empty() {
                if m.parked_lockops().is_empty() {
                    break;
                }
                for t in m.parked_lockops() {
                    m.act(Action::Timeout(t))?;
                }
                continue;
            }
            m.act(enabled[rng.below(enabled.len())])?;
            if m.step > bound {
                return Err(format!(
                    "NO PROGRESS: {bound} steps without every transaction terminal"
                ));
            }
        }
        m.finish()
    }

    impl Model {
        fn new(rules: Rules, cfg: Config) -> Model {
            Model {
                rules,
                cfg,
                credits: cfg.data_credits,
                to_owner: VecDeque::new(),
                to_coord: VecDeque::new(),
                txns: Vec::with_capacity(cfg.txns),
                outbox: VecDeque::new(),
                arrived: 0,
                step: 0,
                queues: vec![VecDeque::new(); cfg.keys.max(1)],
                key_of: BTreeMap::new(),
                parked: BTreeSet::new(),
                stats: Stats::default(),
            }
        }

        fn capacity(&self) -> usize {
            2 * self.cfg.data_credits as usize
        }

        /// Transactions whose `LockOp` was sent and is not yet answered.
        fn parked_lockops(&self) -> Vec<Txn> {
            (0..self.txns.len())
                .filter(|t| matches!(self.txns[*t].state, State::Locking { .. }))
                .collect()
        }

        fn enabled(&self) -> Vec<Action> {
            let mut out = Vec::new();
            if self.arrived < self.cfg.txns {
                out.push(Action::Arrive);
            }
            if !self.to_owner.is_empty() {
                out.push(Action::DeliverToOwner);
            }
            if !self.to_coord.is_empty() {
                out.push(Action::DeliverToCoord);
            }
            for t in self.parked_lockops() {
                if let State::Locking { since } = self.txns[t].state
                    && since + self.cfg.timeout <= self.step
                {
                    out.push(Action::Timeout(t));
                }
            }
            out
        }

        fn act(&mut self, action: Action) -> Result<(), String> {
            self.step += 1;
            match action {
                Action::Arrive => self.arrive(),
                Action::DeliverToOwner => {
                    if let Some(msg) = self.to_owner.pop_front() {
                        self.owner_receives(msg)?;
                    }
                }
                Action::DeliverToCoord => {
                    if let Some(msg) = self.to_coord.pop_front() {
                        self.coord_receives(msg)?;
                    }
                }
                Action::Timeout(t) => self.timeout(t),
                Action::Settle => self.settle()?,
            }
            self.retry_sends()?;
            self.check_bounds()
        }

        /// Admit: chosen reserves every hop's credit or refuses typed;
        /// hop by hop admits unconditionally.
        fn arrive(&mut self) {
            let t = self.arrived;
            self.arrived += 1;
            let mut c = Coordinator { state: State::Refused, spent: 0 };
            if self.rules.credits_reserved_at_admit {
                if self.credits < HOPS_PER_OWNER {
                    self.stats.refused += 1;
                    self.txns.push(c);
                    return;
                }
                self.credits -= HOPS_PER_OWNER;
            }
            c.state = State::Locking { since: self.step };
            self.txns.push(c);
            self.outbox.push_back(Msg::Lock(t));
        }

        /// The coordinator's timer: a `LockOp` still in the outbox is
        /// withdrawn locally (nothing was sent); a sent one is aborted with
        /// `UnlockOp{Abort}`.
        fn timeout(&mut self, t: Txn) {
            if !matches!(self.txns[t].state, State::Locking { .. }) {
                return;
            }
            if self.outbox.contains(&Msg::Lock(t)) {
                self.outbox.retain(|m| *m != Msg::Lock(t));
                self.finish_txn(t, State::Aborted);
                return;
            }
            self.txns[t].state = State::Aborting;
            self.outbox.push_back(Msg::Unlock { txn: t, commit: false });
        }

        /// Sends the outbox head while a credit allows (chosen: the
        /// reservation already holds every credit, so it drains whole).
        fn retry_sends(&mut self) -> Result<(), String> {
            while let Some(msg) = self.outbox.front().copied() {
                if !self.rules.credits_reserved_at_admit {
                    if self.credits == 0 {
                        break;
                    }
                    self.credits -= 1;
                }
                self.outbox.pop_front();
                self.txns[msg.txn()].spent += 1;
                self.to_owner.push_back(msg);
                self.stats.max_ring = self.stats.max_ring.max(self.to_owner.len());
            }
            Ok(())
        }

        fn reply(&mut self, msg: Msg) {
            self.to_coord.push_back(msg);
            self.stats.max_ring = self.stats.max_ring.max(self.to_coord.len());
        }

        fn check_bounds(&mut self) -> Result<(), String> {
            let cap = self.capacity();
            for (name, ring) in
                [("coordinator→owner", &self.to_owner), ("owner→coordinator", &self.to_coord)]
            {
                if ring.len() > cap {
                    return Err(format!(
                        "RING OVERFLOW: the {name} ring holds {} messages over its capacity {cap} (2 × {} data credits)",
                        ring.len(),
                        self.cfg.data_credits
                    ));
                }
            }
            if self.credits > self.cfg.data_credits {
                return Err(format!(
                    "CREDIT OVERFLOW: the coordinator holds {} credits toward the owner of {} — a second reply to one request returned its credit twice",
                    self.credits, self.cfg.data_credits
                ));
            }
            self.stats.max_parked = self.stats.max_parked.max(self.parked.len());
            if self.parked.len() > self.cfg.data_credits as usize {
                return Err(format!(
                    "PARKED LOCKOPS EXCEED THE CREDIT BOUND: {} parked at the owner with {} data credits — a parked LockOp holds no credit",
                    self.parked.len(),
                    self.cfg.data_credits
                ));
            }
            Ok(())
        }

        fn owner_receives(&mut self, msg: Msg) -> Result<(), String> {
            match msg {
                Msg::Lock(t) => {
                    let k = t % self.cfg.keys.max(1);
                    self.key_of.insert(t, k);
                    self.queues[k].push_back(t);
                    if self.queues[k].len() == 1 {
                        self.reply(Msg::Granted(t));
                    } else {
                        self.parked.insert(t);
                        if !self.rules.grant_is_the_deferred_terminal_reply {
                            self.reply(Msg::Queued(t));
                        }
                    }
                }
                Msg::Exec(t) => self.reply(Msg::SubResult(t)),
                Msg::Unlock { txn: t, .. } => {
                    let k = self.key_of[&t];
                    self.queues[k].retain(|x| *x != t);
                    if self.parked.remove(&t) && self.rules.grant_is_the_deferred_terminal_reply {
                        self.reply(Msg::Aborted(t));
                    }
                    self.reply(Msg::UnlockAck(t));
                    if let Some(head) = self.queues[k].front().copied()
                        && self.parked.remove(&head)
                    {
                        self.reply(Msg::Granted(head));
                    }
                }
                Msg::Notify(t) => self.reply(Msg::NotifyAck(t)),
                reply => return Err(format!("MISROUTED: {reply:?} reached the owner")),
            }
            Ok(())
        }

        /// Every reply returns one credit (the mesh's rule); the state
        /// machine advances and queues its next hop.
        fn coord_receives(&mut self, msg: Msg) -> Result<(), String> {
            self.credits += 1;
            let (t, next) = match msg {
                Msg::Queued(t) => (t, None),
                Msg::Granted(t) => match self.txns[t].state {
                    State::Locking { .. } => (t, Some((State::Executing, Msg::Exec(t)))),
                    // A grant that overtook the abort: the abort removes it.
                    _ => (t, None),
                },
                Msg::SubResult(t) => {
                    (t, Some((State::Unlocking, Msg::Unlock { txn: t, commit: true })))
                }
                Msg::UnlockAck(t) => match self.txns[t].state {
                    State::Aborting => {
                        self.finish_txn(t, State::Aborted);
                        (t, None)
                    }
                    _ => (t, Some((State::Notifying, Msg::Notify(t)))),
                },
                Msg::NotifyAck(t) => {
                    self.finish_txn(t, State::Committed);
                    (t, None)
                }
                Msg::Aborted(t) => (t, None),
                request => return Err(format!("MISROUTED: {request:?} reached the coordinator")),
            };
            if let Some((state, request)) = next {
                self.txns[t].state = state;
                self.outbox.push_back(request);
            }
            Ok(())
        }

        /// A terminal transaction returns the reserved credits it never
        /// spent (its spent ones came back with their replies).
        fn finish_txn(&mut self, t: Txn, state: State) {
            self.txns[t].state = state;
            if self.rules.credits_reserved_at_admit {
                self.credits += HOPS_PER_OWNER - self.txns[t].spent;
            }
            match state {
                State::Committed => self.stats.committed += 1,
                State::Aborted => self.stats.aborted += 1,
                _ => {}
            }
        }

        /// Deliver and send until nothing moves; then fire every parked
        /// timer and continue; a stuck transaction at that point is the
        /// deadlock.
        fn settle(&mut self) -> Result<(), String> {
            loop {
                while let Some(msg) = self.to_owner.pop_front() {
                    self.owner_receives(msg)?;
                    self.retry_sends()?;
                    self.check_bounds()?;
                }
                while let Some(msg) = self.to_coord.pop_front() {
                    self.coord_receives(msg)?;
                    self.retry_sends()?;
                    self.check_bounds()?;
                }
                if !self.to_owner.is_empty() {
                    continue;
                }
                let parked = self.parked_lockops();
                if parked.is_empty() {
                    break;
                }
                for t in parked {
                    self.timeout(t);
                }
                self.retry_sends()?;
                if self.to_owner.is_empty() && self.to_coord.is_empty() {
                    break;
                }
            }
            self.quiescent()
        }

        /// No message in flight, no timer left: every transaction must be
        /// terminal.
        fn quiescent(&self) -> Result<(), String> {
            let stuck: Vec<Txn> =
                (0..self.txns.len()).filter(|t| !self.txns[*t].state.terminal()).collect();
            let Some(first) = stuck.first().copied() else { return Ok(()) };
            let wants = self
                .outbox
                .iter()
                .find(|m| m.txn() == first)
                .map(|m| m.name())
                .unwrap_or("a reply that never comes");
            let holds = self
                .key_of
                .get(&first)
                .filter(|k| self.queues[**k].front() == Some(&first))
                .map(|k| format!("holds key {k}@owner and "))
                .unwrap_or_default();
            let parked: Vec<String> = self.parked.iter().map(|t| format!("T{}", t + 1)).collect();
            Err(format!(
                "CREDIT DEADLOCK: {} transaction(s) stuck with no message in flight and {} credit(s) toward the owner — T{} {holds}waits for {wants}; {} parked LockOp(s) ({}) hold every credit",
                stuck.len(),
                self.credits,
                first + 1,
                parked.len(),
                parked.join(", ")
            ))
        }

        fn finish(self) -> Result<Stats, String> {
            self.quiescent()?;
            if self.credits != self.cfg.data_credits {
                return Err(format!(
                    "CREDIT LEAK: {} credits toward the owner after every transaction is terminal, budget {}",
                    self.credits, self.cfg.data_credits
                ));
            }
            if self.queues.iter().any(|q| !q.is_empty()) || !self.parked.is_empty() {
                return Err("INTENT LEAK: the owner still queues an intent after every transaction is terminal".to_string());
            }
            Ok(self.stats)
        }
    }

    /// The review's history: one hot key; T1's `LockOp` is granted at the
    /// owner, then eight contenders send their `LockOp`s — every data
    /// credit of the pair — before T1's grant reaches the coordinator and
    /// T1 needs credits for its `ExecOp` and `UnlockOp`.
    pub fn saturated_pair() -> (Config, Vec<Action>) {
        let cfg = Config { data_credits: 8, keys: 1, txns: 9, timeout: 1 << 20 };
        let mut actions = vec![Action::Arrive, Action::DeliverToOwner];
        for _ in 1..9 {
            actions.push(Action::Arrive);
            actions.push(Action::DeliverToOwner);
        }
        actions.push(Action::Settle);
        (cfg, actions)
    }

    /// A storm: two hot keys, three transactions per credit, short timers.
    pub fn storm() -> Config {
        Config { data_credits: 8, keys: 2, txns: 24, timeout: 12 }
    }
}

// ---------------------------------------------------------------------
// Model 8 — retained-byte budget for pinned images (ADR-0116 A10, review F08)
// ---------------------------------------------------------------------

pub mod retention {
    use super::Rng;
    use std::collections::{BTreeMap, BTreeSet};

    pub type Txid = u64;
    pub type Key = usize;

    /// D3's "pinned bytes are bounded by the retention cap × frame bound"
    /// against a counted budget (ADR-0116 A10).
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct Rules {
        /// The qualified image's bytes — hot arena bytes, or a cold
        /// extent's bytes — are charged against the cell's pinned budgets
        /// at the grant for every written key not already pending, refused
        /// typed when over, and released when the key's pending set is
        /// durably decided (A10). `false` is D3 as written: nothing is
        /// charged; the frame bound is claimed to bound retention.
        pub charge_retained_images_at_grant: bool,
    }

    impl Rules {
        pub fn chosen() -> Rules {
            Rules { charge_retained_images_at_grant: true }
        }

        pub fn withdrawn() -> Rules {
            Rules { charge_retained_images_at_grant: false }
        }
    }

    /// The bounds in force on the cell.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct Budget {
        /// D5: one publishable frame per transaction.
        pub frame_max: u64,
        /// S22: pending (published, not durably decided) decisions.
        pub retention_cap: usize,
        /// A10: `tx-pinned-max-bytes` (arena).
        pub pinned_max: u64,
        /// A10: `tx-pinned-max-extent-bytes` (cold storage).
        pub pinned_extent_max: u64,
    }

    /// A key's live image.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Image {
        Absent,
        Hot(u64),
        Cold(u64),
    }

    impl Image {
        fn hot(self) -> u64 {
            match self {
                Image::Hot(b) => b,
                _ => 0,
            }
        }

        fn extent(self) -> u64 {
            match self {
                Image::Cold(b) => b,
                _ => 0,
            }
        }
    }

    /// A staged effect and its serialized log bytes.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Effect {
        /// A tombstone: `TOMBSTONE_BYTES` in the frame, whatever it pins.
        Delete,
        Set(u64),
    }

    pub const TOMBSTONE_BYTES: u64 = 24;

    impl Effect {
        fn staged(self) -> u64 {
            match self {
                Effect::Delete => TOMBSTONE_BYTES,
                Effect::Set(b) => b,
            }
        }

        fn apply(self) -> Image {
            match self {
                Effect::Delete => Image::Absent,
                Effect::Set(b) => Image::Hot(b),
            }
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Txn {
        pub id: Txid,
        pub writes: Vec<(Key, Effect)>,
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Step {
        /// The grant: admission against every bound; a refused transaction
        /// holds nothing.
        Admit(Txid),
        /// The in-memory decision publishes the staged effects.
        Publish(Txid),
        /// The decision record is durable on the coordinator.
        Decide(Txid),
        /// A plain write outside any transaction.
        Plain(Key, Effect),
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub enum Refusal {
        /// D5: the staged bytes exceed one frame.
        Frame,
        /// S22: the pending-decision cap.
        Retention,
        /// A10: the pinned budgets (`INF TXRETAIN`).
        Retain,
    }

    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct Stats {
        pub committed: usize,
        pub refused: BTreeMap<Refusal, usize>,
        pub max_retained: u64,
        pub max_staged: u64,
    }

    /// One key's pending run: the qualified image and every txid it waits
    /// for (A2).
    #[derive(Clone, Debug)]
    struct Run {
        base: Image,
        pending: BTreeSet<Txid>,
    }

    #[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
    struct Charge {
        hot: u64,
        extent: u64,
    }

    struct Model<'a> {
        rules: Rules,
        budget: Budget,
        txns: &'a [Txn],
        images: Vec<Image>,
        runs: BTreeMap<Key, Run>,
        /// Granted, not yet published: the intents held and, per written
        /// key, the live image's bytes reserved at the grant.
        admitted: BTreeMap<Txid, Vec<(Key, Charge)>>,
        published: BTreeSet<Txid>,
        decided: BTreeSet<Txid>,
        /// `tx_pinned_bytes` / `tx_pinned_extent_bytes` as accounted.
        accounted: Charge,
        stats: Stats,
    }

    /// Runs `steps` over `keys` with the given images; every published
    /// transaction is decided at the end so the leak check is total.
    /// Returns the stats, or the violation.
    pub fn run(
        rules: Rules,
        budget: Budget,
        images: &[Image],
        txns: &[Txn],
        steps: &[Step],
    ) -> Result<Stats, String> {
        let mut m = Model::new(rules, budget, images, txns);
        for step in steps {
            m.apply(*step);
            m.check()?;
        }
        for t in m.admitted.keys().copied().collect::<Vec<_>>() {
            m.apply(Step::Publish(t));
            m.check()?;
        }
        for t in m.published.clone() {
            m.apply(Step::Decide(t));
            m.check()?;
        }
        m.finish()
    }

    impl<'a> Model<'a> {
        fn new(rules: Rules, budget: Budget, images: &[Image], txns: &'a [Txn]) -> Model<'a> {
            Model {
                rules,
                budget,
                txns,
                images: images.to_vec(),
                runs: BTreeMap::new(),
                admitted: BTreeMap::new(),
                published: BTreeSet::new(),
                decided: BTreeSet::new(),
                accounted: Charge::default(),
                stats: Stats::default(),
            }
        }

        fn txn(&self, t: Txid) -> &'a Txn {
            self.txns.iter().find(|x| x.id == t).expect("known txid")
        }

        /// A W intent on `k` is held by an admitted, unpublished transaction.
        fn held(&self, k: Key) -> bool {
            self.admitted.keys().any(|t| self.txn(*t).writes.iter().any(|(w, _)| *w == k))
        }

        fn pending_decisions(&self) -> usize {
            self.published.difference(&self.decided).count()
        }

        /// What the grant reserves: the live image of every written key,
        /// each key once. A key already pending returns it at publication
        /// (A2: its qualified image is already pinned) unless its run was
        /// released under the intent, in which case this is the new pin.
        fn charge_of(&self, txn: &Txn) -> Vec<(Key, Charge)> {
            let keys: BTreeSet<Key> = txn.writes.iter().map(|(k, _)| *k).collect();
            keys.iter()
                .map(|k| {
                    (*k, Charge { hot: self.images[*k].hot(), extent: self.images[*k].extent() })
                })
                .collect()
        }

        fn total(charges: &[(Key, Charge)]) -> Charge {
            charges.iter().fold(Charge::default(), |c, (_, x)| Charge {
                hot: c.hot + x.hot,
                extent: c.extent + x.extent,
            })
        }

        fn refusal(&self, txn: &Txn, charge: Charge) -> Option<Refusal> {
            let staged: u64 = txn.writes.iter().map(|(_, e)| e.staged()).sum();
            if staged > self.budget.frame_max {
                return Some(Refusal::Frame);
            }
            if self.pending_decisions() + self.admitted.len() >= self.budget.retention_cap {
                return Some(Refusal::Retention);
            }
            let over_hot = self.accounted.hot + charge.hot > self.budget.pinned_max;
            let over_extent = self.accounted.extent + charge.extent > self.budget.pinned_extent_max;
            if self.rules.charge_retained_images_at_grant && (over_hot || over_extent) {
                return Some(Refusal::Retain);
            }
            None
        }

        fn apply(&mut self, step: Step) {
            match step {
                Step::Admit(t) => self.admit(t),
                Step::Publish(t) => self.publish(t),
                Step::Decide(t) => self.decide(t),
                Step::Plain(k, effect) => {
                    if !self.held(k) {
                        // A dependent write keeps the qualified image (A2);
                        // an independent one simply replaces its predecessor.
                        self.images[k] = effect.apply();
                    }
                }
            }
        }

        fn admit(&mut self, t: Txid) {
            if self.admitted.contains_key(&t) || self.published.contains(&t) {
                return;
            }
            let txn = self.txn(t);
            if txn.writes.iter().any(|(k, _)| self.held(*k)) {
                return;
            }
            let charges = self.charge_of(txn);
            let charge = Self::total(&charges);
            if let Some(why) = self.refusal(txn, charge) {
                *self.stats.refused.entry(why).or_default() += 1;
                return;
            }
            let staged: u64 = txn.writes.iter().map(|(_, e)| e.staged()).sum();
            self.stats.max_staged = self.stats.max_staged.max(staged);
            if self.rules.charge_retained_images_at_grant {
                self.accounted.hot += charge.hot;
                self.accounted.extent += charge.extent;
            }
            self.admitted.insert(t, charges);
        }

        /// The reservation becomes the pin where the key was not pending
        /// (the image cannot have changed under the intent) and returns
        /// where it was.
        fn publish(&mut self, t: Txid) {
            let Some(charges) = self.admitted.remove(&t) else { return };
            let txn = self.txn(t);
            for (k, charge) in charges {
                if self.runs.contains_key(&k) && self.rules.charge_retained_images_at_grant {
                    self.accounted.hot -= charge.hot;
                    self.accounted.extent -= charge.extent;
                }
                let old = self.images[k];
                self.runs
                    .entry(k)
                    .or_insert(Run { base: old, pending: BTreeSet::new() })
                    .pending
                    .insert(t);
            }
            for (k, effect) in &txn.writes {
                self.images[*k] = effect.apply();
            }
            self.published.insert(t);
            self.stats.committed += 1;
        }

        /// A durable decision releases every pin whose pending set is
        /// decided, and its bytes.
        fn decide(&mut self, t: Txid) {
            if !self.published.contains(&t) {
                return;
            }
            self.decided.insert(t);
            let decided = self.decided.clone();
            let released: Vec<Key> = self
                .runs
                .iter()
                .filter(|(_, r)| r.pending.is_subset(&decided))
                .map(|(k, _)| *k)
                .collect();
            for k in released {
                let run = self.runs.remove(&k).expect("released run");
                if self.rules.charge_retained_images_at_grant {
                    self.accounted.hot -= run.base.hot();
                    self.accounted.extent -= run.base.extent();
                }
            }
        }

        /// What the cell really retains: the qualified image of every
        /// pending key.
        fn retained(&self) -> Charge {
            self.runs.values().fold(Charge::default(), |c, r| Charge {
                hot: c.hot + r.base.hot(),
                extent: c.extent + r.base.extent(),
            })
        }

        /// Charges reserved at the grant and not yet converted or returned.
        fn reserved(&self) -> Charge {
            self.admitted.values().fold(Charge::default(), |c, charges| {
                let x = Self::total(charges);
                Charge { hot: c.hot + x.hot, extent: c.extent + x.extent }
            })
        }

        fn check(&mut self) -> Result<(), String> {
            let retained = self.retained();
            self.stats.max_retained = self.stats.max_retained.max(retained.hot + retained.extent);
            if !self.rules.charge_retained_images_at_grant {
                let claimed = self.budget.retention_cap as u64 * self.budget.frame_max;
                if retained.hot + retained.extent > claimed {
                    return Err(format!(
                        "RETAINED BYTES EXCEED THE CLAIMED BOUND: {} pending transaction(s) staged at most {} B each but retain {} B of images and {} B of extents — above the retention cap × frame bound of {claimed} B",
                        self.pending_decisions(),
                        self.stats.max_staged,
                        retained.hot,
                        retained.extent
                    ));
                }
                return Ok(());
            }
            let reserved = self.reserved();
            let held = Charge {
                hot: retained.hot + reserved.hot,
                extent: retained.extent + reserved.extent,
            };
            if self.accounted != held {
                return Err(format!(
                    "PIN ACCOUNTING DRIFT: tx_pinned_bytes {} / tx_pinned_extent_bytes {} but the cell retains {} / {} and reserves {} / {}",
                    self.accounted.hot,
                    self.accounted.extent,
                    retained.hot,
                    retained.extent,
                    reserved.hot,
                    reserved.extent
                ));
            }
            if held.hot > self.budget.pinned_max || held.extent > self.budget.pinned_extent_max {
                return Err(format!(
                    "PINNED BYTES OVER BUDGET: {} B of images (budget {}) and {} B of extents (budget {})",
                    held.hot, self.budget.pinned_max, held.extent, self.budget.pinned_extent_max
                ));
            }
            Ok(())
        }

        fn finish(self) -> Result<Stats, String> {
            let retained = self.retained();
            if retained != Charge::default()
                || (self.rules.charge_retained_images_at_grant
                    && self.accounted != Charge::default())
            {
                return Err(format!(
                    "PIN LEAK: {} B of images and {} B of extents (accounted {} / {}) retained after every decision is durable",
                    retained.hot, retained.extent, self.accounted.hot, self.accounted.extent
                ));
            }
            Ok(self.stats)
        }
    }

    pub const MIB: u64 = 1 << 20;

    /// The review's budget: 64 B frames, four pending decisions, 2 MiB of
    /// pinned images, 4 MiB of pinned extents.
    pub fn budget() -> Budget {
        Budget { frame_max: 64, retention_cap: 4, pinned_max: 2 * MIB, pinned_extent_max: 4 * MIB }
    }

    /// F08: four 1 MiB records, four transactions each deleting one with a
    /// 24 B tombstone, no decision durable yet.
    pub fn tombstone_storm() -> (Vec<Image>, Vec<Txn>, Vec<Step>) {
        let images = vec![Image::Hot(MIB); 4];
        let txns: Vec<Txn> =
            (0..4).map(|k| Txn { id: k as Txid + 1, writes: vec![(k, Effect::Delete)] }).collect();
        let mut steps = Vec::new();
        for t in 1..=4 {
            steps.push(Step::Admit(t));
            steps.push(Step::Publish(t));
        }
        (images, txns, steps)
    }

    /// The same storm, then the decisions land and the refused
    /// transactions are offered again.
    pub fn tombstone_storm_then_decisions() -> (Vec<Image>, Vec<Txn>, Vec<Step>) {
        let (images, txns, mut steps) = tombstone_storm();
        steps.extend([Step::Decide(1), Step::Decide(2)]);
        for t in 3..=4 {
            steps.push(Step::Admit(t));
            steps.push(Step::Publish(t));
        }
        (images, txns, steps)
    }

    /// A cold document's extent pinned by a tombstone; a dependent plain
    /// write and a second pending write over the same key pin nothing new.
    pub fn cold_extent_and_dependents() -> (Vec<Image>, Vec<Txn>, Vec<Step>) {
        let images = vec![Image::Cold(3 * MIB), Image::Hot(MIB)];
        let txns = vec![
            Txn { id: 1, writes: vec![(0, Effect::Delete), (1, Effect::Set(8))] },
            Txn { id: 2, writes: vec![(1, Effect::Set(16))] },
        ];
        let steps = vec![
            Step::Admit(1),
            Step::Publish(1),
            Step::Plain(1, Effect::Set(32)),
            Step::Admit(2),
            Step::Publish(2),
            Step::Decide(1),
            Step::Decide(2),
        ];
        (images, txns, steps)
    }

    /// Seeded keys, images, transactions and interleavings.
    pub fn random(seed: u64) -> (Vec<Image>, Vec<Txn>, Vec<Step>) {
        let mut rng = Rng::new(seed);
        let keys = 4;
        let images: Vec<Image> = (0..keys)
            .map(|_| match rng.below(3) {
                0 => Image::Absent,
                1 => Image::Hot(1 + rng.next_u64() % MIB),
                _ => Image::Cold(1 + rng.next_u64() % (2 * MIB)),
            })
            .collect();
        let effect = |rng: &mut Rng| {
            if rng.below(2) == 0 { Effect::Delete } else { Effect::Set(rng.below(40) as u64) }
        };
        let txns: Vec<Txn> = (1..=6)
            .map(|id| {
                let mut writes = vec![(rng.below(keys), effect(&mut rng))];
                let k = rng.below(keys);
                if writes[0].0 != k {
                    writes.push((k, effect(&mut rng)));
                }
                Txn { id, writes }
            })
            .collect();
        let steps = (0..24)
            .map(|_| match rng.below(4) {
                0 => Step::Admit(1 + rng.below(6) as Txid),
                1 => Step::Publish(1 + rng.below(6) as Txid),
                2 => Step::Decide(1 + rng.below(6) as Txid),
                _ => Step::Plain(rng.below(keys), effect(&mut rng)),
            })
            .collect();
        (images, txns, steps)
    }
}

#[cfg(test)]
mod tests {
    use super::Variant;
    use super::acquisition::{
        self, Acquisition, Cancel, CancelPhase, CancelUntil, Cmd, Dependent, Outcome, Reply, Rules,
    };
    use super::credits;
    use super::durable;
    use super::identity;
    use super::lineage;
    use super::retention;
    use super::revision;
    use super::watch::{self, Event, RepeatedWatch, Verdict};

    fn rules() -> Rules {
        match Variant::from_env() {
            Variant::Chosen => Rules::chosen(),
            Variant::Withdrawn => Rules::withdrawn(),
        }
    }

    fn reschedule(max_retries: u32) -> Rules {
        Rules { acquisition: Acquisition::Reschedule { max_retries }, ..Rules::chosen() }
    }

    // ---- acquisition ----

    #[test]
    fn two_key_history_completes_without_deadlock() {
        let r = acquisition::two_key_history(rules());
        assert!(r.violations.is_empty(), "{}", r.violations.join("\n"));
        assert_eq!(r.committed, 2);
    }

    #[test]
    fn withdrawn_rule_deadlocks_on_the_two_key_history() {
        let r = acquisition::two_key_history(Rules::withdrawn());
        assert_eq!(r.stuck.len(), 2, "{r:?}");
        assert_eq!(
            r.violations[0],
            "ACQUISITION DEADLOCK: 2 transaction(s) stuck with no message in flight — T1 waits on key 1@cell1 behind T2; T2 waits on key 0@cell0 behind T1"
        );
    }

    #[test]
    fn reschedule_rule_completes_the_two_key_history_with_a_counted_retry() {
        let r = acquisition::two_key_history(reschedule(8));
        assert!(r.violations.is_empty(), "{}", r.violations.join("\n"));
        assert_eq!(r.committed, 2);
        assert!(r.retries >= 1, "{r:?}");
    }

    #[test]
    fn storms_never_deadlock_and_reads_are_serializable() {
        let rules = rules();
        for seed in 1..=64u64 {
            let r = acquisition::storm(rules, 4, 6, 24, seed);
            assert!(r.violations.is_empty(), "seed {seed}: {}", r.violations.join("\n"));
            assert_eq!(r.committed + r.aborted, 24, "seed {seed}: {r:?}");
        }
    }

    #[test]
    fn withdrawn_rule_deadlocks_some_storm_within_64_seeds() {
        let deadlocked = (1..=64u64)
            .filter(|seed| {
                !acquisition::storm(Rules::withdrawn(), 4, 6, 24, *seed).stuck.is_empty()
            })
            .count();
        assert!(deadlocked > 0, "the withdrawn rule survived 64 storms — the model lost its teeth");
    }

    #[test]
    fn reschedule_retries_scale_with_contention_and_canonical_has_none() {
        let hot: u32 =
            (1..=32u64).map(|s| acquisition::storm(reschedule(64), 4, 2, 24, s).retries).sum();
        let cold: u32 =
            (1..=32u64).map(|s| acquisition::storm(reschedule(64), 4, 64, 24, s).retries).sum();
        let canonical: u32 =
            (1..=32u64).map(|s| acquisition::storm(Rules::chosen(), 4, 2, 24, s).retries).sum();
        eprintln!(
            "reschedule retries over 32 storms of 24 txns on 4 cells: hot (2 keys) {hot}, cold (64 keys) {cold}; canonical hot {canonical}"
        );
        assert_eq!(canonical, 0);
        assert!(hot > cold, "hot {hot} retries vs cold {cold}");
        for seed in 1..=32u64 {
            let r = acquisition::storm(reschedule(64), 4, 2, 24, seed);
            assert!(r.violations.is_empty(), "seed {seed}: {}", r.violations.join("\n"));
        }
    }

    #[test]
    fn readers_never_observe_an_uncommitted_write() {
        let rules = rules();
        for seed in 1..=64u64 {
            let r = acquisition::read_pair_history(rules, seed);
            assert!(r.violations.is_empty(), "seed {seed}: {}", r.violations.join("\n"));
        }
    }

    #[test]
    fn withdrawn_read_bypass_observes_an_uncommitted_write_within_64_seeds() {
        let hits = (1..=64u64)
            .filter(|s| {
                acquisition::read_pair_history(Rules::withdrawn(), *s)
                    .violations
                    .iter()
                    .any(|v| v.starts_with("READ UNCOMMITTED"))
            })
            .count();
        assert!(hits > 0);
    }

    #[test]
    fn a_watch_only_key_is_held_through_the_decision() {
        let r = acquisition::watch_interval_history(rules());
        assert!(r.violations.is_empty(), "{}", r.violations.join("\n"));
    }

    #[test]
    fn withdrawn_validate_and_release_commits_over_a_mutated_watch() {
        let r = acquisition::watch_interval_history(Rules {
            hold_watch_intents: false,
            ..Rules::chosen()
        });
        assert_eq!(
            r.violations,
            vec![
                "WATCH INTERVAL VIOLATION: T1 committed after key 1 was mutated between the owner's validation and the decision".to_string()
            ]
        );
    }

    #[test]
    fn every_terminal_path_releases_every_intent_and_every_staged_set() {
        let leak = |v: &String| v.starts_with("INTENT LEAK") || v.starts_with("STAGING LEAK");
        for seed in 1..=32u64 {
            for faults in [false, true] {
                let r = acquisition::storm_with(Rules::chosen(), 3, 4, 16, seed, faults);
                assert!(!r.violations.iter().any(leak), "seed {seed}: {r:?}");
                let r = acquisition::storm_with(reschedule(2), 3, 4, 16, seed, faults);
                assert!(!r.violations.iter().any(leak), "seed {seed}: {r:?}");
            }
        }
    }

    #[test]
    fn outcome_is_typed() {
        let mut m = acquisition::Model::new(Rules::chosen(), 1, 1);
        let t = m.submit(acquisition::TxnSpec {
            coordinator: 0,
            writes: vec![0],
            ..Default::default()
        });
        m.run(100);
        assert_eq!(m.outcome(t), Outcome::Committed);
    }

    // ---- staging, decision functions, cancellation (ADR-0116 A4) ----

    #[test]
    fn a_failed_native_condition_publishes_nothing() {
        let (r, outcome, published) = acquisition::native_failure_history(rules());
        assert!(r.violations.is_empty(), "{}", r.violations.join("\n"));
        assert_eq!(outcome, Outcome::Aborted("condition failed"));
        assert_eq!(published, None);
    }

    #[test]
    fn withdrawn_live_writes_survive_a_failed_native_condition() {
        let (r, outcome, published) = acquisition::native_failure_history(Rules {
            stage_privately: false,
            ..Rules::chosen()
        });
        assert_eq!(outcome, Outcome::Aborted("condition failed"));
        assert_eq!(published, Some(0));
        assert_eq!(
            r.violations,
            vec![
                "STAGING VIOLATION: T1 aborted (condition failed) but its write to key 0@cell0 is published"
                    .to_string()
            ]
        );
    }

    #[test]
    fn a_failed_exec_command_is_embedded_and_the_rest_commits() {
        let (r, outcome, published) = acquisition::exec_failure_history(rules());
        assert!(r.violations.is_empty(), "{}", r.violations.join("\n"));
        assert_eq!(outcome, Outcome::Committed);
        assert_eq!(published, [Some(0), None]);
    }

    #[test]
    fn cancellation_aborts_before_the_decision_and_completes_after_it() {
        let rules = rules();
        for phase in acquisition::PHASES {
            for kind in [Cancel::Disconnect, Cancel::Timeout] {
                for seed in 1..=16u64 {
                    let (r, outcome, published) =
                        acquisition::cancellation_history(rules, phase, kind, seed);
                    assert!(
                        r.violations.is_empty(),
                        "{phase:?} {kind:?} seed {seed}: {}",
                        r.violations.join("\n")
                    );
                    if acquisition::before_decision(phase) {
                        let reason = match kind {
                            Cancel::Disconnect => "disconnect",
                            Cancel::Timeout => "timeout",
                        };
                        assert_eq!(outcome, Outcome::Aborted(reason), "{phase:?} {kind:?}");
                        assert_eq!(published, [false, false], "{phase:?} {kind:?}");
                    } else {
                        assert_eq!(outcome, Outcome::Committed, "{phase:?} {kind:?}");
                        assert_eq!(published, [true, true], "{phase:?} {kind:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn withdrawn_cancel_until_durable_publishes_half_a_commit_within_64_seeds() {
        let rules = Rules { cancel_until: CancelUntil::DurableDecision, ..Rules::chosen() };
        let mut partial = Vec::new();
        let mut false_abort = 0;
        for seed in 1..=64u64 {
            let (r, _, _) = acquisition::cancellation_history(
                rules,
                CancelPhase::Decided,
                Cancel::Disconnect,
                seed,
            );
            partial.extend(r.violations.iter().filter(|v| v.starts_with("PARTIAL")).cloned());
            false_abort += usize::from(r.violations.iter().any(|v| v.starts_with("FALSE ABORT")));
        }
        assert!(
            !partial.is_empty(),
            "no partial publication in 64 seeds — the model lost its teeth"
        );
        eprintln!(
            "cancel-until-durable: {} partial, {false_abort} false aborts over 64 seeds",
            partial.len()
        );
        assert_eq!(
            partial[0],
            "PARTIAL PUBLICATION: T1 (disconnect after the decision) published on cell(s) [0] and discarded on cell(s) [1]"
        );
        assert!(false_abort > 0, "no false abort in 64 seeds");
    }

    #[test]
    fn withdrawn_live_writes_outlive_a_cancel_between_two_legs() {
        let rules = Rules { stage_privately: false, ..Rules::chosen() };
        let (r, outcome, _) = acquisition::cancellation_history(
            rules,
            CancelPhase::Executed(0, 1),
            Cancel::Timeout,
            3,
        );
        assert_eq!(outcome, Outcome::Aborted("timeout"));
        assert_eq!(r.violations.len(), 1, "{r:?}");
        assert!(
            r.violations[0]
                .starts_with("STAGING VIOLATION: T1 aborted (timeout) but its write to key"),
            "{}",
            r.violations[0]
        );
    }

    #[test]
    fn fault_storms_publish_all_or_nothing_and_leak_nothing() {
        let rules = rules();
        for seed in 1..=64u64 {
            let r = acquisition::storm_with(rules, 4, 6, 24, seed, true);
            assert!(r.violations.is_empty(), "seed {seed}: {}", r.violations.join("\n"));
            assert_eq!(r.committed + r.aborted, 24, "seed {seed}: {r:?}");
        }
    }

    #[test]
    fn withdrawn_rules_violate_some_fault_storm_within_256_seeds() {
        let staging = Rules { stage_privately: false, ..Rules::chosen() };
        let cancel = Rules { cancel_until: CancelUntil::DurableDecision, ..Rules::chosen() };
        let hits = |rules: Rules, prefix: &str, seeds: u64| {
            (1..=seeds)
                .filter(|s| {
                    acquisition::storm_with(rules, 4, 6, 24, *s, true)
                        .violations
                        .iter()
                        .any(|v| v.starts_with(prefix))
                })
                .count()
        };
        let staging_hits = hits(staging, "STAGING VIOLATION", 64);
        // A cancellation planted at `Decided` reaches the half-published
        // interleaving in roughly one storm in fifty (the CancelAt must
        // land between two owners' unlocks) — 256 seeds for that arm.
        let partial_hits = hits(cancel, "PARTIAL PUBLICATION", 256);
        let false_aborts = hits(cancel, "FALSE ABORT", 256);
        eprintln!(
            "fault storms: live-write {staging_hits}/64, cancel-until-durable partial {partial_hits}/256, false abort {false_aborts}/256"
        );
        assert!(staging_hits > 0 && partial_hits > 0 && false_aborts > 0);
    }

    // ---- dependent multi-owner commands (ADR-0116 A6) ----

    #[test]
    fn a_cross_owner_rename_ships_the_value_the_transaction_staged() {
        for native in [false, true] {
            let (r, outcome, published) = acquisition::rename_history(rules(), native, false);
            assert!(r.violations.is_empty(), "native {native}: {}", r.violations.join("\n"));
            assert_eq!(outcome, Outcome::Committed, "native {native}");
            assert_eq!(published, [None, Some(0)], "native {native}: a absent, b = T's value");
        }
    }

    #[test]
    fn a_refused_destination_never_strands_the_source() {
        let (r, outcome, published) = acquisition::rename_history(rules(), false, true);
        assert!(r.violations.is_empty(), "{}", r.violations.join("\n"));
        assert_eq!(outcome, Outcome::Committed, "EXEC embeds the refusal");
        assert_eq!(published, [Some(0), None], "the source keeps the SET; nothing at b");
        let (r, outcome, published) = acquisition::rename_history(rules(), true, true);
        assert!(r.violations.is_empty(), "{}", r.violations.join("\n"));
        assert_eq!(outcome, Outcome::Aborted("destination refused"));
        assert_eq!(published, [None, None]);
    }

    #[test]
    fn withdrawn_independent_legs_rename_from_the_pre_transaction_value() {
        let rules = Rules { stage_dependents: false, ..Rules::chosen() };
        let (r, outcome, published) = acquisition::rename_history(rules, false, false);
        assert_eq!(outcome, Outcome::Committed);
        assert_eq!(published, [None, None], "the source is deleted, the destination never set");
        assert_eq!(
            r.violations,
            vec![
                "REPLY MISMATCH: T1's RENAME 0→1 replied no such key where the serial history replies OK"
                    .to_string(),
                "DEPENDENT COMMAND: T1's RENAME 0→1 left key 1@cell1 = absent where the serial history gives T1"
                    .to_string(),
            ]
        );
        let (r, outcome, _) = acquisition::rename_history(rules, true, false);
        assert_eq!(outcome, Outcome::Aborted("no such key"));
        assert_eq!(
            r.violations,
            vec![
                "DEPENDENT LEG: T1 aborted (no such key) but the serial history commits its RENAME 0→1"
                    .to_string()
            ]
        );
        let (r, _, published) = acquisition::rename_history(rules, false, true);
        assert_eq!(published, [None, None], "the refused destination lost the source");
        assert_eq!(
            r.violations,
            vec![
                "DEPENDENT COMMAND: T1's RENAME 0→1 left key 0@cell0 = absent where the serial history gives T1"
                    .to_string()
            ]
        );
    }

    #[test]
    fn a_cross_owner_msetnx_is_one_condition_over_every_owner() {
        for native in [false, true] {
            let (r, outcome, published) = acquisition::msetnx_history(rules(), native);
            assert!(r.violations.is_empty(), "native {native}: {}", r.violations.join("\n"));
            let want =
                if native { Outcome::Aborted("condition failed") } else { Outcome::Committed };
            assert_eq!(outcome, want, "native {native}");
            assert_eq!(published, [None, Some(0)], "native {native}: nothing set");
        }
    }

    #[test]
    fn withdrawn_independent_legs_publish_half_an_msetnx() {
        let rules = Rules { stage_dependents: false, ..Rules::chosen() };
        let (r, outcome, published) = acquisition::msetnx_history(rules, false);
        assert_eq!(outcome, Outcome::Committed);
        assert_eq!(published, [Some(1), Some(0)], "cell 0 set its key; cell 1 refused");
        assert_eq!(
            r.violations,
            vec![
                "DEPENDENT COMMAND: T2's MSETNX [0, 1] left key 0@cell0 = T2 where the serial history gives absent"
                    .to_string()
            ]
        );
    }

    #[test]
    fn dependent_stages_abort_at_the_combine_step_and_complete_after_the_decision() {
        let rules = rules();
        for phase in acquisition::DEP_PHASES {
            for kind in [Cancel::Disconnect, Cancel::Timeout] {
                for seed in 1..=16u64 {
                    let (r, outcome, moved) =
                        acquisition::dependent_cancellation_history(rules, phase, kind, seed);
                    assert!(
                        r.violations.is_empty(),
                        "{phase:?} {kind:?} seed {seed}: {}",
                        r.violations.join("\n")
                    );
                    if acquisition::before_decision(phase) {
                        assert!(
                            matches!(outcome, Outcome::Aborted(_)),
                            "{phase:?} {kind:?}: {outcome:?}"
                        );
                        assert!(!moved, "{phase:?} {kind:?}: published after an abort");
                    } else {
                        assert_eq!(outcome, Outcome::Committed, "{phase:?} {kind:?}");
                        assert!(moved, "{phase:?} {kind:?}: the move did not complete");
                    }
                }
            }
        }
    }

    #[test]
    fn withdrawn_independent_legs_violate_some_fault_storm_within_64_seeds() {
        let rules = Rules { stage_dependents: false, ..Rules::chosen() };
        let hits = (1..=64u64)
            .filter(|s| {
                acquisition::storm_with(rules, 4, 6, 24, *s, true)
                    .violations
                    .iter()
                    .any(|v| v.starts_with("DEPENDENT"))
            })
            .count();
        eprintln!("fault storms: independent-legs {hits}/64");
        assert!(hits > 0, "no dependent-command violation in 64 storms — the model lost its teeth");
    }

    #[test]
    fn a_dependent_command_never_spans_one_owner() {
        let mut m = acquisition::Model::new(Rules::chosen(), 2, 1);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            m.submit(acquisition::TxnSpec {
                coordinator: 0,
                dependent: Some(Dependent::Move { src: 0, dst: 2 }),
                ..Default::default()
            })
        }));
        assert!(r.is_err(), "keys 0 and 2 share cell 0: not a cross-owner command");
    }

    // ---- ordered programs: replies in queue order, read-your-writes
    // across a dependent command (the fix validation's F04 follow-up) ----

    #[test]
    fn the_ordered_rename_history_replies_in_queue_order_and_reads_its_own_move() {
        for native in [false, true] {
            let (r, outcome, reply, published) =
                acquisition::ordered_rename_history(rules(), native);
            assert!(r.violations.is_empty(), "native {native}: {}", r.violations.join("\n"));
            assert_eq!(outcome, Outcome::Committed, "native {native}");
            assert_eq!(
                reply,
                Some(vec![Reply::Ok, Reply::Ok, Reply::Value(Some(0))]),
                "native {native}: SET, RENAME, then GET b = the moved value"
            );
            assert_eq!(published, [None, Some(0)], "native {native}");
        }
    }

    #[test]
    fn withdrawn_independent_legs_answer_the_ordered_rename_from_published_state() {
        let rules = Rules { stage_dependents: false, ..Rules::chosen() };
        let (r, outcome, reply, published) = acquisition::ordered_rename_history(rules, false);
        assert_eq!(outcome, Outcome::Committed);
        assert_eq!(
            reply,
            Some(vec![Reply::Ok, Reply::Err("no such key"), Reply::Value(None)]),
            "the destination never saw the staged value and the GET read the pre-state"
        );
        assert_eq!(published, [None, None]);
        assert_eq!(
            r.violations,
            vec![
                "REPLY MISMATCH: T1's RENAME 0→1 replied no such key where the serial history replies OK"
                    .to_string(),
                "REPLY MISMATCH: T1's GET 1 replied absent where the serial history replies T1"
                    .to_string(),
                "DEPENDENT COMMAND: T1's RENAME 0→1 left key 1@cell1 = absent where the serial history gives T1"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn withdrawn_published_reads_miss_the_transactions_own_writes() {
        let rules = Rules { read_your_writes: false, ..Rules::chosen() };
        let (r, outcome, reply, published) = acquisition::ordered_rename_history(rules, false);
        assert_eq!(outcome, Outcome::Committed);
        assert_eq!(published, [None, Some(0)], "the state is right — only the reply is wrong");
        assert_eq!(reply, Some(vec![Reply::Ok, Reply::Ok, Reply::Value(None)]));
        assert_eq!(
            r.violations,
            vec![
                "REPLY MISMATCH: T1's GET 1 replied absent where the serial history replies T1"
                    .to_string()
            ]
        );
        let (r, _, reply, _) = acquisition::reply_order_history(rules, false);
        assert_eq!(
            reply,
            Some(vec![Reply::Ok, Reply::Ok, Reply::Value(None), Reply::Value(None)]),
            "a GET after the transaction's own SET answers the pre-state"
        );
        assert_eq!(r.violations.len(), 2, "{}", r.violations.join("\n"));
    }

    #[test]
    fn consecutive_dependent_commands_each_see_the_previous_apply() {
        for native in [false, true] {
            let (r, outcome, reply, published) =
                acquisition::consecutive_dependents_history(rules(), native);
            assert!(r.violations.is_empty(), "native {native}: {}", r.violations.join("\n"));
            assert_eq!(outcome, Outcome::Committed, "native {native}");
            assert_eq!(
                reply,
                Some(vec![Reply::Int(1), Reply::Ok, Reply::Value(Some(0)), Reply::Value(None)]),
                "native {native}: MSETNX set both, RENAME moved b to c, GET c = T, GET b absent"
            );
            assert_eq!(published, [Some(0), None, Some(0)], "native {native}");
        }
    }

    #[test]
    fn withdrawn_independent_legs_break_consecutive_dependent_commands() {
        let rules = Rules { stage_dependents: false, ..Rules::chosen() };
        let (r, outcome, reply, published) =
            acquisition::consecutive_dependents_history(rules, false);
        assert_eq!(outcome, Outcome::Committed);
        assert_eq!(
            reply,
            Some(vec![
                Reply::Int(1),
                Reply::Err("no such key"),
                Reply::Value(None),
                Reply::Value(None)
            ]),
            "the RENAME read b as published (absent) and both GETs answered the pre-state"
        );
        assert_eq!(published, [Some(0), None, None], "c never received the value");
        assert!(
            r.violations.iter().any(|v| v.starts_with("REPLY MISMATCH: T1's RENAME 1→2"))
                && r.violations.iter().any(|v| v.starts_with("REPLY MISMATCH: T1's GET 2"))
                && r.violations
                    .iter()
                    .any(|v| v.starts_with("DEPENDENT COMMAND: T1's RENAME 1→2 left key 2@cell2")),
            "{}",
            r.violations.join("\n")
        );
    }

    #[test]
    fn a_refused_command_followed_by_reads_embeds_under_exec_and_aborts_under_native() {
        let (r, outcome, reply, published) = acquisition::refused_then_read_history(rules(), false);
        assert!(r.violations.is_empty(), "{}", r.violations.join("\n"));
        assert_eq!(outcome, Outcome::Committed);
        assert_eq!(
            reply,
            Some(vec![
                Reply::Ok,
                Reply::Err("destination refused"),
                Reply::Value(Some(0)),
                Reply::Value(None)
            ]),
            "the SET is kept, the refusal embedded, the reads see exactly that"
        );
        assert_eq!(published, [Some(0), None]);
        let (r, outcome, reply, published) = acquisition::refused_then_read_history(rules(), true);
        assert!(r.violations.is_empty(), "{}", r.violations.join("\n"));
        assert_eq!(outcome, Outcome::Aborted("destination refused"));
        assert_eq!(reply, None, "an aborted INF.TX has no reply array");
        assert_eq!(published, [None, None]);
    }

    #[test]
    fn replies_come_back_in_queue_order_whatever_the_legs_arrival_order() {
        let (r, outcome, reply, published) = acquisition::reply_order_history(rules(), false);
        assert!(r.violations.is_empty(), "{}", r.violations.join("\n"));
        assert_eq!(outcome, Outcome::Committed);
        assert_eq!(
            reply,
            Some(vec![Reply::Ok, Reply::Ok, Reply::Value(Some(0)), Reply::Value(Some(0))])
        );
        assert_eq!(published, [Some(0), Some(0)]);
        let (r, outcome, reply, published) = acquisition::reply_order_history(rules(), true);
        assert!(r.violations.is_empty(), "{}", r.violations.join("\n"));
        assert_eq!(outcome, Outcome::Committed, "EXEC embeds the failed condition");
        assert_eq!(
            reply,
            Some(vec![
                Reply::Ok,
                Reply::Err("condition failed"),
                Reply::Value(None),
                Reply::Value(Some(0))
            ]),
            "the failed SET staged nothing, so GET b is absent and GET a is the SET"
        );
        assert_eq!(published, [Some(0), None]);
    }

    #[test]
    fn a_stage_compiles_monotonically_in_queue_order() {
        let program = vec![
            Cmd::Get(0),
            Cmd::Dep(Dependent::Move { src: 0, dst: 1 }),
            Cmd::Set(1),
            Cmd::Dep(Dependent::SetIfNoneExist { keys: vec![2, 3] }),
            Cmd::Get(3),
        ];
        assert_eq!(acquisition::stages_of(&program), vec![(0, true), (1, true), (2, false)]);
        assert_eq!(acquisition::stages_of(&[Cmd::Set(0)]), vec![(0, false)]);
        assert_eq!(acquisition::stages_of(&[]), Vec::<(usize, bool)>::new());
    }

    // ---- watch ----

    fn watch_rules() -> watch::Rules {
        match Variant::from_env() {
            Variant::Chosen => watch::Rules::chosen(),
            Variant::Withdrawn => watch::Rules::withdrawn(),
        }
    }

    /// The chosen registration with the batch-22 repeated-WATCH rule.
    fn reregister() -> watch::Rules {
        watch::Rules { repeated_watch: RepeatedWatch::Reregister, ..watch::Rules::chosen() }
    }

    #[test]
    fn absent_present_absent_aborts() {
        let (verdict, oracle) = watch::run(watch_rules(), &watch::ABSENT_PRESENT_ABSENT);
        assert_eq!(oracle, Verdict::Abort);
        assert_eq!(
            verdict,
            oracle,
            "WATCH HISTORY VIOLATION: absent → present → absent accepted by {:?}",
            watch_rules()
        );
    }

    #[test]
    fn withdrawn_endpoint_equality_accepts_absent_present_absent() {
        let (verdict, oracle) =
            watch::run(watch::Rules::withdrawn(), &watch::ABSENT_PRESENT_ABSENT);
        assert_eq!((verdict, oracle), (Verdict::Commit, Verdict::Abort));
        let (verdict, oracle) = watch::run(watch::Rules::withdrawn(), &watch::DELETE_RECREATE);
        assert_eq!((verdict, oracle), (Verdict::Commit, Verdict::Abort));
    }

    #[test]
    fn delete_recreate_aborts() {
        let (verdict, oracle) = watch::run(watch_rules(), &watch::DELETE_RECREATE);
        assert_eq!(oracle, Verdict::Abort);
        assert_eq!(
            verdict,
            oracle,
            "WATCH HISTORY VIOLATION: delete/recreate accepted by {:?}",
            watch_rules()
        );
    }

    #[test]
    fn eviction_and_owner_restart_fail_closed() {
        for h in [
            [Event::Set(0), Event::Watch(0), Event::Evict(0), Event::Exec],
            [Event::Set(0), Event::Watch(0), Event::Restart, Event::Exec],
        ] {
            let (verdict, oracle) = watch::run(watch_rules(), &h);
            assert_eq!(oracle, Verdict::Abort);
            assert_eq!(
                verdict,
                Verdict::Abort,
                "WATCH HISTORY VIOLATION: {h:?} accepted by {:?}",
                watch_rules()
            );
        }
    }

    // ---- repeated WATCH (ADR-0116 A5, the review's F07) ----

    #[test]
    fn a_repeated_watch_keeps_the_first_registrations_history() {
        for h in [
            &watch::REPEATED_WATCH[..],
            &watch::ADDITIONAL_KEY_AFTER_CHANGE,
            &watch::REPEATED_WATCH_AFTER_EVICTION,
            &watch::REPEATED_WATCH_AFTER_RESTART,
        ] {
            let (verdict, oracle) = watch::run(watch_rules(), h);
            assert_eq!(oracle, Verdict::Abort, "{h:?}");
            assert_eq!(
                verdict,
                Verdict::Abort,
                "WATCH ORACLE RESET: {h:?} accepted by {:?}",
                watch_rules()
            );
        }
        // UNWATCH is the reset; the oracle is not vacuously strict.
        assert_eq!(
            watch::run(watch_rules(), &watch::RESET_THEN_WATCH),
            (Verdict::Commit, Verdict::Commit)
        );
    }

    #[test]
    fn withdrawn_reregistration_launders_the_mutation_between_two_watches() {
        assert_eq!(
            watch::run(reregister(), &watch::REPEATED_WATCH),
            (Verdict::Commit, Verdict::Abort),
            "WATCH ORACLE RESET: a second WATCH must not clear an earlier modification"
        );
        for h in [&watch::REPEATED_WATCH_AFTER_EVICTION, &watch::REPEATED_WATCH_AFTER_RESTART] {
            assert_eq!(watch::run(reregister(), h), (Verdict::Commit, Verdict::Abort), "{h:?}");
        }
        // The per-key rule is not what a second key exercises: the
        // additional-key history aborts under both.
        assert_eq!(
            watch::run(reregister(), &watch::ADDITIONAL_KEY_AFTER_CHANGE),
            (Verdict::Abort, Verdict::Abort)
        );
    }

    #[test]
    fn registration_agrees_with_the_history_oracle_on_random_histories() {
        let rules = watch_rules();
        let mut disagreements = Vec::new();
        for seed in 1..=2000u64 {
            let h = watch::random_history(seed, 7);
            let (verdict, oracle) = watch::run(rules, &h);
            if verdict != oracle {
                disagreements.push(format!("seed {seed} {h:?}: {verdict:?} vs oracle {oracle:?}"));
            }
        }
        assert!(
            disagreements.is_empty(),
            "WATCH HISTORY VIOLATION on {} of 2000 histories under {rules:?}; first: {}",
            disagreements.len(),
            disagreements[0]
        );
    }

    #[test]
    fn withdrawn_watch_rules_disagree_on_random_histories() {
        let count = |rules: watch::Rules| {
            (1..=2000u64)
                .filter(|s| {
                    let (v, o) = watch::run(rules, &watch::random_history(*s, 7));
                    v != o
                })
                .count()
        };
        let withdrawn = count(watch::Rules::withdrawn());
        let reregister = count(reregister());
        eprintln!(
            "watch disagreements over 2000 histories: withdrawn {withdrawn}, reregister-only {reregister}"
        );
        assert!(withdrawn > 0 && reregister > 0);
    }

    /// Redis 8.0.5's own verdicts (`seeds/watch-redis-oracle.txt`,
    /// regenerated by `scripts/txmodel-watch-redis-oracle.py`): the
    /// chosen rules and the sticky-window oracle agree with Redis on
    /// every line; the batch-22 repeated-WATCH rule does not.
    #[test]
    fn chosen_rules_and_oracle_agree_with_redis_on_every_fixture_line() {
        let lines: Vec<(Vec<Event>, Verdict)> = watch::REDIS_FIXTURE
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .map(|l| watch::parse_fixture_line(l).unwrap_or_else(|| panic!("fixture line {l:?}")))
            .collect();
        assert!(lines.len() >= 400, "fixture holds {} histories", lines.len());
        let mut disagreements = Vec::new();
        let mut reregister_disagreements = 0;
        for (h, redis) in &lines {
            let (verdict, oracle) = watch::run(watch::Rules::chosen(), h);
            if verdict != *redis || oracle != *redis {
                disagreements
                    .push(format!("{h:?}: model {verdict:?}, oracle {oracle:?}, Redis {redis:?}"));
            }
            let (verdict, _) = watch::run(reregister(), h);
            reregister_disagreements += usize::from(verdict != *redis);
        }
        assert!(
            disagreements.is_empty(),
            "REDIS ORACLE MISMATCH on {} of {} fixture lines; first: {}",
            disagreements.len(),
            lines.len(),
            disagreements[0]
        );
        eprintln!(
            "redis fixture: {} lines, chosen 0 disagreements, reregister {reregister_disagreements}",
            lines.len()
        );
        assert!(reregister_disagreements > 0, "the fixture does not reach the repeated-WATCH rule");
    }

    // ---- durable ----

    fn durable_rules() -> durable::Rules {
        match Variant::from_env() {
            Variant::Chosen => durable::Rules::chosen(),
            Variant::Withdrawn => durable::Rules::withdrawn(),
        }
    }

    #[test]
    fn everysec_checkpoint_never_holds_half_a_transaction() {
        let r = durable::run(durable_rules(), &durable::EVERYSEC_HALF_CHECKPOINT);
        assert!(r.is_ok(), "{}", r.unwrap_err());
    }

    #[test]
    fn withdrawn_rules_leave_half_a_transaction_in_the_checkpoint() {
        let r = durable::run(durable::Rules::withdrawn(), &durable::EVERYSEC_HALF_CHECKPOINT);
        assert_eq!(
            r,
            Err("PARTIAL COMMIT after crash: key0 = 10, key1 = 100 (decision durable: false)"
                .to_string())
        );
        // Each rule alone is insufficient.
        let only_wait = durable::Rules {
            decision_waits_for_durable_prepares: true,
            checkpoint_streams_predecessor: false,
        };
        let only_pin = durable::Rules {
            decision_waits_for_durable_prepares: false,
            checkpoint_streams_predecessor: true,
        };
        let wait_partials = (1..=2000u64)
            .filter(|s| durable::run(only_wait, &durable::random_steps(*s, 8)).is_err())
            .count();
        let pin_partials = (1..=2000u64)
            .filter(|s| durable::run(only_pin, &durable::random_steps(*s, 8)).is_err())
            .count();
        assert!(
            wait_partials > 0 && pin_partials > 0,
            "wait-only {wait_partials}, pin-only {pin_partials}"
        );
    }

    #[test]
    fn random_crash_interleavings_are_all_or_nothing() {
        let rules = durable_rules();
        let mut partials = Vec::new();
        for seed in 1..=2000u64 {
            if let Err(e) = durable::run(rules, &durable::random_steps(seed, 8)) {
                partials.push(format!("seed {seed}: {e}"));
            }
        }
        assert!(
            partials.is_empty(),
            "{} of 2000 interleavings partial under {rules:?}; first: {}",
            partials.len(),
            partials[0]
        );
    }

    // ---- lineage (ADR-0116 A1/A2) ----

    fn lineage_rules() -> lineage::Rules {
        match Variant::from_env() {
            Variant::Chosen => lineage::Rules::chosen(),
            Variant::Withdrawn => lineage::Rules::withdrawn(),
        }
    }

    #[test]
    fn a_durable_successor_never_outlives_its_dependency() {
        let r = lineage::run(lineage_rules(), &lineage::Shape::full_overlap(), &lineage::SUCCESSOR);
        assert!(r.is_ok(), "{}", r.unwrap_err());
    }

    #[test]
    fn withdrawn_plain_successor_outlives_its_dropped_source() {
        let shape = lineage::Shape::full_overlap();
        let r = lineage::run(lineage::Rules::withdrawn(), &shape, &lineage::SUCCESSOR);
        assert_eq!(
            r,
            Err("SERIALIZABILITY VIOLATION after crash: recovered [10, 101] is no serial subset of [Tx(1), Incr { cell: 1, id: 1 }] (decisions durable: {})".to_string())
        );
        // The qualified image alone does not repair it: the successor is still plain.
        let only_pin =
            lineage::Rules { successors_inherit_dependencies: false, ..lineage::Rules::chosen() };
        assert!(lineage::run(only_pin, &shape, &lineage::SUCCESSOR).is_err());
    }

    #[test]
    fn a_checkpoint_streams_the_decision_qualified_image() {
        let r = lineage::run(lineage_rules(), &lineage::Shape::full_overlap(), &lineage::CHAIN);
        assert!(r.is_ok(), "{}", r.unwrap_err());
    }

    #[test]
    fn withdrawn_immediate_predecessor_leaks_half_of_the_older_transaction() {
        let shape = lineage::Shape::full_overlap();
        let r = lineage::run(lineage::Rules::withdrawn(), &shape, &lineage::CHAIN);
        assert_eq!(
            r,
            Err("SERIALIZABILITY VIOLATION after crash: recovered [100, 11] is no serial subset of [Tx(1), Tx(2)] (decisions durable: {})".to_string())
        );
        // Inheritance alone does not repair it: the image is still T1's.
        let only_inherit =
            lineage::Rules { pin_decision_qualified_image: false, ..lineage::Rules::chosen() };
        assert!(lineage::run(only_inherit, &shape, &lineage::CHAIN).is_err());
    }

    #[test]
    fn an_acked_dependent_transaction_is_never_dropped() {
        let shape = lineage::Shape::full_overlap();
        let r = lineage::run(lineage::Rules::chosen(), &shape, &lineage::DEPENDENT_ACK);
        assert!(r.is_ok(), "{}", r.unwrap_err());
    }

    #[test]
    fn without_ack_gating_an_acked_dependent_transaction_is_dropped() {
        let rules =
            lineage::Rules { ack_waits_for_dependencies: false, ..lineage::Rules::chosen() };
        let r = lineage::run(rules, &lineage::Shape::full_overlap(), &lineage::DEPENDENT_ACK);
        assert_eq!(
            r,
            Err("ACKED WRITE LOST after crash: [Tx(2)] acked under `always` but recovered [10, 11] needs a subset without one of them (decisions durable: {2})".to_string())
        );
    }

    // ADR-0116 A8 — the fix validation's F01: a transaction's dependencies
    // are one set, or half of it recovers.

    #[test]
    fn a_partially_overlapping_transaction_recovers_whole() {
        let shape = lineage::Shape::partial_overlap();
        let r = lineage::run(lineage_rules(), &shape, &lineage::PARTIAL_OVERLAP);
        assert!(r.is_ok(), "{}", r.unwrap_err());
        assert_eq!(
            lineage::run(lineage::Rules::chosen(), &shape, &lineage::PARTIAL_OVERLAP),
            Ok(vec![10, 11, 12])
        );
    }

    #[test]
    fn withdrawn_per_record_dependencies_recover_half_of_a_partially_overlapping_transaction() {
        let shape = lineage::Shape::partial_overlap();
        let r = lineage::run(lineage::Rules::per_record(), &shape, &lineage::PARTIAL_OVERLAP);
        assert_eq!(
            r,
            Err("SERIALIZABILITY VIOLATION after crash: recovered [10, 11, 200] is no serial subset of [Tx(1), Tx(2)] (decisions durable: {2})".to_string())
        );
        // Neither decision durable, T1's only, and both: the controls pass.
        let mut t1_only = lineage::PARTIAL_OVERLAP[..9].to_vec();
        t1_only.extend([lineage::Step::Fsync(0), lineage::Step::Crash]);
        let mut both = lineage::PARTIAL_OVERLAP[..10].to_vec();
        both.extend([lineage::Step::Fsync(0), lineage::Step::Crash]);
        for control in [lineage::PARTIAL_OVERLAP[..9].to_vec(), t1_only, both] {
            let r = lineage::run(lineage::Rules::per_record(), &shape, &control);
            assert!(r.is_ok(), "{}", r.unwrap_err());
        }
    }

    #[test]
    fn a_read_leg_makes_the_whole_transaction_dependent() {
        let shape = lineage::Shape::asymmetric_read();
        let r = lineage::run(lineage_rules(), &shape, &lineage::ASYMMETRIC_READ);
        assert!(r.is_ok(), "{}", r.unwrap_err());
        assert_eq!(
            lineage::run(lineage::Rules::chosen(), &shape, &lineage::ASYMMETRIC_READ),
            Ok(vec![10, 11, 12])
        );
    }

    #[test]
    fn withdrawn_per_record_dependencies_keep_a_write_whose_read_was_dropped() {
        let shape = lineage::Shape::asymmetric_read();
        let r = lineage::run(lineage::Rules::per_record(), &shape, &lineage::ASYMMETRIC_READ);
        assert_eq!(
            r,
            Err("DEPENDENCY VIOLATION after crash: recovered [10, 11, 200] matches only subsets of [Tx(1), Tx(2)] that keep Tx(2) without Tx(1) it observed (decisions durable: {2})".to_string())
        );
    }

    #[test]
    fn a_decision_never_outlives_a_plain_write_its_read_leg_observed() {
        let shape = lineage::Shape::read_watermark();
        let r = lineage::run(lineage_rules(), &shape, &lineage::READ_WATERMARK);
        assert!(r.is_ok(), "{}", r.unwrap_err());
        assert_eq!(
            lineage::run(lineage::Rules::chosen(), &shape, &lineage::READ_WATERMARK),
            Ok(vec![10, 11])
        );
    }

    #[test]
    fn withdrawn_prepare_only_watermarks_let_a_decision_outlive_what_it_read() {
        let shape = lineage::Shape::read_watermark();
        let rules = lineage::Rules {
            decision_waits_for_read_watermarks: false,
            ..lineage::Rules::chosen()
        };
        let r = lineage::run(rules, &shape, &lineage::READ_WATERMARK);
        assert_eq!(
            r,
            Err("DEPENDENCY VIOLATION after crash: recovered [10, 100] matches only subsets of [Incr { cell: 0, id: 1 }, Tx(1)] that keep Tx(1) without Incr { cell: 0, id: 1 } it observed (decisions durable: {1})".to_string())
        );
    }

    #[test]
    fn a_transitive_successor_inherits_the_whole_closure() {
        let shape = lineage::Shape::transitive();
        let r = lineage::run(lineage_rules(), &shape, &lineage::TRANSITIVE);
        assert!(r.is_ok(), "{}", r.unwrap_err());
        assert_eq!(
            lineage::run(lineage::Rules::chosen(), &shape, &lineage::TRANSITIVE),
            Ok(vec![10, 11, 12, 13])
        );
    }

    #[test]
    fn withdrawn_per_record_dependencies_recover_a_transaction_two_hops_from_its_dropped_root() {
        let shape = lineage::Shape::transitive();
        let r = lineage::run(lineage::Rules::per_record(), &shape, &lineage::TRANSITIVE);
        assert_eq!(
            r,
            Err("DEPENDENCY VIOLATION after crash: recovered [10, 11, 300, 300] matches only subsets of [Tx(1), Tx(2), Tx(3)] that keep Tx(3) without Tx(1) it observed (decisions durable: {2, 3})".to_string())
        );
    }

    #[test]
    fn random_lineage_interleavings_are_serializable_and_keep_every_ack() {
        let rules = lineage_rules();
        let shape = lineage::Shape::full_overlap();
        let mut violations = Vec::new();
        for seed in 1..=2000u64 {
            if let Err(e) = lineage::run(rules, &shape, &lineage::random_steps(&shape, seed, 20)) {
                violations.push(format!("seed {seed}: {e}"));
            }
        }
        assert!(
            violations.is_empty(),
            "{} of 2000 interleavings violated under {rules:?}; first: {}",
            violations.len(),
            violations[0]
        );
    }

    #[test]
    fn random_lineage_shapes_are_serializable_and_keep_every_ack() {
        let rules = lineage_rules();
        let mut violations = Vec::new();
        for seed in 1..=2000u64 {
            let shape = lineage::Shape::random(seed);
            if let Err(e) = lineage::run(rules, &shape, &lineage::random_program(&shape, seed)) {
                violations.push(format!("seed {seed} {shape:?}: {e}"));
            }
        }
        assert!(
            violations.is_empty(),
            "{} of 2000 shapes violated under {rules:?}; first: {}",
            violations.len(),
            violations[0]
        );
    }

    #[test]
    fn withdrawn_lineage_rules_violate_some_random_interleavings() {
        let shape = lineage::Shape::full_overlap();
        let count = |rules: lineage::Rules| {
            (1..=2000u64)
                .filter(|s| {
                    lineage::run(rules, &shape, &lineage::random_steps(&shape, *s, 20)).is_err()
                })
                .count()
        };
        let withdrawn = count(lineage::Rules::withdrawn());
        let only_inherit = count(lineage::Rules {
            pin_decision_qualified_image: false,
            ..lineage::Rules::chosen()
        });
        let only_pin = count(lineage::Rules {
            successors_inherit_dependencies: false,
            ..lineage::Rules::chosen()
        });
        let no_ack_gate =
            count(lineage::Rules { ack_waits_for_dependencies: false, ..lineage::Rules::chosen() });
        eprintln!(
            "lineage violations over 2000 interleavings: withdrawn {withdrawn}, inherit-only {only_inherit}, pin-only {only_pin}, no-ack-gate {no_ack_gate}"
        );
        assert!(withdrawn > 0 && only_inherit > 0 && only_pin > 0 && no_ack_gate > 0);
    }

    #[test]
    fn withdrawn_per_record_dependencies_violate_some_random_shapes() {
        let count = |rules: lineage::Rules| {
            (1..=2000u64)
                .filter(|s| {
                    let shape = lineage::Shape::random(*s);
                    lineage::run(rules, &shape, &lineage::random_program(&shape, *s)).is_err()
                })
                .count()
        };
        let per_record = count(lineage::Rules::per_record());
        let prepare_only = count(lineage::Rules {
            decision_waits_for_read_watermarks: false,
            ..lineage::Rules::chosen()
        });
        let withdrawn = count(lineage::Rules::withdrawn());
        eprintln!(
            "lineage violations over 2000 random shapes: per-record {per_record}, prepare-only watermarks {prepare_only}, withdrawn {withdrawn}"
        );
        assert!(per_record > 0 && prepare_only > 0 && withdrawn > 0, "the model lost its teeth");
    }

    // ---- identity (ADR-0116 A3) ----

    fn identity_rules() -> identity::Rules {
        match Variant::from_env() {
            Variant::Chosen => identity::Rules::chosen(),
            Variant::Withdrawn => identity::Rules::withdrawn(),
        }
    }

    #[test]
    fn a_remote_prepare_never_meets_a_reissued_txid() {
        let r = identity::run(identity_rules(), &identity::remote_prepare_history());
        assert!(r.is_ok(), "{}", r.unwrap_err());
    }

    #[test]
    fn withdrawn_replay_maximum_reissues_the_remote_prepares_txid() {
        let r = identity::run(identity::Rules::withdrawn(), &identity::remote_prepare_history());
        assert_eq!(
            r,
            Err("TXID REISSUE: (0, 41) issued again while a remote participant's durable prepare still carries it".to_string())
        );
    }

    #[test]
    fn reservation_without_the_checkpoint_field_reissues_after_truncation() {
        let rules =
            identity::Rules { checkpoint_carries_reservation: false, ..identity::Rules::chosen() };
        let steps = [
            identity::Step::Issue,
            identity::Step::RemoteSurvives,
            identity::Step::Checkpoint,
            identity::Step::Crash,
            identity::Step::Issue,
        ];
        assert!(identity::run(rules, &steps).is_err(), "truncation dropped the reservation");
        assert!(identity::run(identity::Rules::chosen(), &steps).is_ok());
    }

    #[test]
    fn random_identity_histories_never_reissue() {
        let rules = identity_rules();
        let mut reissues = Vec::new();
        for seed in 1..=2000u64 {
            if let Err(e) = identity::run(rules, &identity::random_steps(seed, 24)) {
                reissues.push(format!("seed {seed}: {e}"));
            }
        }
        assert!(
            reissues.is_empty(),
            "TXID REISSUE on {} of 2000 histories under {rules:?}; first: {}",
            reissues.len(),
            reissues[0]
        );
    }

    #[test]
    fn withdrawn_identity_reissues_within_2000_seeds() {
        let n = (1..=2000u64)
            .filter(|s| {
                identity::run(identity::Rules::withdrawn(), &identity::random_steps(*s, 24))
                    .is_err()
            })
            .count();
        assert!(
            n > 0,
            "the withdrawn identity rule survived 2000 histories — the model lost its teeth"
        );
    }

    // ---- revision (ADR-0116 A7) ----

    fn revision_rules() -> revision::Rules {
        match Variant::from_env() {
            Variant::Chosen => revision::Rules::chosen(),
            Variant::Withdrawn => revision::Rules::withdrawn(),
        }
    }

    #[test]
    fn a_revision_token_never_repeats_across_the_version_wrap() {
        let stats = revision::run(revision_rules(), &revision::wrap_history());
        let stats = stats.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(stats.issued, 1 + (1 << revision::VERSION_BITS));
        assert_eq!(stats.reincarnations, 1, "the wrapping mutation re-incarnated");
    }

    #[test]
    fn withdrawn_incarnation_at_creation_repeats_after_the_version_wraps() {
        let r = revision::run(revision::Rules::withdrawn(), &revision::wrap_history());
        assert_eq!(
            r.map(|_| ()),
            Err("REVISION REPEAT: (1, 0) issued again after 8 mutations of a record that never left — the u3 version wrapped under incarnation 1".to_string())
        );
    }

    #[test]
    fn a_dead_incarnation_is_never_reissued_after_a_checkpointed_restart() {
        let stats = revision::run(revision_rules(), &revision::dead_incarnation_history());
        assert!(stats.is_ok(), "{}", stats.unwrap_err());
    }

    #[test]
    fn withdrawn_replayed_maximum_reissues_a_dead_incarnation() {
        let rules =
            revision::Rules { resume_from_durable_reservation: false, ..revision::Rules::chosen() };
        let r = revision::run(rules, &revision::dead_incarnation_history());
        assert_eq!(
            r.map(|_| ()),
            Err("REVISION REPEAT: (1, 0) issued again for a new incarnation — the counter resumed below a dead incarnation after a restart".to_string())
        );
    }

    #[test]
    fn an_exhausted_incarnation_counter_refuses_instead_of_repeating() {
        let stats = revision::run(revision_rules(), &revision::exhaustion_history());
        let stats = stats.unwrap_or_else(|e| panic!("{e}"));
        assert!(stats.refused >= 1, "{stats:?}");
        let rules = revision::Rules { refuse_at_exhaustion: false, ..revision::Rules::chosen() };
        let r = revision::run(rules, &revision::exhaustion_history());
        assert!(
            r.as_ref().is_err_and(|e| e.ends_with("the u4 incarnation counter wrapped")),
            "{r:?}"
        );
    }

    #[test]
    fn random_revision_histories_never_repeat_a_durable_token() {
        let rules = revision_rules();
        let mut repeats = Vec::new();
        for seed in 1..=2000u64 {
            if let Err(e) = revision::run(rules, &revision::random_steps(seed, 48)) {
                repeats.push(format!("seed {seed}: {e}"));
            }
        }
        assert!(
            repeats.is_empty(),
            "REVISION REPEAT on {} of 2000 histories under {rules:?}; first: {}",
            repeats.len(),
            repeats[0]
        );
    }

    #[test]
    fn withdrawn_revision_rules_repeat_within_2000_seeds() {
        let count = |rules: revision::Rules| {
            (1..=2000u64)
                .filter(|s| revision::run(rules, &revision::random_steps(*s, 48)).is_err())
                .count()
        };
        let wrap =
            count(revision::Rules { reincarnate_on_wrap: false, ..revision::Rules::chosen() });
        let restart = count(revision::Rules {
            resume_from_durable_reservation: false,
            ..revision::Rules::chosen()
        });
        let exhaustion =
            count(revision::Rules { refuse_at_exhaustion: false, ..revision::Rules::chosen() });
        eprintln!(
            "revision repeats over 2000 histories: wrap {wrap}, restart {restart}, exhaustion {exhaustion}"
        );
        assert!(wrap > 0 && restart > 0 && exhaustion > 0, "the model lost its teeth");
    }

    // ---- credits (ADR-0116 A9, review F06) ----

    fn credits_rules() -> credits::Rules {
        match Variant::from_env() {
            Variant::Chosen => credits::Rules::chosen(),
            Variant::Withdrawn => credits::Rules::withdrawn(),
        }
    }

    #[test]
    fn a_saturated_pair_with_contenders_behind_a_holder_completes() {
        let (cfg, actions) = credits::saturated_pair();
        let r = credits::run_script(credits_rules(), cfg, &actions);
        assert!(r.is_ok(), "{}", r.unwrap_err());
        let stats = credits::run_script(credits::Rules::chosen(), cfg, &actions).expect("chosen");
        assert_eq!((stats.committed, stats.aborted, stats.refused), (2, 0, 7));
        assert!(stats.max_parked <= 1 && stats.max_ring <= cfg.data_credits as usize);
    }

    #[test]
    fn withdrawn_queued_grant_returns_one_credit_twice() {
        let (cfg, actions) = credits::saturated_pair();
        let r = credits::run_script(credits::Rules::withdrawn(), cfg, &actions);
        assert_eq!(
            r,
            Err("CREDIT OVERFLOW: the coordinator holds 9 credits toward the owner of 8 — a second reply to one request returned its credit twice".to_string())
        );
        // Reserving at Admit does not repair a second reply.
        let queued_reserved = credits::Rules {
            grant_is_the_deferred_terminal_reply: false,
            ..credits::Rules::chosen()
        };
        let r = credits::run_script(queued_reserved, cfg, &actions);
        assert!(r.as_ref().is_err_and(|e| e.starts_with("CREDIT OVERFLOW")), "{r:?}");
    }

    #[test]
    fn withdrawn_hop_by_hop_credits_deadlock_a_holder_behind_its_own_contenders() {
        let (cfg, actions) = credits::saturated_pair();
        let r = credits::run_script(credits::Rules::deferred_hop_by_hop(), cfg, &actions);
        assert_eq!(
            r,
            Err("CREDIT DEADLOCK: 9 transaction(s) stuck with no message in flight and 0 credit(s) toward the owner — T1 holds key 0@owner and waits for ExecOp; 8 parked LockOp(s) (T2, T3, T4, T5, T6, T7, T8, T9) hold every credit".to_string())
        );
    }

    #[test]
    fn credit_storms_complete_and_drain_the_pair() {
        let rules = credits_rules();
        let mut violations = Vec::new();
        let mut committed = 0;
        for seed in 1..=64u64 {
            match credits::run(rules, credits::storm(), seed) {
                Ok(stats) => committed += stats.committed,
                Err(e) => violations.push(format!("seed {seed}: {e}")),
            }
        }
        assert!(
            violations.is_empty(),
            "{} of 64 storms violated under {rules:?}; first: {}",
            violations.len(),
            violations[0]
        );
        assert!(committed > 0);
    }

    #[test]
    fn withdrawn_credit_rules_violate_some_storm_within_64_seeds() {
        let count = |rules: credits::Rules| {
            (1..=64u64).filter(|s| credits::run(rules, credits::storm(), *s).is_err()).count()
        };
        let withdrawn = count(credits::Rules::withdrawn());
        let hop_by_hop = count(credits::Rules::deferred_hop_by_hop());
        eprintln!("credit storms: queued-grant {withdrawn}/64, hop-by-hop {hop_by_hop}/64");
        assert!(withdrawn > 0 && hop_by_hop > 0, "the model lost its teeth");
    }

    // ---- retention (ADR-0116 A10, review F08) ----

    fn retention_rules() -> retention::Rules {
        match Variant::from_env() {
            Variant::Chosen => retention::Rules::chosen(),
            Variant::Withdrawn => retention::Rules::withdrawn(),
        }
    }

    #[test]
    fn a_tombstone_storm_is_refused_at_the_pinned_budget_and_admitted_after_the_decisions() {
        let (images, txns, steps) = retention::tombstone_storm_then_decisions();
        let r = retention::run(retention_rules(), retention::budget(), &images, &txns, &steps);
        assert!(r.is_ok(), "{}", r.unwrap_err());
        let stats =
            retention::run(retention::Rules::chosen(), retention::budget(), &images, &txns, &steps)
                .expect("chosen");
        assert_eq!(stats.committed, 4);
        assert_eq!(stats.refused.get(&retention::Refusal::Retain), Some(&2));
        assert_eq!(stats.max_retained, 2 * retention::MIB);
    }

    #[test]
    fn withdrawn_frame_bound_does_not_bound_what_a_tombstone_pins() {
        let (images, txns, steps) = retention::tombstone_storm();
        let r = retention::run(
            retention::Rules::withdrawn(),
            retention::budget(),
            &images,
            &txns,
            &steps,
        );
        assert_eq!(
            r,
            Err("RETAINED BYTES EXCEED THE CLAIMED BOUND: 1 pending transaction(s) staged at most 24 B each but retain 1048576 B of images and 0 B of extents — above the retention cap × frame bound of 256 B".to_string())
        );
    }

    #[test]
    fn a_cold_extent_is_charged_and_dependent_writes_pin_nothing_new() {
        let (images, txns, steps) = retention::cold_extent_and_dependents();
        let r = retention::run(retention_rules(), retention::budget(), &images, &txns, &steps);
        assert!(r.is_ok(), "{}", r.unwrap_err());
        let stats =
            retention::run(retention::Rules::chosen(), retention::budget(), &images, &txns, &steps)
                .expect("chosen");
        assert_eq!((stats.committed, stats.max_retained), (2, 4 * retention::MIB));
        assert!(stats.refused.is_empty());
    }

    #[test]
    fn random_retention_histories_are_conserved_bounded_and_leak_free() {
        let rules = retention_rules();
        let mut violations = Vec::new();
        for seed in 1..=2000u64 {
            let (images, txns, steps) = retention::random(seed);
            if let Err(e) = retention::run(rules, retention::budget(), &images, &txns, &steps) {
                violations.push(format!("seed {seed}: {e}"));
            }
        }
        assert!(
            violations.is_empty(),
            "{} of 2000 histories violated under {rules:?}; first: {}",
            violations.len(),
            violations[0]
        );
    }

    #[test]
    fn withdrawn_retention_rules_exceed_the_claimed_bound_within_2000_seeds() {
        let count = (1..=2000u64)
            .filter(|s| {
                let (images, txns, steps) = retention::random(*s);
                retention::run(
                    retention::Rules::withdrawn(),
                    retention::budget(),
                    &images,
                    &txns,
                    &steps,
                )
                .is_err()
            })
            .count();
        eprintln!("retention claimed-bound violations over 2000 histories: {count}");
        assert!(count > 0, "the model lost its teeth");
    }
}
