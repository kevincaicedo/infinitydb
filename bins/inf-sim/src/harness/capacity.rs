//! `m0-capacity`: a cell's capacity and its connection closes, each a named
//! leg (`--close-leg <name>`, a leg named, never drawn from the seed).
//!
//! A connection's work is one task, alive while its slot is occupied, with at
//! most a window of 32 gate holders (ADR-0177 D2, D4); a close runs every
//! byte its client handed over: a half-closed client is answered, a gone
//! client's commands run unanswered, and the queue drains at `Closed`
//! (ADR-0196 and its A1, ADR-0123 A2). Each leg drives scripted clients
//! over the sim net against the real plane on three cells, with a peer
//! whose replies are held when the leg needs one: that cell runs no
//! iteration until the leg releases it. The oracles read the wire (the
//! replies, the store read back by a surviving client, `CLIENT LIST`,
//! `PUBSUB NUMSUB`), the net's own counts and the harness's own counts;
//! they share no code with the plane. Every leg checks its engagement, and
//! a clause that never engaged is `VACUOUS`, never green.

use core::cell::RefCell;
use std::os::fd::RawFd;
use std::rc::Rc;

use inf_alloc::BufferPool;
use inf_fabric::Mesh;
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::{Nanos, VirtualClock};
use inf_foundation::{CellId, hash64};
use inf_runtime::{CellLoop, LoopConfig};
use inf_server::{NodeInfo, NoopObserver, ServerPlane, StdSegmentFs};
use inf_store::{KeyspaceSeed, NsSpec, SlotRouter, StoreConfig};

use super::{FROZEN_MESH, STEP_NS_FLOOR, namespace_specs, node_hasher};
use crate::census::{CellFacts, CensusVerdict};
use crate::driver::Violations;
use crate::net::{CellNet, Plant, SimDriver, drain_violations, listener_fd};
use crate::resp::reply_len;
use crate::state::StateHash;

mod gated;
mod small_credit;

/// The legs, each one row of `scripts/sim-smoke.sh` or one pinned red.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseLeg {
    /// Pattern subscribers that close while a peer holds their deltas'
    /// acks, then more clients: the tasks a cell holds stay within its
    /// connection share and its plane-start pumps (O1).
    Tasks,
    /// Remote `DEL`s held at a peer: a connection holds at most its window
    /// of 32 gate holders.
    Fan,
    /// A `DEL` held at a peer, 1,023 `SET`s queued behind it, then a reset:
    /// every queued `SET` lands, unanswered.
    Close,
    /// The same with 1,024 `SET`s, so the receive is disarmed and re-armed,
    /// then a FIN: every reply is read, then the close.
    Rearm,
    /// A `SET` and a FIN reaped together: the `SET` lands and is answered.
    EofWithData,
    /// A `SUBSCRIBE` queued behind the held `DEL`, then a reset: the owner
    /// counts no subscriber once quiet.
    SubscribeReset,
    /// The same with a FIN, released when its EOF is posted: every reply,
    /// then the close, and no subscriber left.
    SubscribeFin,
    /// `CLIENT SETNAME`, `SELECT` and a `SUBSCRIBE` queued behind the held
    /// `DEL`, then a reset: no row of the closed client in `CLIENT LIST`.
    RegistryReset,
    /// The same with a FIN: the three replies, then the close, and no row.
    RegistryFin,
    /// A subscriber sends `QUIT` while another client publishes to it: no
    /// message after its `+OK`, and its end follows within 2 s.
    QuitSubscriber,
    /// Four clients that end their streams while another's `DEBUG SLEEP`
    /// stalls their cell: what each sent before its end runs.
    Stall,
    /// A held `DEL`, 1,000 `SET`s and a FIN, the peer released once the EOF
    /// is posted: the `DEL`'s count and 1,000 `+OK`s, then the close.
    HalfClose,
    /// 5,000 pipelined `SET`s and a FIN, the last chunk and the FIN reaped
    /// together: 5,000 `+OK`s, then the close.
    HalfCloseEofWithData,
    /// A held `DEL`, 1,000 `SET`s and a FIN, then a reset once the EOF is
    /// posted: every `SET` lands and nothing is answered.
    HalfCloseReset,
    /// A held `DEL`, `QUIT` and 10 `SET`s in one buffer: the `DEL`'s count
    /// and the `QUIT`'s `+OK`, then the close; no `SET` runs.
    DeferredQuit,
    /// `INF.NS USE`, 10 `SET`s, a malformed frame and 10 more in one buffer:
    /// 11 `+OK`s, then the protocol error, then the close.
    ProtocolOrder,
    /// 8 tiered `SET`s of 4,096 B toward a peer on a mesh of 8 credits: the
    /// peer parks 7 of them, and its parked bytes are back at 0 once quiet.
    SmallCredit,
    /// 8 `always` `SET`s toward a peer while its fsync is held: no ack before
    /// the hold lifts, every ack after it.
    GatedHold,
    /// A tiered `always` `APPEND` to a peer's cold key while its fsync and
    /// tier reads are held, then a flat `always` `SET`: the `SET` parks
    /// behind the suspended apply, and both verdicts wait for the fsync.
    GatedCold,
    /// An `always` `INCR` that errors, read under the held fsync, then a
    /// flat `always` `SET`, acked after the hold.
    GatedError,
}

impl CloseLeg {
    /// Every leg, in the order the help text lists them.
    pub const ALL: [CloseLeg; 20] = [
        CloseLeg::Tasks,
        CloseLeg::Fan,
        CloseLeg::Close,
        CloseLeg::Rearm,
        CloseLeg::EofWithData,
        CloseLeg::SubscribeReset,
        CloseLeg::SubscribeFin,
        CloseLeg::RegistryReset,
        CloseLeg::RegistryFin,
        CloseLeg::QuitSubscriber,
        CloseLeg::Stall,
        CloseLeg::HalfClose,
        CloseLeg::HalfCloseEofWithData,
        CloseLeg::HalfCloseReset,
        CloseLeg::DeferredQuit,
        CloseLeg::ProtocolOrder,
        CloseLeg::SmallCredit,
        CloseLeg::GatedHold,
        CloseLeg::GatedCold,
        CloseLeg::GatedError,
    ];

    /// The legs red at this tree. Each runs in one ignored test of `reds`,
    /// which fails while the leg is red and fails when it reads green, so a
    /// green leg leaves this table for its `scripts/sim-smoke.sh` row, where
    /// it runs with its twin; `tests/lanes.rs` holds every leg to one of the
    /// two.
    pub const PINNED_RED: &'static [CloseLeg] = &[CloseLeg::Fan];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            CloseLeg::Tasks => "tasks",
            CloseLeg::Fan => "fan",
            CloseLeg::Close => "close",
            CloseLeg::Rearm => "rearm",
            CloseLeg::EofWithData => "eof-with-data",
            CloseLeg::SubscribeReset => "subscribe-reset",
            CloseLeg::SubscribeFin => "subscribe-fin",
            CloseLeg::RegistryReset => "registry-reset",
            CloseLeg::RegistryFin => "registry-fin",
            CloseLeg::QuitSubscriber => "quit-subscriber",
            CloseLeg::Stall => "stall",
            CloseLeg::HalfClose => "half-close",
            CloseLeg::HalfCloseEofWithData => "half-close-eof-with-data",
            CloseLeg::HalfCloseReset => "half-close-reset",
            CloseLeg::DeferredQuit => "deferred-quit",
            CloseLeg::ProtocolOrder => "protocol-order",
            CloseLeg::SmallCredit => "small-credit",
            CloseLeg::GatedHold => "gated-hold",
            CloseLeg::GatedCold => "gated-cold",
            CloseLeg::GatedError => "gated-error",
        }
    }

    #[must_use]
    pub fn parse(name: &str) -> Option<CloseLeg> {
        CloseLeg::ALL.into_iter().find(|leg| leg.name() == name)
    }
}

/// One leg's run.
#[derive(Clone, Copy, Debug)]
pub struct CapacityScenario {
    pub seed: u64,
    pub leg: CloseLeg,
}

impl CapacityScenario {
    #[must_use]
    pub fn m0_capacity(seed: u64, leg: CloseLeg) -> CapacityScenario {
        CapacityScenario { seed, leg }
    }
}

/// A leg's report: its verdict's inputs and the readings the CLI prints.
#[derive(Debug)]
pub struct CapacityReport {
    pub leg: CloseLeg,
    pub trace_hash: u64,
    pub state_hash: u64,
    pub violations: Violations,
    pub scheduler_steps: u64,
    /// The leg's own readings, `name=value`, in the order the leg took them.
    pub readings: Vec<(&'static str, u64)>,
}

impl CapacityReport {
    #[must_use]
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }
}

/// The node's cells.
const CELLS: u16 = 3;
/// The cell every leg's clients connect to, and the peer it holds.
const X: usize = 0;
const Y: usize = 1;
/// The third cell: an owner the held peer is not.
const Z: usize = 2;
/// Scheduler steps a leg's wait may take before it calls it stalled.
const STEPS_MAX: u64 = 60_000;
/// A half-closed client is closed within this many harness steps of the
/// later of its FIN and the last reply byte it read, else
/// `half-close-unclosed`: under the stall detector's 20,000.
const HALF_CLOSE_STEPS_MAX: u64 = 4_096;
/// The pump's window of remote legs, and the queue's high water.
const WINDOW: usize = 32;
const HIGH_WATER: usize = 1_024;
/// A cell's receive buffers: their count and their size, the most bytes one
/// reap hands PARSE for one connection.
const RECV_BUFFERS: usize = 128;
const RECV_BUFFER_BYTES: usize = 1_024;

/// One scripted client of the leg.
#[derive(Debug, Default)]
struct Client {
    cell: usize,
    fd: RawFd,
    rx: Vec<u8>,
    replies: Vec<Vec<u8>>,
    /// The step of the client's FIN, and of the last byte it read.
    fin_at: Option<u64>,
    last_byte_at: Option<u64>,
    /// The step at which the harness saw the server's close.
    closed_at: Option<u64>,
    /// A reset client reads nothing more.
    reset: bool,
    /// A half-closed client past its close bound, ended by the harness.
    abandoned: bool,
}

type Plane = ServerPlane<NoopObserver, StdSegmentFs>;

/// The node of one leg, its clients and its readings.
struct Rig {
    seed: u64,
    cells: Vec<(CellLoop<SimDriver, Rc<VirtualClock>>, Plane)>,
    nets: Vec<Rc<RefCell<CellNet>>>,
    clock: Rc<VirtualClock>,
    rng: SplitMix64,
    step_ns_max: u64,
    /// The cell whose iterations are skipped: its replies are held.
    held: Option<usize>,
    clients: Vec<Client>,
    steps: u64,
    trace: Vec<u8>,
    violations: Violations,
    readings: Vec<(&'static str, u64)>,
    /// The most tasks each cell's executor held after any step, while the
    /// leg reads them.
    tasks_most: Option<Vec<usize>>,
    /// Each cell's node state, for the gauges a leg reads beside the wire.
    nodes: Vec<Rc<NodeInfo>>,
    /// The cells whose O1 breach is recorded, once each.
    owners_breached: Vec<bool>,
}

impl Rig {
    /// Three cells, each with `maxclients` (0: the default) and the seeded
    /// step size.
    fn new(seed: u64, maxclients: u32, step_ns_max: u64) -> Rig {
        Rig::with_namespaces(seed, maxclients, step_ns_max, &[])
    }

    /// The same, each cell's keyspace also holding `namespaces`, created
    /// uncataloged at boot.
    fn with_namespaces(seed: u64, maxclients: u32, step_ns_max: u64, namespaces: &[NsSpec]) -> Rig {
        let clock = Rc::new(VirtualClock::new(Nanos(1)));
        let mut cells = Vec::new();
        let mut nets = Vec::new();
        let mut nodes = Vec::new();
        for (i, fabric) in Mesh::new(CELLS, FROZEN_MESH).into_iter().enumerate() {
            let cell = u16::try_from(i).expect("a u16 cell");
            let net = CellNet::new(cell, seed, Plant::None);
            // `maxclients` is the node's boot capacity, as `infinityd
            // --maxclients` sets it (ADR-0123 A1).
            let mut config = inf_server::ConfigStore::default();
            if maxclients != 0 {
                let cells = inf_foundation::CellCount::new(CELLS).expect("three cells");
                let capacity = inf_server::BootCapacity::new(maxclients, cells)
                    .expect("a leg's boot capacity");
                config = config.with_boot_capacity(capacity);
            }
            let (node, admission) = NodeInfo::try_new(config).expect("fixture cache allocation");
            let node = Rc::new(node);
            node.rng_state.set(seed ^ (0xCA9A_0000 + i as u64));
            nodes.push(Rc::clone(&node));
            let seed_store = KeyspaceSeed::Uncataloged {
                cfg: StoreConfig { hasher: node_hasher(seed), ..Default::default() },
                specs: namespaces.to_vec(),
            };
            let (plane, serve_mark, timer_set, executor) = Plane::new(
                CellId(cell),
                CELLS,
                listener_fd(cell),
                seed_store,
                fabric,
                node,
                admission,
                NoopObserver,
                false,
            )
            .unwrap_or_else(|refused| panic!("cell {i}'s assembly refused: {refused}"));
            let config = LoopConfig { spin_iters: 4, ..Default::default() };
            let driver = SimDriver::new(Rc::clone(&net));
            let pool = BufferPool::new(RECV_BUFFERS, RECV_BUFFER_BYTES);
            let cell_loop = CellLoop::new(
                driver,
                Rc::clone(&clock),
                pool,
                config,
                serve_mark,
                timer_set,
                executor,
            );
            nets.push(net);
            cells.push((cell_loop, plane));
        }
        let mut rig = Rig {
            seed,
            cells,
            nets,
            clock,
            rng: SplitMix64::new(seed ^ 0xCA9A_C17E),
            step_ns_max,
            held: None,
            clients: Vec::new(),
            steps: 0,
            trace: Vec::new(),
            violations: Violations::default(),
            readings: Vec::new(),
            tasks_most: None,
            nodes,
            owners_breached: vec![false; usize::from(CELLS)],
        };
        for _ in 0..8 {
            rig.step();
        }
        rig
    }

    fn plane(&self, cell: usize) -> &Plane {
        &self.cells[cell].1
    }

    fn violate(&mut self, text: String) {
        self.violations.push(text);
    }

    fn read(&mut self, name: &'static str, value: u64) {
        self.readings.push((name, value));
    }

    /// One scheduler step: a seeded rotation of the cells, one iteration of
    /// each but the held one, then every client's bytes and the nets' own
    /// invariants, then the seeded clock advance.
    fn step(&mut self) {
        let n = self.cells.len();
        let rotate = (self.rng.next_u64() as usize) % n;
        for i in 0..n {
            let index = (i + rotate) % n;
            if Some(index) == self.held {
                continue;
            }
            let (cell_loop, plane) = &mut self.cells[index];
            cell_loop.run_iteration(plane).expect("sim iteration");
        }
        if let Some(most) = self.tasks_most.as_mut() {
            for (cell, (cell_loop, _)) in self.cells.iter().enumerate() {
                most[cell] = most[cell].max(cell_loop.executor().live_tasks());
            }
        }
        self.judge_owners();
        for id in 0..self.clients.len() {
            self.observe(id);
        }
        drain_violations(&self.nets, &mut self.violations);
        self.clock.advance(Nanos(STEP_NS_FLOOR + self.rng.next_u64() % self.step_ns_max));
        self.steps += 1;
    }

    /// O1 on every step (ADR-0177 D2): on each cell the `conn` tasks alive
    /// equal the occupied connection slots, both within the share. Two
    /// owners report and the harness compares; each cell's first breach is
    /// recorded.
    fn judge_owners(&mut self) {
        for (cell, (cell_loop, plane)) in self.cells.iter().enumerate() {
            let Some(breach) = plane.conn_owners(cell_loop.executor()).breach() else { continue };
            if !self.owners_breached[cell] {
                self.owners_breached[cell] = true;
                let step = self.steps;
                self.violations.push(format!("O1 {breach} on cell {cell} at step {step}"));
            }
        }
    }

    /// Reads what reached client `id`, frames its replies and notes the
    /// server's close; a half-closed client past its close bound is ended
    /// as `half-close-unclosed`.
    fn observe(&mut self, id: usize) {
        let steps = self.steps;
        let client = &mut self.clients[id];
        if client.reset || client.abandoned || client.closed_at.is_some() {
            return;
        }
        let mut net = self.nets[client.cell].borrow_mut();
        let bytes = net.client_recv(client.fd);
        if !bytes.is_empty() {
            self.trace.extend_from_slice(&steps.to_le_bytes());
            self.trace.extend_from_slice(&(id as u64).to_le_bytes());
            self.trace.extend_from_slice(&bytes);
            client.rx.extend_from_slice(&bytes);
            client.last_byte_at = Some(steps);
        }
        while let Some(n) = reply_len(&client.rx) {
            client.replies.push(client.rx.drain(..n).collect());
        }
        if net.closed(client.fd) {
            client.closed_at = Some(steps);
            self.trace.extend_from_slice(&steps.to_le_bytes());
            self.trace.extend_from_slice(b"closed");
            return;
        }
        let Some(fin) = client.fin_at else { return };
        let since = fin.max(client.last_byte_at.unwrap_or(fin));
        if steps > since + HALF_CLOSE_STEPS_MAX {
            client.abandoned = true;
            net.client_abandon(client.fd);
            drop(net);
            self.violations.push(format!(
                "half-close-unclosed: client {id} still open {HALF_CLOSE_STEPS_MAX} steps after \
                 its FIN and its last reply byte"
            ));
        }
    }

    /// A client of `cell`; the net queues its connection for the next
    /// accept.
    fn connect(&mut self, cell: usize) -> usize {
        let fd = self.nets[cell].borrow_mut().connect();
        self.clients.push(Client { cell, fd, ..Client::default() });
        self.clients.len() - 1
    }

    fn send(&mut self, id: usize, bytes: &[u8]) {
        let client = &self.clients[id];
        self.nets[client.cell].borrow_mut().client_send(client.fd, bytes);
    }

    /// The client's FIN: it keeps reading until the server closes it.
    fn fin(&mut self, id: usize) {
        let client = &mut self.clients[id];
        self.nets[client.cell].borrow_mut().client_close(client.fd);
        client.fin_at = Some(self.steps);
    }

    /// The client is gone: it reads nothing more.
    fn reset(&mut self, id: usize) {
        let client = &mut self.clients[id];
        self.nets[client.cell].borrow_mut().client_reset(client.fd);
        client.reset = true;
    }

    /// Steps until `done` holds, at most `STEPS_MAX` steps; a wait that
    /// runs out is the leg's stall, `what`.
    fn until(&mut self, what: &str, done: impl Fn(&Rig) -> bool) -> bool {
        for _ in 0..STEPS_MAX {
            if done(self) {
                return true;
            }
            self.step();
        }
        let held = self.held;
        self.violate(format!("STALL {what}: {STEPS_MAX} steps (held cell {held:?})"));
        false
    }

    /// `argv` from client `id`, stepped until its reply frames.
    fn call(&mut self, id: usize, argv: &[&[u8]]) -> Vec<u8> {
        let before = self.clients[id].replies.len();
        self.send(id, &encode(argv));
        let framed = |rig: &Rig| rig.clients[id].replies.len() > before;
        if !self.until("a call", framed) {
            return Vec::new();
        }
        self.clients[id].replies[before].clone()
    }

    /// Whether the node is quiet: no byte queued toward any server, no
    /// remote work outstanding, no cell held.
    fn quiet(&self) -> bool {
        self.held.is_none()
            && self.nets.iter().all(|net| net.borrow().pending_bytes() == 0)
            && self.cells.iter().all(|(_, plane)| plane.suspended() == 0)
    }

    /// Steps until quiet, then 64 more so closes and drains complete.
    fn settle(&mut self) {
        self.until("quiescence", Rig::quiet);
        for _ in 0..64 {
            self.step();
        }
    }

    /// How many of `names` exist, read by `reader` with `EXISTS` in batches.
    fn existing(&mut self, reader: usize, names: &[Vec<u8>]) -> usize {
        let mut count = 0;
        for batch in names.chunks(256) {
            let mut argv: Vec<&[u8]> = vec![b"EXISTS"];
            argv.extend(batch.iter().map(Vec::as_slice));
            count += integer(&self.call(reader, &argv)).unwrap_or(0) as usize;
        }
        count
    }

    /// The parked-bytes gauge's control (ADR-0177 D6): a leg that parks no
    /// work for a peer reads both the gauge and its high water at 0 on
    /// every cell.
    fn parked_nothing(&mut self) {
        for cell in 0..self.nodes.len() {
            let (now, max) = (
                self.nodes[cell].fabric_parked_bytes.get(),
                self.nodes[cell].fabric_parked_bytes_max.get(),
            );
            if now != 0 || max != 0 {
                self.violate(format!(
                    "parked-bytes-control: cell {cell} parked nothing and reads \
                     fabric_parked_bytes {now}, its high water {max}"
                ));
            }
        }
    }

    /// A close's drain counter of cell X, read beside the wire (`INFO
    /// stats`' own lines): the leg's engagement and a reading it prints.
    fn drain_count(&mut self, name: &'static str, read: fn(&NodeInfo) -> u64) -> u64 {
        let value = read(&self.nodes[X]);
        self.read(name, value);
        value
    }

    /// The connection slots `cell` holds now.
    fn occupied(&self, cell: usize) -> usize {
        self.plane(cell).connections()
    }

    /// The leg's report: every cell's cap census judged after its last
    /// iteration (the cap bound, the assembly rule and their liveness), and
    /// the state hash over the readings and every cell's keyspace.
    fn report(mut self, leg: CloseLeg) -> CapacityReport {
        // No op refused at a full per-origin deque (ADR-0177 D6; derived
        // never).
        for cell in 0..self.nodes.len() {
            let refused = self.nodes[cell].fabric_parked_refused.get();
            if refused != 0 {
                self.violate(format!("parked-refused: cell {cell}, {refused} op(s)"));
            }
        }
        let mut census = CensusVerdict::default();
        for (index, (_, plane)) in self.cells.iter().enumerate() {
            let facts = CellFacts {
                server_plane: true,
                peers: CELLS - 1,
                tiered_tables: plane.keyspace().tiered_tables(),
                turn_refused: plane.durable_stats().map(|stats| stats.turn_refused),
            };
            census.judge(index, plane.cap_census(), facts);
        }
        self.read("cap_rows_checked", census.rows_checked);
        self.read("cap_rows_crossed", census.rows_crossed);
        self.violations.extend(census.violations);
        let now = self.clock_now();
        let mut state = StateHash::default();
        for (name, value) in &self.readings {
            state.number(name.as_bytes(), *value);
        }
        state.number(b"finish", now.0);
        state.number(b"steps", self.steps);
        for (index, (_, plane)) in self.cells.iter().enumerate() {
            state.number(b"cell", index as u64);
            state.keyspace(&plane.keyspace(), now);
        }
        CapacityReport {
            leg,
            trace_hash: hash64(&self.trace, self.seed),
            state_hash: state.value(),
            violations: self.violations,
            scheduler_steps: self.steps,
            readings: self.readings,
        }
    }
}

/// `argv` as one RESP multibulk frame.
fn encode(argv: &[&[u8]]) -> Vec<u8> {
    crate::durable::encode(argv)
}

/// A pipeline of `SET name 1` for every name.
fn sets(names: &[Vec<u8>]) -> Vec<u8> {
    names.iter().flat_map(|name| encode(&[b"SET", name, b"1"])).collect()
}

/// An integer reply's value.
fn integer(reply: &[u8]) -> Option<u64> {
    let text = core::str::from_utf8(reply.strip_prefix(b":")?).ok()?;
    text.trim_end().parse().ok()
}

/// `count` names that `owner` owns on the leg's node, after `stem`.
fn names_on(owner: usize, stem: &str, count: usize) -> Vec<Vec<u8>> {
    let router = SlotRouter::new_contiguous(CELLS);
    (0..)
        .map(|n| format!("{stem}:{n}").into_bytes())
        .filter(|name| router.cell_of(SlotRouter::slot_of(name)).0 as usize == owner)
        .take(count)
        .collect()
}

/// `count` names spread over every cell, after `stem`.
fn names_anywhere(stem: &str, count: usize) -> Vec<Vec<u8>> {
    (0..count).map(|n| format!("{stem}:{n}").into_bytes()).collect()
}

/// `n` `+OK` replies.
fn oks(n: usize) -> Vec<Vec<u8>> {
    vec![b"+OK\r\n".to_vec(); n]
}

/// Runs one leg.
#[must_use]
pub fn run_capacity_scenario(scenario: &CapacityScenario) -> CapacityReport {
    let seed = scenario.seed;
    let leg = scenario.leg;
    let rig = match leg {
        CloseLeg::Tasks => tasks_leg(seed),
        CloseLeg::Fan => fan_leg(seed),
        CloseLeg::Close => close_leg(seed),
        CloseLeg::Rearm => rearm_leg(seed),
        CloseLeg::EofWithData => eof_with_data_leg(seed),
        CloseLeg::SubscribeReset => subscribe_leg(seed, Form::Reset),
        CloseLeg::SubscribeFin => subscribe_leg(seed, Form::Fin),
        CloseLeg::RegistryReset => registry_leg(seed, Form::Reset),
        CloseLeg::RegistryFin => registry_leg(seed, Form::Fin),
        CloseLeg::QuitSubscriber => quit_subscriber_leg(seed),
        CloseLeg::Stall => stall_leg(seed),
        CloseLeg::HalfClose => half_close_leg(seed),
        CloseLeg::HalfCloseEofWithData => half_close_shared_leg(seed),
        CloseLeg::HalfCloseReset => half_close_reset_leg(seed),
        CloseLeg::DeferredQuit => deferred_quit_leg(seed),
        CloseLeg::ProtocolOrder => protocol_order_leg(seed),
        // A durable node on its own mesh, judged by its own oracles.
        CloseLeg::SmallCredit => return small_credit::small_credit_leg(seed),
        CloseLeg::GatedHold => return gated::gated_hold_leg(seed),
        CloseLeg::GatedCold => return gated::gated_cold_leg(seed),
        CloseLeg::GatedError => return gated::gated_error_leg(seed),
    };
    rig.report(leg)
}

/// How a client of a close leg ends: its socket reset (the peer gone), or
/// its FIN.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Form {
    Reset,
    Fin,
}

// ---- the shared readings -------------------------------------------------------

impl Rig {
    fn fd(&self, id: usize) -> RawFd {
        self.clients[id].fd
    }

    /// Whether the server has closed client `id`.
    fn server_closed(&self, id: usize) -> bool {
        let client = &self.clients[id];
        self.nets[client.cell].borrow().closed(client.fd)
    }

    /// Whether a half-closed client is done: closed, or ended past its bound.
    fn ended(&self, id: usize) -> bool {
        let client = &self.clients[id];
        client.closed_at.is_some() || client.abandoned
    }

    /// A client admitted on `cell`: its `PING` answered.
    fn admitted(&mut self, cell: usize) -> usize {
        let id = self.connect(cell);
        let pong = self.call(id, &[b"PING"]);
        if pong != b"+PONG\r\n" {
            let text = String::from_utf8_lossy(&pong).into_owned();
            self.violate(format!("STALL admission: a leg client on cell {cell} read {text:?}"));
        }
        id
    }

    /// The replies client `id` read after its first `skip`, against those it
    /// is owed: fewer is `half-close-unanswered`, a wrong one `reply-wrong`,
    /// more or a partial frame after them `byte-after-replies`.
    fn judge_replies(&mut self, id: usize, skip: usize, owed: &[Vec<u8>], who: &str) {
        let client = &self.clients[id];
        let read = &client.replies[skip.min(client.replies.len())..];
        let matched = read.iter().zip(owed).take_while(|(read, owed)| read == owed).count();
        let (read_len, trailing) = (read.len(), client.rx.len());
        if read_len < owed.len() && matched == read_len {
            self.violate(format!(
                "half-close-unanswered: {who} read {read_len} of the {} replies it is owed",
                owed.len()
            ));
        } else if matched < owed.len() {
            let wrong = String::from_utf8_lossy(&read[matched]).into_owned();
            self.violate(format!("reply-wrong: {who}'s reply {matched} read {wrong:?}"));
        } else if read_len > owed.len() || trailing > 0 {
            self.violate(format!(
                "byte-after-replies: {who} read {} replies and {trailing} bytes past its {}",
                read_len - owed.len(),
                owed.len()
            ));
        }
    }

    /// Every one of `names` is set, read by `reader`, else `token`.
    fn expect_landed(&mut self, reader: usize, names: &[Vec<u8>], token: &str, who: &str) {
        let landed = self.existing(reader, names);
        self.read("landed", landed as u64);
        if landed < names.len() {
            self.violate(format!("{token}: {landed} of {who}'s {} SETs ran", names.len()));
        }
    }

    /// The leg's clients of `cell` have left: only `kept` slots are
    /// occupied, the close's slots vacant.
    fn expect_vacant(&mut self, cell: usize, kept: usize) {
        let occupied = self.occupied(cell);
        if occupied != kept {
            self.violate(format!(
                "slot-not-vacant: cell {cell} holds {occupied} connection slots, its {kept} \
                 surviving clients'"
            ));
        }
    }

    /// The `DEL` of a window of keys `owner` owns, which holds the pump's
    /// window while `owner` is held, then `rest`.
    fn held_pipeline(owner: usize, rest: &[u8]) -> Vec<u8> {
        let held = names_on(owner, "held", WINDOW);
        let mut argv: Vec<&[u8]> = vec![b"DEL"];
        argv.extend(held.iter().map(Vec::as_slice));
        let mut bytes = encode(&argv);
        bytes.extend_from_slice(rest);
        bytes
    }
}

/// The reply of a `DEL` of keys that do not exist.
const DEL_NONE: &[u8] = b":0\r\n";

/// A `SUBSCRIBE` confirmation of `channel`, the client's first subscription.
fn subscribed(channel: &[u8]) -> Vec<u8> {
    let mut frame = b"*3\r\n$9\r\nsubscribe\r\n".to_vec();
    frame.extend_from_slice(format!("${}\r\n", channel.len()).as_bytes());
    frame.extend_from_slice(channel);
    frame.extend_from_slice(b"\r\n:1\r\n");
    frame
}

/// The last integer of a reply (`PUBSUB NUMSUB`'s count).
fn last_integer(reply: &[u8]) -> Option<u64> {
    let text = core::str::from_utf8(reply).ok()?;
    text.split("\r\n").filter_map(|line| line.strip_prefix(':')).last()?.parse().ok()
}

// ---- E's legs: the tasks and the holders a cell keeps ------------------------------

/// A cell's connection share in the capacity legs: `maxclients` 12 on three
/// cells.
const SHARE: usize = 4;
/// The pumps plane start makes on the leg's node (ADR-0177 D2): one
/// `ns-apply` per peer, the `owner-pub` and the `gated` pump.
const PLANE_START_TASKS: usize = CELLS as usize + 1;

/// Pattern subscribers on cell 0 close while cell 2 holds their deltas'
/// acks, in three waves of the share, then 48 clients churn through cells 0
/// and 1. The most tasks a cell held at any step stays within its share and
/// its plane-start pumps; at HEAD each closed subscriber's delta is a task
/// of its own. Once quiet no cell counts a pattern: every closed
/// subscriber's delta reached every peer.
/// Engagement: the first wave confirmed, then closed while cell 2 was held,
/// cell 0 owing a delta after the closes, else `VACUOUS tasks-unengaged`.
fn tasks_leg(seed: u64) -> Rig {
    let mut rig = Rig::new(seed, (SHARE * usize::from(CELLS)) as u32, 15_000);
    rig.tasks_most = Some(vec![0; usize::from(CELLS)]);
    let first: Vec<usize> = (0..SHARE).map(|_| rig.connect(X)).collect();
    for (n, &id) in first.iter().enumerate() {
        rig.send(id, &encode(&[b"PSUBSCRIBE", format!("tasks:{n}:*").as_bytes()]));
    }
    let confirmed = rig.until("the first wave's confirmations", |rig| {
        first.iter().all(|&id| !rig.clients[id].replies.is_empty())
    });
    rig.held = Some(Z);
    for &id in &first {
        rig.fin(id);
    }
    let closed = rig.until("the first wave's closes", |rig| first.iter().all(|&id| rig.ended(id)));
    let owed = rig.plane(X).suspended();
    rig.read("deltas_owed", owed as u64);
    for wave in 1..3 {
        let ids: Vec<usize> = (0..SHARE).map(|_| rig.connect(X)).collect();
        for (n, &id) in ids.iter().enumerate() {
            let pattern = format!("tasks:{wave}:{n}:*");
            rig.send(id, &encode(&[b"PSUBSCRIBE", pattern.as_bytes()]));
            rig.fin(id);
        }
        rig.until("a later wave's closes", |rig| ids.iter().all(|&id| rig.ended(id)));
    }
    for _ in 0..6 {
        let ids: Vec<usize> =
            (0..2 * SHARE).map(|n| rig.connect(if n % 2 == 0 { X } else { Y })).collect();
        for &id in &ids {
            rig.send(id, &encode(&[b"PING"]));
            rig.fin(id);
        }
        rig.until("a churn wave's closes", |rig| ids.iter().all(|&id| rig.ended(id)));
    }
    rig.held = None;
    rig.settle();
    let most = rig.tasks_most.take().unwrap_or_default();
    for (cell, &tasks) in most.iter().enumerate() {
        rig.read("tasks_most", tasks as u64);
        if tasks > SHARE + PLANE_START_TASKS {
            rig.violate(format!(
                "tasks-over-share: cell {cell} held {tasks} tasks at once, past its share of \
                 {SHARE} connections and {PLANE_START_TASKS} plane-start pumps"
            ));
        }
    }
    for cell in 0..usize::from(CELLS) {
        rig.expect_vacant(cell, 0);
    }
    rig.expect_no_pattern();
    if !(confirmed && closed && owed >= 1) {
        let text = "VACUOUS tasks-unengaged: the first wave never closed owing a held delta";
        rig.violate(text.to_string());
    }
    rig
}

impl Rig {
    /// No cell counts a pattern, each read over the wire by a fresh client
    /// of its own (`PUBSUB NUMPAT`), else `pattern-count-after-close`.
    fn expect_no_pattern(&mut self) {
        for cell in 0..usize::from(CELLS) {
            let probe = self.admitted(cell);
            let count = integer(&self.call(probe, &[b"PUBSUB", b"NUMPAT"]));
            self.read("patterns", count.unwrap_or(u64::MAX));
            if count != Some(0) {
                self.violate(format!(
                    "pattern-count-after-close: cell {cell} counts {count:?} patterns once quiet"
                ));
            }
        }
    }
}

/// The share's clients on cell 0 each send a `DEL` of 100 keys cell 1 owns
/// while cell 1 is held: the remote legs the cell holds (gate waiters and
/// credit waits) stay within the share's windows of 32; at HEAD each sends a
/// leg per key. Then every reply is the exact count.
fn fan_leg(seed: u64) -> Rig {
    const KEYS: usize = 100;
    let mut rig = Rig::new(seed, (SHARE * usize::from(CELLS)) as u32, 15_000);
    let fanners: Vec<usize> = (0..SHARE).map(|_| rig.admitted(X)).collect();
    rig.held = Some(Y);
    let keys = names_on(Y, "fan", KEYS);
    let mut argv: Vec<&[u8]> = vec![b"DEL"];
    argv.extend(keys.iter().map(Vec::as_slice));
    let del = encode(&argv);
    for &id in &fanners {
        rig.send(id, &del);
    }
    rig.until("the fans' bytes taken", |rig| {
        fanners.iter().all(|&id| rig.nets[X].borrow().unread(rig.fd(id)) == Some(0))
    });
    for _ in 0..64 {
        rig.step();
    }
    let held = rig.plane(X).suspended();
    rig.read("held_legs", held as u64);
    if held > SHARE * WINDOW {
        rig.violate(format!(
            "fan-over-window: cell {X} holds {held} remote legs for {SHARE} connections, past \
             {SHARE} windows of {WINDOW}"
        ));
    }
    rig.held = None;
    rig.until("the fans' replies", |rig| {
        fanners.iter().all(|&id| rig.clients[id].replies.len() > 1)
    });
    for &id in &fanners {
        rig.judge_replies(id, 1, &[DEL_NONE.to_vec()], "a fan");
    }
    rig.settle();
    if held == 0 {
        rig.violate("VACUOUS fan-unengaged: no remote leg was held".to_string());
    }
    rig
}

// ---- E-close's legs: a close runs the bytes its client handed over ------------------

/// A `DEL` of a window of keys cell 1 holds, 1,023 `SET`s of keys cell 0
/// owns queued behind it (under the high water, so no disarm), then the
/// client's reset; cell 1's replies are released after the close. Every
/// queued `SET` runs, unanswered (`closed-queue-unrun`); the plane sends the
/// reset client nothing; its slot is vacant at the end. Engagement: cell 1
/// still held the `DEL`'s legs when the close completed.
fn close_leg(seed: u64) -> Rig {
    let mut rig = Rig::new(seed, 0, 15_000);
    let reader = rig.admitted(X);
    let client = rig.admitted(X);
    rig.held = Some(Y);
    let names = names_on(X, "close", HIGH_WATER - 1);
    rig.send(client, &Rig::held_pipeline(Y, &sets(&names)));
    rig.reset(client);
    rig.until("the reset client's close", |rig| rig.server_closed(client));
    let engaged = rig.plane(X).suspended() >= WINDOW;
    rig.held = None;
    rig.settle();
    rig.expect_landed(reader, &names, "closed-queue-unrun", "the reset client");
    let answered = rig.nets[X].borrow().reset_send_errors();
    if answered != 0 {
        rig.violate(format!(
            "reply-to-reset-client: {answered} replies were sent to a reset client"
        ));
    }
    rig.expect_vacant(X, 1);
    let drained = rig.drain_count("closed_queue_commands", |n| n.closed_queue_commands.get());
    if !engaged || drained < names.len() as u64 {
        let text = "VACUOUS closed-queue-unengaged: the held peer answered before the close, or \
                    the queue was not dequeued from a closed slot";
        rig.violate(text.to_string());
    }
    rig
}

/// The close leg with 1,024 `SET`s: the queue reaches its high water and the
/// receive is disarmed, the client sends its FIN, and cell 1's replies are
/// released at it; the pump drains, the receive is re-armed and the end of
/// stream then meets a running pump. Every `SET` lands and the client reads
/// every reply, then the close. Engagement: the net posted the EOF of an fd
/// re-armed after its client's FIN, else `VACUOUS rearm-unengaged`.
fn rearm_leg(seed: u64) -> Rig {
    let mut rig = Rig::new(seed, 0, 15_000);
    let reader = rig.admitted(X);
    let client = rig.admitted(X);
    rig.held = Some(Y);
    let names = names_on(X, "rearm", HIGH_WATER);
    rig.send(client, &Rig::held_pipeline(Y, &sets(&names)));
    rig.until("the high water's disarm", |rig| rig.nets[X].borrow().recv_disarmed(rig.fd(client)));
    rig.fin(client);
    rig.held = None;
    rig.until("the client's close", |rig| rig.ended(client));
    rig.settle();
    rig.expect_landed(reader, &names, "rearm-queue-unrun", "the re-armed client");
    let mut owed = vec![DEL_NONE.to_vec()];
    owed.extend(oks(HIGH_WATER));
    rig.judge_replies(client, 1, &owed, "the re-armed client");
    rig.expect_vacant(X, 1);
    if rig.nets[X].borrow().eof_after_rearm() == 0 {
        rig.violate("VACUOUS rearm-unengaged: no EOF followed a re-arm after a FIN".to_string());
    }
    rig
}

/// A `SET` and the FIN, the net posting the chunk and the EOF in one reap:
/// the `SET` lands (`pre-eof-unrun`) and its `+OK` is read, then the close.
/// Engagement: a shared reap, else `VACUOUS eof-shared-unengaged`.
fn eof_with_data_leg(seed: u64) -> Rig {
    let mut rig = Rig::new(seed, 0, 15_000);
    let reader = rig.admitted(X);
    rig.nets[X].borrow_mut().set_eof_with_data(true);
    let writer = rig.connect(X);
    let name = names_on(X, "pre-eof", 1);
    rig.send(writer, &sets(&name));
    rig.fin(writer);
    rig.until("the writer's close", |rig| rig.ended(writer));
    rig.settle();
    rig.expect_landed(reader, &name, "pre-eof-unrun", "the writer");
    rig.judge_replies(writer, 0, &oks(1), "the writer");
    if rig.nets[X].borrow().eof_shared_reaps() == 0 {
        let text = "VACUOUS eof-shared-unengaged: no reap posted a chunk and its EOF";
        rig.violate(text.to_string());
    }
    rig
}

/// Ends client `id` in `form` while cell 1 holds its pipeline, and releases
/// cell 1: after the close (a reset), or once the net posted the client's
/// EOF (a FIN), since its slot stays flushing until the replies are sent.
/// Returns the form's engagement: cell 1 held the window at the close, or
/// the net received a `Send` for the fd after its EOF.
fn end_held(rig: &mut Rig, id: usize, form: Form) -> bool {
    match form {
        Form::Reset => {
            rig.reset(id);
            rig.until("the reset client's close", |rig| rig.server_closed(id));
            let engaged = rig.plane(X).suspended() >= WINDOW;
            rig.held = None;
            rig.settle();
            engaged
        }
        Form::Fin => {
            rig.fin(id);
            rig.until("the client's EOF", |rig| rig.nets[X].borrow().eof_posted(rig.fd(id)));
            rig.held = None;
            rig.until("the client's close", |rig| rig.ended(id));
            rig.settle();
            rig.nets[X].borrow().sends_after_eof(rig.fd(id)) > 0
        }
    }
}

/// 1,022 `SET`s and a `SUBSCRIBE` of a channel cell 2 owns queued behind
/// the held `DEL` (1,023 queued, under the high water). When quiet the owner
/// counts no subscriber of the channel (`sub-count-after-close`); the FIN
/// form also reads every reply and the confirmation, then the close.
fn subscribe_leg(seed: u64, form: Form) -> Rig {
    let mut rig = Rig::new(seed, 0, 15_000);
    let owner = rig.admitted(Z);
    let client = rig.admitted(X);
    let channel = names_on(Z, "sub-channel", 1).remove(0);
    let names = names_on(X, "sub", HIGH_WATER - 2);
    let mut rest = sets(&names);
    rest.extend(encode(&[b"SUBSCRIBE", &channel]));
    rig.held = Some(Y);
    rig.send(client, &Rig::held_pipeline(Y, &rest));
    let engaged = end_held(&mut rig, client, form);
    let count = last_integer(&rig.call(owner, &[b"PUBSUB", b"NUMSUB", &channel]));
    rig.read("subscribers", count.unwrap_or(u64::MAX));
    if count != Some(0) {
        rig.violate(format!(
            "sub-count-after-close: the owner counts {count:?} subscribers of a closed \
             client's channel"
        ));
    }
    if form == Form::Fin {
        let mut owed = vec![DEL_NONE.to_vec()];
        owed.extend(oks(names.len()));
        owed.push(subscribed(&channel));
        rig.judge_replies(client, 1, &owed, "the subscribing client");
    }
    rig.expect_vacant(X, 0);
    let drained = rig.drain_count("closed_queue_commands", |n| n.closed_queue_commands.get());
    let engaged = engaged && (form == Form::Fin || drained > names.len() as u64);
    if !engaged {
        let token = match form {
            Form::Reset => "closed-queue-unengaged",
            Form::Fin => "sub-fin-unengaged",
        };
        rig.violate(format!("VACUOUS {token}: the close met no held pipeline"));
    }
    rig
}

/// The client reads its `CLIENT ID` reply, then queues `CLIENT SETNAME
/// ghost`, `SELECT 1` and a `SUBSCRIBE` of a channel cell 0 owns behind the
/// held `DEL`. Once its slot is vacant a surviving client's `CLIENT LIST`
/// holds no row with its id and no `name=ghost` (`ghost-client-row`); the
/// FIN form also reads the replies, then the close.
fn registry_leg(seed: u64, form: Form) -> Rig {
    let mut rig = Rig::new(seed, 0, 15_000);
    let reader = rig.admitted(X);
    let client = rig.admitted(X);
    let id = integer(&rig.call(client, &[b"CLIENT", b"ID"])).unwrap_or(0);
    let channel = names_on(X, "registry-channel", 1).remove(0);
    let mut rest = encode(&[b"CLIENT", b"SETNAME", b"ghost"]);
    rest.extend(encode(&[b"SELECT", b"1"]));
    rest.extend(encode(&[b"SUBSCRIBE", &channel]));
    rig.held = Some(Y);
    rig.send(client, &Rig::held_pipeline(Y, &rest));
    let engaged = end_held(&mut rig, client, form);
    rig.expect_vacant(X, 1);
    let list = String::from_utf8_lossy(&rig.call(reader, &[b"CLIENT", b"LIST"])).into_owned();
    let row = list.lines().any(|line| line.contains(&format!("id={id} ")));
    if row || list.contains("name=ghost ") {
        rig.violate(format!("ghost-client-row: CLIENT LIST names the closed client {id}"));
    }
    if form == Form::Fin {
        let owed =
            [DEL_NONE.to_vec(), b"+OK\r\n".to_vec(), b"+OK\r\n".to_vec(), subscribed(&channel)];
        rig.judge_replies(client, 2, &owed, "the registry client");
    }
    let drained = rig.drain_count("closed_queue_commands", |n| n.closed_queue_commands.get());
    let engaged = engaged && (form == Form::Fin || drained >= 3);
    if !engaged {
        let token = match form {
            Form::Reset => "registry-unengaged",
            Form::Fin => "registry-fin-unengaged",
        };
        rig.violate(format!("VACUOUS {token}: the close met no held pipeline"));
    }
    rig
}

/// How long a marked subscriber's end may follow its `QUIT`'s `+OK`, in
/// virtual time, while it is still published to.
const QUIT_FLUSH_NS_MAX: u64 = 2_000_000_000;

/// A subscriber of a channel cell 0 owns reads its confirmation and a
/// message, then sends `QUIT`, while another client of the cell publishes to
/// the channel at every step. No message follows its `+OK`
/// (`push-after-quit`), its end follows within 2 virtual seconds
/// (`flush-unbounded`), and no `PUBLISH` counts more than one receiver.
/// Engagement: a `PUBLISH` was sent between the `QUIT` and its `+OK`.
fn quit_subscriber_leg(seed: u64) -> Rig {
    let mut rig = Rig::new(seed, 0, 15_000);
    let channel = names_on(X, "quit-channel", 1).remove(0);
    let subscriber = rig.connect(X);
    let confirmation = rig.call(subscriber, &[b"SUBSCRIBE", &channel]);
    if confirmation != subscribed(&channel) {
        rig.violate("STALL quit-subscriber: no confirmation".to_string());
    }
    let publisher = rig.admitted(X);
    let mut published = 0u64;
    for _ in 0..STEPS_MAX {
        if rig.clients[subscriber].replies.len() > 1 {
            break;
        }
        rig.publish(publisher, &channel, &mut published);
    }
    rig.send(subscriber, &encode(&[b"QUIT"]));
    let quit_sent = published;
    let mut ok_at: Option<(Nanos, u64)> = None;
    for _ in 0..STEPS_MAX {
        if rig.ended(subscriber) {
            break;
        }
        let ok = rig.clients[subscriber].replies.iter().position(|r| r.as_slice() == b"+OK\r\n");
        if ok.is_some() && ok_at.is_none() {
            ok_at = Some((rig.clock_now(), published));
        }
        if let Some((at, _)) = ok_at
            && rig.clock_now().0 > at.0 + QUIT_FLUSH_NS_MAX
        {
            rig.violate("flush-unbounded: the subscriber's end did not follow its +OK".to_string());
            break;
        }
        rig.publish(publisher, &channel, &mut published);
    }
    rig.until("the publisher's replies", |rig| {
        rig.clients[publisher].replies.len() as u64 > published
    });
    let replies = &rig.clients[subscriber].replies;
    let after_ok =
        replies.iter().position(|r| r.as_slice() == b"+OK\r\n").map(|at| replies.len() - at - 1);
    if let Some(after) = after_ok.filter(|&after| after > 0) {
        rig.violate(format!("push-after-quit: {after} frames followed the subscriber's +OK"));
    }
    let over = rig.clients[publisher]
        .replies
        .iter()
        .skip(1)
        .filter_map(|r| integer(r))
        .filter(|&n| n > 1)
        .count();
    if over > 0 {
        rig.violate(format!("publish-count-over: {over} PUBLISHes counted more than one receiver"));
    }
    rig.settle();
    let dropped =
        rig.drain_count("flushing_deliveries_dropped", |n| n.flushing_deliveries_dropped.get());
    let engaged = ok_at.is_some_and(|(_, at)| at > quit_sent) && dropped >= 1;
    if !engaged {
        let text = "VACUOUS quit-sub-unengaged: no PUBLISH between the QUIT and its +OK";
        rig.violate(text.to_string());
    }
    rig
}

impl Rig {
    fn clock_now(&self) -> Nanos {
        use inf_foundation::time::Clock;
        self.clock.now()
    }

    /// One `PUBLISH` to `channel` from `publisher`, then one step.
    fn publish(&mut self, publisher: usize, channel: &[u8], published: &mut u64) {
        let payload = format!("m{published}");
        self.send(publisher, &encode(&[b"PUBLISH", channel, payload.as_bytes()]));
        self.step();
        *published += 1;
    }
}

/// A stall across closes, on coarse steps: a client's `DEBUG SLEEP 1`; in
/// the sleep four clients of the cell end their streams: B 100 `SET`s and a
/// FIN, C a FIN alone, D 100 `SET`s and a reset, E 150 `SET`s, `QUIT`, 200
/// `SET`s and a FIN. When quiet every name of B and D and every name of E's
/// first 150 is set (`pre-eof-unrun`), none of E's last 200
/// (`post-quit-ran`); B reads 100 `+OK`s and E 151, each then the close; the
/// four slots are vacant. Engagement: B's and E's EOFs were posted before the
/// sleep's end and each fd took a `Send` after it, else `VACUOUS
/// stall-close-unengaged`.
fn stall_leg(seed: u64) -> Rig {
    const SLEEP_NS: u64 = 1_000_000_000;
    let mut rig = Rig::new(seed, 0, 1_000_000);
    let reader = rig.admitted(X);
    let sleeper = rig.admitted(X);
    let [b, c, d, e] = [0; 4].map(|_| rig.admitted(X));
    rig.send(sleeper, &encode(&[b"DEBUG", b"SLEEP", b"1"]));
    let slept = rig.clock_now();
    rig.until("the sleep's start", |rig| rig.nets[X].borrow().unread(rig.fd(sleeper)) == Some(0));
    rig.step();
    let names_b = names_on(X, "stall-b", 100);
    let names_d = names_on(X, "stall-d", 100);
    let names_ea = names_on(X, "stall-ea", 150);
    let names_eb = names_on(X, "stall-eb", 200);
    rig.send(b, &sets(&names_b));
    rig.fin(b);
    rig.fin(c);
    rig.send(d, &sets(&names_d));
    rig.reset(d);
    let mut quit = sets(&names_ea);
    quit.extend(encode(&[b"QUIT"]));
    quit.extend(sets(&names_eb));
    rig.send(e, &quit);
    rig.fin(e);
    rig.until("the sleep's end", |rig| rig.clock_now().0 >= slept.0 + SLEEP_NS);
    let eofs_in_sleep = {
        let net = rig.nets[X].borrow();
        net.eof_posted(rig.fd(b)) && net.eof_posted(rig.fd(e))
    };
    rig.until("the sleeper's +OK", |rig| rig.clients[sleeper].replies.len() > 1);
    rig.until("the four closes", |rig| {
        [b, c, e].iter().all(|&id| rig.ended(id)) && rig.server_closed(d)
    });
    rig.settle();
    rig.expect_landed(reader, &names_b, "pre-eof-unrun", "client B");
    rig.expect_landed(reader, &names_d, "pre-eof-unrun", "client D");
    rig.expect_landed(reader, &names_ea, "pre-eof-unrun", "client E before its QUIT");
    let after_quit = rig.existing(reader, &names_eb);
    if after_quit > 0 {
        rig.violate(format!("post-quit-ran: {after_quit} of E's SETs behind its QUIT ran"));
    }
    rig.judge_replies(b, 1, &oks(100), "client B");
    rig.judge_replies(c, 1, &[], "client C");
    rig.judge_replies(e, 1, &oks(151), "client E");
    rig.expect_vacant(X, 2);
    let net = rig.nets[X].borrow();
    let sent = net.sends_after_eof(rig.fd(b)) > 0 && net.sends_after_eof(rig.fd(e)) > 0;
    drop(net);
    let entries = rig.drain_count("closed_reader_entries", |n| n.closed_reader_entries.get());
    let waits = rig.drain_count("reader_take_waits", |n| n.reader_take_waits.get());
    if !(eofs_in_sleep && sent && entries >= 1 && waits >= 3) {
        let text = "VACUOUS stall-close-unengaged: B's and E's ends were not read in the sleep, \
                    or no held entry parsed past Closed and no run-out waited for a take";
        rig.violate(text.to_string());
    }
    rig
}

/// A held `DEL`, 1,000 `SET`s of keys the three cells own and a FIN, under
/// the high water so no disarm withholds the EOF; cell 1's replies are
/// released once the net posted the EOF, so the window holds every reply
/// past it. Every `SET` lands, the client reads the `DEL`'s count and 1,000
/// `+OK`s in order, then the close. Engagement: the fd took a `Send` after
/// its EOF, else `VACUOUS half-close-unengaged`.
fn half_close_leg(seed: u64) -> Rig {
    const SETS: usize = 1_000;
    let mut rig = Rig::new(seed, 0, 15_000);
    let reader = rig.admitted(X);
    let client = rig.admitted(X);
    let names = names_anywhere("half", SETS);
    rig.held = Some(Y);
    rig.send(client, &Rig::held_pipeline(Y, &sets(&names)));
    rig.fin(client);
    rig.until("the client's EOF", |rig| rig.nets[X].borrow().eof_posted(rig.fd(client)));
    rig.held = None;
    rig.until("the half-closed client's close", |rig| rig.ended(client));
    rig.settle();
    rig.expect_landed(reader, &names, "half-close-queue-unrun", "the half-closed client");
    let mut owed = vec![DEL_NONE.to_vec()];
    owed.extend(oks(SETS));
    rig.judge_replies(client, 1, &owed, "the half-closed client");
    rig.expect_vacant(X, 1);
    rig.expect_sent_after_eof(client);
    rig
}

/// 5,000 pipelined `SET`s of keys the three cells own and a FIN, the net
/// posting the last chunk and the EOF in one reap (the queue passes its high
/// water on the way, so the receive is disarmed and re-armed): every `SET`
/// lands, the client reads 5,000 `+OK`s in order, then the close.
/// Engagement: the fd took a `Send` after its EOF, else `VACUOUS
/// half-close-unengaged`, and a reap posted a chunk with its EOF, else
/// `VACUOUS eof-shared-unengaged`.
fn half_close_shared_leg(seed: u64) -> Rig {
    const SETS: usize = 5_000;
    let mut rig = Rig::new(seed, 0, 15_000);
    let reader = rig.admitted(X);
    rig.nets[X].borrow_mut().set_eof_with_data(true);
    let client = rig.connect(X);
    let names = names_anywhere("half", SETS);
    rig.send(client, &sets(&names));
    rig.fin(client);
    rig.until("the half-closed client's close", |rig| rig.ended(client));
    rig.settle();
    rig.expect_landed(reader, &names, "half-close-queue-unrun", "the half-closed client");
    rig.judge_replies(client, 0, &oks(SETS), "the half-closed client");
    rig.expect_vacant(X, 1);
    rig.expect_sent_after_eof(client);
    if rig.nets[X].borrow().eof_shared_reaps() == 0 {
        let text = "VACUOUS eof-shared-unengaged: no reap posted a chunk and its EOF";
        rig.violate(text.to_string());
    }
    rig
}

impl Rig {
    /// The half-close forms' engagement: the fd took a `Send` after its
    /// EOF, else `VACUOUS half-close-unengaged`.
    fn expect_sent_after_eof(&mut self, id: usize) {
        if self.nets[X].borrow().sends_after_eof(self.fd(id)) == 0 {
            let text = "VACUOUS half-close-unengaged: no Send followed the client's EOF";
            self.violate(text.to_string());
        }
    }
}

/// A held `DEL`, 1,000 `SET`s of keys the three cells own and a FIN; once
/// the net posted the EOF the client resets, and cell 1's replies are
/// released after it. The window holds the `SET`s queued at the reset, so
/// the reset reaches the plane as a `Send`'s `ECONNRESET`. Every `SET`
/// lands and nothing is read. Engagement: a `Send` was answered
/// `ECONNRESET`, else `VACUOUS reset-drain-unengaged`.
fn half_close_reset_leg(seed: u64) -> Rig {
    const SETS: usize = 1_000;
    let mut rig = Rig::new(seed, 0, 15_000);
    let reader = rig.admitted(X);
    let client = rig.admitted(X);
    let names = names_anywhere("reset-drain", SETS);
    rig.held = Some(Y);
    rig.send(client, &Rig::held_pipeline(Y, &sets(&names)));
    rig.fin(client);
    rig.until("the client's EOF", |rig| rig.nets[X].borrow().eof_posted(rig.fd(client)));
    rig.reset(client);
    rig.held = None;
    rig.until("the reset client's close", |rig| rig.server_closed(client));
    rig.settle();
    rig.expect_landed(reader, &names, "reset-drain-unrun", "the reset client");
    rig.expect_vacant(X, 1);
    let drained = rig.drain_count("closed_queue_commands", |n| n.closed_queue_commands.get());
    if rig.nets[X].borrow().reset_send_errors() == 0 || drained == 0 {
        let text = "VACUOUS reset-drain-unengaged: no Send met the reset, or no command was \
                    dequeued from a closed slot";
        rig.violate(text.to_string());
    }
    rig
}

/// The `DEL` of a window of keys cell 1 holds, then `QUIT`, then 10 `SET`s
/// of keys cell 0 owns, in one buffer, so the `QUIT` runs on the pump behind
/// the held `DEL`; cell 1's replies are released once the buffer is taken.
/// Nothing behind a `QUIT` runs (`post-quit-ran`), its `+OK` follows the
/// `DEL`'s count and the socket closes (ADR-0196 D6.2). Engagement: cell 1
/// held the `DEL`'s legs and the client had read nothing after the buffer was
/// taken, else `VACUOUS deferred-quit-unengaged`.
fn deferred_quit_leg(seed: u64) -> Rig {
    const SETS: usize = 10;
    let mut rig = Rig::new(seed, 0, 15_000);
    let reader = rig.admitted(X);
    let client = rig.admitted(X);
    let names = names_on(X, "deferred-quit", SETS);
    let mut rest = encode(&[b"QUIT"]);
    rest.extend(sets(&names));
    rig.held = Some(Y);
    rig.send_one_buffer(client, &Rig::held_pipeline(Y, &rest));
    for _ in 0..64 {
        rig.step();
    }
    let held = rig.plane(X).suspended();
    rig.read("held_legs", held as u64);
    let engaged = held >= WINDOW && rig.clients[client].replies.len() == 1;
    rig.held = None;
    rig.until("the quitting client's close", |rig| rig.ended(client));
    rig.settle();
    let ran = rig.existing(reader, &names);
    rig.read("post_quit_ran", ran as u64);
    if ran > 0 {
        rig.violate(format!("post-quit-ran: {ran} of the {SETS} SETs behind the QUIT ran"));
    }
    let owed = [DEL_NONE.to_vec(), b"+OK\r\n".to_vec()];
    rig.judge_replies(client, 1, &owed, "the quitting client");
    rig.expect_vacant(X, 1);
    rig.parked_nothing();
    if !engaged {
        let text = "VACUOUS deferred-quit-unengaged: the QUIT met no held DEL ahead of it";
        rig.violate(text.to_string());
    }
    rig
}

/// A frame whose bulk length is not a number: a protocol error.
const MALFORMED: &[u8] = b"*1\r\n$x\r\n";
/// Its reply, Redis's text, after which the server closes the connection.
const PROTOCOL_ERROR: &[u8] = b"-ERR Protocol error: invalid bulk length\r\n";

/// `INF.NS USE` of a memory namespace, 10 `SET`s of keys cell 0 owns, a
/// malformed frame and 10 more `SET`s, in one buffer. A namespace-bound
/// connection's every command rides its pump, so PARSE meets the error while
/// the pump owes the 11 replies before it. The client reads the 11 `+OK`s,
/// then the error, then the close (`reply-order`); the first 10 land
/// (`pre-error-unrun`) and the last 10 never run (`post-error-ran`).
/// Engagement: the `SET`s before the error landed in the namespace, so they
/// ran namespace-bound, else `VACUOUS protocol-order-unengaged`.
fn protocol_order_leg(seed: u64) -> Rig {
    const SETS: usize = 10;
    let namespace = namespace_specs(1);
    let name = namespace[0].name.clone();
    let mut rig = Rig::with_namespaces(seed, 0, 15_000, &namespace);
    let reader = rig.admitted(X);
    let bound = rig.call(reader, &[b"INF.NS", b"USE", &name]) == b"+OK\r\n";
    let client = rig.admitted(X);
    let before = names_on(X, "po-before", SETS);
    let after = names_on(X, "po-after", SETS);
    let mut bytes = encode(&[b"INF.NS", b"USE", &name]);
    bytes.extend(sets(&before));
    bytes.extend_from_slice(MALFORMED);
    bytes.extend(sets(&after));
    rig.send_one_buffer(client, &bytes);
    rig.until("the client's close", |rig| rig.ended(client));
    rig.settle();
    let mut owed = oks(SETS + 1);
    owed.push(PROTOCOL_ERROR.to_vec());
    rig.judge_order(client, 1, &owed, "the client");
    let landed = rig.existing(reader, &before);
    rig.read("landed", landed as u64);
    let ran = rig.existing(reader, &after);
    rig.read("post_error_ran", ran as u64);
    if landed < SETS {
        rig.violate(format!("pre-error-unrun: {landed} of the {SETS} SETs before the error ran"));
    }
    if ran > 0 {
        rig.violate(format!("post-error-ran: {ran} of the {SETS} SETs behind the error ran"));
    }
    rig.expect_vacant(X, 1);
    let queued = rig.drain_count("parse_replies_queued", |n| n.parse_replies_queued.get());
    if !(bound && landed > 0 && queued >= 1) {
        let text = "VACUOUS protocol-order-unengaged: no SET before the error ran namespace-bound, \
                    or PARSE queued no reply behind the owed ones";
        rig.violate(text.to_string());
    }
    rig
}

impl Rig {
    /// Sends `bytes` from client `id` as one receive buffer: the net hands
    /// them to the server whole, at one reap, then goes back to its seeded
    /// chunking.
    fn send_one_buffer(&mut self, id: usize, bytes: &[u8]) {
        let length = bytes.len();
        assert!(length <= RECV_BUFFER_BYTES, "{length} bytes fit no receive buffer");
        let cell = self.clients[id].cell;
        let delivery = self.nets[cell].borrow_mut().deliver_whole();
        self.send(id, bytes);
        self.until("the buffer's reap", |rig| {
            rig.nets[cell].borrow().unread(rig.fd(id)) == Some(0)
        });
        self.nets[cell].borrow_mut().restore_delivery(delivery);
    }

    /// The replies client `id` read after its first `skip` are exactly
    /// `owed`, in order, else `reply-order`, naming the first that differs.
    fn judge_order(&mut self, id: usize, skip: usize, owed: &[Vec<u8>], who: &str) {
        let client = &self.clients[id];
        let read = &client.replies[skip.min(client.replies.len())..];
        let Some(at) = (0..read.len().max(owed.len())).find(|&n| read.get(n) != owed.get(n)) else {
            return;
        };
        let text = |reply: Option<&Vec<u8>>| reply.map(|r| String::from_utf8_lossy(r).into_owned());
        let (got, want) = (text(read.get(at)), text(owed.get(at)));
        self.violate(format!(
            "reply-order: {who}'s reply {at} of {} read {got:?}, owed {want:?}",
            read.len()
        ));
    }
}

/// The legs of `CloseLeg::PINNED_RED`, one ignored test each, at the smoke
/// seed.
#[cfg(test)]
mod reds {
    use super::{CapacityScenario, CloseLeg, run_capacity_scenario};

    /// Runs a pinned leg. Red, it fails with the leg's violations, the defect
    /// its test's ignore reason names; green, it fails too, since a green leg
    /// leaves `CloseLeg::PINNED_RED` for its smoke row: an ignore removed
    /// never runs a leg in this lane, unchecked by its twin.
    fn pinned_red(leg: CloseLeg) {
        let name = leg.name();
        assert!(CloseLeg::PINNED_RED.contains(&leg), "--close-leg {name}: not a pinned red");
        let report = run_capacity_scenario(&CapacityScenario::m0_capacity(0xC0FFEE, leg));
        let violations: Vec<&str> = report.violations.iter().collect();
        assert!(violations.is_empty(), "m0-capacity --close-leg {name}: {violations:?}");
        panic!(
            "m0-capacity --close-leg {name} reads green: it leaves CloseLeg::PINNED_RED for its \
             scripts/sim-smoke.sh row"
        );
    }

    #[test]
    #[ignore = "a fan sends a leg per remote key past the connection's window of 32; red until \
                each leg takes one of the connection's gate units"]
    fn the_fan_leg_holds_a_window_per_connection() {
        pinned_red(CloseLeg::Fan);
    }
}
