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
//! Five models, one withdrawn rule set each:
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
//!   never do.
//! - [`identity`] (ADR-0116 A3, F03): resuming `local_seq` above the
//!   coordinator's replayed maximum reissues a txid a remote participant's
//!   durable prepare still carries; a durable reservation carried by the
//!   checkpoint never does.

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
    }

    impl Rules {
        pub fn chosen() -> Rules {
            Rules {
                acquisition: Acquisition::Canonical,
                reads_wait: true,
                hold_watch_intents: true,
                stage_privately: true,
                cancel_until: CancelUntil::Decision,
            }
        }

        pub fn withdrawn() -> Rules {
            Rules {
                acquisition: Acquisition::WithdrawnTxidSorted,
                reads_wait: false,
                hold_watch_intents: false,
                stage_privately: false,
                cancel_until: CancelUntil::DurableDecision,
            }
        }
    }

    /// Where a disconnect or timeout lands (ADR-0116 A4's phase table).
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum CancelPhase {
        /// Admitted, no `LockOp` sent yet.
        Admit,
        /// The `LockOp` for round `n` is in flight.
        Acquire(usize),
        /// Every round granted; `ExecOp` fanned.
        Execute,
        /// The `n`-th `SubResult` arrived with legs still outstanding.
        Executed(usize),
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

    /// One transaction: writes take W intents, reads and WATCH-only keys
    /// take R intents (a key both read/watched and written takes W).
    #[derive(Clone, Debug, Default)]
    pub struct TxnSpec {
        pub coordinator: Cell,
        pub writes: Vec<Key>,
        pub reads: Vec<Key>,
        pub watches: Vec<Key>,
        /// Native `INF.TX`: a leg's failed condition aborts the whole
        /// transaction. `false` is Redis `EXEC`: the failure is embedded
        /// in that leg's reply and the decision is still `Commit`.
        pub native: bool,
        /// The owner whose leg fails its condition (`NX`, `IF REV`, …)
        /// before writing anything.
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
        reads_seen: Vec<(Key, Option<usize>)>,
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
        },
        SubResult {
            txn: usize,
            failed: bool,
        },
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
                | Msg::Exec { txn, owner }
                | Msg::Unlock { txn, owner, .. } => (txn, owner),
                Msg::SubResult { txn, .. } | Msg::CancelAt { txn, .. } => (txn, COORD),
                Msg::DecisionDurable { txn } => (txn, DISK),
            }
        }
    }

    #[derive(Clone, Debug, Default)]
    struct Owner {
        queues: BTreeMap<Key, VecDeque<Entry>>,
        /// Published values: the last writer, if any.
        values: BTreeMap<Key, Option<usize>>,
        /// Private write sets awaiting `UnlockOp`, per transaction.
        staged: BTreeMap<usize, Vec<Key>>,
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
        committed_writes: BTreeMap<Key, Vec<(u64, usize)>>,
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
            for keys in [&mut spec.writes, &mut spec.reads, &mut spec.watches] {
                keys.sort_unstable();
                keys.dedup();
            }
            let reg =
                spec.watches.iter().map(|k| (*k, *self.mutations.get(k).unwrap_or(&0))).collect();
            let mut owners: Vec<Cell> = spec
                .writes
                .iter()
                .chain(&spec.reads)
                .chain(&spec.watches)
                .map(|k| self.owner_of(*k))
                .collect();
            owners.sort_unstable();
            owners.dedup();
            let bypass = !self.rules.reads_wait
                && spec.writes.is_empty()
                && spec.watches.is_empty()
                && !spec.reads.is_empty();
            let idx = self.txns.len();
            let n = owners.len();
            self.txns.push(Txn {
                spec,
                txid: 0,
                phase: Phase::Acquiring,
                owners,
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
                    .filter(|(_, o)| t.spec.writes.iter().any(|k| self.owner_of(*k) == *o))
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
                            .spec
                            .writes
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
            for k in &spec.writes {
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
                Msg::Exec { txn, owner } => self.owner_exec(txn, owner),
                Msg::SubResult { txn, failed } => self.coord_subresult(txn, failed),
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
                for (key, _) in &intents {
                    self.observe_read(txn, owner, *key);
                }
                self.pending.push(Msg::SubResult { txn, failed: false });
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

        fn exec_fan(&mut self, txn: usize) {
            self.txns[txn].phase = Phase::Executing;
            let owners = self.txns[txn].owners.clone();
            self.txns[txn].cursor = owners.len();
            for owner in owners {
                self.pending.push(Msg::Exec { txn, owner });
            }
            self.enter(txn, CancelPhase::Execute);
        }

        fn observe_read(&mut self, txn: usize, owner: Cell, key: Key) {
            let seen = self.owners[owner].values.get(&key).copied().flatten();
            if let Some(w) = seen
                && self.txns[w].commit_seq.is_none()
            {
                self.report.violations.push(format!(
                    "READ UNCOMMITTED: T{} read key {key} written by T{} before T{}'s decision",
                    txn + 1,
                    w + 1,
                    w + 1
                ));
            }
            self.txns[txn].reads_seen.push((key, seen));
        }

        /// The leg: reads, then the condition, then the writes — staged
        /// privately (chosen) or written live (withdrawn).
        fn owner_exec(&mut self, txn: usize, owner: Cell) {
            let spec = self.txns[txn].spec.clone();
            let reads: Vec<Key> =
                spec.reads.iter().copied().filter(|k| self.owner_of(*k) == owner).collect();
            let writes: Vec<Key> =
                spec.writes.iter().copied().filter(|k| self.owner_of(*k) == owner).collect();
            for key in reads {
                self.observe_read(txn, owner, key);
            }
            if spec.fails_at == Some(owner) {
                self.pending.push(Msg::SubResult { txn, failed: true });
                return;
            }
            if !writes.is_empty() {
                if self.rules.stage_privately {
                    self.owners[owner].staged.insert(txn, writes);
                } else {
                    self.publish(txn, owner, &writes);
                }
            }
            self.pending.push(Msg::SubResult { txn, failed: false });
        }

        fn publish(&mut self, txn: usize, owner: Cell, keys: &[Key]) {
            for key in keys {
                self.owners[owner].values.insert(*key, Some(txn));
                *self.mutations.entry(*key).or_insert(0) += 1;
            }
            let oi = self.txns[txn].owners.iter().position(|o| *o == owner).expect("a participant");
            self.txns[txn].published[oi] = true;
        }

        fn coord_subresult(&mut self, txn: usize, failed: bool) {
            if failed {
                // The failing leg is the one still outstanding; mark by
                // elimination is ambiguous, so record the spec's owner.
                let owner = self.txns[txn].spec.fails_at.expect("a failing leg");
                let oi = self.txns[txn].owners.iter().position(|o| *o == owner).expect("owner");
                self.txns[txn].failed[oi] = true;
            }
            self.txns[txn].cursor -= 1;
            if self.txns[txn].cursor != 0 {
                let arrived = self.txns[txn].owners.len() - self.txns[txn].cursor;
                self.enter(txn, CancelPhase::Executed(arrived));
                return;
            }
            if self.txns[txn].phase != Phase::Executing {
                // Cancelled while the legs were executing: the abort's
                // unlocks are already in flight.
                return;
            }
            if self.txns[txn].spec.native && self.txns[txn].failed.iter().any(|f| *f) {
                self.abort(txn, "condition failed");
            } else {
                self.decide(txn);
            }
        }

        fn decide(&mut self, txn: usize) {
            // Ground truth: a mutation of a watched key between the
            // registration and this decision must abort the EXEC. Under
            // the withdrawn live-write rule the transaction's own write of
            // a watched key is already counted and is not foreign.
            let own = &self.txns[txn].spec.writes;
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
            let committed: Vec<Key> = t
                .spec
                .writes
                .iter()
                .copied()
                .filter(|k| {
                    let oi = t.owners.iter().position(|o| *o == self.owner_of(*k)).expect("owner");
                    !t.failed[oi]
                })
                .collect();
            for key in committed {
                self.committed_writes.entry(key).or_default().push((seq, txn));
            }
            // A bypass read has no exclusion interval to pin its point to;
            // its uncommitted-read check above is the meaningful one.
            if !self.txns[txn].bypass {
                for (key, seen) in self.txns[txn].reads_seen.clone() {
                    let last = self
                        .committed_writes
                        .get(&key)
                        .and_then(|v| v.iter().rev().find(|(s, _)| *s < seq))
                        .map(|(_, w)| *w);
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
            if let Some(keys) = self.owners[owner].staged.remove(&txn)
                && commit
            {
                self.publish(txn, owner, &keys);
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
    /// cells, 1–4 keys each, mixed reads/writes/watches, random delivery;
    /// with `faults`, one in four is native, one in four fails a leg and
    /// one in four is cancelled at a random phase.
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
                    0 => spec.reads.push(k),
                    1 => spec.watches.push(k),
                    _ => spec.writes.push(k),
                }
            }
            if faults {
                spec.native = rng.below(4) == 0;
                if rng.below(4) == 0 {
                    let owner = spec.writes.first().map(|k| *k as usize % cells);
                    spec.fails_at = owner;
                }
                if rng.below(4) == 0 {
                    let phase = match rng.below(6) {
                        0 => CancelPhase::Admit,
                        1 => CancelPhase::Acquire(rng.below(3)),
                        2 => CancelPhase::Execute,
                        3 => CancelPhase::Executed(1),
                        4 => CancelPhase::Decided,
                        _ => CancelPhase::Durable,
                    };
                    let kind = if rng.below(2) == 0 { Cancel::Disconnect } else { Cancel::Timeout };
                    spec.cancel = Some((phase, kind));
                }
            }
            m.submit(spec);
        }
        m.run(200_000).clone()
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

    /// Every phase a cancellation can land in, for the two-owner history.
    pub const PHASES: [CancelPhase; 7] = [
        CancelPhase::Admit,
        CancelPhase::Acquire(0),
        CancelPhase::Acquire(1),
        CancelPhase::Execute,
        CancelPhase::Executed(1),
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
    pub type Key = u32;

    /// The D3 rules the 2026-09-10 review reopened (ADR-0116 A1/A2).
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
    }

    impl Rules {
        pub fn chosen() -> Rules {
            Rules {
                successors_inherit_dependencies: true,
                pin_decision_qualified_image: true,
                ack_waits_for_dependencies: true,
            }
        }

        pub fn withdrawn() -> Rules {
            Rules {
                successors_inherit_dependencies: false,
                pin_decision_qualified_image: false,
                ack_waits_for_dependencies: false,
            }
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Rec {
        Prepare { txid: Txid, key: Key, value: u32, deps: Vec<Txid> },
        Plain { op: usize, key: Key, value: u32, deps: Vec<Txid> },
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
        ram: BTreeMap<Key, u32>,
        runs: BTreeMap<Key, Run>,
        /// D3 as written: (writer txid, immediate predecessor).
        pinned: BTreeMap<Key, (Txid, u32)>,
        ckpt_image: BTreeMap<Key, u32>,
        ckpt_begin: usize,
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Step {
        /// Participant `c` executes transaction `t`'s leg under its
        /// intents and logs the prepare.
        Prepare(Txid, usize),
        /// The coordinator decides `t` in memory once both legs prepared;
        /// `UnlockOp{Commit}` publishes on both cells and releases the
        /// intents.
        Publish(Txid),
        /// A plain `INCR` of cell `c`'s key — the successor.
        Incr(usize),
        /// An everysec timer on cell `c`.
        Fsync(usize),
        /// A checkpoint on cell `c` (its cut is durable first).
        Checkpoint(usize),
        Crash,
    }

    /// An operation in execution order, for the serial oracle.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Op {
        Tx(Txid),
        Incr { cell: usize, id: usize },
    }

    /// T1 arrives first; T2 queues behind it on both keys.
    const TXNS: [Txid; 2] = [1, 2];

    /// T1 is coordinated by cell 0, T2 by cell 1: their decisions live in
    /// different logs and become durable independently.
    fn coordinator(t: Txid) -> usize {
        (t as usize + 1) % 2
    }

    fn tx_value(t: Txid) -> u32 {
        100 * t as u32
    }

    fn durable_decisions(cells: &[CellLog; 2]) -> BTreeSet<Txid> {
        cells
            .iter()
            .flat_map(|c| c.records.iter().take(c.durable))
            .filter_map(|r| match r {
                Rec::Decision { txid } => Some(*txid),
                _ => None,
            })
            .collect()
    }

    /// The dependencies a record logged now inherits from its key.
    fn inherited(rules: Rules, cell: &CellLog, key: Key) -> Vec<Txid> {
        if !rules.successors_inherit_dependencies {
            return Vec::new();
        }
        match (cell.runs.get(&key), cell.pinned.get(&key)) {
            (Some(run), _) => run.pending.iter().copied().collect(),
            (None, Some((writer, _))) => vec![*writer],
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

    /// Two participants, one key each (`10`/`11` at boot, each cell booted
    /// from a checkpoint holding its pre-image); T1 then T2 write both
    /// keys (`100`, `200`); plain `INCR`s land between them. Returns the
    /// recovered values, or the violation.
    pub fn run(rules: Rules, steps: &[Step]) -> Result<[u32; 2], String> {
        let mut m = Model::new(rules);
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

    struct Model {
        rules: Rules,
        cells: [CellLog; 2],
        prepared: BTreeMap<Txid, [bool; 2]>,
        published: BTreeSet<Txid>,
        decision_logged: BTreeSet<Txid>,
        /// Executed operations in order.
        ops: Vec<Op>,
    }

    impl Model {
        fn new(rules: Rules) -> Model {
            let mut cells = [CellLog::default(), CellLog::default()];
            for (c, cell) in cells.iter_mut().enumerate() {
                cell.ram.insert(c as Key, 10 + c as u32);
                cell.ckpt_image.insert(c as Key, 10 + c as u32);
            }
            Model {
                rules,
                cells,
                prepared: TXNS.iter().map(|t| (*t, [false; 2])).collect(),
                published: BTreeSet::new(),
                decision_logged: BTreeSet::new(),
                ops: Vec::new(),
            }
        }

        /// A W intent on cell `c`'s key is held by a prepared, unpublished
        /// transaction other than `t`.
        fn held(&self, t: Option<Txid>, c: usize) -> bool {
            TXNS.iter().any(|o| Some(*o) != t && self.prepared[o][c] && !self.published.contains(o))
        }

        fn prepare(&mut self, t: Txid, c: usize) {
            // Canonical acquisition: T2 arrives after T1 and queues behind it.
            let arrived = t == TXNS[0] || self.published.contains(&TXNS[0]);
            if self.prepared[&t][c] || !arrived || self.held(Some(t), c) {
                return;
            }
            self.prepared.get_mut(&t).expect("known txid")[c] = true;
            let cell = &mut self.cells[c];
            let deps = inherited(self.rules, cell, c as Key);
            cell.records.push(Rec::Prepare { txid: t, key: c as Key, value: tx_value(t), deps });
        }

        /// The in-memory decision: publish on both cells, release intents,
        /// start (or extend) each key's pending run.
        fn publish(&mut self, t: Txid) {
            if self.published.contains(&t) || !self.prepared[&t].iter().all(|p| *p) {
                return;
            }
            self.published.insert(t);
            self.ops.push(Op::Tx(t));
            for (c, cell) in self.cells.iter_mut().enumerate() {
                let key = c as Key;
                let (pos, deps) = cell
                    .records
                    .iter()
                    .enumerate()
                    .find_map(|(i, r)| match r {
                        Rec::Prepare { txid, deps, .. } if *txid == t => Some((i, deps.clone())),
                        _ => None,
                    })
                    .expect("prepared leg");
                let old = cell.ram.insert(key, tx_value(t)).expect("key present");
                let run = cell.runs.entry(key).or_insert(Run {
                    base: old,
                    pending: BTreeSet::new(),
                    first_record: pos,
                });
                run.pending.insert(t);
                run.pending.extend(deps);
                cell.pinned.insert(key, (t, old));
            }
        }

        fn incr(&mut self, c: usize) {
            if self.held(None, c) {
                return;
            }
            let id = self.ops.iter().filter(|o| matches!(o, Op::Incr { .. })).count() + 1;
            self.ops.push(Op::Incr { cell: c, id });
            let cell = &mut self.cells[c];
            let key = c as Key;
            let deps = inherited(self.rules, cell, key);
            let value = cell.ram[&key] + 1;
            cell.records.push(Rec::Plain { op: id, key, value, deps });
            cell.ram.insert(key, value);
            if !self.rules.successors_inherit_dependencies {
                // D3 as written: a later plain write releases the pin.
                cell.pinned.remove(&key);
                cell.runs.remove(&key);
            }
        }

        /// The cut is durable first; the image streams the pinned image
        /// of a pending key; `ckpt-begin` covers every conditional record
        /// and (A2) every pending run from its first record.
        fn checkpoint(&mut self, c: usize) {
            let decided = durable_decisions(&self.cells);
            let qualified = self.rules.pin_decision_qualified_image;
            let cell = &mut self.cells[c];
            cell.durable = cell.records.len();
            cell.ckpt_image = cell
                .ram
                .iter()
                .map(|(k, v)| {
                    let streamed = if qualified {
                        cell.runs.get(k).map(|run| run.base)
                    } else {
                        cell.pinned.get(k).map(|(_, old)| *old)
                    };
                    (*k, streamed.unwrap_or(*v))
                })
                .collect();
            let first_conditional = if qualified {
                cell.records.iter().position(|r| conditional(r, &decided))
            } else {
                cell.records
                    .iter()
                    .position(|r| matches!(r, Rec::Prepare { txid, .. } if !decided.contains(txid)))
            };
            let run_origin =
                if qualified { cell.runs.values().map(|r| r.first_record).min() } else { None };
            cell.ckpt_begin = [Some(cell.records.len()), first_conditional, run_origin]
                .into_iter()
                .flatten()
                .min()
                .expect("a candidate");
        }

        /// The decision is appended to the coordinator's log once every
        /// participant's prepare is durable (D3, unchanged); a durable
        /// decision releases the pins that depend on nothing else.
        fn settle(&mut self) {
            for t in TXNS {
                if !self.published.contains(&t) || self.decision_logged.contains(&t) {
                    continue;
                }
                let prepares_durable = self.cells.iter().all(|cell| {
                    cell.records
                        .iter()
                        .take(cell.durable)
                        .any(|r| matches!(r, Rec::Prepare { txid, .. } if *txid == t))
                });
                if prepares_durable {
                    self.cells[coordinator(t)].records.push(Rec::Decision { txid: t });
                    self.decision_logged.insert(t);
                }
            }
            let decided = durable_decisions(&self.cells);
            for cell in &mut self.cells {
                cell.runs.retain(|_, run| !run.pending.is_subset(&decided));
                cell.pinned.retain(|_, (writer, _)| !decided.contains(writer));
            }
        }

        /// Checkpoint image + durable tail; a tagged or dependent record
        /// applies iff its txid and every dependency committed.
        fn recover(&self, decided: &BTreeSet<Txid>) -> [u32; 2] {
            let mut out = [0u32; 2];
            for (c, cell) in self.cells.iter().enumerate() {
                let mut state = cell.ckpt_image.clone();
                let tail = &cell.records[cell.ckpt_begin.min(cell.durable)..cell.durable];
                for rec in tail {
                    match rec {
                        Rec::Prepare { key, value, .. } | Rec::Plain { key, value, .. }
                            if !conditional(rec, decided) =>
                        {
                            state.insert(*key, *value);
                        }
                        _ => {}
                    }
                }
                out[c] = state[&(c as Key)];
            }
            out
        }

        /// `always` acks at the crash: a durable decision or plain record
        /// and, under A1, every dependency durably decided.
        fn acked(&self, decided: &BTreeSet<Txid>) -> Vec<Op> {
            let tx_deps = |t: Txid| -> Vec<Txid> {
                self.cells
                    .iter()
                    .flat_map(|cell| cell.records.iter())
                    .filter_map(|r| match r {
                        Rec::Prepare { txid, deps, .. } if *txid == t => Some(deps.clone()),
                        _ => None,
                    })
                    .flatten()
                    .collect()
            };
            let mut acked = Vec::new();
            for (c, cell) in self.cells.iter().enumerate() {
                for rec in &cell.records[..cell.durable] {
                    let (op, deps) = match rec {
                        Rec::Decision { txid } => (Op::Tx(*txid), tx_deps(*txid)),
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
        /// recovered state, and one such subset must contain every ack.
        fn verdict(&self) -> Result<[u32; 2], String> {
            let decided = durable_decisions(&self.cells);
            let out = self.recover(&decided);
            let acked = self.acked(&decided);
            let ops = &self.ops;
            let matching: Vec<Vec<Op>> = (0..1u32 << ops.len())
                .map(|mask| {
                    ops.iter()
                        .enumerate()
                        .filter(|(i, _)| mask & (1 << i) != 0)
                        .map(|(_, o)| *o)
                        .collect()
                })
                .filter(|subset: &Vec<Op>| replay(subset) == out)
                .collect();
            if matching.is_empty() {
                return Err(format!(
                    "SERIALIZABILITY VIOLATION after crash: recovered {out:?} is no serial subset of {ops:?} (decisions durable: {decided:?})"
                ));
            }
            if !matching.iter().any(|s| acked.iter().all(|a| s.contains(a))) {
                return Err(format!(
                    "ACKED WRITE LOST after crash: {acked:?} acked under `always` but recovered {out:?} needs a subset without one of them (decisions durable: {decided:?})"
                ));
            }
            Ok(out)
        }
    }

    fn replay(ops: &[Op]) -> [u32; 2] {
        let mut state = [10, 11];
        for op in ops {
            match *op {
                Op::Tx(t) => state = [tx_value(t); 2],
                Op::Incr { cell, .. } => state[cell] += 1,
            }
        }
        state
    }

    /// F01: T1 publishes; `INCR` on cell 1 reads the published 100; cell
    /// 1's timer fsyncs; the crash beats T1's decision.
    pub const SUCCESSOR: [Step; 6] = [
        Step::Prepare(1, 0),
        Step::Prepare(1, 1),
        Step::Publish(1),
        Step::Incr(1),
        Step::Fsync(1),
        Step::Crash,
    ];

    /// F02: T1 then T2 publish over both keys; cell 0 checkpoints; the
    /// crash beats both decisions.
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
    /// durable, T1's is not; crash.
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

    /// The oracle enumerates subsets of the executed operations, so a
    /// history carries at most this many successors.
    pub const MAX_INCRS: usize = 6;

    /// A seeded interleaving of both transactions, successors, fsyncs and
    /// checkpoints.
    pub fn random_steps(seed: u64, len: usize) -> Vec<Step> {
        let mut rng = Rng::new(seed);
        let mut out = Vec::with_capacity(len + 1);
        let mut incrs = 0;
        for _ in 0..len {
            let choices = if incrs < MAX_INCRS { 12 } else { 10 };
            out.push(match rng.below(choices) {
                0 => Step::Prepare(1, 0),
                1 => Step::Prepare(1, 1),
                2 => Step::Prepare(2, 0),
                3 => Step::Prepare(2, 1),
                4 => Step::Publish(1),
                5 => Step::Publish(2),
                6 => Step::Fsync(0),
                7 => Step::Fsync(1),
                8 => Step::Checkpoint(0),
                9 => Step::Checkpoint(1),
                10 => {
                    incrs += 1;
                    Step::Incr(0)
                }
                _ => {
                    incrs += 1;
                    Step::Incr(1)
                }
            });
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
                    let replayed_max = log
                        .iter()
                        .filter_map(|r| match r {
                            Rec::Tag { seq } => Some(*seq),
                            _ => None,
                        })
                        .max();
                    let reserved_max = log
                        .iter()
                        .filter_map(|r| match r {
                            Rec::Reserve { upto } => Some(*upto),
                            _ => None,
                        })
                        .max();
                    let floor = if rules.resume_from_durable_reservation {
                        replayed_max.unwrap_or(0).max(reserved_max.unwrap_or(0)).max(ckpt_reserved)
                    } else {
                        replayed_max.unwrap_or(0)
                    };
                    next = floor + 1;
                    reserved_upto = floor;
                    last_issued = None;
                }
            }
        }
        Ok(issued)
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

#[cfg(test)]
mod tests {
    use super::Variant;
    use super::acquisition::{self, Acquisition, Cancel, CancelPhase, CancelUntil, Outcome, Rules};
    use super::durable;
    use super::identity;
    use super::lineage;
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
        let (r, outcome, _) =
            acquisition::cancellation_history(rules, CancelPhase::Executed(1), Cancel::Timeout, 3);
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
    fn withdrawn_rules_violate_some_fault_storm_within_64_seeds() {
        let staging = Rules { stage_privately: false, ..Rules::chosen() };
        let cancel = Rules { cancel_until: CancelUntil::DurableDecision, ..Rules::chosen() };
        let hits = |rules: Rules, prefix: &str| {
            (1..=64u64)
                .filter(|s| {
                    acquisition::storm_with(rules, 4, 6, 24, *s, true)
                        .violations
                        .iter()
                        .any(|v| v.starts_with(prefix))
                })
                .count()
        };
        let staging_hits = hits(staging, "STAGING VIOLATION");
        let partial_hits = hits(cancel, "PARTIAL PUBLICATION");
        eprintln!(
            "fault storms: live-write {staging_hits}/64, cancel-until-durable {partial_hits}/64"
        );
        assert!(staging_hits > 0 && partial_hits > 0);
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
        let r = lineage::run(lineage_rules(), &lineage::SUCCESSOR);
        assert!(r.is_ok(), "{}", r.unwrap_err());
    }

    #[test]
    fn withdrawn_plain_successor_outlives_its_dropped_source() {
        let r = lineage::run(lineage::Rules::withdrawn(), &lineage::SUCCESSOR);
        assert_eq!(
            r,
            Err("SERIALIZABILITY VIOLATION after crash: recovered [10, 101] is no serial subset of [Tx(1), Incr { cell: 1, id: 1 }] (decisions durable: {})".to_string())
        );
        // The qualified image alone does not repair it: the successor is still plain.
        let only_pin =
            lineage::Rules { successors_inherit_dependencies: false, ..lineage::Rules::chosen() };
        assert!(lineage::run(only_pin, &lineage::SUCCESSOR).is_err());
    }

    #[test]
    fn a_checkpoint_streams_the_decision_qualified_image() {
        let r = lineage::run(lineage_rules(), &lineage::CHAIN);
        assert!(r.is_ok(), "{}", r.unwrap_err());
    }

    #[test]
    fn withdrawn_immediate_predecessor_leaks_half_of_the_older_transaction() {
        let r = lineage::run(lineage::Rules::withdrawn(), &lineage::CHAIN);
        assert_eq!(
            r,
            Err("SERIALIZABILITY VIOLATION after crash: recovered [100, 11] is no serial subset of [Tx(1), Tx(2)] (decisions durable: {})".to_string())
        );
        // Inheritance alone does not repair it: the image is still T1's.
        let only_inherit =
            lineage::Rules { pin_decision_qualified_image: false, ..lineage::Rules::chosen() };
        assert!(lineage::run(only_inherit, &lineage::CHAIN).is_err());
    }

    #[test]
    fn an_acked_dependent_transaction_is_never_dropped() {
        let r = lineage::run(lineage::Rules::chosen(), &lineage::DEPENDENT_ACK);
        assert!(r.is_ok(), "{}", r.unwrap_err());
    }

    #[test]
    fn without_ack_gating_an_acked_dependent_transaction_is_dropped() {
        let rules =
            lineage::Rules { ack_waits_for_dependencies: false, ..lineage::Rules::chosen() };
        let r = lineage::run(rules, &lineage::DEPENDENT_ACK);
        assert_eq!(
            r,
            Err("ACKED WRITE LOST after crash: [Tx(2)] acked under `always` but recovered [10, 11] needs a subset without one of them (decisions durable: {2})".to_string())
        );
    }

    #[test]
    fn random_lineage_interleavings_are_serializable_and_keep_every_ack() {
        let rules = lineage_rules();
        let mut violations = Vec::new();
        for seed in 1..=2000u64 {
            if let Err(e) = lineage::run(rules, &lineage::random_steps(seed, 20)) {
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
    fn withdrawn_lineage_rules_violate_some_random_interleavings() {
        let count = |rules: lineage::Rules| {
            (1..=2000u64)
                .filter(|s| lineage::run(rules, &lineage::random_steps(*s, 20)).is_err())
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
}
