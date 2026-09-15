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
            return Err(format!("NO PROGRESS: {bound} steps without every transaction terminal"));
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
                    "RING OVERFLOW: the {name} ring holds {} messages over its capacity {cap} (2 × \
                         {} data credits)",
                    ring.len(),
                    self.cfg.data_credits
                ));
            }
        }
        if self.credits > self.cfg.data_credits {
            return Err(format!(
                "CREDIT OVERFLOW: the coordinator holds {} credits toward the owner of {} — a \
                     second reply to one request returned its credit twice",
                self.credits, self.cfg.data_credits
            ));
        }
        self.stats.max_parked = self.stats.max_parked.max(self.parked.len());
        if self.parked.len() > self.cfg.data_credits as usize {
            return Err(format!(
                "PARKED LOCKOPS EXCEED THE CREDIT BOUND: {} parked at the owner with {} data \
                     credits — a parked LockOp holds no credit",
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
            "CREDIT DEADLOCK: {} transaction(s) stuck with no message in flight and {} credit(s) \
                 toward the owner — T{} {holds}waits for {wants}; {} parked LockOp(s) ({}) hold \
                 every credit",
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
                "CREDIT LEAK: {} credits toward the owner after every transaction is terminal, \
                     budget {}",
                self.credits, self.cfg.data_credits
            ));
        }
        if self.queues.iter().any(|q| !q.is_empty()) || !self.parked.is_empty() {
            return Err(
                "INTENT LEAK: the owner still queues an intent after every transaction is terminal"
                    .to_string(),
            );
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
