//! The M2-S19 durable scenario + durability oracle (ADR-0021, master plan
//! §17.1): the **real** durable node — cells, log spine, group commit,
//! checkpoints, MANIFEST swaps, truncation, catalog DDL, recovery — runs
//! single-threaded over one shared [`SimDisk`], and the oracle checks the
//! §8.2 promise against the **ack stream**:
//!
//! - every `always`-acked write survives any power cut;
//! - `everysec` loses at most 1 s of **simulated** time;
//! - an un-acked write may legally land or vanish (recovery replays a log
//!   prefix — exactly one suffix point materializes).
//!
//! Keys are client-private (`k:<client>:<n>`) so per-key op order is the
//! connection's send order; cross-client conflict semantics stay the
//! M0/M1 linearizability oracle's territory (ADR-0021 D3). A violation
//! reports the seed, key, ledger tail, and recovered value — replayable
//! byte-identically via `inf-sim --scenario m2-durable --seed N`.
//!
//! The control plane runs **detached** (ADR-0021 D2): the harness drains
//! catalog swaps + delegated unlinks inline each scheduler step, so DDL
//! acks, checkpoint epochs, truncation, and the ADR-0017 unlink
//! resurrection cases all happen inside the deterministic loop.

use core::cell::RefCell;
use std::collections::BTreeMap;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::rc::Rc;

use inf_alloc::BufferPool;
use inf_fabric::{Mesh, MeshConfig};
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::{Clock, Nanos, VirtualClock};
use inf_foundation::{CellId, hash64};
use inf_log::ckpt::{IckReaderConfig, ick_file_name, read_ick};
use inf_log::{ReaderConfig, SegmentId, SegmentReader, read_manifest, scan_log_dir_from};
use inf_runtime::{CellLoop, LoopConfig};
use inf_server::{
    ControlInbox, ExecOrigin, NodeInfo, PlaneObserver, SegmentIoMode, ServerPlane, SimDisk,
    SimDiskConfig, StallConfig, load_catalog_from,
};
use inf_store::{Keyspace, NsId, StoreConfig, WallAnchor};

use crate::net::{CellNet, Plant, SimDriver, listener_fd};
use crate::resp::reply_len;

/// The everysec loss window plus scheduler slop (virtual time): a write
/// acked this long before the cut must survive it (§8.2).
pub(crate) const EVERYSEC_WINDOW: Nanos = Nanos(1_100_000_000);

/// Scheduler steps with zero progress before a stall verdict.
pub(crate) const STALL_STEPS: u64 = 50_000;

/// Command/state vocabulary driven through the same durability machine.
/// Keeping this as data on the scenario prevents M3 from growing a second
/// node, disk, ledger, or recovery implementation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DurableWorkload {
    KeyValue,
    Document,
}

/// The everysec-deferral bound (M2.5-S14, virtual time): an `everysec`
/// write acks on execution (no watermark gate — plane.rs), so its
/// client-visible latency is bounded by scheduling alone, independent of
/// the device. Honest scheduling costs ~ln(wire bytes) delivery reaps ×
/// ≤ 2 ms steps ≈ 6 ms mean; 30 ms sits beyond that tail yet far below
/// the 50–90 ms stall episodes (and the ~1 s tick) that a device-gated
/// everysec ack would inherit. (The design sketched 10 ms; measured
/// honest chunking tails cross it — bound revised, deviation recorded.)
pub(crate) const EVERYSEC_ACK_BOUND: Nanos = Nanos(30_000_000);

#[derive(Clone, Debug)]
pub struct DurableScenario {
    pub seed: u64,
    pub workload: DurableWorkload,
    pub cells: u16,
    /// Writers per namespace class (`always` / `everysec`), plus
    /// default-DB memory writers interleaved (the zero-cost coexistence).
    pub always_writers: usize,
    pub esec_writers: usize,
    pub mem_writers: usize,
    /// Ops per writer.
    pub ops_per_writer: u64,
    pub keys_per_writer: u64,
    pub value_max: u64,
    /// Max virtual nanoseconds per scheduler step. Sized so runs span
    /// several simulated seconds — the everysec window is real.
    pub step_ns_max: u64,
    /// A second power cut lands *during* recovery (interrupted-recovery
    /// idempotence, in-sim).
    pub double_cut: bool,
    pub plant: Plant,
    /// Small segments/intervals: rotations, checkpoint cycles, MANIFEST
    /// swaps, and truncation all happen inside a short run.
    pub segment_bytes: u32,
    pub ckpt_interval_bytes: u64,
    /// Checkpoint stream pacing override (M4.5-S06): `Some(rate)` keeps
    /// a stream open across many scheduler steps so scenarios can storm
    /// the fuzzy window; `None` = the production default.
    pub ckpt_stream_bytes_per_sec: Option<u32>,
    /// Device service-time model (M2.5-S14). `None` = instant fsyncs
    /// (the pre-S14 device); `m2_durable` arms the reference stall
    /// device so the fleet sees nonzero fsync latency every night.
    pub stall: Option<StallConfig>,
    /// M3-S23 canary (ADR-0045 D2): the equivalence oracle's shadow
    /// replay skips the first `DocDelta` it sees — the planted bug the
    /// fleet must catch within 100 seeds.
    pub replay_canary: bool,
    /// Log-segment I/O mode (M4.5-S34, ADR-0086 D8): `Direct` runs the
    /// zero-fill state machine, v3 frames, write-through barriers, and
    /// the class-upgrade/not-ready rotations on the sim disk; `Buffered`
    /// is the pre-S34 scenario byte-for-byte. `m2_durable` alternates by
    /// seed so every sweep covers both classes.
    pub io_mode: SegmentIoMode,
}

/// The S14 reference stall device: ~120 µs base (warm NVMe fdatasync),
/// 3% heavy tail up to 8× base, and a 50–90 ms stall episode roughly
/// every 1.5 sim-seconds — 1–3 episodes land inside a durable run's
/// several-sim-second span.
pub(crate) fn m2_stall_config() -> StallConfig {
    StallConfig {
        base_ns: 120_000,
        tail_permille: 30,
        tail_mult: 8,
        episode_gap_ns: 1_500_000_000,
        episode_ms_min: 50,
        episode_ms_max: 90,
        // ~3× cheaper than the fsync base: the probed FUA/FLUSH ratio on
        // the reference device (ADR-0086 D8); scenarios without `Direct`
        // segments never draw it.
        through_base_ns: 40_000,
    }
}

/// Builds the scenario's disk: stall-modeled when armed, instant
/// otherwise. The stall seed derives from the scenario seed (L7).
pub(crate) fn build_disk(seed: u64, stall: Option<&StallConfig>) -> SimDisk {
    match stall {
        Some(cfg) => SimDisk::with_stall(SimDiskConfig::default(), cfg.clone(), seed ^ 0x57A1_1ED0),
        None => SimDisk::new(),
    }
}

impl DurableScenario {
    #[must_use]
    pub fn m2_durable(seed: u64) -> DurableScenario {
        DurableScenario {
            seed,
            workload: DurableWorkload::KeyValue,
            cells: 2,
            always_writers: 3,
            esec_writers: 3,
            mem_writers: 2,
            ops_per_writer: 140,
            keys_per_writer: 6,
            value_max: 48,
            // ~1 ms average steps: runs span several simulated seconds, so
            // everysec ticks fire mid-run and the loss window genuinely
            // divides the ledger into required vs allowed-lost.
            step_ns_max: 2_000_000,
            // Seed-diverse shapes: every 8th seed double-cuts.
            double_cut: seed % 8 == 3,
            plant: Plant::None,
            segment_bytes: 16 << 10,
            ckpt_interval_bytes: 24 << 10,
            ckpt_stream_bytes_per_sec: None,
            stall: Some(m2_stall_config()),
            replay_canary: false,
            // Odd seeds run the FUA class (ADR-0086 D8): half of every
            // sweep exercises mixed write-through/FLUSH frames, zero-fill
            // barriers, and seal × write-through crossings under cuts.
            io_mode: if seed % 2 == 1 { SegmentIoMode::Direct } else { SegmentIoMode::Buffered },
        }
    }

    /// M3-S18/S23/S24 document workload. It deliberately retains the M2
    /// disk, scheduling, checkpoint, fsync, and ack machinery; only the
    /// commands and audit reads change. One key per writer plus ~90
    /// mutations between root sets crosses the 64-delta covering-full
    /// cadence in every completed writer stream; the merge-heavy op mix
    /// and fuzz-corpus subtrees are ADR-0045 D3. Segments are 64 KiB
    /// (vs the M2 scenario's 16 KiB) so the worst-case group-commit frame
    /// with ≤ 6 KiB corpus blobs always fits one segment; the checkpoint
    /// interval stays at 24 KiB, so rotation, truncation, and
    /// fuzzy-overlap classes remain exercised.
    #[must_use]
    pub fn m3_document(seed: u64) -> DurableScenario {
        DurableScenario {
            seed,
            workload: DurableWorkload::Document,
            cells: 2,
            always_writers: 3,
            esec_writers: 2,
            mem_writers: 0,
            ops_per_writer: 180,
            keys_per_writer: 1,
            value_max: 1,
            step_ns_max: 2_000_000,
            double_cut: seed % 8 == 3,
            plant: Plant::None,
            segment_bytes: 64 << 10,
            ckpt_interval_bytes: 24 << 10,
            ckpt_stream_bytes_per_sec: None,
            stall: Some(m2_stall_config()),
            replay_canary: false,
            io_mode: SegmentIoMode::Buffered,
        }
    }
}

/// What one seeded run produced. `trace` is the determinism artifact
/// (every apply event incl. the post-recovery audit reads).
#[derive(Debug)]
pub struct DurableReport {
    pub trace: Vec<u8>,
    pub trace_hash: u64,
    pub violations: Vec<String>,
    pub stalled: bool,
    pub commands_done: u64,
    pub sim_seconds: f64,
    /// Ledger ops the oracle *required* to survive (acked `always` +
    /// out-of-window `everysec`).
    pub required_ops: u64,
    /// Acked ops the promise allowed to be lost (in-window everysec) plus
    /// un-acked sends — the disclosure counters.
    pub allowed_lost_ops: u64,
    pub audited_keys: u64,
    pub scheduler_steps: u64,
    /// The reboot refused with the named ADR-0018 taxonomy error (a
    /// validating frame beyond lost un-fsynced bytes — reorder physics):
    /// §8.4 prefers refusing to serve over truncating what *might* be
    /// covered data. Not a durability violation — nothing acked was
    /// destroyed — but counted and disclosed (the availability cost of
    /// frame format v1: a gap in the un-synced suffix is not *provably*
    /// un-covered without per-frame sequencing).
    pub refused_boot: bool,
    /// Largest `always` ack latency observed (M2.5-S14 disclosure, L10):
    /// on a stall run this should approach an episode length — a stall
    /// fleet whose gated acks never felt the device is a dead oracle.
    pub always_ack_latency_ms_max: u64,
    /// Equivalence-oracle disclosure (M3-S23): checks that actually ran
    /// and documents byte-compared — a dead oracle must be visible.
    pub equivalence_checks: u64,
    pub documents_compared: u64,
    /// Fuzz-corpus documents that entered the workload (M3-S24).
    pub corpus_documents_used: u64,
    /// Cut-boundary classes observed on the surviving image (ADR-0045
    /// D4): coverage is disclosed, never assumed.
    pub cut_classes: Vec<&'static str>,
}

impl DurableReport {
    #[must_use]
    pub fn ok(&self) -> bool {
        !self.stalled && self.violations.is_empty()
    }
}

// ---- trace observer (no model replay — the ledger is the oracle) -------

#[derive(Clone, Default)]
pub(crate) struct TraceObserver(Rc<RefCell<Vec<u8>>>);

impl TraceObserver {
    /// The accumulated apply-event trace (the determinism artifact). Used
    /// by every scenario's `finish` (durable, combined).
    pub(crate) fn trace_bytes(&self) -> Vec<u8> {
        self.0.borrow().clone()
    }
}

impl PlaneObserver for TraceObserver {
    fn on_execute(
        &mut self,
        cell: CellId,
        origin: ExecOrigin,
        argv: &[&[u8]],
        reply: &[u8],
        _now: Nanos,
    ) {
        let mut trace = self.0.borrow_mut();
        trace.extend_from_slice(&cell.0.to_le_bytes());
        match origin {
            ExecOrigin::Conn(slot, generation) => {
                trace.push(0);
                trace.extend_from_slice(&slot.to_le_bytes());
                trace.extend_from_slice(&generation.to_le_bytes());
            }
            ExecOrigin::Fabric(from) => {
                trace.push(1);
                trace.extend_from_slice(&from.0.to_le_bytes());
                trace.extend_from_slice(&[0, 0]);
            }
        }
        trace.push(argv.len() as u8);
        for arg in argv {
            trace.extend_from_slice(&(arg.len() as u32).to_le_bytes());
            trace.extend_from_slice(arg);
        }
        trace.extend_from_slice(&(reply.len() as u32).to_le_bytes());
        trace.extend_from_slice(reply);
    }
}

// ---- the ledger -----------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum NsClass {
    Always,
    Everysec,
    Memory,
}

impl NsClass {
    pub(crate) fn name(self) -> &'static [u8] {
        match self {
            NsClass::Always => b"alw",
            NsClass::Everysec => b"esec",
            // The combined scenario's named memory-mode namespace
            // (M2.5-S14); the durable scenario's memory writers stay on
            // the default DB (`setup == false`) and never USE it.
            NsClass::Memory => b"mem",
        }
    }
}

/// One sent op's ledger entry: the key state it leaves behind, when
/// (virtual time) it was sent, and when its ack arrived (`None` = still
/// in flight at the cut).
#[derive(Clone, Debug)]
pub(crate) struct OpRec {
    pub(crate) state_after: Option<Vec<u8>>,
    pub(crate) sent_at: Nanos,
    pub(crate) acked_at: Option<Nanos>,
}

pub(crate) type Ledger = BTreeMap<Vec<u8>, Vec<OpRec>>;

/// What the in-flight command will do to its key when it lands + the
/// exact reply the client must see (sequential ⇒ exact expectations).
#[derive(Clone, Debug)]
pub(crate) struct Pending {
    pub(crate) key: Vec<u8>,
    pub(crate) state_after: Option<Vec<u8>>,
    pub(crate) expect: Vec<u8>,
    /// GETs assert correctness but do not append a ledger op.
    pub(crate) mutates: bool,
    /// A short-TTL PEXPIRE landed on `key` (combined scenario, M2.5-S14):
    /// exact GET/DEL expectations are void until the next SET.
    pub(crate) taints: bool,
}

pub(crate) struct Writer {
    pub(crate) id: usize,
    pub(crate) cell: usize,
    pub(crate) fd: RawFd,
    pub(crate) class: NsClass,
    pub(crate) rng: SplitMix64,
    pub(crate) sent: u64,
    pub(crate) replied: u64,
    pub(crate) quota: u64,
    pub(crate) rx: Vec<u8>,
    pub(crate) inflight: Option<Pending>,
    /// USE handshake outstanding (named-ns writers).
    pub(crate) setup: bool,
    pub(crate) ledger: Ledger,
    /// Combined-scenario bookkeeping (M2.5-S14): TTL-tainted keys and
    /// per-channel PUBLISH sequence counters. Empty and unused in the
    /// pure-durable scenario.
    pub(crate) tainted: std::collections::BTreeSet<Vec<u8>>,
    pub(crate) pub_seq: Vec<u64>,
    /// Document-workload model per key (M3-S23/S24): the exact expected
    /// state the merge-heavy generator maintains. Empty outside the
    /// document workload.
    pub(crate) models: BTreeMap<Vec<u8>, crate::document::DocModel>,
    /// Fuzz-corpus documents this writer embedded (M3-S24 disclosure).
    pub(crate) corpus_docs_used: u64,
}

pub(crate) fn encode(argv: &[&[u8]]) -> Vec<u8> {
    let mut wire = format!("*{}\r\n", argv.len()).into_bytes();
    for arg in argv {
        wire.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        wire.extend_from_slice(arg);
        wire.extend_from_slice(b"\r\n");
    }
    wire
}

pub(crate) fn bulk(value: &[u8]) -> Vec<u8> {
    let mut wire = format!("${}\r\n", value.len()).into_bytes();
    wire.extend_from_slice(value);
    wire.extend_from_slice(b"\r\n");
    wire
}

impl Writer {
    /// One writer with the frozen per-id RNG stream (`0xD11E_0000 + id`
    /// — the m2 trace contract). `channels` sizes the PUBLISH sequence
    /// counters (0 outside the combined scenario).
    #[allow(clippy::too_many_arguments)] // writer identity + stream seed + combined-scenario channels
    pub(crate) fn new(
        id: usize,
        cell: usize,
        fd: RawFd,
        class: NsClass,
        scenario_seed: u64,
        quota: u64,
        setup: bool,
        channels: usize,
    ) -> Writer {
        Writer {
            id,
            cell,
            fd,
            class,
            rng: SplitMix64::new(scenario_seed ^ (0xD11E_0000 + id as u64)),
            sent: 0,
            replied: 0,
            quota,
            rx: Vec::new(),
            inflight: None,
            setup,
            ledger: Ledger::new(),
            tainted: std::collections::BTreeSet::new(),
            pub_seq: vec![0; channels],
            models: BTreeMap::new(),
            corpus_docs_used: 0,
        }
    }

    pub(crate) fn key(&mut self, keys_per_writer: u64) -> Vec<u8> {
        format!("k:{}:{}", self.id, self.rng.next_below(keys_per_writer)).into_bytes()
    }

    pub(crate) fn last_state(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.ledger.get(key).and_then(|ops| ops.last()).and_then(|op| op.state_after.clone())
    }

    /// Builds the next command + its exact expected reply.
    pub(crate) fn next_command(&mut self, scenario: &DurableScenario) -> (Vec<u8>, Pending) {
        if scenario.workload == DurableWorkload::Document {
            return crate::document::next_document_command(self, scenario);
        }
        let key = self.key(scenario.keys_per_writer);
        let roll = self.rng.next_below(100);
        if roll < 70 {
            let value =
                format!("v:{}:{}:{}", self.id, self.sent, self.rng.next_below(scenario.value_max))
                    .into_bytes();
            let wire = if roll < 10 {
                // Far-future TTL: the ExpireAt record rides the log too.
                encode(&[b"SET", &key, &value, b"EX", b"100000"])
            } else {
                encode(&[b"SET", &key, &value])
            };
            let pending = Pending {
                key,
                state_after: Some(value),
                expect: b"+OK\r\n".to_vec(),
                mutates: true,
                taints: false,
            };
            (wire, pending)
        } else if roll < 85 {
            let existed = self.last_state(&key).is_some();
            let expect = if existed { b":1\r\n".to_vec() } else { b":0\r\n".to_vec() };
            let wire = encode(&[b"DEL", &key]);
            (wire, Pending { key, state_after: None, expect, mutates: true, taints: false })
        } else {
            let expect = match self.last_state(&key) {
                Some(value) => bulk(&value),
                None => b"$-1\r\n".to_vec(),
            };
            let state_after = self.last_state(&key);
            let wire = encode(&[b"GET", &key]);
            (wire, Pending { key, state_after, expect, mutates: false, taints: false })
        }
    }
}

// ---- one node boot ---------------------------------------------------------

pub(crate) type SimPlane = ServerPlane<TraceObserver, SimDisk>;
type SimLoop = CellLoop<SimDriver, Rc<VirtualClock>>;

pub(crate) struct Node {
    cells: Vec<(SimLoop, SimPlane)>,
    pub(crate) nets: Vec<Rc<RefCell<CellNet>>>,
    pub(crate) control: std::sync::Arc<inf_server::ControlHandle>,
    inbox: ControlInbox,
    data_dir: PathBuf,
}

pub(crate) fn boot(
    scenario: &DurableScenario,
    data_dir: PathBuf,
    disk: &SimDisk,
    clock: &Rc<VirtualClock>,
    observer: &TraceObserver,
) -> std::io::Result<Node> {
    let catalog = load_catalog_from(disk, &data_dir)?;
    let (control, inbox) = inf_server::ControlHandle::detached_with_catalog(
        catalog.as_ref(),
        scenario.cells,
        // Virtual boot instant (ms): control-plane display only.
        clock.now().as_millis(),
    );
    let fabrics = Mesh::new(scenario.cells, MeshConfig { ring_capacity: 1024, data_credits: 256 });
    let mut nets = Vec::new();
    let mut cells = Vec::new();
    for (i, fabric) in fabrics.into_iter().enumerate() {
        let net = CellNet::new(i as u16, scenario.seed, scenario.plant);
        // The clock rides into the driver for the stall device (M2.5-S14);
        // with no stall model armed this is exactly `with_disk`.
        let driver = SimDriver::with_disk_stall(Rc::clone(&net), disk.clone(), Rc::clone(clock));
        let pool = BufferPool::new(128, 1024);
        let node_info = Rc::new(NodeInfo::default());
        node_info.rng_state.set(scenario.seed ^ (0xA11D_0000 + i as u64));
        let mut ks = Keyspace::new(StoreConfig::default());
        if let Some(catalog) = &catalog {
            ks.seed_catalog(catalog).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e:?}"))
            })?;
        }
        let mut plane = SimPlane::new(
            CellId(i as u16),
            scenario.cells,
            listener_fd(i as u16),
            ks,
            fabric,
            node_info,
            observer.clone(),
            false,
        );
        let cfg = inf_server::DurableConfig {
            data_dir: data_dir.clone(),
            staging: inf_server::StagingConfig::default(),
            segment: inf_server::SegmentConfig {
                segment_bytes: scenario.segment_bytes,
                io_mode: scenario.io_mode,
                ..Default::default()
            },
            ckpt: inf_server::CkptConfig {
                interval_bytes: scenario.ckpt_interval_bytes,
                stream_bytes_per_sec: scenario
                    .ckpt_stream_bytes_per_sec
                    .unwrap_or(inf_server::CkptConfig::default().stream_bytes_per_sec),
                ..Default::default()
            },
            recover: Default::default(),
            sync_pipeline: 1,
            fua_p50_us_probed: 0,
        };
        plane.set_control(std::sync::Arc::clone(&control));
        plane.begin_recovery(disk.clone(), &cfg, i as u16, clock.now());
        let config = LoopConfig { spin_iters: 4, ..Default::default() };
        let cell_loop = CellLoop::new(driver, Rc::clone(clock), pool, config);
        nets.push(net);
        cells.push((cell_loop, plane));
    }
    Ok(Node { cells, nets, control, inbox, data_dir })
}

impl Node {
    /// One scheduler step: seeded cell order, one loop iteration each,
    /// one control-inbox drain, one seeded clock advance.
    pub(crate) fn step(
        &mut self,
        rng: &mut SplitMix64,
        clock: &Rc<VirtualClock>,
        disk: &SimDisk,
        step_ns_max: u64,
    ) -> std::io::Result<()> {
        let n = self.cells.len();
        let rotate = (rng.next_u64() as usize) % n;
        for i in 0..n {
            let idx = (i + rotate) % n;
            let (cell_loop, plane) = &mut self.cells[idx];
            cell_loop.run_iteration(plane).expect("sim iteration");
            if let Some(err) = plane.take_boot_error() {
                return Err(err);
            }
        }
        self.inbox.drain(disk, &self.data_dir)?;
        clock.advance(Nanos(1_000 + rng.next_u64() % step_ns_max));
        Ok(())
    }

    pub(crate) fn ready(&self) -> bool {
        self.control.recovery_board().all_ready()
    }

    /// Read-only plane access for the M3-S23 equivalence oracle — used
    /// only between scheduler steps (the borrow never crosses one).
    pub(crate) fn plane(&self, cell: usize) -> &SimPlane {
        &self.cells[cell].1
    }

    /// Summed pub/sub registry gauges across cells (combined-scenario
    /// quiescence oracle, M2.5-S14): (channels, patterns, bytes).
    pub(crate) fn pubsub_gauges(&self) -> (u64, u64, usize) {
        let mut total = (0u64, 0u64, 0usize);
        for (_, plane) in &self.cells {
            let (channels, patterns, bytes) = plane.pubsub_gauges();
            total.0 += channels;
            total.1 += patterns;
            total.2 += bytes;
        }
        total
    }
}

/// A minimal sequential client for setup/audit: send one command, pump
/// steps until its framed reply arrives (or a stall).
pub(crate) struct MiniClient {
    cell: usize,
    fd: RawFd,
    rx: Vec<u8>,
}

impl MiniClient {
    pub(crate) fn connect(node: &mut Node, cell: usize) -> MiniClient {
        let fd = node.nets[cell].borrow_mut().connect();
        MiniClient { cell, fd, rx: Vec::new() }
    }

    pub(crate) fn call(
        &mut self,
        node: &mut Node,
        rng: &mut SplitMix64,
        clock: &Rc<VirtualClock>,
        disk: &SimDisk,
        step_ns_max: u64,
        argv: &[&[u8]],
    ) -> std::io::Result<Option<Vec<u8>>> {
        node.nets[self.cell].borrow_mut().client_send(self.fd, &encode(argv));
        for _ in 0..STALL_STEPS {
            node.step(rng, clock, disk, step_ns_max)?;
            let bytes = node.nets[self.cell].borrow_mut().client_recv(self.fd);
            self.rx.extend_from_slice(&bytes);
            if let Some(n) = reply_len(&self.rx) {
                let reply: Vec<u8> = self.rx.drain(..n).collect();
                return Ok(Some(reply));
            }
        }
        Ok(None) // stall — the caller records the verdict
    }
}

// ---- the §8.2 admissible-state rule (shared with the combined run) ---------

/// The §8.2 required index: the last op the promise binds for `class`
/// at the cut instant (`None` = nothing required).
pub(crate) fn required_index(class: NsClass, ops: &[OpRec], cut_time: Nanos) -> Option<usize> {
    ops.iter().rposition(|op| match op.acked_at {
        None => false,
        Some(at) => match class {
            NsClass::Always => true,
            NsClass::Everysec => at + EVERYSEC_WINDOW <= cut_time,
            NsClass::Memory => false,
        },
    })
}

/// Admissible post-recovery key states — any state at or after the
/// required op (recovery replays a log prefix: exactly one suffix point
/// materialized). Nothing required ⇒ absent is admissible too.
pub(crate) fn admissible_states(ops: &[OpRec], required: Option<usize>) -> Vec<Option<Vec<u8>>> {
    let from = required.unwrap_or(0);
    let mut admissible: Vec<Option<Vec<u8>>> =
        ops[from..].iter().map(|op| op.state_after.clone()).collect();
    if required.is_none() {
        admissible.push(None);
    }
    admissible
}

/// What one audit pass tallied (shared by the durable and combined runs).
#[derive(Debug, Default)]
pub(crate) struct AuditTally {
    pub(crate) required_ops: u64,
    pub(crate) allowed_lost_ops: u64,
    pub(crate) audited_keys: u64,
    pub(crate) violations: Vec<String>,
}

impl AuditTally {
    fn count(&mut self, ops: &[OpRec], required: Option<usize>) {
        self.audited_keys += 1;
        self.required_ops += required.map_or(0, |i| i as u64 + 1);
        self.allowed_lost_ops += ops.len() as u64 - required.map_or(0, |i| i as u64 + 1);
    }
}

// ---- the run ---------------------------------------------------------------

/// Runs one seeded durable scenario: boot → DDL → seeded traffic → power
/// cut mid-run → reboot (optionally cut again mid-recovery) → recover →
/// audit every ledger key against the §8.2 admissible-state rule.
#[allow(clippy::too_many_lines)] // one linear phase script, like run_scenario
#[must_use]
pub fn run_durable_scenario(scenario: &DurableScenario) -> DurableReport {
    let clock = Rc::new(VirtualClock::new(Nanos(1)));
    let disk = build_disk(scenario.seed, scenario.stall.as_ref());
    let observer = TraceObserver::default();
    let mut rng = SplitMix64::new(scenario.seed ^ 0xD07A_B1E5);
    let mut report = DurableReport {
        trace: Vec::new(),
        trace_hash: 0,
        violations: Vec::new(),
        stalled: false,
        commands_done: 0,
        sim_seconds: 0.0,
        required_ops: 0,
        allowed_lost_ops: 0,
        audited_keys: 0,
        scheduler_steps: 0,
        refused_boot: false,
        always_ack_latency_ms_max: 0,
        equivalence_checks: 0,
        documents_compared: 0,
        corpus_documents_used: 0,
        cut_classes: Vec::new(),
    };
    let fail = |report: &mut DurableReport, what: String| {
        report.violations.push(what);
    };

    // ---- boot 1 + DDL ------------------------------------------------
    let mut node = match boot(scenario, PathBuf::from("node"), &disk, &clock, &observer) {
        Ok(node) => node,
        Err(err) => {
            fail(&mut report, format!("boot 1 failed: {err}"));
            return finish(report, &observer, &clock);
        }
    };
    let mut setup = MiniClient::connect(&mut node, 0);
    for (name, class) in [(b"alw".as_slice(), b"always".as_slice()), (b"esec", b"everysec")] {
        let reply = setup.call(
            &mut node,
            &mut rng,
            &clock,
            &disk,
            scenario.step_ns_max,
            &[b"INF.NS", b"CREATE", name, b"MODE", b"durable", b"FSYNC", class],
        );
        match reply {
            Ok(Some(ok)) if ok == b"+OK\r\n" => {}
            Ok(other) => {
                fail(&mut report, format!("DDL CREATE {name:?} answered {other:?}"));
                return finish(report, &observer, &clock);
            }
            Err(err) => {
                fail(&mut report, format!("DDL phase: {err}"));
                return finish(report, &observer, &clock);
            }
        }
    }

    // ---- writers -----------------------------------------------------
    let mut writers = Vec::new();
    let classes = [
        (NsClass::Always, scenario.always_writers),
        (NsClass::Everysec, scenario.esec_writers),
        (NsClass::Memory, scenario.mem_writers),
    ];
    let mut id = 0usize;
    for (class, count) in classes {
        for _ in 0..count {
            let cell = (rng.next_u64() % u64::from(scenario.cells)) as usize;
            let fd = node.nets[cell].borrow_mut().connect();
            let writer = Writer::new(
                id,
                cell,
                fd,
                class,
                scenario.seed,
                scenario.ops_per_writer,
                class != NsClass::Memory,
                0,
            );
            if writer.setup {
                node.nets[cell]
                    .borrow_mut()
                    .client_send(fd, &encode(&[b"INF.NS", b"USE", class.name()]));
            }
            writers.push(writer);
            id += 1;
        }
    }

    // ---- traffic until the seeded cut point ---------------------------
    // The cut lands somewhere inside (or just past) the traffic window so
    // every pipeline stage — staged, framed, written, fsynced, acked,
    // checkpoint/manifest mid-swap — gets cut across the seed corpus.
    let total_ops: u64 = writers.iter().map(|w| w.quota).sum();
    let cut_step = 200 + rng.next_below(total_ops * 6);
    // M3-S23: two seeded mid-run equivalence instants. Each quiesces
    // (drain in-flight, send nothing new), compares live state against a
    // read-only shadow replay, then resumes. The cut itself is never
    // quiesced — draining before it would erase the unacked-tail cases
    // the durability oracle exists for (ADR-0045 D1).
    let document_workload = scenario.workload == DurableWorkload::Document;
    let mut equivalence = crate::document::EquivalenceStats::default();
    let checks_at = [cut_step / 3, cut_step / 3 * 2];
    let mut next_check = if document_workload { 0 } else { checks_at.len() };
    let mut idle_steps = 0u64;
    let mut last_progress = 0u64;
    for step in 0..cut_step {
        report.scheduler_steps += 1;
        if let Err(err) = node.step(&mut rng, &clock, &disk, scenario.step_ns_max) {
            fail(&mut report, format!("traffic phase: {err}"));
            return finish(report, &observer, &clock);
        }
        let quiesce = next_check < checks_at.len() && step >= checks_at[next_check];
        let mut progress = 0u64;
        for writer in &mut writers {
            let mut net = node.nets[writer.cell].borrow_mut();
            let bytes = net.client_recv(writer.fd);
            progress += bytes.len() as u64;
            writer.rx.extend_from_slice(&bytes);
            while let Some(n) = reply_len(&writer.rx) {
                let reply: Vec<u8> = writer.rx.drain(..n).collect();
                if writer.setup {
                    if reply != b"+OK\r\n" {
                        report
                            .violations
                            .push(format!("writer {}: USE answered {reply:?}", writer.id));
                    }
                    writer.setup = false;
                    continue;
                }
                let Some(pending) = writer.inflight.take() else {
                    report
                        .violations
                        .push(format!("writer {}: unsolicited reply {reply:?}", writer.id));
                    continue;
                };
                if reply != pending.expect {
                    report.violations.push(format!(
                        "writer {} key {:?}: expected {:?}, got {:?}",
                        writer.id,
                        String::from_utf8_lossy(&pending.key),
                        String::from_utf8_lossy(&pending.expect),
                        String::from_utf8_lossy(&reply)
                    ));
                }
                if pending.mutates {
                    let ops = writer.ledger.entry(pending.key.clone()).or_default();
                    let rec = ops.last_mut().expect("sent op has a ledger entry");
                    rec.acked_at = Some(clock.now());
                    // The group-commit-class oracle (M2.5-S14, §1.5): an
                    // everysec ack is deferred, never device-gated —
                    // under a stall a gated ack inherits the episode.
                    let latency = clock.now().saturating_sub(rec.sent_at);
                    match writer.class {
                        NsClass::Everysec if latency > EVERYSEC_ACK_BOUND => {
                            report.violations.push(format!(
                                "EVERYSEC DEFERRAL VIOLATION seed {:#x} writer {} key {:?}: \
                                 ack latency {} ms exceeds {} ms — everysec acked behind the \
                                 device",
                                scenario.seed,
                                writer.id,
                                String::from_utf8_lossy(&pending.key),
                                latency.as_millis(),
                                EVERYSEC_ACK_BOUND.as_millis()
                            ));
                        }
                        NsClass::Always => {
                            report.always_ack_latency_ms_max =
                                report.always_ack_latency_ms_max.max(latency.as_millis());
                        }
                        _ => {}
                    }
                }
                writer.replied += 1;
                report.commands_done += 1;
            }
            if writer.setup || writer.inflight.is_some() || writer.sent >= writer.quota {
                continue;
            }
            if quiesce {
                // Mid-run oracle instant: drain, don't send.
                continue;
            }
            let (wire, pending) = writer.next_command(scenario);
            if pending.mutates {
                writer.ledger.entry(pending.key.clone()).or_default().push(OpRec {
                    state_after: pending.state_after.clone(),
                    sent_at: clock.now(),
                    acked_at: None,
                });
            }
            writer.inflight = Some(pending);
            net.client_send(writer.fd, &wire);
            writer.sent += 1;
            progress += 1;
        }
        if quiesce && writers.iter().all(|w| !w.setup && w.inflight.is_none()) {
            crate::document::equivalence_check(
                scenario,
                &format!("mid-run-{}", next_check + 1),
                &node,
                &disk,
                clock.now(),
                &mut equivalence,
                &mut report.violations,
            );
            next_check += 1;
        }
        if progress == 0 {
            idle_steps += 1;
            // Quiesced early: idle time still ticks (everysec fsyncs,
            // checkpoint cycles) until the seeded cut arrives.
            if idle_steps >= STALL_STEPS && writers.iter().any(|w| w.replied < w.sent) {
                // The watermark-liveness verdict (M2.5-S14): name the
                // writers stuck behind an unadvancing fsync watermark —
                // "stalled forever behind a stuck fsync" is a finding,
                // not a timeout.
                let stuck: Vec<String> = writers
                    .iter()
                    .filter(|w| w.replied < w.sent)
                    .map(|w| format!("writer {} ({:?})", w.id, w.class))
                    .collect();
                report.stalled = true;
                fail(
                    &mut report,
                    format!(
                        "WATERMARK LIVENESS VIOLATION seed {:#x}: traffic stalled before the \
                         cut with unacked in-flight ops ({})",
                        scenario.seed,
                        stuck.join(", ")
                    ),
                );
                return finish(report, &observer, &clock);
            }
        } else {
            idle_steps = 0;
            last_progress = report.commands_done;
        }
    }
    let _ = last_progress;

    // ---- POWER CUT ----------------------------------------------------
    let cut_time = clock.now();
    drop(node); // the process dies: in-flight state vanishes
    disk.power_cut(scenario.seed ^ 0x0FF5_EED0);
    if document_workload {
        // M3-S24 (ADR-0045 D4): disclose which record class the surviving
        // image ends on — cut coverage is measured, never assumed.
        report.cut_classes =
            crate::document::classify_cut(&disk, &PathBuf::from("node"), scenario.cells);
    }

    // ---- reboot (+ optional second cut mid-recovery) -------------------
    let mut boots = 0;
    let node = loop {
        boots += 1;
        let mut node = match boot(scenario, PathBuf::from("node"), &disk, &clock, &observer) {
            Ok(node) => node,
            Err(err) => {
                fail(&mut report, format!("reboot {boots} refused: {err}"));
                return finish(report, &observer, &clock);
            }
        };
        let double = scenario.double_cut && boots == 1;
        let recovery_budget = if double { 1 + rng.next_below(200) } else { u64::MAX };
        let mut steps = 0u64;
        let mut failed = None;
        while !node.ready() && steps < recovery_budget {
            steps += 1;
            report.scheduler_steps += 1;
            if let Err(err) = node.step(&mut rng, &clock, &disk, scenario.step_ns_max) {
                failed = Some(err);
                break;
            }
            if steps > STALL_STEPS {
                report.stalled = true;
                fail(&mut report, format!("recovery stalled on boot {boots}"));
                return finish(report, &observer, &clock);
            }
        }
        if let Some(err) = failed {
            // The ADR-0018 taxonomy refusal is a LEGAL outcome: interior
            // data beyond lost un-fsynced bytes fail-stops the boot
            // (never silent truncation). But §8.2 binds SURVIVAL, not
            // serving: acked data must still exist in the surviving
            // image — audited directly, so an ack-ahead-of-durability
            // bug (the canary) cannot hide behind the refusal.
            if err.to_string().contains("log corruption") {
                if scenario.workload == DurableWorkload::Document {
                    fail(
                        &mut report,
                        format!(
                            "DOCUMENT RECOVERY VIOLATION seed {:#x}: honest power-cut image \
                             refused boot: {err}",
                            scenario.seed
                        ),
                    );
                    return finish(report, &observer, &clock);
                }
                report.refused_boot = true;
                // Refusals are counted in the sweep manifest; the *class*
                // must be visible too (§8.4 never-silent, M2.5-S12: the
                // residual-refusal taxonomy after ADR-0031 is a ledger
                // observable).
                eprintln!("refused boot {boots}: {err}");
                let mut tally = AuditTally::default();
                survival_audit(scenario, &disk, &writers, cut_time, &mut tally);
                report.required_ops += tally.required_ops;
                report.allowed_lost_ops += tally.allowed_lost_ops;
                report.audited_keys += tally.audited_keys;
                report.violations.extend(tally.violations);
            } else {
                fail(&mut report, format!("recovery failed on boot {boots}: {err}"));
            }
            return finish(report, &observer, &clock);
        }
        if node.ready() {
            break node;
        }
        // The second cut: recovery itself was interrupted (idempotence).
        drop(node);
        disk.power_cut(scenario.seed ^ 0x0FF5_EED1 ^ boots);
    };
    let mut node = node;

    // ---- the "at end" equivalence check (M3-S23) -----------------------
    // Runs post-recovery by design: recovered live state must equal an
    // independent replay of the post-cut disk. Quiescing *before* the cut
    // instead would erase the unacked-tail durability cases (ADR-0045 D1).
    if document_workload {
        crate::document::equivalence_check(
            scenario,
            "post-recovery",
            &node,
            &disk,
            clock.now(),
            &mut equivalence,
            &mut report.violations,
        );
    }
    report.equivalence_checks = equivalence.checks;
    report.documents_compared = equivalence.documents_compared;
    report.corpus_documents_used = writers.iter().map(|w| w.corpus_docs_used).sum();

    // ---- audit ----------------------------------------------------------
    let mut audit = MiniClient::connect(&mut node, 0);
    for class in [NsClass::Always, NsClass::Everysec] {
        let reply = audit.call(
            &mut node,
            &mut rng,
            &clock,
            &disk,
            scenario.step_ns_max,
            &[b"INF.NS", b"USE", class.name()],
        );
        if !matches!(reply, Ok(Some(ref ok)) if ok == b"+OK\r\n") {
            fail(&mut report, format!("audit USE {class:?} answered {reply:?}"));
            return finish(report, &observer, &clock);
        }
        for writer in writers.iter().filter(|w| w.class == class) {
            for (key, ops) in &writer.ledger {
                report.audited_keys += 1;
                let required = required_index(class, ops, cut_time);
                report.required_ops += required.map_or(0, |i| i as u64 + 1);
                report.allowed_lost_ops += ops.len() as u64 - required.map_or(0, |i| i as u64 + 1);
                let command: [&[u8]; 2] = match scenario.workload {
                    DurableWorkload::KeyValue => [b"GET", key],
                    DurableWorkload::Document => [b"JSON.GET", key],
                };
                let reply = match audit.call(
                    &mut node,
                    &mut rng,
                    &clock,
                    &disk,
                    scenario.step_ns_max,
                    &command,
                ) {
                    Ok(Some(reply)) => reply,
                    other => {
                        fail(&mut report, format!("audit GET {key:?} answered {other:?}"));
                        return finish(report, &observer, &clock);
                    }
                };
                let admissible: Vec<Vec<u8>> = admissible_states(ops, required)
                    .iter()
                    .map(|state| state.as_ref().map_or(b"$-1\r\n".to_vec(), |v| bulk(v)))
                    .collect();
                if !admissible.contains(&reply) {
                    report.violations.push(format!(
                        "DURABILITY VIOLATION seed {:#x} class {class:?} key {:?}: recovered \
                         {:?} is outside the admissible set (required op index {required:?}, \
                         {} ops, ledger tail: {:?})",
                        scenario.seed,
                        String::from_utf8_lossy(key),
                        String::from_utf8_lossy(&reply),
                        ops.len(),
                        ops.iter()
                            .rev()
                            .take(3)
                            .map(|op| (
                                op.state_after
                                    .as_ref()
                                    .map(|v| String::from_utf8_lossy(v).into_owned()),
                                op.acked_at
                            ))
                            .collect::<Vec<_>>()
                    ));
                }
            }
        }
    }

    finish(report, &observer, &clock)
}

/// The §8.2 survival audit for a legally-refused boot (ADR-0021 D3):
/// reconstructs each cell's recoverable prefix — manifest → named `.ick`
/// → tail replay from begin, stopping at the first invalid frame — and
/// audits every durable ledger key against the admissible-state rule.
/// Sound because fsync-covered bytes always survive the sim disk's cut:
/// on an honest node the corruption point lies strictly above the
/// watermark, so the prefix contains everything the promise binds; a
/// lying fsync scatters required data past the gap and is caught here.
pub(crate) fn survival_audit(
    scenario: &DurableScenario,
    disk: &SimDisk,
    writers: &[Writer],
    cut_time: Nanos,
    tally: &mut AuditTally,
) {
    debug_assert_eq!(scenario.workload, DurableWorkload::KeyValue);
    let data_dir = PathBuf::from("node");
    let catalog = match load_catalog_from(disk, &data_dir) {
        Ok(Some(catalog)) => catalog,
        other => {
            tally.violations.push(format!(
                "SURVIVAL VIOLATION seed {:#x}: acked DDL lost — catalog unreadable after the \
                 cut ({other:?})",
                scenario.seed
            ));
            return;
        }
    };
    let mut ks = Keyspace::new(StoreConfig::default());
    if let Err(err) = ks.seed_catalog(&catalog) {
        tally.violations.push(format!("survival audit: seed_catalog failed: {err:?}"));
        return;
    }
    let now = cut_time;
    let anchor = WallAnchor { internal_ms: 0, unix_ms: 0 };

    for cell in 0..scenario.cells {
        let shard = data_dir.join(format!("shard-{cell}"));
        let log_dir = shard.join("log");
        let manifest = match read_manifest(disk, &shard) {
            Ok(manifest) => manifest,
            Err(err) => {
                tally.violations.push(format!(
                    "SURVIVAL VIOLATION seed {:#x} cell {cell}: MANIFEST unreadable after the \
                     cut: {err}",
                    scenario.seed
                ));
                continue;
            }
        };
        if let Some(manifest) = &manifest {
            let ick = shard.join("ckpt").join(ick_file_name(manifest.ckpt_id));
            let loaded = read_ick(disk, &ick, IckReaderConfig::default(), |record| {
                ks.apply_record(&record, now, anchor).map(|_| ()).map_err(|e| format!("{e:?}"))
            });
            if let Err(err) = loaded {
                tally.violations.push(format!(
                    "SURVIVAL VIOLATION seed {:#x} cell {cell}: the manifest-named checkpoint \
                     is unreadable (fsync-covered loss): {err:?}",
                    scenario.seed
                ));
                continue;
            }
        }
        let begin = manifest.as_ref().map(|m| m.begin_lsn);
        let floor = manifest.as_ref().map_or(SegmentId(0), inf_log::Manifest::floor);
        let scan = match scan_log_dir_from(disk, &log_dir, floor) {
            Ok(outcome) => outcome.scan,
            Err(err) => {
                tally
                    .violations
                    .push(format!("survival audit: cell {cell} log scan failed: {err:?}"));
                continue;
            }
        };
        // Strict prefix: stop at the first invalid frame anywhere — on an
        // honest node everything required is below it (watermark ≤ gap).
        'segments: for &segment in scan.segments() {
            let Ok(mut reader) =
                SegmentReader::open(disk, &log_dir, segment, ReaderConfig::default())
            else {
                break 'segments;
            };
            loop {
                match reader.next_frame() {
                    Ok(Some(frame)) => {
                        for record in frame.records() {
                            let Ok((lsn, record)) = record else { break 'segments };
                            if begin.is_some_and(|b| lsn < b) {
                                continue;
                            }
                            let _ = ks.apply_record(&record, now, anchor);
                        }
                    }
                    Ok(None) => break,
                    Err(_) => break 'segments,
                }
            }
        }
    }

    // Audit the durable ledgers directly against the reconstructed state.
    let ns_of = |name: &[u8]| -> Option<NsId> {
        catalog.entries.iter().find(|spec| spec.name == name).map(|spec| spec.id)
    };
    for class in [NsClass::Always, NsClass::Everysec] {
        let Some(ns) = ns_of(class.name()) else {
            tally.violations.push(format!(
                "SURVIVAL VIOLATION seed {:#x}: acked CREATE for {class:?} lost from the catalog",
                scenario.seed
            ));
            continue;
        };
        let Some(store) = ks.ns_store_mut(ns) else {
            tally.violations.push(format!("survival audit: ns {ns:?} has no store"));
            continue;
        };
        for writer in writers.iter().filter(|w| w.class == class) {
            for (key, ops) in &writer.ledger {
                let required = required_index(class, ops, cut_time);
                tally.count(ops, required);
                let got = store.get(key, now).map(<[u8]>::to_vec);
                let admissible = admissible_states(ops, required);
                if !admissible.contains(&got) {
                    tally.violations.push(format!(
                        "SURVIVAL VIOLATION seed {:#x} class {class:?} key {:?}: surviving \
                         image holds {:?}, outside the admissible set (required op index \
                         {required:?}, {} ops)",
                        scenario.seed,
                        String::from_utf8_lossy(key),
                        got.as_ref().map(|v| String::from_utf8_lossy(v).into_owned()),
                        ops.len()
                    ));
                }
            }
        }
    }
}

fn finish(
    mut report: DurableReport,
    observer: &TraceObserver,
    clock: &Rc<VirtualClock>,
) -> DurableReport {
    report.trace = observer.0.borrow().clone();
    report.trace_hash = hash64(&report.trace, 0xD07A);
    report.sim_seconds = clock.now().0.saturating_sub(1) as f64 / 1e9;
    report
}
