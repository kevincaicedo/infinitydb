//! The deterministic harness: scenario config, seeded scheduler, simulated
//! clients, the single-store oracle, and the event-trace recorder.
//!
//! ## Oracle
//! Every apply point (the `PlaneObserver` seam) replays the same argv
//! against ONE model `CellStore` — in apply order, which on a single thread
//! is a true total order — and the model's reply must equal the observed
//! reply byte-for-byte. This is the single-key linearizability oracle at M0
//! strength: cross-cell routing, fabric transport, and pump ordering cannot
//! corrupt, lose, duplicate, or reorder an apply without the model
//! diverging. Per-connection reply ordering is checked client-side (every
//! client validates its replies arrive in request order and parse).
//!
//! ## Trace
//! The trace is the byte log of `(cell, origin, argv, reply)` events plus
//! client completions. Same seed ⇒ byte-identical trace (the determinism
//! AC); the comparator just memcmps two runs.

use core::cell::RefCell;
use std::collections::VecDeque;
use std::os::fd::RawFd;
use std::rc::Rc;

use inf_alloc::BufferPool;
use inf_fabric::{Mesh, MeshConfig};
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::{Nanos, VirtualClock};
use inf_foundation::{CellId, hash64};
use inf_runtime::{CellLoop, LoopConfig};
use inf_server::{ConnCx, ExecOrigin, NodeInfo, PlaneObserver, ServerPlane, execute_slices};
use inf_store::{CellStore, StoreConfig};
use inf_wire::Protocol;

use crate::net::{CellNet, Plant, SimDriver, listener_fd};
use crate::resp::reply_len;

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
}

impl SimReport {
    pub fn ok(&self) -> bool {
        !self.stalled && self.oracle_violations.is_empty()
    }
}

// ---- oracle observer -----------------------------------------------------------

struct Oracle {
    model: CellStore,
    trace: Vec<u8>,
    events: u64,
    violations: Vec<String>,
}

impl Oracle {
    fn new(keys: usize) -> Oracle {
        Oracle {
            model: CellStore::new(StoreConfig { initial_keys: keys, ..Default::default() }),
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
}

impl SimClient {
    fn next_command(&mut self, key_space: u64) -> Vec<u8> {
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
        let mut wire = format!("*{}\r\n", argv.len()).into_bytes();
        for arg in &argv {
            wire.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
            wire.extend_from_slice(arg);
            wire.extend_from_slice(b"\r\n");
        }
        wire
    }
}

// ---- the run ----------------------------------------------------------------------

/// Steps with zero progress (no apply events, no client bytes) before the
/// run is declared stalled — the lost-wakeup detector.
const STALL_STEPS: u64 = 20_000;

/// Runs one scenario to quiescence (all clients done, all connections torn
/// down) or to a stall verdict.
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
            CellStore::new(StoreConfig::default()),
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
            cell,
            fd,
            quota: per_client + u64::from((i as u64) < remainder),
            sent: 0,
            replied: 0,
            window,
            rx: Vec::new(),
            rng: SplitMix64::new(scenario.seed ^ (0xC11E_0000 + i as u64)),
            closed: false,
        });
    }

    let mut report = SimReport {
        trace: Vec::new(),
        trace_hash: 0,
        events: 0,
        commands_done: 0,
        oracle_violations: Vec::new(),
        stalled: false,
        scheduler_steps: 0,
    };

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

        // Client pump: drain replies, send while the window has room.
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
                client.rx.drain(..n);
                client.replied += 1;
                report.commands_done += 1;
            }
            assert!(
                client.replied <= client.sent,
                "client got more replies than requests (reply reordering/duplication)"
            );
            while client.sent < client.quota && client.sent - client.replied < client.window {
                let wire = client.next_command(scenario.key_space);
                client_bytes += wire.len() as u64;
                net.client_send(client.fd, &wire);
                client.sent += 1;
            }
            if client.replied == client.quota {
                net.client_close(client.fd);
                client.closed = true;
            }
        }

        // Virtual time: 1–16 µs per scheduler step, seeded.
        clock.advance(Nanos(1_000 + rng.next_u64() % 15_000));

        let all_done = clients.iter().all(|c| c.closed)
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

        let progress = (oracle.0.borrow().events, report.commands_done + client_bytes);
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

    let oracle = oracle.0.borrow();
    report.events = oracle.events;
    report.trace = oracle.trace.clone();
    report.trace_hash = hash64(&report.trace, 0x51A1);
    report.oracle_violations = oracle.violations.clone();
    report
}
