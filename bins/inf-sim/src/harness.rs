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
//!
//! ## Trace
//! The trace is the byte log of `(cell, origin, argv, reply)` events plus
//! client completions. Same seed ⇒ byte-identical trace (the determinism
//! AC); the comparator just memcmps two runs.

use core::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::os::fd::RawFd;
use std::rc::Rc;

use inf_alloc::BufferPool;
use inf_fabric::{Mesh, MeshConfig};
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::{Clock, Nanos, VirtualClock};
use inf_foundation::{CellId, hash64};
use inf_runtime::{CellLoop, LoopConfig};
use inf_server::{ConnCx, ExecOrigin, NodeInfo, PlaneObserver, ServerPlane, execute_slices};
use inf_store::{ExpiryBudget, Keyspace, StoreConfig};
use inf_wire::Protocol;

use crate::net::{CellNet, Plant, SimDriver, listener_fd};
use crate::resp::{SubFrame, parse_sub_frame, reply_len};

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
}

impl Oracle {
    fn new(keys: usize) -> Oracle {
        Oracle {
            model: Keyspace::new(StoreConfig { initial_keys: keys, ..Default::default() }),
            trace: Vec::new(),
            events: 0,
            violations: Vec::new(),
        }
    }
}

#[derive(Clone)]
struct SharedOracle(Rc<RefCell<Oracle>>);

impl PlaneObserver for SharedOracle {
    fn on_execute(
        &mut self,
        cell: CellId,
        origin: ExecOrigin,
        argv: &[&[u8]],
        reply: &[u8],
        now: Nanos,
    ) {
        let mut oracle = self.0.borrow_mut();
        oracle.events += 1;
        // Trace record: cell, origin tag, argv, reply (length-prefixed).
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
        oracle.trace.push(argv.len() as u8);
        for arg in argv {
            oracle.trace.extend_from_slice(&(arg.len() as u32).to_le_bytes());
            oracle.trace.extend_from_slice(arg);
        }
        oracle.trace.extend_from_slice(&(reply.len() as u32).to_le_bytes());
        oracle.trace.extend_from_slice(reply);

        // Model replay: same argv, same injected time, RESP2 (the scenario
        // mixes never switch protocols).
        let mut expected = Vec::new();
        let mut cx = ConnCx { proto: Protocol::Resp2, id: 0, ..Default::default() };
        let Oracle { model, violations, .. } = &mut *oracle;
        execute_slices(argv, model, &mut cx, now, &mut expected);
        if expected != reply {
            let argv_text: Vec<String> =
                argv.iter().map(|a| String::from_utf8_lossy(a).into_owned()).collect();
            violations.push(format!(
                "apply divergence on cell {cell} {argv_text:?}: node {:?} vs model {:?}",
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
    /// Expected integer reply per in-flight command (`Some` = PUBLISH with
    /// its planned receiver count; `None` = unchecked, the apply oracle owns
    /// it). Parallel to the in-flight window.
    expect: VecDeque<Option<i64>>,
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
    /// Next command. Returns the wire bytes plus, for a PUBLISH, the channel
    /// index (the harness updates the delivery ledger at send time).
    fn next_command(&mut self, scenario: &Scenario) -> (Vec<u8>, Option<u64>) {
        if scenario.publish_percent == 0 {
            return (self.next_command_m0(scenario.key_space), None);
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
            return (encode(&argv), Some(chan));
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
        (encode(&argv), None)
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
enum SubPlan {
    /// Subscribed to these channel indexes via SUBSCRIBE.
    Channels(Vec<u64>),
    /// PSUBSCRIBE chan:* — receives every publish as pmessage.
    Pattern,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubState {
    /// Waiting for N confirmation frames.
    Subscribing(usize),
    Listening,
    /// Waiting for N unsubscribe confirmations.
    Unsubscribing(usize),
    Closed,
}

struct SimSubscriber {
    index: usize,
    cell: usize,
    fd: RawFd,
    plan: SubPlan,
    state: SubState,
    rx: Vec<u8>,
    /// Messages received, total and per (channel, publisher) with the last
    /// sequence seen (per-publisher FIFO check).
    received: u64,
    last_seq: BTreeMap<(u64, usize), u64>,
}

impl SimSubscriber {
    fn watches(&self, chan: u64) -> bool {
        match &self.plan {
            SubPlan::Channels(set) => set.contains(&chan),
            SubPlan::Pattern => true,
        }
    }

    fn subscribe_wire(&self) -> Vec<u8> {
        match &self.plan {
            SubPlan::Channels(set) => {
                let mut argv = vec![b"SUBSCRIBE".to_vec()];
                argv.extend(set.iter().map(|c| format!("chan:{c}").into_bytes()));
                encode(&argv)
            }
            SubPlan::Pattern => encode(&[b"PSUBSCRIBE".to_vec(), b"chan:*".to_vec()]),
        }
    }

    fn subscriptions(&self) -> usize {
        match &self.plan {
            SubPlan::Channels(set) => set.len(),
            SubPlan::Pattern => 1,
        }
    }

    fn unsubscribe_wire(&self) -> Vec<u8> {
        match &self.plan {
            SubPlan::Channels(_) => encode(&[b"UNSUBSCRIBE".to_vec()]),
            SubPlan::Pattern => encode(&[b"PUNSUBSCRIBE".to_vec()]),
        }
    }

    /// Feeds one delivery, checking channel membership, payload shape, and
    /// the per-(channel, publisher) sequence. Violations describe the seed's
    /// finding precisely.
    fn deliver(&mut self, channel: &[u8], payload: &[u8], violations: &mut Vec<String>) {
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
fn subscription_plan(index: usize, channels: u64) -> SubPlan {
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
    let oracle = SharedOracle(Rc::new(RefCell::new(Oracle::new(scenario.key_space as usize))));
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
        let plane = ServerPlane::new(
            CellId(i as u16),
            scenario.cells,
            listener_fd(i as u16),
            Keyspace::new(StoreConfig::default()),
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
            expect: VecDeque::new(),
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
                if let Some(Some(want)) = client.expect.pop_front() {
                    let got = client.rx[..n].to_vec();
                    let want_wire = format!(":{want}\r\n").into_bytes();
                    if got != want_wire {
                        violations.push(format!(
                            "publisher {}: PUBLISH replied {:?}, planned receiver count {want}",
                            client.id,
                            String::from_utf8_lossy(&got)
                        ));
                    }
                }
                client.rx.drain(..n);
                client.replied += 1;
                report.commands_done += 1;
            }
            assert!(
                client.replied <= client.sent,
                "client got more replies than requests (reply reordering/duplication)"
            );
            if !subs_ready {
                continue;
            }
            while client.sent < client.quota && client.sent - client.replied < client.window {
                let (wire, publish) = client.next_command(scenario);
                if let Some(chan) = publish {
                    report.published += 1;
                    chan_published[chan as usize] += 1;
                    *published_per.entry((chan, client.id)).or_insert(0) += 1;
                    client.expect.push_back(Some(chan_receivers[chan as usize]));
                } else {
                    client.expect.push_back(None);
                }
                client_bytes += wire.len() as u64;
                net.client_send(client.fd, &wire);
                client.sent += 1;
            }
            if client.replied == client.quota {
                net.client_close(client.fd);
                client.closed = true;
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
        }
    }

    let oracle = oracle.0.borrow();
    report.events = oracle.events;
    report.trace = oracle.trace.clone();
    report.trace_hash = hash64(&report.trace, 0x51A1);
    report.oracle_violations = oracle.violations.clone();
    report.oracle_violations.extend(violations);
    report.sim_seconds = clock.now().0.saturating_sub(1) as f64 / 1e9;
    report
}
