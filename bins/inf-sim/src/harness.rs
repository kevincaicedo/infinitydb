//! The deterministic harness: scenario config, seeded scheduler, simulated
//! clients, the single-store oracle, and the event-trace recorder.
//!
//! ## Oracles (M0 base + M1-S15 additions)
//! - **Per-key linearizability** (M0): every apply point (the
//!   `PlaneObserver` seam) replays the same argv against ONE model
//!   `Keyspace` — in apply order, which on a single thread is a true total
//!   order — and the model's reply must equal the observed reply
//!   byte-for-byte. TTL semantics ride the same replay (same injected
//!   `now`), so an early or ghost expiry diverges a later read.
//! - **Pub/sub delivery** (M1-S15): pub/sub commands bypass the apply seam
//!   (they are plane programs, not store ops), so they get their own oracle:
//!   subscribers confirm before any publisher starts (the harness enforces
//!   the happens-before the plane pins: confirmed ⇒ reachable), every
//!   PUBLISH reply must equal the planned receiver count, every delivered
//!   frame must carry a per-(channel, publisher) sequence exactly one past
//!   the last (per-publisher FIFO, no loss, no dup, no reorder), and at
//!   quiescence every subscriber must have received exactly the published
//!   count for its channels. A lost message stalls phase C and fails the
//!   run with a replayable seed.
//! - **Accounting reconciliation** (M1-S15): at quiescence both engines
//!   drain expired-but-unreaped wheel entries at the same instant (active
//!   vs lazy expiry equalized), then the per-cell live-record sum must equal
//!   the model's, every pub/sub registry must be empty (bytes = 0), and no
//!   server-side connection may remain.
//! - **Content reconciliation** (review of 2026-08-30, F-L19-06): at
//!   quiescence — and at every quiescent audit of a surface scenario — the
//!   node's live entries `(scope, key) → (value, expiry deadline)`, folded
//!   over every cell's numbered dbs and memory namespaces through the same
//!   walker the model uses, must equal the model's exactly. A count oracle
//!   is blind to *count right, contents wrong*; a key-set oracle is blind to
//!   a corrupted value or a dropped deadline; this one is blind to neither.
//!   `Canary` plants each of those defects in the model so the tests prove
//!   the comparator sees them.
//! - **Scoped replay + surface audit** (F-L19-05): the apply seam names the
//!   store each command addressed (`ExecScope`), so namespace-bound and
//!   `SELECT`ed clients replay against the model's matching store. Scatter
//!   commands (`DBSIZE`/`KEYS`/`SCAN`/`RANDOMKEY`/`FLUSH*`) decompose into
//!   per-cell legs whose replies are partial by construction, so they are
//!   not replayed; the quiescent audit checks them end to end over the
//!   wire (served set == model set per scope) and applies flushes to the
//!   model at the audit point. Concurrent `SCAN` walks carry the weak
//!   oracle: scope isolation (no foreign-alphabet key), bounded termination,
//!   glob conformance.
//!
//! ## Trace
//! The trace is the byte log of `(cell, origin, argv, reply)` events plus
//! client completions. Same seed ⇒ byte-identical trace (the determinism
//! AC); the comparator just memcmps two runs.

use core::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::os::fd::RawFd;
use std::rc::Rc;

use inf_alloc::BufferPool;
use inf_fabric::{Mesh, MeshConfig};
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::{Clock, Nanos, VirtualClock};
use inf_foundation::{CellId, hash64};
use inf_runtime::{CellLoop, LoopConfig};
use inf_server::{
    ConnCx, ConnNamespace, ExecOrigin, ExecScope, NodeInfo, PlaneObserver, ServerPlane,
    execute_slices, fold_live_entries, glob_match,
};
use inf_store::{
    ExpireCond, ExpiryBudget, FIRST_NAMED_NS_ID, KeyHasher, Keyspace, NsId, NsMode, NsSpec,
    SetOptions, StoreConfig,
};
use inf_wire::{CommandId, Protocol, lookup};

use crate::net::{CellNet, Plant, SimDriver, listener_fd};
use crate::resp::{Reply, SubFrame, parse_reply, parse_sub_frame, reply_len};

/// Scenario config (the DSL v0: a struct, not a language).
#[derive(Clone, Debug)]
pub struct Scenario {
    pub seed: u64,
    pub cells: u16,
    pub connections: usize,
    /// Total commands across all clients.
    pub commands: u64,
    pub key_space: u64,
    /// Every Nth client pipelines 4-deep instead of awaiting each reply.
    pub pipelined_every: usize,
    pub plant: Plant,
    /// Pub/sub plane (M1-S15): dedicated subscriber connections. 0 keeps the
    /// M0 shape (and the M0 RNG stream) exactly.
    pub subscribers: usize,
    /// Channel namespace size (`chan:0..channels`).
    pub channels: u64,
    /// PUBLISH share of the regular-client mix, percent. 0 = M0 mix.
    pub publish_percent: u64,
    /// Max virtual nanoseconds per scheduler step (advance is seeded in
    /// `1µs..=1µs+step_ns_max`). The m1 scenario uses bigger steps so its
    /// PEXPIRE deadlines genuinely fire mid-run (wheel slices under load).
    pub step_ns_max: u64,
    /// Adversarial-length share of the mix, percent (review of
    /// 2026-08-30, §5.5 Group 0 item 3: the frozen mixes are 9-byte keys
    /// and 6-byte values, so the `MAX_KEY_LEN` edge, big values, and the
    /// partial-application multi-key shapes were unreachable by any
    /// seed). 0 keeps every existing scenario's RNG stream exactly.
    pub adversarial_percent: u64,
    /// Named memory namespaces seeded into every cell and the model before
    /// traffic (review of 2026-08-30, F-L19-05 surface half): clients bind
    /// with `INF.NS USE` or `SELECT 1`, so the `ApplyNs` route, the
    /// namespace-aware scatter programs and scope isolation run under the
    /// standing oracles. 0 keeps every existing scenario's RNG stream.
    pub namespaces: u16,
    /// Iteration-surface share of the mix, percent: concurrent `SCAN`
    /// walks, `KEYS`, `DBSIZE`, `RANDOMKEY`, `TYPE`, `STRLEN` under the weak
    /// concurrent oracle (scope isolation, bounded walk termination, glob
    /// conformance). 0 = the m0 shape.
    pub surface_percent: u64,
    /// Quiescent audits every N completed commands (0 = none): every client
    /// drains, then per scope the served surface (`DBSIZE`, a full `SCAN`
    /// walk, `KEYS`, `RANDOMKEY` over the wire) and the stored content
    /// (values + deadlines) must equal the model exactly; a seeded
    /// `FLUSHDB`/`FLUSHALL` sometimes precedes a second pass.
    pub audit_every: u64,
    /// A defect planted in the model right before the quiescence content
    /// reconciliation — the oracle's teeth, testable. `Canary::None` in
    /// every shipping scenario.
    pub canary: Canary,
}

/// Model-side plants for the content oracle's canary tests (F-L19-06's
/// "planted-bug canary" ask): each must be caught by the quiescence
/// reconciliation naming the affected key.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum Canary {
    #[default]
    None,
    /// One live key deleted from the model (a key the node has and the
    /// model lacks — the C1 shape mirrored).
    DropKey,
    /// One live value overwritten in the model (count and key set intact).
    CorruptValue,
    /// One expiry deadline removed in the model (count, key set, value
    /// intact).
    DropDeadline,
}

impl Scenario {
    /// The M0-S20 AC scenario: 3 cells, 100 connections, 10⁵ mixed commands
    /// including cross-cell.
    pub fn m0_smoke(seed: u64) -> Scenario {
        Scenario {
            seed,
            cells: 3,
            connections: 100,
            commands: 100_000,
            key_space: 2_000,
            pipelined_every: 5,
            plant: Plant::None,
            subscribers: 0,
            channels: 0,
            publish_percent: 0,
            step_ns_max: 15_000,
            adversarial_percent: 0,
            namespaces: 0,
            surface_percent: 0,
            audit_every: 0,
            canary: Canary::None,
        }
    }

    /// Group 0 item 3 (review of 2026-08-30, §5.5): the m0 shape with a
    /// 35% adversarial-length slice — `MAX_KEY_LEN`-edge keys (254, 255,
    /// 256, 300 bytes), values to 64 KiB (past the 16,368 B cold-window
    /// class), and the partial-application multi-key shapes (`MSET`/
    /// `MSETNX` with an over-bound pair, ADR-0098's atomicity contract).
    /// Every reply byte-diffs against the model, and the key-set content
    /// oracle binds at quiescence — the two lanes no prior seed could
    /// reach. Runs at `--cells 4` in sim-smoke (Group 0 item 1).
    pub fn m0_adversarial(seed: u64) -> Scenario {
        Scenario {
            seed,
            cells: 3,
            connections: 50,
            commands: 40_000,
            key_space: 500,
            pipelined_every: 5,
            plant: Plant::None,
            subscribers: 0,
            channels: 0,
            publish_percent: 0,
            step_ns_max: 15_000,
            adversarial_percent: 35,
            namespaces: 0,
            surface_percent: 0,
            audit_every: 0,
            canary: Canary::None,
        }
    }

    /// The surface mix (review of 2026-08-30, F-L19-05 surface half +
    /// F-L19-06 value half): four scopes — db 0, db 1 (`SELECT`) and two
    /// memory namespaces (`INF.NS USE`), each with its own key alphabet —
    /// the m0 shape plus a 20% iteration slice (concurrent `SCAN` walks,
    /// `KEYS`, `DBSIZE`, `RANDOMKEY`, `TYPE`, `STRLEN`), and a quiescent
    /// audit every 5,000 commands comparing the served surface and the
    /// stored content (values, deadlines) with the model, with seeded
    /// flushes. Runs at `--cells 4` in sim-smoke and PR CI.
    pub fn m0_surface(seed: u64) -> Scenario {
        Scenario {
            seed,
            cells: 3,
            connections: 60,
            commands: 40_000,
            key_space: 400,
            pipelined_every: 5,
            plant: Plant::None,
            subscribers: 0,
            channels: 0,
            publish_percent: 0,
            step_ns_max: 15_000,
            adversarial_percent: 0,
            namespaces: 2,
            surface_percent: 20,
            audit_every: 5_000,
            canary: Canary::None,
        }
    }

    /// The M1-S15 scenario: the m0 mix plus TTL traffic, cross-cell pub/sub
    /// fan-out (channel + pattern subscribers), and the delivery/accounting
    /// oracles armed.
    pub fn m1_cache(seed: u64) -> Scenario {
        Scenario {
            seed,
            cells: 3,
            connections: 80,
            commands: 60_000,
            key_space: 2_000,
            pipelined_every: 5,
            plant: Plant::None,
            subscribers: 8,
            channels: 8,
            publish_percent: 10,
            step_ns_max: 250_000,
            adversarial_percent: 0,
            namespaces: 0,
            surface_percent: 0,
            audit_every: 0,
            canary: Canary::None,
        }
    }
}

/// What a run produces. `trace` is the determinism artifact.
#[derive(Debug)]
pub struct SimReport {
    pub trace: Vec<u8>,
    pub trace_hash: u64,
    pub events: u64,
    pub commands_done: u64,
    pub oracle_violations: Vec<String>,
    pub stalled: bool,
    pub scheduler_steps: u64,
    /// Virtual time the run covered (nightly-fleet budget accounting).
    pub sim_seconds: f64,
    /// Pub/sub oracle counters (0 when the scenario has no subscribers).
    pub published: u64,
    pub delivered: u64,
    /// Surface-oracle coverage (0 outside surface scenarios): quiescent
    /// audits run, flushes issued inside them, concurrent `SCAN` walks that
    /// reached cursor 0, and apply events the replay oracle handed to the
    /// audit oracle (scatter legs). Disclosed so a seed that never audited
    /// or never completed a walk cannot pass as coverage.
    pub audits: u64,
    pub flushes: u64,
    pub scan_walks: u64,
    pub replays_skipped: u64,
}

impl SimReport {
    pub fn ok(&self) -> bool {
        !self.stalled && self.oracle_violations.is_empty()
    }
}

// ---- oracle observer -----------------------------------------------------------

struct Oracle {
    model: Keyspace,
    trace: Vec<u8>,
    events: u64,
    violations: Vec<String>,
    /// Scatter legs the replay oracle left to the audit oracle.
    replays_skipped: u64,
}

/// The named memory namespaces a surface scenario seeds into every cell's
/// keyspace and into the model before traffic — the boot-time catalog seed
/// a durable node performs, minus the catalog (memory nodes have no
/// control plane, so `INF.NS CREATE` is not available to the sim; the DDL
/// program has its own DST, `m2-ns-create-window`). Ids start at
/// `FIRST_NAMED_NS_ID`, names are `ns0`, `ns1`, …
pub(crate) fn seed_namespaces(ks: &mut Keyspace, namespaces: u16) {
    for i in 0..namespaces {
        ks.ns_create(NsSpec {
            id: NsId(FIRST_NAMED_NS_ID + u32::from(i)),
            name: format!("ns{i}").into_bytes(),
            mode: NsMode::Memory,
            fsync: None,
            policy: None,
            maxmemory: None,
            tier: None,
        })
        .expect("seeded namespace names are valid and unique");
    }
}

/// Appends the apply scope to a trace record (shared by every observer so
/// the durable and memory traces agree on the format).
pub(crate) fn trace_scope(trace: &mut Vec<u8>, scope: ExecScope) {
    match scope {
        ExecScope::Db(db) => {
            trace.push(0);
            trace.extend_from_slice(&db.to_le_bytes());
            trace.extend_from_slice(&[0, 0]);
        }
        ExecScope::Ns(ns) => {
            trace.push(1);
            trace.extend_from_slice(&ns.0.to_le_bytes());
        }
        ExecScope::Unavailable => trace.extend_from_slice(&[2, 0, 0, 0, 0]),
    }
}

/// The model connection for one apply scope: the same store the node used.
fn model_cx(scope: ExecScope) -> ConnCx {
    let mut cx = ConnCx { proto: Protocol::Resp2, id: 0, ..Default::default() };
    match scope {
        ExecScope::Db(db) => cx.db = db,
        ExecScope::Ns(ns) => cx.ns = ConnNamespace::Named(ns),
        ExecScope::Unavailable => cx.ns = ConnNamespace::RequiredUnavailable,
    }
    cx
}

/// Commands the plane scatters across cells. The seam reports each leg
/// separately and a leg's reply is partial by construction (one cell's
/// `DBSIZE`, one cell's `KEYS` page, one hop of a `SCAN`, one cell's
/// `FLUSHALL`), so no single-store replay can equal it: the audit oracle
/// owns these end to end.
fn is_scatter_class(id: CommandId) -> bool {
    matches!(
        id,
        CommandId::Dbsize
            | CommandId::Keys
            | CommandId::Scan
            | CommandId::Randomkey
            | CommandId::Flushdb
            | CommandId::Flushall
    )
}

/// The node's key hasher for a scenario (ADR-0094 D2, injected — L7):
/// every cell of the simulated node, and every model keyspace that
/// replays the node's checkpoints, derives the same secret from the
/// scenario seed, so placement is reproducible and never a constant.
pub(crate) fn node_hasher(seed: u64) -> KeyHasher {
    KeyHasher::from_seed(seed ^ 0x4B45_5948_4153_4845)
}

impl Oracle {
    fn new(keys: usize, namespaces: u16) -> Oracle {
        let mut model = Keyspace::new(StoreConfig { initial_keys: keys, ..Default::default() });
        seed_namespaces(&mut model, namespaces);
        Oracle { model, trace: Vec::new(), events: 0, violations: Vec::new(), replays_skipped: 0 }
    }
}

#[derive(Clone)]
struct SharedOracle(Rc<RefCell<Oracle>>);

impl PlaneObserver for SharedOracle {
    fn on_execute(
        &mut self,
        cell: CellId,
        origin: ExecOrigin,
        scope: ExecScope,
        argv: &[&[u8]],
        reply: &[u8],
        now: Nanos,
    ) {
        let mut oracle = self.0.borrow_mut();
        oracle.events += 1;
        // Trace record: cell, origin tag, scope, argv, reply (length-prefixed).
        oracle.trace.extend_from_slice(&cell.0.to_le_bytes());
        match origin {
            ExecOrigin::Conn(slot, generation) => {
                oracle.trace.push(0);
                oracle.trace.extend_from_slice(&slot.to_le_bytes());
                oracle.trace.extend_from_slice(&generation.to_le_bytes());
            }
            ExecOrigin::Fabric(from) => {
                oracle.trace.push(1);
                oracle.trace.extend_from_slice(&from.0.to_le_bytes());
                oracle.trace.extend_from_slice(&[0, 0]);
            }
        }
        trace_scope(&mut oracle.trace, scope);
        oracle.trace.push(argv.len() as u8);
        for arg in argv {
            oracle.trace.extend_from_slice(&(arg.len() as u32).to_le_bytes());
            oracle.trace.extend_from_slice(arg);
        }
        oracle.trace.extend_from_slice(&(reply.len() as u32).to_le_bytes());
        oracle.trace.extend_from_slice(reply);

        // Scatter legs belong to the audit oracle (see `is_scatter_class`).
        if lookup(argv[0]).is_some_and(|meta| is_scatter_class(meta.id)) {
            oracle.replays_skipped += 1;
            return;
        }
        // Model replay: same argv, same store, same injected time, RESP2
        // (the scenario mixes never switch protocols).
        let mut expected = Vec::new();
        let mut cx = model_cx(scope);
        let Oracle { model, violations, .. } = &mut *oracle;
        execute_slices(argv, model, &mut cx, now, &mut expected);
        if expected != reply {
            let argv_text: Vec<String> =
                argv.iter().map(|a| String::from_utf8_lossy(a).into_owned()).collect();
            violations.push(format!(
                "apply divergence on cell {cell} scope {scope:?} {argv_text:?}: node {:?} vs \
                 model {:?}",
                String::from_utf8_lossy(reply),
                String::from_utf8_lossy(&expected),
            ));
        }
    }
}

// ---- simulated clients -----------------------------------------------------------

struct SimClient {
    id: usize,
    cell: usize,
    fd: RawFd,
    quota: u64,
    sent: u64,
    replied: u64,
    /// Commands in flight (1 = sequential; >1 = pipelined).
    window: u64,
    rx: Vec<u8>,
    rng: SplitMix64,
    closed: bool,
    /// Per-channel PUBLISH sequence counters (m1 mixes only).
    pub_seq: Vec<u64>,
    /// The client-side check per in-flight command, parallel to the
    /// in-flight window (`Check::None` = the apply oracle owns the reply).
    checks: VecDeque<Check>,
    /// The store this client's commands address (surface scenarios; `Db0`
    /// otherwise).
    scope: ClientScope,
    /// A concurrent `SCAN` walk in progress: `(cursor to continue from,
    /// pages so far)`. `None` = no walk open.
    scan: Option<(u64, u32)>,
    /// A `SCAN` page is in flight — the generator issues something else
    /// rather than fork the walk.
    scan_inflight: bool,
}

/// What the client itself verifies about one reply. Everything the apply
/// oracle cannot see or cannot replay lands here.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Check {
    /// The apply oracle owns the reply.
    None,
    /// `PUBLISH`: the reply must equal the planned receiver count.
    Publish(i64),
    /// A scope binding (`SELECT 1` / `INF.NS USE`): must be `+OK`.
    Bind,
    /// One `SCAN` page of this client's walk: scope isolation + progress.
    ScanPage,
    /// `KEYS <glob>`: every key in scope and matching the glob.
    Keys(Vec<u8>),
    /// `RANDOMKEY`: nil or a key in scope.
    RandomKey,
    /// `DBSIZE`: a non-negative integer.
    Dbsize,
}

/// Pages one concurrent `SCAN` walk may take before the harness calls it
/// non-terminating (the N1 class: a cursor that never reaches 0). A cell's
/// walk is bounded by its home-group count and the cursor hops once per
/// cell, so a legal walk over a few hundred keys on ≤ 8 cells takes far
/// fewer pages even at `COUNT 1`.
const SCAN_WALK_PAGES_MAX: u32 = 4_096;

/// Distinct key alphabets per scope, so a key served under the wrong scope
/// is visible by its prefix (the C1 class: a scatter serving another store).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum ClientScope {
    Db0,
    Db1,
    /// Named memory namespace `ns<i>` (index into the seeded set).
    Ns(u16),
}

impl ClientScope {
    /// Round-robin over `{db0, db1, ns0..}` by client id; every client is
    /// `Db0` when the scenario seeds no namespaces (the m0/m1 shapes).
    fn for_client(id: usize, namespaces: u16) -> ClientScope {
        if namespaces == 0 {
            return ClientScope::Db0;
        }
        match id % (usize::from(namespaces) + 2) {
            0 => ClientScope::Db0,
            1 => ClientScope::Db1,
            k => ClientScope::Ns((k - 2) as u16),
        }
    }

    /// Every scope enumerated once (the auditor set).
    fn all(namespaces: u16) -> Vec<ClientScope> {
        let mut scopes = vec![ClientScope::Db0, ClientScope::Db1];
        scopes.extend((0..namespaces).map(ClientScope::Ns));
        scopes
    }

    fn prefix(self) -> Vec<u8> {
        match self {
            ClientScope::Db0 => b"key:".to_vec(),
            ClientScope::Db1 => b"d1:".to_vec(),
            ClientScope::Ns(i) => format!("n{i}:").into_bytes(),
        }
    }

    fn key(self, n: u64) -> Vec<u8> {
        let mut key = self.prefix();
        key.extend_from_slice(n.to_string().as_bytes());
        key
    }

    fn counter(self, n: u64) -> Vec<u8> {
        let mut key = self.prefix();
        key.extend_from_slice(format!("c{n}").as_bytes());
        key
    }

    /// `<prefix><digit>*` — a glob that selects a slice of the alphabet.
    fn glob(self, digit: u64) -> Vec<u8> {
        let mut glob = self.prefix();
        glob.extend_from_slice(format!("{digit}*").as_bytes());
        glob
    }

    fn owns(self, key: &[u8]) -> bool {
        key.starts_with(&self.prefix())
    }

    /// The conn-state command that binds a fresh connection to this scope.
    fn bind_wire(self) -> Option<Vec<u8>> {
        match self {
            ClientScope::Db0 => None,
            ClientScope::Db1 => Some(encode(&[b"SELECT".to_vec(), b"1".to_vec()])),
            ClientScope::Ns(i) => {
                Some(encode(&[b"INF.NS".to_vec(), b"USE".to_vec(), format!("ns{i}").into_bytes()]))
            }
        }
    }

    fn exec_scope(self) -> ExecScope {
        match self {
            ClientScope::Db0 => ExecScope::Db(0),
            ClientScope::Db1 => ExecScope::Db(1),
            ClientScope::Ns(i) => ExecScope::Ns(NsId(FIRST_NAMED_NS_ID + u32::from(i))),
        }
    }

    fn name(self) -> String {
        match self {
            ClientScope::Db0 => "db0".to_string(),
            ClientScope::Db1 => "db1".to_string(),
            ClientScope::Ns(i) => format!("ns{i}"),
        }
    }
}

fn encode(argv: &[Vec<u8>]) -> Vec<u8> {
    let mut wire = format!("*{}\r\n", argv.len()).into_bytes();
    for arg in argv {
        wire.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        wire.extend_from_slice(arg);
        wire.extend_from_slice(b"\r\n");
    }
    wire
}

impl SimClient {
    /// Next command. Returns the wire bytes plus the client-side check; a
    /// `PUBLISH` carries its channel index (the harness updates the
    /// delivery ledger at send time).
    fn next_command(&mut self, scenario: &Scenario) -> (Vec<u8>, Check, Option<u64>) {
        // Surface scenarios bind the connection first (the reply must be
        // `+OK`, so a refused binding is a violation, never a silent
        // fall-through to db 0).
        if self.sent == 0
            && let Some(bind) = self.scope.bind_wire()
        {
            return (bind, Check::Bind, None);
        }
        if scenario.surface_percent > 0 || scenario.namespaces > 0 {
            let (wire, check) = self.next_command_surface(scenario);
            return (wire, check, None);
        }
        if scenario.adversarial_percent > 0 {
            return (self.next_command_adversarial(scenario), Check::None, None);
        }
        if scenario.publish_percent == 0 {
            return (self.next_command_m0(scenario.key_space), Check::None, None);
        }
        let key = format!("key:{}", self.rng.next_u64() % scenario.key_space);
        let roll = self.rng.next_u64() % 100;
        if roll < scenario.publish_percent {
            let chan = self.rng.next_u64() % scenario.channels;
            self.pub_seq[chan as usize] += 1;
            let payload = format!("m:{}:{}", self.id, self.pub_seq[chan as usize]);
            let argv = vec![
                b"PUBLISH".to_vec(),
                format!("chan:{chan}").into_bytes(),
                payload.into_bytes(),
            ];
            return (encode(&argv), Check::None, Some(chan));
        }
        // Remaining 90%: the m0 shape compressed, with a heavier TTL slice.
        let argv: Vec<Vec<u8>> = match roll {
            ..40 => vec![b"GET".to_vec(), key.into_bytes()],
            40..62 => {
                let value = format!("v{}", self.rng.next_u64() % 100_000);
                vec![b"SET".to_vec(), key.into_bytes(), value.into_bytes()]
            }
            62..70 => {
                vec![b"INCR".to_vec(), format!("ctr:{}", self.rng.next_u64() % 64).into_bytes()]
            }
            70..76 => vec![b"DEL".to_vec(), key.into_bytes()],
            76..82 => {
                let key2 = format!("key:{}", self.rng.next_u64() % scenario.key_space);
                vec![b"EXISTS".to_vec(), key.into_bytes(), key2.into_bytes()]
            }
            82..86 => vec![b"APPEND".to_vec(), key.into_bytes(), b"+tail".to_vec()],
            86..94 => {
                // Millisecond TTLs sized to the scenario's virtual-time
                // span: many genuinely fire mid-run, exercising wheel
                // slices under the linearizability oracle.
                let ms = format!("{}", 20 + self.rng.next_u64() % 500);
                vec![b"PEXPIRE".to_vec(), key.into_bytes(), ms.into_bytes()]
            }
            _ => vec![b"TTL".to_vec(), key.into_bytes()],
        };
        (encode(&argv), Check::None, None)
    }

    /// The surface mix (review of 2026-08-30, F-L19-05): the m0 shape on
    /// this client's scope alphabet, plus a `surface_percent` slice of
    /// iteration commands. A `SCAN` walk continues from the client's own
    /// cursor (one walk open at a time); the reply checks live in
    /// `SimClient::check_reply`.
    fn next_command_surface(&mut self, scenario: &Scenario) -> (Vec<u8>, Check) {
        let scope = self.scope;
        let roll = self.rng.next_u64() % 100;
        if roll < scenario.surface_percent {
            let sub = self.rng.next_u64() % 8;
            match sub {
                0..=2 if !self.scan_inflight => {
                    let cursor = self.scan.map_or(0, |(cursor, _)| cursor);
                    let count = [1u64, 5, 20][(self.rng.next_u64() % 3) as usize];
                    self.scan_inflight = true;
                    let argv = vec![
                        b"SCAN".to_vec(),
                        cursor.to_string().into_bytes(),
                        b"COUNT".to_vec(),
                        count.to_string().into_bytes(),
                    ];
                    return (encode(&argv), Check::ScanPage);
                }
                3 => {
                    let glob = scope.glob(self.rng.next_u64() % 10);
                    return (encode(&[b"KEYS".to_vec(), glob.clone()]), Check::Keys(glob));
                }
                4 => return (encode(&[b"DBSIZE".to_vec()]), Check::Dbsize),
                5 => return (encode(&[b"RANDOMKEY".to_vec()]), Check::RandomKey),
                6 => {
                    let key = scope.key(self.rng.next_u64() % scenario.key_space);
                    return (encode(&[b"TYPE".to_vec(), key]), Check::None);
                }
                _ => {
                    let key = scope.key(self.rng.next_u64() % scenario.key_space);
                    return (encode(&[b"STRLEN".to_vec(), key]), Check::None);
                }
            }
        }
        // The m0 shape on the scope's alphabet.
        let key = scope.key(self.rng.next_u64() % scenario.key_space);
        let roll = self.rng.next_u64() % 100;
        let argv: Vec<Vec<u8>> = match roll {
            0..=44 => vec![b"GET".to_vec(), key],
            45..=69 => {
                let value = format!("v{}", self.rng.next_u64() % 100_000);
                vec![b"SET".to_vec(), key, value.into_bytes()]
            }
            70..=79 => vec![b"INCR".to_vec(), scope.counter(self.rng.next_u64() % 64)],
            80..=86 => vec![b"DEL".to_vec(), key],
            87..=92 => {
                let key2 = scope.key(self.rng.next_u64() % scenario.key_space);
                vec![b"EXISTS".to_vec(), key, key2]
            }
            93..=95 => vec![b"APPEND".to_vec(), key, b"+tail".to_vec()],
            96..=97 => {
                let secs = format!("{}", 1 + self.rng.next_u64() % 50);
                vec![b"EXPIRE".to_vec(), key, secs.into_bytes()]
            }
            _ => vec![b"TTL".to_vec(), key],
        };
        (encode(&argv), Check::None)
    }

    /// The client-side verdict on one reply (the checks the apply oracle
    /// cannot make). `scan_walks` counts walks that reached cursor 0.
    fn check_reply(
        &mut self,
        check: Check,
        raw: &[u8],
        violations: &mut Vec<String>,
        scan_walks: &mut u64,
    ) {
        let who = format!("client {} ({})", self.id, self.scope.name());
        match check {
            Check::None => {}
            Check::Publish(want) => {
                let want_wire = format!(":{want}\r\n").into_bytes();
                if raw != want_wire {
                    violations.push(format!(
                        "publisher {}: PUBLISH replied {:?}, planned receiver count {want}",
                        self.id,
                        String::from_utf8_lossy(raw)
                    ));
                }
            }
            Check::Bind => {
                if raw != b"+OK\r\n" {
                    violations.push(format!(
                        "{who}: scope binding refused: {:?}",
                        String::from_utf8_lossy(raw)
                    ));
                }
            }
            Check::ScanPage => {
                self.scan_inflight = false;
                let pages = self.scan.map_or(0, |(_, pages)| pages) + 1;
                match parse_reply(raw) {
                    Reply::Array(items) if items.len() == 2 => {
                        let cursor = match &items[0] {
                            Reply::Bulk(digits) => core::str::from_utf8(digits)
                                .ok()
                                .and_then(|text| text.parse::<u64>().ok()),
                            _ => None,
                        };
                        let Some(cursor) = cursor else {
                            violations.push(format!("{who}: SCAN cursor not numeric: {items:?}"));
                            self.scan = None;
                            return;
                        };
                        let Reply::Array(keys) = &items[1] else {
                            violations.push(format!("{who}: SCAN page not an array: {items:?}"));
                            self.scan = None;
                            return;
                        };
                        for key in keys {
                            match key {
                                Reply::Bulk(key) if self.scope.owns(key) => {}
                                other => violations.push(format!(
                                    "{who}: SCAN returned a key outside its scope: {other:?}"
                                )),
                            }
                        }
                        if cursor == 0 {
                            *scan_walks += 1;
                            self.scan = None;
                        } else if pages >= SCAN_WALK_PAGES_MAX {
                            violations.push(format!(
                                "{who}: SCAN walk did not terminate within {SCAN_WALK_PAGES_MAX} pages"
                            ));
                            self.scan = None;
                        } else {
                            self.scan = Some((cursor, pages));
                        }
                    }
                    other => {
                        violations.push(format!("{who}: SCAN answered {other:?}"));
                        self.scan = None;
                    }
                }
            }
            Check::Keys(glob) => match parse_reply(raw) {
                Reply::Array(keys) => {
                    for key in keys {
                        match key {
                            Reply::Bulk(key)
                                if self.scope.owns(&key) && glob_match(&glob, &key, false) => {}
                            other => violations.push(format!(
                                "{who}: KEYS {:?} returned {other:?} (out of scope or off-glob)",
                                String::from_utf8_lossy(&glob)
                            )),
                        }
                    }
                }
                other => violations.push(format!("{who}: KEYS answered {other:?}")),
            },
            Check::RandomKey => match parse_reply(raw) {
                Reply::Nil => {}
                Reply::Bulk(key) if self.scope.owns(&key) => {}
                other => violations.push(format!("{who}: RANDOMKEY answered {other:?}")),
            },
            Check::Dbsize => match parse_reply(raw) {
                Reply::Int(n) if n >= 0 => {}
                other => violations.push(format!("{who}: DBSIZE answered {other:?}")),
            },
        }
    }

    /// The adversarial-length mix (Group 0 item 3, review 2026-08-30
    /// §5.5): a `adversarial_percent` slice of `MAX_KEY_LEN`-edge keys,
    /// big values, and the multi-key partial-application shapes; the
    /// remainder is the m0 shape. Correctness rides the standing
    /// oracles — every reply byte-diffs against the model at the apply
    /// seam, and the key-set content oracle binds at quiescence.
    fn next_command_adversarial(&mut self, scenario: &Scenario) -> Vec<u8> {
        let roll = self.rng.next_u64() % 100;
        if roll >= scenario.adversarial_percent {
            return self.next_command_m0(scenario.key_space);
        }
        // Deterministic long key: an id inside the key space, padded to a
        // boundary length (254/255 legal, 256/300 over `MAX_KEY_LEN`).
        let id = self.rng.next_u64() % scenario.key_space;
        let klen = [254usize, 255, 256, 300][(self.rng.next_u64() % 4) as usize];
        let mut key = format!("K:{id}:").into_bytes();
        key.resize(klen, b'k');
        let short = format!("key:{id}").into_bytes();
        let vlen = [0usize, 64, 4096, 16 << 10, 64 << 10][(self.rng.next_u64() % 5) as usize];
        let value: Vec<u8> = format!("A:{id}:{}:", self.sent)
            .into_bytes()
            .iter()
            .copied()
            .cycle()
            .take(vlen)
            .collect();
        let argv: Vec<Vec<u8>> = match self.rng.next_u64() % 12 {
            0 => vec![b"GET".to_vec(), key],
            1 => vec![b"SET".to_vec(), key, value],
            // C3's exact trigger: INCR on an over-bound key must refuse
            // typed on both engines, never panic the cell.
            2 => vec![b"INCR".to_vec(), key],
            3 => vec![b"DEL".to_vec(), key],
            4 => vec![b"EXISTS".to_vec(), key, short],
            5 => vec![b"APPEND".to_vec(), short, value],
            // H2's class on the memory path (ADR-0098): an over-bound
            // pair anywhere makes the whole command a typed no-op — the
            // model and the node must agree, and the content oracle
            // catches a half-applied prefix.
            6 => vec![b"MSET".to_vec(), short, value, key, b"v".to_vec()],
            7 => vec![b"MSETNX".to_vec(), short, value, key, b"v".to_vec()],
            8 => vec![b"GETRANGE".to_vec(), short, b"0".to_vec(), b"9223372036854775807".to_vec()],
            9 => {
                let offset = format!("{}", self.rng.next_u64() % (64 << 10));
                vec![b"SETRANGE".to_vec(), short, offset.into_bytes(), b"patch".to_vec()]
            }
            10 => vec![b"SET".to_vec(), short, value],
            _ => vec![b"STRLEN".to_vec(), key],
        };
        encode(&argv)
    }

    /// The frozen M0 mix — byte-for-byte the RNG stream the m0-smoke trace
    /// hash was pinned on. Do not touch without re-baselining the hash.
    fn next_command_m0(&mut self, key_space: u64) -> Vec<u8> {
        let key = format!("key:{}", self.rng.next_u64() % key_space);
        let roll = self.rng.next_u64() % 100;
        let argv: Vec<Vec<u8>> = match roll {
            0..=44 => vec![b"GET".to_vec(), key.into_bytes()],
            45..=69 => {
                let value = format!("v{}", self.rng.next_u64() % 100_000);
                vec![b"SET".to_vec(), key.into_bytes(), value.into_bytes()]
            }
            70..=79 => {
                vec![b"INCR".to_vec(), format!("ctr:{}", self.rng.next_u64() % 64).into_bytes()]
            }
            80..=86 => vec![b"DEL".to_vec(), key.into_bytes()],
            87..=92 => {
                let key2 = format!("key:{}", self.rng.next_u64() % key_space);
                vec![b"EXISTS".to_vec(), key.into_bytes(), key2.into_bytes()]
            }
            93..=95 => vec![b"APPEND".to_vec(), key.into_bytes(), b"+tail".to_vec()],
            96..=97 => {
                let secs = format!("{}", 1 + self.rng.next_u64() % 50);
                vec![b"EXPIRE".to_vec(), key.into_bytes(), secs.into_bytes()]
            }
            _ => vec![b"TTL".to_vec(), key.into_bytes()],
        };
        encode(&argv)
    }
}

// ---- simulated subscribers (M1-S15) ------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SubPlan {
    /// Subscribed to these channel indexes via SUBSCRIBE.
    Channels(Vec<u64>),
    /// PSUBSCRIBE chan:* — receives every publish as pmessage.
    Pattern,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubState {
    /// Waiting for N confirmation frames.
    Subscribing(usize),
    Listening,
    /// Waiting for N unsubscribe confirmations.
    Unsubscribing(usize),
    Closed,
}

pub(crate) struct SimSubscriber {
    pub(crate) index: usize,
    pub(crate) cell: usize,
    pub(crate) fd: RawFd,
    pub(crate) plan: SubPlan,
    pub(crate) state: SubState,
    pub(crate) rx: Vec<u8>,
    /// Messages received, total and per (channel, publisher) with the last
    /// sequence seen (per-publisher FIFO check).
    pub(crate) received: u64,
    pub(crate) last_seq: BTreeMap<(u64, usize), u64>,
}

impl SimSubscriber {
    pub(crate) fn watches(&self, chan: u64) -> bool {
        match &self.plan {
            SubPlan::Channels(set) => set.contains(&chan),
            SubPlan::Pattern => true,
        }
    }

    pub(crate) fn subscribe_wire(&self) -> Vec<u8> {
        match &self.plan {
            SubPlan::Channels(set) => {
                let mut argv = vec![b"SUBSCRIBE".to_vec()];
                argv.extend(set.iter().map(|c| format!("chan:{c}").into_bytes()));
                encode(&argv)
            }
            SubPlan::Pattern => encode(&[b"PSUBSCRIBE".to_vec(), b"chan:*".to_vec()]),
        }
    }

    pub(crate) fn subscriptions(&self) -> usize {
        match &self.plan {
            SubPlan::Channels(set) => set.len(),
            SubPlan::Pattern => 1,
        }
    }

    pub(crate) fn unsubscribe_wire(&self) -> Vec<u8> {
        match &self.plan {
            SubPlan::Channels(_) => encode(&[b"UNSUBSCRIBE".to_vec()]),
            SubPlan::Pattern => encode(&[b"PUNSUBSCRIBE".to_vec()]),
        }
    }

    /// Feeds one delivery, checking channel membership, payload shape, and
    /// the per-(channel, publisher) sequence. Violations describe the seed's
    /// finding precisely.
    pub(crate) fn deliver(&mut self, channel: &[u8], payload: &[u8], violations: &mut Vec<String>) {
        self.received += 1;
        let chan: u64 = match channel.strip_prefix(b"chan:") {
            Some(digits) => core::str::from_utf8(digits).ok().and_then(|s| s.parse().ok()),
            None => None,
        }
        .unwrap_or(u64::MAX);
        if !self.watches(chan) {
            violations.push(format!(
                "subscriber {} got a message for unwatched channel {:?}",
                self.index,
                String::from_utf8_lossy(channel)
            ));
            return;
        }
        let parts: Vec<&[u8]> = payload.split(|&b| b == b':').collect();
        let parsed = (parts.len() == 3 && parts[0] == b"m")
            .then(|| {
                let publisher = core::str::from_utf8(parts[1]).ok()?.parse::<usize>().ok()?;
                let seq = core::str::from_utf8(parts[2]).ok()?.parse::<u64>().ok()?;
                Some((publisher, seq))
            })
            .flatten();
        let Some((publisher, seq)) = parsed else {
            violations.push(format!(
                "subscriber {} got a malformed payload {:?}",
                self.index,
                String::from_utf8_lossy(payload)
            ));
            return;
        };
        let last = self.last_seq.entry((chan, publisher)).or_insert(0);
        if seq != *last + 1 {
            violations.push(format!(
                "subscriber {} chan {chan} publisher {publisher}: seq {seq} after {} \
                 (loss, dup, or reorder)",
                self.index, *last
            ));
        }
        *last = seq.max(*last);
    }
}

/// Deterministic subscription plan: every 4th subscriber watches the
/// pattern, the rest watch two adjacent channels.
pub(crate) fn subscription_plan(index: usize, channels: u64) -> SubPlan {
    if index % 4 == 3 {
        return SubPlan::Pattern;
    }
    let a = index as u64 % channels;
    let b = (index as u64 + 1) % channels;
    let mut set = vec![a];
    if b != a {
        set.push(b);
    }
    SubPlan::Channels(set)
}

// ---- the run ----------------------------------------------------------------------

/// Steps with zero progress (no apply events, no client bytes) before the
/// run is declared stalled — the lost-wakeup detector.
const STALL_STEPS: u64 = 20_000;

/// Runs one scenario to quiescence (all clients done, all connections torn
/// down) or to a stall verdict.
#[allow(clippy::too_many_lines)] // one linear phase script; splitting would scatter the invariants
pub fn run_scenario(scenario: &Scenario) -> SimReport {
    let clock = Rc::new(VirtualClock::new(Nanos(1)));
    let oracle = SharedOracle(Rc::new(RefCell::new(Oracle::new(
        scenario.key_space as usize,
        scenario.namespaces,
    ))));
    let mut rng = SplitMix64::new(scenario.seed);

    // Cells: real plane + loop over the sim driver.
    let mut nets = Vec::new();
    let mut cells = Vec::new();
    let fabrics = Mesh::new(scenario.cells, MeshConfig { ring_capacity: 1024, data_credits: 256 });
    for (i, fabric) in fabrics.into_iter().enumerate() {
        let net = CellNet::new(i as u16, scenario.seed, scenario.plant);
        let driver = SimDriver::new(Rc::clone(&net));
        let pool = BufferPool::new(128, 1024);
        // Sim wall anchor stays (0, 0): wall time == virtual time, fully
        // deterministic; the RANDOMKEY stream is seeded from the scenario.
        let node = Rc::new(NodeInfo::default());
        node.rng_state.set(scenario.seed ^ (0xA11D_0000 + i as u64));
        // Memory-only scenarios never enable the durable tier; pin the
        // defaulted filesystem parameter (M2-S19).
        // Surface scenarios seed their memory namespaces into every cell
        // before it serves — the catalog seed a durable node's boot performs.
        let mut keyspace =
            Keyspace::new(StoreConfig { hasher: node_hasher(scenario.seed), ..Default::default() });
        seed_namespaces(&mut keyspace, scenario.namespaces);
        let plane = ServerPlane::<_, inf_server::StdSegmentFs>::new(
            CellId(i as u16),
            scenario.cells,
            listener_fd(i as u16),
            keyspace,
            fabric,
            node,
            oracle.clone(),
            false,
        );
        let config = LoopConfig { spin_iters: 4, ..Default::default() };
        let cell_loop = CellLoop::new(driver, Rc::clone(&clock), pool, config);
        nets.push(net);
        cells.push((cell_loop, plane));
    }

    // Clients: seeded placement (the SO_REUSEPORT spread analog), per-client
    // command quota, every Nth pipelined.
    let mut clients = Vec::new();
    let per_client = scenario.commands / scenario.connections as u64;
    let remainder = scenario.commands % scenario.connections as u64;
    for i in 0..scenario.connections {
        let cell = (rng.next_u64() % u64::from(scenario.cells)) as usize;
        let fd = nets[cell].borrow_mut().connect();
        let window =
            if scenario.pipelined_every > 0 && i % scenario.pipelined_every == 0 { 4 } else { 1 };
        clients.push(SimClient {
            id: i,
            cell,
            fd,
            quota: per_client + u64::from((i as u64) < remainder),
            sent: 0,
            replied: 0,
            window,
            rx: Vec::new(),
            rng: SplitMix64::new(scenario.seed ^ (0xC11E_0000 + i as u64)),
            closed: false,
            pub_seq: vec![0; scenario.channels as usize],
            checks: VecDeque::new(),
            scope: ClientScope::for_client(i, scenario.namespaces),
            scan: None,
            scan_inflight: false,
        });
    }

    // Subscribers (M1-S15): connect + subscribe up front; publishers are
    // gated on every confirmation (the plane's confirmed ⇒ reachable
    // happens-before, made assertable).
    let mut subs = Vec::new();
    for s in 0..scenario.subscribers {
        let cell = (rng.next_u64() % u64::from(scenario.cells)) as usize;
        let fd = nets[cell].borrow_mut().connect();
        let plan = subscription_plan(s, scenario.channels.max(1));
        let sub = SimSubscriber {
            index: s,
            cell,
            fd,
            plan,
            state: SubState::Subscribing(0),
            rx: Vec::new(),
            received: 0,
            last_seq: BTreeMap::new(),
        };
        nets[cell].borrow_mut().client_send(fd, &sub.subscribe_wire());
        subs.push(SimSubscriber { state: SubState::Subscribing(sub.subscriptions()), ..sub });
    }

    // Auditors (F-L19-05/06): one connection per scope, seeded onto a cell,
    // bound inside the first audit; they speak only while every other
    // client is drained. Created only for audited scenarios, so the m0/m1
    // RNG streams are untouched.
    let mut auditors: Vec<Auditor> = Vec::new();
    if scenario.audit_every > 0 {
        for scope in ClientScope::all(scenario.namespaces) {
            let cell = (rng.next_u64() % u64::from(scenario.cells)) as usize;
            let fd = nets[cell].borrow_mut().connect();
            auditors.push(Auditor { scope, cell, fd, rx: Vec::new(), bound: false, closed: false });
        }
    }
    let mut audit_state = AuditState::Idle;
    let mut next_audit_at = scenario.audit_every;
    let mut final_audit_done = false;
    // The canary is planted once: before the final audit when the scenario
    // audits (so the served-surface comparators are proven too), else at
    // quiescence.
    let mut canary: Option<(ExecScope, Vec<u8>)> = None;
    let mut canary_armed = scenario.canary != Canary::None;

    // Delivery ledger: per-channel receiver plan + per-(channel, publisher)
    // publish counts, filled at send time.
    let chan_count = scenario.channels.max(1) as usize;
    let mut chan_receivers = vec![0i64; chan_count];
    for sub in &subs {
        for (c, slot) in chan_receivers.iter_mut().enumerate() {
            if sub.watches(c as u64) {
                *slot += 1;
            }
        }
    }
    let mut published_per: BTreeMap<(u64, usize), u64> = BTreeMap::new();
    let mut chan_published = vec![0u64; chan_count];

    let mut report = SimReport {
        trace: Vec::new(),
        trace_hash: 0,
        events: 0,
        commands_done: 0,
        oracle_violations: Vec::new(),
        stalled: false,
        scheduler_steps: 0,
        sim_seconds: 0.0,
        published: 0,
        delivered: 0,
        audits: 0,
        flushes: 0,
        scan_walks: 0,
        replays_skipped: 0,
    };
    let mut violations: Vec<String> = Vec::new();

    let mut last_progress = (0u64, 0u64);
    let mut idle_steps = 0u64;
    let mut order: VecDeque<usize> = (0..cells.len()).collect();

    loop {
        report.scheduler_steps += 1;

        // Seeded round-robin with perturbation: rotate, occasionally swap.
        order.rotate_left(1);
        if cells.len() > 1 && rng.next_u64().is_multiple_of(7) {
            let a = (rng.next_u64() as usize) % cells.len();
            let b = (rng.next_u64() as usize) % cells.len();
            order.swap(a, b);
        }
        for &i in &order {
            let (cell_loop, plane) = &mut cells[i];
            cell_loop.run_iteration(plane).expect("sim iteration");
        }

        // Subscriber pump: drain frames, classify, verify deliveries.
        let mut sub_bytes = 0u64;
        for sub in &mut subs {
            if sub.state == SubState::Closed {
                continue;
            }
            let rx = nets[sub.cell].borrow_mut().client_recv(sub.fd);
            sub_bytes += rx.len() as u64;
            sub.rx.extend_from_slice(&rx);
            while let Some(n) = reply_len(&sub.rx) {
                let frame: Vec<u8> = sub.rx.drain(..n).collect();
                match (parse_sub_frame(&frame), sub.state) {
                    (SubFrame::Confirm { .. }, SubState::Subscribing(left)) => {
                        sub.state = if left == 1 {
                            SubState::Listening
                        } else {
                            SubState::Subscribing(left - 1)
                        };
                    }
                    (SubFrame::Confirm { .. }, SubState::Unsubscribing(left)) => {
                        if left == 1 {
                            nets[sub.cell].borrow_mut().client_close(sub.fd);
                            sub.state = SubState::Closed;
                        } else {
                            sub.state = SubState::Unsubscribing(left - 1);
                        }
                    }
                    (
                        SubFrame::Message { channel, payload }
                        | SubFrame::PMessage { channel, payload },
                        SubState::Listening | SubState::Unsubscribing(_),
                    ) => {
                        report.delivered += 1;
                        sub.deliver(&channel, &payload, &mut violations);
                    }
                    (frame, state) => violations
                        .push(format!("subscriber {} got {frame:?} in state {state:?}", sub.index)),
                }
            }
        }
        let subs_ready = subs.iter().all(|s| !matches!(s.state, SubState::Subscribing(_)));

        // Client pump: drain replies, send while the window has room.
        // Publishers hold fire until every subscriber confirmed.
        let mut client_bytes = 0u64;
        for client in &mut clients {
            if client.closed {
                continue;
            }
            let mut net = nets[client.cell].borrow_mut();
            let rx = net.client_recv(client.fd);
            client_bytes += rx.len() as u64;
            client.rx.extend_from_slice(&rx);
            while let Some(n) = reply_len(&client.rx) {
                let check = client.checks.pop_front().unwrap_or(Check::None);
                let raw: Vec<u8> = client.rx.drain(..n).collect();
                client.check_reply(check, &raw, &mut violations, &mut report.scan_walks);
                client.replied += 1;
                report.commands_done += 1;
            }
            assert!(
                client.replied <= client.sent,
                "client got more replies than requests (reply reordering/duplication)"
            );
            // A draining audit holds every client's window until the node
            // is quiescent; closing a finished client is not sending.
            if subs_ready && audit_state == AuditState::Idle {
                while client.sent < client.quota && client.sent - client.replied < client.window {
                    let (wire, check, publish) = client.next_command(scenario);
                    let check = match publish {
                        Some(chan) => {
                            report.published += 1;
                            chan_published[chan as usize] += 1;
                            *published_per.entry((chan, client.id)).or_insert(0) += 1;
                            Check::Publish(chan_receivers[chan as usize])
                        }
                        None => check,
                    };
                    client.checks.push_back(check);
                    client_bytes += wire.len() as u64;
                    net.client_send(client.fd, &wire);
                    client.sent += 1;
                }
            }
            if client.replied == client.quota {
                net.client_close(client.fd);
                client.closed = true;
            }
        }

        // Quiescent audits (F-L19-05/06): arm at the seeded command count,
        // drain, audit, resume; one final audit once every client is done,
        // then the auditors leave so the connection-leak oracle stays exact.
        if scenario.audit_every > 0 {
            let clients_done = clients.iter().all(|c| c.closed);
            let drained = clients.iter().all(|c| c.closed || c.sent == c.replied)
                && nets.iter().all(|n| n.borrow().pending_bytes() == 0)
                && cells.iter().all(|(_, plane)| plane.suspended() == 0);
            match audit_state {
                AuditState::Idle if !clients_done && report.commands_done >= next_audit_at => {
                    audit_state = AuditState::Draining;
                }
                AuditState::Draining if drained => {
                    let label = format!("audit {}", report.audits + 1);
                    run_audit(
                        &mut cells,
                        &nets,
                        &clock,
                        &mut rng,
                        &oracle,
                        &mut auditors,
                        &label,
                        &mut report,
                        &mut violations,
                    );
                    next_audit_at += scenario.audit_every;
                    audit_state = AuditState::Idle;
                }
                _ => {}
            }
            if clients_done && drained && !final_audit_done {
                if canary_armed {
                    canary = plant_canary(
                        &mut oracle.0.borrow_mut().model,
                        scenario.canary,
                        clock.now(),
                    );
                    canary_armed = false;
                }
                run_audit(
                    &mut cells,
                    &nets,
                    &clock,
                    &mut rng,
                    &oracle,
                    &mut auditors,
                    "final audit",
                    &mut report,
                    &mut violations,
                );
                final_audit_done = true;
                for auditor in &mut auditors {
                    nets[auditor.cell].borrow_mut().client_close(auditor.fd);
                    auditor.closed = true;
                }
            }
        }

        // Phase C: publishers done ⇒ subscribers unwind once every published
        // message reached them (a loss parks this transition ⇒ stall ⇒ a
        // replayable seed).
        let publishers_done = clients.iter().all(|c| c.closed);
        if publishers_done {
            for sub in &mut subs {
                if sub.state != SubState::Listening {
                    continue;
                }
                let expected: u64 = (0..chan_count as u64)
                    .filter(|&c| sub.watches(c))
                    .map(|c| chan_published[c as usize])
                    .sum();
                if sub.received == expected {
                    nets[sub.cell].borrow_mut().client_send(sub.fd, &sub.unsubscribe_wire());
                    sub.state = SubState::Unsubscribing(sub.subscriptions());
                }
            }
        }

        // Virtual time per scheduler step, seeded (m0: 1–16 µs).
        clock.advance(Nanos(1_000 + rng.next_u64() % scenario.step_ns_max));

        let all_done = clients.iter().all(|c| c.closed)
            && subs.iter().all(|s| s.state == SubState::Closed)
            && auditors.iter().all(|a| a.closed)
            && nets.iter().all(|n| n.borrow().pending_bytes() == 0)
            && cells.iter().all(|(_, plane)| plane.suspended() == 0);
        if all_done {
            // Run teardown iterations so server-side closes complete.
            for _ in 0..32 {
                for (cell_loop, plane) in &mut cells {
                    cell_loop.run_iteration(plane).expect("teardown iteration");
                }
            }
            break;
        }

        let progress = (
            oracle.0.borrow().events,
            report.commands_done + client_bytes + sub_bytes + report.delivered,
        );
        if progress == last_progress {
            idle_steps += 1;
            if idle_steps >= STALL_STEPS {
                report.stalled = true;
                break;
            }
        } else {
            idle_steps = 0;
            last_progress = progress;
        }
    }

    // Delivery oracle, final ledger check: per (channel, publisher), every
    // watching subscriber saw exactly the published count (no loss survives
    // the stall gate; no dup survives this).
    for sub in &subs {
        for (&(chan, publisher), &count) in &published_per {
            if !sub.watches(chan) {
                continue;
            }
            let got = sub.last_seq.get(&(chan, publisher)).copied().unwrap_or(0);
            if got != count {
                violations.push(format!(
                    "subscriber {} chan {chan} publisher {publisher}: saw seq {got}, \
                     published {count}",
                    sub.index
                ));
            }
        }
    }

    // Accounting reconciliation oracle (M1-S15): equalize active-vs-lazy
    // expiry at one instant, then live records must reconcile exactly; every
    // pub/sub registry and server-side connection must have unwound.
    if !report.stalled {
        let final_now = clock.now();
        let mut node_live = 0u64;
        for (i, (_, plane)) in cells.iter().enumerate() {
            plane.drain_expiry(final_now);
            node_live += plane.keyspace_report().live_records;
            let (channels, patterns, bytes) = plane.pubsub_gauges();
            if channels != 0 || patterns != 0 || bytes != 0 {
                violations.push(format!(
                    "cell {i}: pub/sub registries not empty at quiescence \
                     ({channels} channels, {patterns} patterns, {bytes} bytes)"
                ));
            }
            if plane.connections() != 0 {
                violations.push(format!(
                    "cell {i}: {} server-side connections leaked at quiescence",
                    plane.connections()
                ));
            }
        }
        {
            let mut oracle = oracle.0.borrow_mut();
            loop {
                let stats = oracle.model.expire_tick(
                    final_now,
                    ExpiryBudget { max_fires: u32::MAX, max_steps: u32::MAX },
                );
                if stats.reaped == 0 && stats.stale == 0 {
                    break;
                }
            }
            let model_live = oracle.model.report().live_records;
            if node_live != model_live {
                violations.push(format!(
                    "live-record reconciliation failed: node {node_live} vs model {model_live}"
                ));
            }
            // Content oracle (review of 2026-08-30, §5.5 Group 0 and
            // F-L19-06): the scalar comparison above is blind to *count
            // right, contents wrong* — the proven Criticals' exact signature
            // — so the entries themselves (key, value, deadline) must agree
            // at quiescence, over the numbered dbs and the memory namespaces.
            // Cells partition every store by slot, so the per-cell union is
            // the node set and a key on two cells is its own violation. The
            // canary, when armed, damages the model first — the tests prove
            // the comparator sees each damage class.
            if canary_armed {
                canary = plant_canary(&mut oracle.model, scenario.canary, final_now);
            }
            let node = fold_node_entries(&cells, final_now, &mut violations);
            let model = fold_model_entries(&mut oracle.model, final_now);
            if let Some(violation) = reconcile_entries(&node, &model, "quiescence") {
                violations.push(violation);
            } else if let Some((scope, key)) = canary {
                violations.push(format!(
                    "canary {:?} on {scope:?} {:?} went unnoticed by the content oracle",
                    scenario.canary,
                    String::from_utf8_lossy(&key)
                ));
            }
        }
    }

    let oracle = oracle.0.borrow();
    report.events = oracle.events;
    report.replays_skipped = oracle.replays_skipped;
    report.trace = oracle.trace.clone();
    report.trace_hash = hash64(&report.trace, 0x51A1);
    report.oracle_violations = oracle.violations.clone();
    report.oracle_violations.extend(violations);
    report.sim_seconds = clock.now().0.saturating_sub(1) as f64 / 1e9;
    report
}

// ---- quiescent audits + content reconciliation (review of 2026-08-30) ---------------

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum AuditState {
    Idle,
    /// Clients hold their windows until every in-flight command answered.
    Draining,
}

/// One scope's audit connection.
struct Auditor {
    scope: ClientScope,
    cell: usize,
    fd: RawFd,
    rx: Vec<u8>,
    bound: bool,
    closed: bool,
}

/// `(scope, key) → (value, expiry deadline in internal ms)` — the content
/// both sides are compared on.
type Entries = BTreeMap<(ExecScope, Vec<u8>), (Vec<u8>, Option<u64>)>;

type Cells = Vec<(
    CellLoop<SimDriver, Rc<VirtualClock>>,
    ServerPlane<SharedOracle, inf_server::StdSegmentFs>,
)>;

/// Every cell's live entries at `now`. A key present on two cells breaks
/// slot ownership and is reported as such rather than folded away.
fn fold_node_entries(cells: &Cells, now: Nanos, violations: &mut Vec<String>) -> Entries {
    let mut node: Entries = BTreeMap::new();
    for (i, (_, plane)) in cells.iter().enumerate() {
        plane.fold_live_entries(now, |scope, key, value, deadline| {
            if node.insert((scope, key.to_vec()), (value.to_vec(), deadline)).is_some() {
                violations.push(format!(
                    "key {:?} on {scope:?} is live on cell {i} and on another cell",
                    String::from_utf8_lossy(key)
                ));
            }
        });
    }
    node
}

/// The model's live entries at `now`, through the node's own walker.
fn fold_model_entries(model: &mut Keyspace, now: Nanos) -> Entries {
    let mut entries: Entries = BTreeMap::new();
    fold_live_entries(model, now, |scope, key, value, deadline| {
        entries.insert((scope, key.to_vec()), (value.to_vec(), deadline));
    });
    entries
}

fn entry_name(scope: ExecScope, key: &[u8]) -> String {
    format!("{scope:?}:{}", String::from_utf8_lossy(key))
}

/// Exact comparison of two entry maps; `None` when they agree, otherwise
/// one violation naming counts and up to five examples per difference
/// class (missing, extra, value, deadline).
fn reconcile_entries(node: &Entries, model: &Entries, label: &str) -> Option<String> {
    if node == model {
        return None;
    }
    let sample =
        |iter: &mut dyn Iterator<Item = String>| iter.take(5).collect::<Vec<_>>().join(", ");
    let node_only = sample(
        &mut node.keys().filter(|k| !model.contains_key(*k)).map(|(s, k)| entry_name(*s, k)),
    );
    let model_only = sample(
        &mut model.keys().filter(|k| !node.contains_key(*k)).map(|(s, k)| entry_name(*s, k)),
    );
    let values = sample(&mut node.iter().filter_map(|(k, (nv, _))| {
        let (mv, _) = model.get(k)?;
        (nv != mv).then(|| {
            format!(
                "{} node {:?} vs model {:?}",
                entry_name(k.0, &k.1),
                String::from_utf8_lossy(&nv[..nv.len().min(24)]),
                String::from_utf8_lossy(&mv[..mv.len().min(24)])
            )
        })
    }));
    let deadlines = sample(&mut node.iter().filter_map(|(k, (_, nd))| {
        let (_, md) = model.get(k)?;
        (nd != md).then(|| format!("{} node {nd:?} vs model {md:?}", entry_name(k.0, &k.1)))
    }));
    Some(format!(
        "content reconciliation failed ({label}): node {} entries vs model {} \
         (node-only: [{node_only}] model-only: [{model_only}] value mismatches: [{values}] \
         deadline mismatches: [{deadlines}])",
        node.len(),
        model.len(),
    ))
}

/// Reaps every already-expired entry on both sides at `now` (active vs lazy
/// expiry equalized) so served counts are comparable.
fn equalize_expiry(cells: &Cells, model: &mut Keyspace, now: Nanos) {
    for (_, plane) in cells {
        plane.drain_expiry(now);
    }
    loop {
        let stats =
            model.expire_tick(now, ExpiryBudget { max_fires: u32::MAX, max_steps: u32::MAX });
        if stats.reaped == 0 && stats.stale == 0 {
            break;
        }
    }
}

/// Sends one command on an auditor connection and drives the cells until
/// its reply is complete. The clock does **not** advance: nothing else is
/// in flight, and a frozen `now` keeps every served count comparable with
/// the model folded at the same instant. Bounded by the stall detector's
/// step budget.
fn audit_roundtrip(
    cells: &mut Cells,
    nets: &[Rc<RefCell<CellNet>>],
    auditor: &mut Auditor,
    wire: &[u8],
) -> Result<Vec<u8>, String> {
    nets[auditor.cell].borrow_mut().client_send(auditor.fd, wire);
    for _ in 0..STALL_STEPS {
        for (cell_loop, plane) in cells.iter_mut() {
            cell_loop.run_iteration(plane).expect("audit iteration");
        }
        let rx = nets[auditor.cell].borrow_mut().client_recv(auditor.fd);
        auditor.rx.extend_from_slice(&rx);
        if let Some(n) = reply_len(&auditor.rx) {
            return Ok(auditor.rx.drain(..n).collect());
        }
    }
    Err(format!(
        "auditor {} did not answer within {STALL_STEPS} steps (command {:?})",
        auditor.scope.name(),
        String::from_utf8_lossy(wire)
    ))
}

/// Bulks of a `KEYS` / `SCAN` page reply.
fn bulk_set(items: &[Reply]) -> Option<BTreeSet<Vec<u8>>> {
    items
        .iter()
        .map(|item| match item {
            Reply::Bulk(key) => Some(key.clone()),
            _ => None,
        })
        .collect()
}

/// Pages one audit `SCAN` walk may take (a full node walk at `COUNT 1` over
/// a few hundred keys stays far below it).
const AUDIT_SCAN_PAGES_MAX: u32 = 65_536;

/// One quiescent audit: expiry equalized, stored content reconciled, then
/// per scope the served surface over the wire against the model's entries
/// for that scope, then (seeded) a flush replayed on the model and a second
/// content pass.
#[allow(clippy::too_many_arguments)] // one linear audit script over the run's state
fn run_audit(
    cells: &mut Cells,
    nets: &[Rc<RefCell<CellNet>>],
    clock: &Rc<VirtualClock>,
    rng: &mut SplitMix64,
    oracle: &SharedOracle,
    auditors: &mut [Auditor],
    label: &str,
    report: &mut SimReport,
    violations: &mut Vec<String>,
) {
    let now = clock.now();
    report.audits += 1;
    // The oracle is borrowed only between roundtrips: driving the cells
    // re-enters `on_execute`, which takes the same `RefCell`.
    let model = {
        let mut oracle = oracle.0.borrow_mut();
        equalize_expiry(cells, &mut oracle.model, now);
        fold_model_entries(&mut oracle.model, now)
    };
    let node = fold_node_entries(cells, now, violations);
    if let Some(violation) = reconcile_entries(&node, &model, label) {
        violations.push(violation);
    }
    // Served surface per scope — the plane's scatter programs end to end.
    for auditor in auditors.iter_mut() {
        let who = format!("{label}, auditor {}", auditor.scope.name());
        if !auditor.bound
            && let Some(bind) = auditor.scope.bind_wire()
        {
            match audit_roundtrip(cells, nets, auditor, &bind) {
                Ok(reply) if reply == b"+OK\r\n" => {}
                Ok(reply) => violations
                    .push(format!("{who}: binding refused: {:?}", String::from_utf8_lossy(&reply))),
                Err(e) => violations.push(format!("{who}: {e}")),
            }
        }
        auditor.bound = true;
        let scope = auditor.scope.exec_scope();
        let expected: BTreeSet<Vec<u8>> =
            model.keys().filter(|(s, _)| *s == scope).map(|(_, k)| k.clone()).collect();
        // DBSIZE: the scope's node-wide count.
        match audit_roundtrip(cells, nets, auditor, &encode(&[b"DBSIZE".to_vec()])) {
            Ok(reply) => {
                let want = Reply::Int(expected.len() as i64);
                let got = parse_reply(&reply);
                if got != want {
                    violations.push(format!("{who}: DBSIZE {got:?}, model {want:?}"));
                }
            }
            Err(e) => violations.push(format!("{who}: {e}")),
        }
        // SCAN: a full walk at a seeded COUNT must enumerate the scope's set.
        let count = [1u64, 7, 50][(rng.next_u64() % 3) as usize];
        let mut cursor = 0u64;
        let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
        let mut pages = 0u32;
        loop {
            let argv = vec![
                b"SCAN".to_vec(),
                cursor.to_string().into_bytes(),
                b"COUNT".to_vec(),
                count.to_string().into_bytes(),
            ];
            let reply = match audit_roundtrip(cells, nets, auditor, &encode(&argv)) {
                Ok(reply) => parse_reply(&reply),
                Err(e) => {
                    violations.push(format!("{who}: {e}"));
                    break;
                }
            };
            let page = match &reply {
                Reply::Array(items) if items.len() == 2 => match (&items[0], &items[1]) {
                    (Reply::Bulk(digits), Reply::Array(keys)) => core::str::from_utf8(digits)
                        .ok()
                        .and_then(|text| text.parse::<u64>().ok())
                        .zip(bulk_set(keys)),
                    _ => None,
                },
                _ => None,
            };
            let Some((next, keys)) = page else {
                violations.push(format!("{who}: SCAN answered {reply:?}"));
                break;
            };
            seen.extend(keys);
            pages += 1;
            cursor = next;
            if cursor == 0 {
                break;
            }
            if pages >= AUDIT_SCAN_PAGES_MAX {
                violations.push(format!("{who}: SCAN walk did not terminate"));
                break;
            }
        }
        if cursor == 0 && seen != expected {
            violations.push(format!(
                "{who}: SCAN COUNT {count} walk enumerated {} keys, model holds {} \
                 (missing: [{}] extra: [{}])",
                seen.len(),
                expected.len(),
                sample_keys(expected.difference(&seen)),
                sample_keys(seen.difference(&expected)),
            ));
        }
        // KEYS: a seeded glob over the alphabet.
        let glob = auditor.scope.glob(rng.next_u64() % 10);
        let want: BTreeSet<Vec<u8>> =
            expected.iter().filter(|k| glob_match(&glob, k, false)).cloned().collect();
        match audit_roundtrip(cells, nets, auditor, &encode(&[b"KEYS".to_vec(), glob.clone()])) {
            Ok(reply) => match parse_reply(&reply) {
                Reply::Array(items) if bulk_set(&items).is_some_and(|got| got == want) => {}
                other => violations.push(format!(
                    "{who}: KEYS {:?} answered {other:?}, model set has {} keys",
                    String::from_utf8_lossy(&glob),
                    want.len()
                )),
            },
            Err(e) => violations.push(format!("{who}: {e}")),
        }
        // RANDOMKEY: membership (the two-level draw is the recorded deviation).
        match audit_roundtrip(cells, nets, auditor, &encode(&[b"RANDOMKEY".to_vec()])) {
            Ok(reply) => match parse_reply(&reply) {
                Reply::Nil if expected.is_empty() => {}
                Reply::Bulk(key) if expected.contains(&key) => {}
                other => violations.push(format!(
                    "{who}: RANDOMKEY answered {other:?} against a {}-key scope",
                    expected.len()
                )),
            },
            Err(e) => violations.push(format!("{who}: {e}")),
        }
    }
    // A seeded flush, replayed on the model at this quiescent point (the
    // apply seam never replays flush legs), then the content pass again.
    if !auditors.is_empty() && rng.next_u64().is_multiple_of(4) {
        let index = (rng.next_u64() as usize) % auditors.len();
        let auditor = &mut auditors[index];
        let argv: Vec<Vec<u8>> = if rng.next_u64().is_multiple_of(2) {
            vec![b"FLUSHALL".to_vec()]
        } else {
            vec![b"FLUSHDB".to_vec()]
        };
        let who = format!("{label}, auditor {}", auditor.scope.name());
        let mut expected = Vec::new();
        {
            let slices: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
            let mut cx = model_cx(auditor.scope.exec_scope());
            let mut oracle = oracle.0.borrow_mut();
            execute_slices(&slices, &mut oracle.model, &mut cx, now, &mut expected);
        }
        match audit_roundtrip(cells, nets, auditor, &encode(&argv)) {
            Ok(reply) if reply == expected => {}
            Ok(reply) => violations.push(format!(
                "{who}: {:?} answered {:?}, model {:?}",
                String::from_utf8_lossy(&argv[0]),
                String::from_utf8_lossy(&reply),
                String::from_utf8_lossy(&expected)
            )),
            Err(e) => violations.push(format!("{who}: {e}")),
        }
        report.flushes += 1;
        let node = fold_node_entries(cells, now, violations);
        let model = fold_model_entries(&mut oracle.0.borrow_mut().model, now);
        if let Some(violation) = reconcile_entries(
            &node,
            &model,
            &format!("{label} after {}", String::from_utf8_lossy(&argv[0])),
        ) {
            violations.push(violation);
        }
    }
}

fn sample_keys<'k>(keys: impl Iterator<Item = &'k Vec<u8>>) -> String {
    keys.take(5).map(|k| String::from_utf8_lossy(k).into_owned()).collect::<Vec<_>>().join(", ")
}

/// Damages the model per `canary` (tests only) and names the entry it
/// touched; `None` when nothing was planted or no suitable entry exists.
fn plant_canary(model: &mut Keyspace, canary: Canary, now: Nanos) -> Option<(ExecScope, Vec<u8>)> {
    if canary == Canary::None {
        return None;
    }
    let entries = fold_model_entries(model, now);
    let target = entries
        .iter()
        .find(|(_, (_, deadline))| canary != Canary::DropDeadline || deadline.is_some())
        .map(|((scope, key), _)| (*scope, key.clone()))?;
    let (scope, key) = &target;
    let store = match scope {
        ExecScope::Db(db) => model.db_mut(usize::from(*db)),
        ExecScope::Ns(ns) => model.ns_store_mut(*ns).expect("folded entry's namespace exists"),
        ExecScope::Unavailable => unreachable!("no entries fold under an unavailable scope"),
    };
    match canary {
        Canary::None => unreachable!("handled above"),
        Canary::DropKey => {
            store.del(key, now);
        }
        Canary::CorruptValue => {
            store.set(key, b"canary", SetOptions::default(), now).expect("in-bounds canary value");
        }
        Canary::DropDeadline => {
            store.expire(key, None, ExpireCond::Always, now);
        }
    }
    Some(target)
}
