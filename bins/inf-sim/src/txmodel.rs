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
//!   decision.
//! - [`watch`]: endpoint version equality (`Version | MISSING`) accepts
//!   the F3 absent → present → absent history and a delete/recreate;
//!   the owner's registration (ADR-0116 D4) agrees with the history
//!   oracle on every history, including eviction and owner restart.
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
// Model 1 — intent acquisition across owners
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
    }

    impl Rules {
        pub fn chosen() -> Rules {
            Rules {
                acquisition: Acquisition::Canonical,
                reads_wait: true,
                hold_watch_intents: true,
            }
        }

        pub fn withdrawn() -> Rules {
            Rules {
                acquisition: Acquisition::WithdrawnTxidSorted,
                reads_wait: false,
                hold_watch_intents: false,
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
        Lock { txn: usize, owner: Cell },
        Granted { txn: usize, owner: Cell },
        WatchFail { txn: usize, owner: Cell },
        Refused { txn: usize, owner: Cell },
        Cancel { txn: usize, txid: u64, owner: Cell },
        Exec { txn: usize, owner: Cell },
        SubResult { txn: usize },
        Unlock { txn: usize, owner: Cell },
    }

    impl Msg {
        fn endpoint(&self) -> (usize, Cell) {
            match *self {
                Msg::Lock { txn, owner }
                | Msg::Granted { txn, owner }
                | Msg::WatchFail { txn, owner }
                | Msg::Refused { txn, owner }
                | Msg::Cancel { txn, owner, .. }
                | Msg::Exec { txn, owner }
                | Msg::Unlock { txn, owner } => (txn, owner),
                Msg::SubResult { txn } => (txn, usize::MAX),
            }
        }
    }

    #[derive(Clone, Debug, Default)]
    struct Owner {
        queues: BTreeMap<Key, VecDeque<Entry>>,
        values: BTreeMap<Key, Option<usize>>,
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
        /// Ground-truth mutation count per key.
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
            if t.owners.is_empty() {
                self.decide(txn);
                return;
            }
            let parallel = t.bypass || !matches!(self.rules.acquisition, Acquisition::Canonical);
            if t.bypass {
                t.phase = Phase::Executing;
            }
            if parallel {
                let owners = self.txns[txn].owners.clone();
                self.txns[txn].cursor = owners.len();
                for owner in owners {
                    self.pending.push(Msg::Lock { txn, owner });
                }
            } else {
                let owner = self.txns[txn].owners[0];
                self.pending.push(Msg::Lock { txn, owner });
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
            }
            let residue = self.held_intents();
            if residue != 0 && self.report.stuck.is_empty() {
                self.report.violations.push(format!(
                    "INTENT LEAK: {residue} intent(s) held after every transaction reached a terminal state"
                ));
            }
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
            let pos = self.rng.below(self.pending.len());
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
                Msg::SubResult { txn } => self.coord_subresult(txn),
                Msg::Unlock { txn, owner } => self.remove_entries(txn, None, owner),
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
                self.pending.push(Msg::SubResult { txn });
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

        fn owner_exec(&mut self, txn: usize, owner: Cell) {
            let spec = self.txns[txn].spec.clone();
            let reads: Vec<Key> =
                spec.reads.iter().copied().filter(|k| self.owner_of(*k) == owner).collect();
            let writes: Vec<Key> =
                spec.writes.iter().copied().filter(|k| self.owner_of(*k) == owner).collect();
            for key in reads {
                self.observe_read(txn, owner, key);
            }
            for key in writes {
                self.owners[owner].values.insert(key, Some(txn));
                *self.mutations.entry(key).or_insert(0) += 1;
            }
            self.pending.push(Msg::SubResult { txn });
        }

        fn coord_subresult(&mut self, txn: usize) {
            self.txns[txn].cursor -= 1;
            if self.txns[txn].cursor == 0 {
                self.decide(txn);
            }
        }

        fn decide(&mut self, txn: usize) {
            // Ground truth: a mutation of a watched key between the
            // registration and this decision must abort the EXEC.
            // The transaction's own write of a watched key is not a
            // foreign mutation.
            let own = &self.txns[txn].spec.writes;
            let dirty: Vec<Key> = self.txns[txn]
                .reg
                .iter()
                .filter(|(k, reg)| {
                    self.mutations.get(*k).copied().unwrap_or(0)
                        != **reg + u64::from(own.contains(k))
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
            for key in self.txns[txn].spec.writes.clone() {
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
            self.finish(txn);
        }

        fn abort(&mut self, txn: usize, reason: &'static str) {
            if matches!(self.txns[txn].phase, Phase::Terminal(_)) {
                return;
            }
            self.txns[txn].phase = Phase::Terminal(Outcome::Aborted(reason));
            self.report.aborted += 1;
            self.finish(txn);
        }

        /// Every terminal path releases every intent on every owner.
        fn finish(&mut self, txn: usize) {
            for owner in self.txns[txn].owners.clone() {
                self.pending.push(Msg::Unlock { txn, owner });
            }
            if let Some(client) = self.client_of.get(&txn).copied() {
                self.issue_next(client);
            }
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
    /// cells, 1–4 keys each, mixed reads/writes/watches, random delivery.
    pub fn storm(rules: Rules, cells: usize, keys: u32, txns: usize, seed: u64) -> Report {
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
        // decision and unlock on b@1; then T1's exec and decision.
        m.script(&[
            (t1, 0),
            (t1, 0),
            (t1, 1),
            (t1, 1),
            (w, 1),
            (w, 1),
            (w, 1),
            (w, usize::MAX),
            (w, 1),
        ]);
        m.run(10_000).clone()
    }
}

// ---------------------------------------------------------------------
// Model 2 — WATCH history on one owner
// ---------------------------------------------------------------------

pub mod watch {
    use super::Rng;

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

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Event {
        Watch,
        Set,
        Del,
        Expire,
        Flush,
        /// The registration table evicts this entry (capacity).
        Evict,
        /// The owner restarts: registrations and intents are lost.
        Restart,
        Exec,
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Verdict {
        Commit,
        Abort,
    }

    #[derive(Clone, Debug, Default)]
    struct Record {
        present: bool,
        /// u24 record version: bumps per mutation; a recreated record
        /// starts at 0 again (interfaces-m0 record header).
        version: u32,
    }

    /// Run `history` (must end with `Exec`) and return the verdict the
    /// representation gives and the history oracle's verdict.
    pub fn run(repr: Representation, history: &[Event]) -> (Verdict, Verdict) {
        let mut rec = Record::default();
        let mut observed: Option<Option<u32>> = None;
        let mut registered = false;
        let mut dirty = false;
        let mut evicted = false;
        let mut restarted = false;
        let mut mutated_since_watch = false;
        let mut watching = false;
        for ev in history {
            match ev {
                Event::Watch => {
                    watching = true;
                    mutated_since_watch = false;
                    observed = Some(rec.present.then_some(rec.version));
                    registered = true;
                    dirty = false;
                    evicted = false;
                    restarted = false;
                }
                Event::Set => {
                    if rec.present {
                        rec.version = (rec.version + 1) & 0x00FF_FFFF;
                    } else {
                        rec = Record { present: true, version: 0 };
                    }
                    dirty |= registered;
                    mutated_since_watch |= watching;
                }
                Event::Del | Event::Expire | Event::Flush => {
                    if rec.present {
                        rec = Record::default();
                        dirty |= registered;
                        mutated_since_watch |= watching;
                    }
                }
                Event::Evict => evicted |= registered,
                Event::Restart => {
                    registered = false;
                    restarted = true;
                }
                Event::Exec => {
                    let oracle = if !watching {
                        Verdict::Commit
                    } else if mutated_since_watch || evicted || restarted {
                        // Fail-closed: an evicted or lost registration
                        // cannot prove the absence of a mutation.
                        Verdict::Abort
                    } else {
                        Verdict::Commit
                    };
                    let verdict = match repr {
                        Representation::EndpointEquality => {
                            let now = rec.present.then_some(rec.version);
                            if observed.is_none_or(|o| o == now) {
                                Verdict::Commit
                            } else {
                                Verdict::Abort
                            }
                        }
                        Representation::OwnerRegistration => {
                            if !watching {
                                Verdict::Commit
                            } else if !registered || evicted || dirty {
                                Verdict::Abort
                            } else {
                                Verdict::Commit
                            }
                        }
                    };
                    return (verdict, oracle);
                }
            }
        }
        panic!("history must end with Exec");
    }

    /// The F3 history: WATCH an absent key, another client creates and
    /// deletes it, EXEC sees `MISSING` again.
    pub const ABSENT_PRESENT_ABSENT: [Event; 4] =
        [Event::Watch, Event::Set, Event::Del, Event::Exec];
    /// Delete/recreate: the recreated record's initial version equals the
    /// observed one.
    pub const DELETE_RECREATE: [Event; 5] =
        [Event::Set, Event::Watch, Event::Del, Event::Set, Event::Exec];

    /// A seeded random history of `len` events ending in `Exec`.
    pub fn random_history(seed: u64, len: usize) -> Vec<Event> {
        let mut rng = Rng::new(seed);
        let mut out = Vec::with_capacity(len + 1);
        for _ in 0..len {
            out.push(match rng.below(8) {
                0 => Event::Watch,
                1 | 2 => Event::Set,
                3 => Event::Del,
                4 => Event::Expire,
                5 => Event::Flush,
                6 => Event::Evict,
                _ => Event::Restart,
            });
        }
        out.push(Event::Exec);
        out
    }
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
    use super::acquisition::{self, Acquisition, Outcome, Rules};
    use super::durable;
    use super::identity;
    use super::lineage;
    use super::watch::{self, Event, Representation, Verdict};

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
    fn every_terminal_path_releases_every_intent() {
        for seed in 1..=32u64 {
            let r = acquisition::storm(Rules::chosen(), 3, 4, 16, seed);
            assert!(
                !r.violations.iter().any(|v| v.starts_with("INTENT LEAK")),
                "seed {seed}: {r:?}"
            );
            let r = acquisition::storm(reschedule(2), 3, 4, 16, seed);
            assert!(
                !r.violations.iter().any(|v| v.starts_with("INTENT LEAK")),
                "seed {seed}: {r:?}"
            );
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

    // ---- watch ----

    fn repr() -> Representation {
        match Variant::from_env() {
            Variant::Chosen => Representation::OwnerRegistration,
            Variant::Withdrawn => Representation::EndpointEquality,
        }
    }

    #[test]
    fn absent_present_absent_aborts() {
        let (verdict, oracle) = watch::run(repr(), &watch::ABSENT_PRESENT_ABSENT);
        assert_eq!(oracle, Verdict::Abort);
        assert_eq!(
            verdict,
            oracle,
            "WATCH HISTORY VIOLATION: absent → present → absent accepted by {:?}",
            repr()
        );
    }

    #[test]
    fn withdrawn_endpoint_equality_accepts_absent_present_absent() {
        let (verdict, oracle) =
            watch::run(Representation::EndpointEquality, &watch::ABSENT_PRESENT_ABSENT);
        assert_eq!((verdict, oracle), (Verdict::Commit, Verdict::Abort));
        let (verdict, oracle) =
            watch::run(Representation::EndpointEquality, &watch::DELETE_RECREATE);
        assert_eq!((verdict, oracle), (Verdict::Commit, Verdict::Abort));
    }

    #[test]
    fn delete_recreate_aborts() {
        let (verdict, oracle) = watch::run(repr(), &watch::DELETE_RECREATE);
        assert_eq!(oracle, Verdict::Abort);
        assert_eq!(
            verdict,
            oracle,
            "WATCH HISTORY VIOLATION: delete/recreate accepted by {:?}",
            repr()
        );
    }

    #[test]
    fn eviction_and_owner_restart_fail_closed() {
        for h in [
            [Event::Set, Event::Watch, Event::Evict, Event::Exec],
            [Event::Set, Event::Watch, Event::Restart, Event::Exec],
        ] {
            let (verdict, oracle) = watch::run(repr(), &h);
            assert_eq!(oracle, Verdict::Abort);
            assert_eq!(
                verdict,
                Verdict::Abort,
                "WATCH HISTORY VIOLATION: {h:?} accepted by {:?}",
                repr()
            );
        }
    }

    #[test]
    fn registration_agrees_with_the_history_oracle_on_random_histories() {
        let repr = repr();
        let mut disagreements = Vec::new();
        for seed in 1..=2000u64 {
            let h = watch::random_history(seed, 6);
            let (verdict, oracle) = watch::run(repr, &h);
            if verdict != oracle {
                disagreements.push(format!("seed {seed} {h:?}: {verdict:?} vs oracle {oracle:?}"));
            }
        }
        assert!(
            disagreements.is_empty(),
            "WATCH HISTORY VIOLATION on {} of 2000 histories under {repr:?}; first: {}",
            disagreements.len(),
            disagreements[0]
        );
    }

    #[test]
    fn withdrawn_endpoint_equality_disagrees_on_random_histories() {
        let n = (1..=2000u64)
            .filter(|s| {
                let (v, o) =
                    watch::run(Representation::EndpointEquality, &watch::random_history(*s, 6));
                v != o
            })
            .count();
        assert!(n > 0);
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
