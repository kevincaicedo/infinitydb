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
use std::path::{Path, PathBuf};
use std::rc::Rc;

use inf_alloc::BufferPool;
use inf_doc::path::compile;
use inf_fabric::{Mesh, MeshConfig};
use inf_foundation::fault::FaultSpec;
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::{Clock, Nanos, VirtualClock};
use inf_foundation::{CellId, hash64};
use inf_log::ckpt::{IckReaderConfig, ick_file_name, read_ick};
use inf_log::{
    REORDER_WINDOW_FRAMES, ReaderConfig, SegmentId, SegmentReader, WRITE_THROUGH_WINDOW_ENTRIES,
    read_manifest, scan_log_dir_from,
};
use inf_runtime::{CellLoop, LoopConfig};
use inf_server::{
    ControlInbox, ExecOrigin, ExecScope, NodeInfo, PlaneObserver, SegmentIoMode, ServerPlane,
    SimDisk, SimDiskConfig, StallConfig, load_catalog_from,
};
use inf_store::{
    IndexId, IndexKeyType, IndexSpec, IndexState, Keyspace, NsId, StoreConfig, WallAnchor,
};

use crate::harness::node_hasher;
use crate::lift::LiftNs;

use crate::net::{CellNet, Plant, SimDriver, listener_fd};
use crate::resp::reply_len;

mod audit;
mod run;
pub use run::run_durable_scenario;

pub(crate) use audit::survival_audit;
use audit::{
    audit_ledgers, budget_oracles, engagement_checks, finish, lift_plant_oracle, lift_regime_ddl,
    lift_regime_index_oracle, lift_regime_seed_checkpoint, lift_regime_seed_life,
};

// ---- shared data definitions (behaviour lives in the child modules) ----------

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
    /// Batch 42: an everysec writer's think time after each send, drawn
    /// per op below this bound (0 = closed loop). A quiet cell is the
    /// tick-contract oracle's precondition: the finding needs a frame
    /// staged into a byte-clean ledger inside the hold before a tick.
    pub esec_think_ns_max: u64,
    /// Batch 43: the same think time for `always` writers (0 = closed
    /// loop). Bursts with quiet gaps between them are the group hold's
    /// standalone regime: a round of ≥ 2 acked, then a tick over plain
    /// frames with nothing staged.
    pub always_think_ns_max: u64,
    pub esec_writers: usize,
    pub mem_writers: usize,
    /// Ops per writer.
    pub ops_per_writer: u64,
    pub keys_per_writer: u64,
    pub value_max: u64,
    /// Filler bytes appended to every `always` writer's `SET` value,
    /// `0..value_pad_max` (0 = none): the only way a KeyValue frame spans
    /// several blocks — `value_max` bounds a number inside a ~10-byte
    /// string. ADR-0090 A15's clean-stop class needs multi-block residue
    /// frames; `everysec` values stay small so the deferral oracle
    /// measures the ack, not the pad.
    pub value_pad_max: u64,
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
    /// Checkpoint section target override (M4.5-S36): the budget's
    /// per-class cap can never be below one slice, so a scenario whose
    /// checkpoints are smaller than the 256 KiB default section never
    /// engages the budget — `m2_device_budget` sets 8 KiB sections.
    pub ckpt_section_bytes: Option<u32>,
    /// The writer's section bound (ADR-0117 D1; `None` = the format's
    /// 64 MiB + slack). A small bound makes every image boundary a
    /// split point: the walk seals before the record and resumes at it
    /// through the in-chain cursor (D2) on every slice — the generator
    /// widening for review F-L03-02. `m2_durable`/`m4_tiered` arm it on
    /// one seed in four.
    pub ckpt_section_bound: Option<u32>,
    /// Device service-time model (M2.5-S14). `None` = instant fsyncs
    /// (the pre-S14 device); `m2_durable` arms the reference stall
    /// device so the fleet sees nonzero fsync latency every night.
    pub stall: Option<StallConfig>,
    /// M3-S23 canary (ADR-0045 D2): the equivalence oracle's shadow
    /// replay skips the first `DocDelta` it sees — the planted bug the
    /// fleet must catch within 100 seeds.
    pub replay_canary: bool,
    /// ADR-0124 (batch 51): after the traffic window every cell is asked
    /// to stop and stepped until `Drained` before the power cut; the
    /// §8.2 rule then requires every acked `everysec` op, every client
    /// must have seen its close, and the reboot must replay nothing.
    pub clean_stop: bool,
    /// Log-segment I/O mode (M4.5-S34, ADR-0086 D8): `Direct` runs the
    /// zero-fill state machine, v3 frames, write-through barriers, and
    /// the class-upgrade/not-ready rotations on the sim disk; `Buffered`
    /// is the pre-S34 scenario byte-for-byte. `m2_durable` alternates by
    /// seed so every sweep covers both classes.
    pub io_mode: SegmentIoMode,
    /// Frames in flight per cell (M4.5-S35, ADR-0087 D7): the staging
    /// ring's pipeline depth. `m2_durable` varies it by seed so every
    /// sweep covers K = 1 (the pre-S35 scenario byte-for-byte) and K > 1
    /// under both barrier classes — out-of-order frame completions, the
    /// hold-for-drain waits, and cuts between a later durable frame and
    /// an earlier torn one.
    pub frames_in_flight: u8,
    /// M4.5-S36 (ADR-0088 D8): the cell's device-budget inputs (`Default`
    /// = model absent = unbudgeted, the pre-S36 behaviour) and whether
    /// the budget oracles run at the cut (the `m2-device-budget`
    /// scenario arms both; every other scenario stays byte-identical).
    pub device: inf_server::DeviceConfig,
    pub budget_oracle: bool,
    /// The reorder-window oracle (ADR-0087 D2 as amended): the scenario
    /// wedges plain writes, so the cut must find the window engaged
    /// (`frame_waits_reorder ≥ 1` on some cell) — a sweep whose window
    /// never filled proves nothing about the bound — while the ledger's
    /// release assert and the m2 durability oracle hold as they are.
    pub reorder_oracle: bool,
    /// The checkpoint direct-write refusal (ADR-0088 D3 as amended):
    /// `Some(n)` lets the disk take `n` direct writes, then refuse every
    /// later one with `EINVAL`. `Some(1)` passes the boot probe's block
    /// and refuses the first checkpoint write — the in-band downgrade —
    /// and arms the oracle: every cell downgrades exactly once and still
    /// publishes a checkpoint before the cut. `None` = direct writes
    /// always succeed, every existing trace byte-identical.
    pub ckpt_direct_refused_after: Option<u64>,
    /// M4.5-S39a: the frame-fill policy (`Default` = off, every existing
    /// trace byte-identical). `m2_durable` arms it on a quarter of its
    /// seeds; `m2_reorder_window` arms it everywhere and asserts it
    /// engaged on the aligned class (every frame there is barrier-less
    /// and far below the target).
    pub fill: inf_server::FillConfig,
    /// M4.5-S43 (ADR-0092 D3): the FLUSH-class group hold, armed by
    /// seed on `Buffered` K = 1 seeds (the class it targets); the
    /// durability, quiescence and budget oracles run unchanged.
    pub group: inf_server::GroupHoldConfig,
    /// A first life in another barrier class (ADR-0086 D4 as amended,
    /// 2026-08-21): the cell boots in `prelude.io_mode`, the writers run
    /// `prelude.ops_per_writer` ops, the log quiesces to full durability,
    /// the node restarts cleanly into `io_mode`, and the scenario proper
    /// runs from there — the FLUSH ↔ FUA transition on an existing log
    /// (a packed tail reopened under a `Direct` rotor, a v3 tail reopened
    /// `Buffered`) under the same durability oracle. `None` = one life,
    /// byte-identical to the pre-amendment scenarios.
    pub prelude: Option<Prelude>,
    /// M4.5-S39b (ADR-0090 D5): the recycle-pool bound (`0` = off, the
    /// pre-S39b truncation path byte-for-byte; the product default is 1).
    /// `m2_durable` runs the default on three of every four `Direct`
    /// seeds and off on the fourth (the baseline arm stays covered);
    /// `m2_recycle` runs it everywhere with the recycle oracle armed.
    pub recycle_slots: u8,
    /// The pool wait (ADR-0090 D9): `m2_recycle` varies it by seed so the
    /// sweep covers the immediate path, expiry and satisfaction alike.
    pub prealloc: inf_server::PreallocPolicy,
    /// The recycle oracle (ADR-0090 D5): every cell that rotated ≥ 3
    /// times and truncated ≥ 2 segments must have recycled ≥ 1, and the
    /// zero-fill accounting identity must hold — `zero_fill_bytes ≤
    /// (preallocs − recycled) × segment_bytes` (every fill is a prealloc
    /// the pool did not serve). The m2 durability and log-quiescence
    /// oracles hold unchanged.
    pub recycle_oracle: bool,
    /// Review 2026-08-30, F-L14-01 — the lift regime (batch 20): the
    /// main life also drives a **tiered** namespace (displacement pairs,
    /// blob extents) and an **indexed** document namespace, so the
    /// segments a later boot lifts past a discarded life's residue carry
    /// those records — the class batch 19 fixed (the end-of-replay checks
    /// ran before the lifted segments applied). Oracles: the §8.2 audit on
    /// both classes, the index digest-walk equality after the final boot,
    /// and the lift's own coverage (`stale_residue_slacks`).
    ///
    /// Batch 21 — the residue plant (`crate::lift`): the index is live
    /// from a clean restart after the DDL, seed documents and a forced
    /// checkpoint give every cell a sidecar, and after the prelude cut
    /// the lift shape is written into every cell's log, so the
    /// transition boot lifts on every arm seed and the index oracle runs
    /// against a loaded sidecar right there — the falsifier the natural
    /// sweep never produced (3 lifts in 400 seeds, none on this class).
    pub lift_regime: bool,
    /// Review 2026-08-30 (F-L02-01, ADR-0090 A14): arm `recycle_open_fail`
    /// once — the first pooled file this life reuses fails to open. The
    /// generation must fall back fresh (counted) and the run must go on;
    /// the pre-fix rotor turned this into an `AlreadyExists` fail-stop.
    /// `m2_recycle` sets it on one seed class in eight.
    pub recycle_open_fault: bool,
}

/// The transition prelude (see [`DurableScenario::prelude`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Prelude {
    pub io_mode: SegmentIoMode,
    pub ops_per_writer: u64,
}

/// Per-step log oracles (batch 42). One sample per cell per traffic
/// step: `(idle_ticks, records_staged, frames_queued)`.
///
/// **Tick contract (F-L01-01):** records leave the staging builder only
/// through a seal, and a seal bumps `frames_queued`; so records staged at
/// two consecutive samples with `frames_queued` unchanged sat staged for
/// the whole step — including at any tick that fired in it. An
/// `idle_ticks` increment across such a step is a tick that dropped its
/// due over acked records. No false positives: a tick before the step's
/// EXECUTE sees the previous sample's records, which only a seal (a
/// `frames_queued` change) could have removed.
///
/// **Ack-map bound (F-L01-03):** the map's depth and the ledger's entry
/// count are tracked as maxima; the post-cut check compares them.
///
/// **Hold episode (F-L01-04, batch 43):** a fill or group-hold episode
/// clock is set by a hold and cleared by the issue (ADR-0092 D1 rule 6:
/// "cleared at the seal" — the standalone fdatasync is an issue too), so
/// an open clock at a sample is a held frame or standalone
/// (`hold_open`). An open clock with nothing held is a stale episode:
/// its next first sight reads as elapsed and seals at once.
struct LogOracle {
    prev: Vec<Option<(u64, u64, u64)>>,
}

impl LogOracle {
    fn new(cells: u16) -> LogOracle {
        LogOracle { prev: vec![None; usize::from(cells)] }
    }

    fn sample(&mut self, node: &Node, report: &mut DurableReport) {
        for (cell, prev) in self.prev.iter_mut().enumerate() {
            let Some(stats) = node.plane(cell).durable_stats() else { continue };
            let now = (stats.everysec_idle_ticks, stats.records_staged, stats.frames_queued);
            if let Some((idle, staged, queued)) = *prev
                && now.0 > idle
                && staged > 0
                && now.1 > 0
                && now.2 == queued
            {
                report.idle_tick_violations += now.0 - idle;
            }
            report.frames_awaiting_max =
                report.frames_awaiting_max.max(stats.frames_awaiting_watermark);
            report.fsync_entries_max = report.fsync_entries_max.max(stats.fsync_entries);
            report.write_through_entries_max =
                report.write_through_entries_max.max(stats.write_through_entries);
            if (stats.fill_hold_open > 0 || stats.group_hold_open > 0) && stats.hold_open == 0 {
                report.hold_episode_violations += 1;
            }
            *prev = Some(now);
        }
    }
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
        // Plain writes pay a page-cache copy's worth (~8 µs base, same
        // tail), drawn per write off the flush timeline (ADR-0087 D7):
        // with K frames in flight they land in any order, and a cut
        // between a later-landed and an earlier-pending frame is the
        // shape the completion-ordered written prefix must survive.
        write_base_ns: 8_000,
        // No bandwidth term in the m2 shape (ADR-0088 D8): every m2 trace
        // stays byte-identical; the `m2-device-budget` scenario arms it.
        write_bytes_per_s: 0,
        read_bytes_per_s: 0,
        wedge_every_writes: 0,
        wedge_ns: 0,
    }
}

/// Builds the scenario's disk: stall-modeled when armed, instant
/// otherwise. The stall seed derives from the scenario seed (L7).
pub fn build_disk(seed: u64, stall: Option<&StallConfig>) -> SimDisk {
    match stall {
        Some(cfg) => SimDisk::with_stall(SimDiskConfig::default(), cfg.clone(), seed ^ 0x57A1_1ED0),
        None => SimDisk::new(),
    }
}

impl DurableScenario {
    #[must_use]
    /// ADR-0117 D4's generator widening: one seed in four walks every
    /// checkpoint under a tiny section bound, so the in-chain resume
    /// runs at nearly every image under the scenario's churn and cuts.
    /// `bound` is the shape's: 1 KiB on the tiered shape (values up to
    /// several KiB), 64 B on the m2 shapes — batch 34 found the 1 KiB
    /// bound never split a 48-byte-value checkpoint (~400 B a file), so
    /// the m2 arm had run vacuous since batch 33.
    pub fn section_bound_for(seed: u64, bound: u32) -> Option<u32> {
        (seed % 4 == 1).then_some(bound)
    }

    /// The m2 shapes' section bound (see [`Self::section_bound_for`]).
    pub const M2_SECTION_BOUND: u32 = 64;

    /// `m2-clean-stop` (ADR-0124): `m2-durable`'s shape, stopped
    /// gracefully before the cut. Single-cut: the second cut lands
    /// mid-recovery, which a clean stop never reaches.
    pub fn m2_clean_stop(seed: u64) -> DurableScenario {
        let mut scenario = Self::m2_durable(seed);
        scenario.clean_stop = true;
        scenario.double_cut = false;
        scenario
    }

    pub fn m2_durable(seed: u64) -> DurableScenario {
        DurableScenario {
            seed,
            workload: DurableWorkload::KeyValue,
            cells: 2,
            always_writers: 3,
            esec_think_ns_max: 0,
            always_think_ns_max: 0,
            esec_writers: 3,
            mem_writers: 2,
            ops_per_writer: 140,
            keys_per_writer: 6,
            value_max: 48,
            value_pad_max: 0,
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
            ckpt_section_bytes: None,
            ckpt_section_bound: Self::section_bound_for(seed, Self::M2_SECTION_BOUND),
            stall: Some(m2_stall_config()),
            replay_canary: false,
            clean_stop: false,
            // Odd seeds run the FUA class (ADR-0086 D8): half of every
            // sweep exercises mixed write-through/FLUSH frames, zero-fill
            // barriers, and seal × write-through crossings under cuts.
            io_mode: if seed % 2 == 1 { SegmentIoMode::Direct } else { SegmentIoMode::Buffered },
            // K = 1..=4 on Direct seeds (the class that pipelines
            // barriers), 1 or 3 on Buffered seeds (barrier-less everysec
            // frames pipeline; a due frame drains first) — ADR-0087 D7.
            frames_in_flight: if seed % 2 == 1 {
                1 + ((seed / 2) % 4) as u8
            } else if (seed / 2) % 2 == 1 {
                3
            } else {
                1
            },
            device: Default::default(),
            budget_oracle: false,
            reorder_oracle: false,
            ckpt_direct_refused_after: None,
            // M4.5-S39a: seeds ≡ 3 (mod 4) — odd, so the `Direct` class
            // whose aligned frames the policy holds (on packed segments
            // it never engages) — run the fill policy at its design
            // point (1 ms window, 16 KiB target); the m2 durability and
            // log-quiescence oracles hold unchanged.
            fill: if seed % 4 == 3 { m2_fill_config() } else { Default::default() },
            // M4.5-S43 (ADR-0092 D3): seeds ≡ 0 (mod 4) — `Buffered`, K = 1 —
            // run the FLUSH-class group hold at the measured arm's window.
            group: if seed.is_multiple_of(4) {
                inf_server::GroupHoldConfig::ARM
            } else {
                Default::default()
            },
            prelude: None,
            // M4.5-S39b (ADR-0090 A7.5): the product default on `Direct`
            // seeds, off on every fourth of them (seeds ≡ 5 mod 8) so the
            // unlink path stays in every sweep; `Buffered` rotors never
            // pool regardless. The product pool wait rides the default.
            recycle_slots: if seed % 8 == 5 { 0 } else { inf_server::DEFAULT_RECYCLE_SLOTS },
            prealloc: inf_server::PreallocPolicy::DEFAULT,
            recycle_oracle: false,
            recycle_open_fault: false,
            lift_regime: false,
        }
    }

    /// `m2-recycle` (M4.5-S39b, ADR-0090 D5): the m2 durable shape under
    /// the FUA class with an `always` namespace on every seed (zero-fill
    /// and so recycling engaged), small segments and a checkpoint
    /// interval at the segment size so a run performs many rotations,
    /// checkpoints and truncations — and the recycle oracle armed: a cell
    /// that rotated and truncated must have recycled, and the zero-fill
    /// accounting identity must hold. The cut lands anywhere in the
    /// rename → dir-barrier → first-frame protocol (the sim disk keeps a
    /// seeded prefix of the directory's metadata ops, so a lost rename
    /// resurrects the old name with the new life's frames inside it); the
    /// durability oracle refuses any acked `always` record that does not
    /// replay. K and the fill policy vary by seed as in `m2_durable`.
    #[must_use]
    pub fn m2_recycle(seed: u64) -> DurableScenario {
        let mut scenario = DurableScenario::m2_durable(seed);
        scenario.io_mode = SegmentIoMode::Direct;
        scenario.frames_in_flight = 1 + ((seed / 2) % 4) as u8;
        scenario.always_writers = 4;
        scenario.esec_writers = 2;
        scenario.ops_per_writer = 200;
        scenario.segment_bytes = 16 << 10;
        scenario.ckpt_interval_bytes = 16 << 10;
        scenario.fill = if seed % 2 == 1 { m2_fill_config() } else { Default::default() };
        scenario.recycle_slots = if seed % 16 == 7 { 2 } else { 1 };
        // ADR-0090 D9: the pool wait by seed — a third each of immediate,
        // quarter and eighth. A 16 KiB segment's quarter is the rotating
        // frame itself (never eligible: the slice runs after it, and a
        // LOG step lands up to K frames before the next slice), so every
        // other waiting seed runs 128 KiB segments (bound eight / four
        // aligned frames) at twice the ops, with a checkpoint every half
        // segment, so the feeding truncation lands inside the bound on
        // some generations and after it on others: both wait outcomes
        // occur in every sweep (the manifest counts them) and the cut
        // meets both.
        scenario.prealloc = match seed % 3 {
            0 => inf_server::PreallocPolicy::Immediate,
            1 => inf_server::PreallocPolicy::WaitForPool {
                bound: inf_server::PoolWaitBound::Quarter,
            },
            _ => {
                inf_server::PreallocPolicy::WaitForPool { bound: inf_server::PoolWaitBound::Eighth }
            }
        };
        if !seed.is_multiple_of(3) && (seed / 3) % 2 == 1 {
            scenario.segment_bytes = 128 << 10;
            scenario.ckpt_interval_bytes = 64 << 10;
            scenario.ops_per_writer = 400;
        }
        scenario.recycle_oracle = true;
        scenario.recycle_open_fault = seed % 8 == 3;
        // ADR-0090 A15 (F-L02-02): one seed class in four stops cleanly
        // with values large enough for multi-block `everysec` frames, so
        // a life's data end can land inside the last residue frame's
        // body — the placement no surviving foreign header proves. The
        // clean-stop boot then asserts no torn tail (the oracle above).
        if seed % 4 == 1 {
            scenario.clean_stop = true;
            scenario.double_cut = false;
            scenario.cells = 4;
            scenario.segment_bytes = 64 << 10;
            scenario.ckpt_interval_bytes = 32 << 10;
            scenario.ops_per_writer = 300;
            // Frames of one to four blocks (four `always` writers batch
            // at most ~48 KiB, under the 64 KiB segment): the last
            // residue frame's body spans several aligned offsets, so a
            // stop's data end lands inside one on a fair share of boots.
            scenario.value_pad_max = 12_000;
        }
        scenario
    }

    /// `m2-ckpt-refused` (ADR-0088 D3 as amended, 2026-08-22): the m2
    /// durable shape on a disk that takes the `O_DIRECT` open and the
    /// boot probe's aligned block, then refuses the first checkpoint
    /// write with `EINVAL` — the per-write refusal the boot probe cannot
    /// see. The oracle: every cell downgrades its staging mode exactly
    /// once, retries at once, and **publishes a checkpoint** before the
    /// cut (the 24 KiB interval fires several times per run) — never the
    /// abort-and-back-off loop the review named. The m2 durability oracle
    /// runs unchanged across the downgrade. Both segment classes by seed
    /// parity; the checkpoint probe is class-independent.
    #[must_use]
    pub fn m2_ckpt_refused(seed: u64) -> DurableScenario {
        let mut scenario = DurableScenario::m2_durable(seed);
        scenario.ckpt_direct_refused_after = Some(1);
        // A packed (v2) life writes ~20 KiB of frames in the m2 span —
        // under the 24 KiB trigger, so `Buffered` seeds would never start
        // a checkpoint and prove nothing. 4 KiB fires on both classes.
        scenario.ckpt_interval_bytes = 4 << 10;
        scenario
    }

    /// `m2-reorder-window` (ADR-0087 D2 as amended, 2026-08-22): the m2
    /// durable shape with every 40th plain write **wedged 150 ms** on a
    /// K ≥ 2 pipeline — one late frame at the front while the frames
    /// behind it land in 8 µs, so the completion ledger's reorder window
    /// fills within ~16 iterations and the next frame holds for the rest
    /// of the wedge. Before the bound the queue grew by one frame per
    /// iteration for the whole wedge (the review of `2cb6074`). The
    /// oracle asserts the window engaged; the ledger release-asserts the
    /// bound; the m2 durability oracle is unchanged. **`everysec`-only
    /// writers**: the window is reachable only across barrier-less
    /// frames — a due `always` record behind a late write already holds
    /// the frame at the barrier `Wait` (D3), which `m2_durable` covers —
    /// so the everysec tick is the one sync this shape sees. K = 2..=4
    /// by seed (a K = 1 ring cannot reorder: the wedged write holds the
    /// only slot), both segment classes by seed parity as in `m2_durable`,
    /// on 256 KiB segments (below).
    #[must_use]
    pub fn m2_reorder_window(seed: u64) -> DurableScenario {
        let mut scenario = DurableScenario::m2_durable(seed);
        let mut stall = m2_stall_config();
        stall.wedge_every_writes = 40;
        stall.wedge_ns = 150_000_000;
        scenario.stall = Some(stall);
        scenario.always_writers = 0;
        scenario.esec_writers = 6;
        scenario.frames_in_flight = 2 + ((seed / 2) % 3) as u8;
        // Rotation is a drain point (D4): on the 16 KiB m2 segment an
        // aligned `Direct` frame pipeline rotates every four frames and
        // the rotation wait lands before the window can fill. 256 KiB
        // segments hold 64 aligned frames — the wedge shape is the
        // window's, on both classes.
        scenario.segment_bytes = 256 << 10;
        scenario.reorder_oracle = true;
        scenario.fill = m2_fill_config();
        scenario
    }

    /// `m2-fua-pending` (batch 44, ADR-0087 D2 third amendment): a
    /// **single-cell, pure-`always`, `Direct`** shape with many gated
    /// clients — the regime where every frame mints a write-through
    /// ticket and the ledger's `pending` FIFO is bounded only by the
    /// clients' outstanding commands. The front wedge is the rotation's
    /// seal (a FLUSH-class fdatasync on a device whose flush base is
    /// 10 ms while FUA writes complete in 40 µs), so on 64 KiB segments
    /// every rotation opens a window in which dozens of FUA frames land
    /// and fold nowhere. 48 writers, one command in flight each: the
    /// pre-fix peak is ~the writer count; the fixed ledger holds at
    /// `WRITE_THROUGH_WINDOW_ENTRIES`. No stall episodes (the wedge is
    /// the seal itself); the m2 durability oracle unchanged.
    #[must_use]
    pub fn m2_fua_pending(seed: u64) -> DurableScenario {
        let mut scenario = DurableScenario::m2_durable(seed);
        scenario.cells = 1;
        scenario.always_writers = 48;
        scenario.esec_writers = 0;
        scenario.mem_writers = 0;
        scenario.ops_per_writer = 60;
        scenario.io_mode = SegmentIoMode::Direct;
        scenario.frames_in_flight = 1 + ((seed / 2) % 4) as u8;
        scenario.segment_bytes = 64 << 10;
        // Fine steps: a 3 ms seal spans many LOG iterations.
        scenario.step_ns_max = 200_000;
        scenario.double_cut = false;
        scenario.ckpt_section_bound = None;
        scenario.fill = Default::default();
        scenario.group = Default::default();
        scenario.recycle_slots = inf_server::DEFAULT_RECYCLE_SLOTS;
        let mut stall = m2_stall_config();
        stall.base_ns = 10_000_000;
        stall.tail_permille = 0;
        stall.episode_gap_ns = u64::MAX / 4;
        scenario.stall = Some(stall);
        scenario
    }

    /// `m2-fill-tick` (batch 42, F-L01-01): a **quiet** everysec cell on
    /// the `Direct` class with the fill policy at an operator-reachable
    /// but wide point — a 50 ms window (the binary caps at 100 ms) and a
    /// 64 KiB target, so a held frame outlives a scheduler step. One
    /// writer with seconds of think time: a burst lands on a byte-clean
    /// ledger (the previous frame covered by an earlier tick), and when
    /// the next tick fires inside the hold the pre-fix ledger counts it
    /// idle — the finding's exact precondition, which continuous traffic
    /// never meets (a plain frame is always queued at the tick). The
    /// per-step tick-contract oracle discriminates; the fixed ledger
    /// seals the held frame with its barrier. No write wedge; the m2
    /// stall device and the loss-window oracle unchanged.
    #[must_use]
    pub fn m2_fill_tick(seed: u64) -> DurableScenario {
        let mut scenario = DurableScenario::m2_durable(seed);
        scenario.always_writers = 0;
        scenario.esec_writers = 1;
        // One writer thinking up to 3 s between ops: most ops land on a
        // byte-clean ledger, ~5 % of them inside the 50 ms before a tick.
        // The quota is never reached — the cut (a step count scaled by
        // the quota) lands after ~1–6 sim-minutes at the m2 step size,
        // ~40–240 ops.
        scenario.esec_think_ns_max = 3_000_000_000;
        scenario.ops_per_writer = 60_000;
        // A quiet cell checkpoints tiny images: the section-bound arm's
        // engagement witness is not this scenario's subject.
        scenario.ckpt_section_bound = None;
        scenario.io_mode = SegmentIoMode::Direct;
        scenario.frames_in_flight = 1 + ((seed / 2) % 4) as u8;
        scenario.segment_bytes = 256 << 10;
        scenario.fill =
            inf_server::FillConfig { window: Nanos::from_millis(50), target_bytes: 64 << 10 };
        scenario
    }

    /// `m2-group-hold` (batch 43, F-L01-04): the FLUSH class (`Buffered`,
    /// packed segments, K = 1) with the group hold armed on every seed
    /// and *bursty* writers — two `always` and one `everysec`, each
    /// thinking up to 20 ms between ops. A burst acks a round of ≥ 2, so
    /// the next barrier's target is ≥ `MIN_GROUP`; a tick then fires over
    /// plain everysec frames with nothing staged — the standalone hold —
    /// and elapses in the quiet gap. The hold-episode oracle samples
    /// every step; the m2 durability oracle holds unchanged. Continuous
    /// traffic never meets the precondition (a frame is always staged at
    /// the tick — the frame path, never the standalone).
    #[must_use]
    pub fn m2_group_hold(seed: u64) -> DurableScenario {
        let mut scenario = DurableScenario::m2_durable(seed);
        scenario.always_writers = 2;
        scenario.esec_writers = 1;
        scenario.always_think_ns_max = 20_000_000;
        scenario.esec_think_ns_max = 20_000_000;
        scenario.ops_per_writer = 3_000;
        scenario.ckpt_section_bound = None;
        scenario.io_mode = SegmentIoMode::Buffered;
        scenario.frames_in_flight = 1;
        scenario.fill = Default::default();
        scenario.group = inf_server::GroupHoldConfig::ARM;
        scenario
    }

    /// `m2-mode-transition` (ADR-0086 D4 as amended, 2026-08-21): the m2
    /// durable shape with a **prelude life in the other barrier class**.
    /// Even seeds go FLUSH → FUA (the packed tail reopened under a
    /// `Direct` rotor — the review's lost-acked-record shape: the
    /// prelude's last frame is v2, its end unaligned, and the main life
    /// acks `always` writes on that tail before the cut); odd seeds go
    /// FUA → FLUSH (packed frames at a v3 tail's aligned end). K varies
    /// as in `m2_durable`; the stall device and the double cut ride
    /// along. The prelude quiesces to full durability before its clean
    /// restart, so the one loss window the oracle reasons about is the
    /// main life's cut.
    #[must_use]
    pub fn m2_mode_transition(seed: u64) -> DurableScenario {
        let mut scenario = DurableScenario::m2_durable(seed);
        let (first, second) = if seed.is_multiple_of(2) {
            (SegmentIoMode::Buffered, SegmentIoMode::Direct)
        } else {
            (SegmentIoMode::Direct, SegmentIoMode::Buffered)
        };
        scenario.io_mode = second;
        scenario.prelude = Some(Prelude { io_mode: first, ops_per_writer: 40 });
        // The device budget (ADR-0088 D2) at the `m2-device-budget`
        // model, 32 KiB/s per device: the zero-fill class's share grants
        // the 16 KiB next-segment fill only after ~1–2 sim-seconds, so
        // the first frames of a `Direct` life land in the reopened tail
        // *before* the class-upgrade rotation — the production window (a
        // 256 MiB zero-fill is ~0.5–1 s; an `everysec`-only cell never
        // upgrades at all) where the pre-amendment round-up lost acked
        // records. Foreground frames are metered, never deferred, so the
        // durability oracle's timing stays the m2 shape's.
        let model = inf_runtime::DeviceModel {
            write_bytes_per_s: 32 << 10,
            write_ops_per_s: 4_000,
            read_bytes_per_s: 32 << 10,
            read_ops_per_s: 4_000,
        };
        scenario.device = inf_server::DeviceConfig {
            model_share: model.share(scenario.cells),
            seal_barriers_per_s: 0,
            provenance: Default::default(),
        };
        // Half of the FLUSH → FUA seeds run without automatic checkpoints:
        // the reopened segment then stays inside the replayed log until
        // the main life's cut, so the boot after it must cross the
        // transition point — with the 24 KiB interval the segment is
        // checkpointed and truncated away first, and the other half keeps
        // that (checkpoint + truncation across the transition) covered.
        if seed.is_multiple_of(4) {
            scenario.ckpt_interval_bytes = 0;
        }
        // M4.5-S39a: the fill policy rides the seeds whose *main* life is
        // the aligned class (even seeds go FLUSH → FUA) — a quarter of the
        // sweep, the FUA tail reopened packed then upgraded under a held
        // frame. (`m2_durable`'s odd-seed arming would land in the
        // prelude, whose counters the cut never scrapes.)
        scenario.fill = if seed.is_multiple_of(4) { m2_fill_config() } else { Default::default() };
        // The lift regime on a quarter of the seeds (≡ 2, 6 mod 8: both
        // barrier orders, checkpoints on — the sidecar-commit arm needs
        // a published checkpoint before the lifted tail).
        scenario.lift_regime = seed % 4 == 2;
        scenario
    }

    /// M4.5-S36 (ADR-0088 D8) — `m2-device-budget`: the m2 durable shape
    /// on `Direct` segments (zero-fill on) with K = 3, a dataset large
    /// enough that every checkpoint streams several sections, the
    /// checkpoint interval at the m2 24 KiB so checkpoints run
    /// continuously, a **tight budget model** (128 KiB/s per device — the
    /// background classes are offered orders of magnitude more than
    /// their share by construction) over a **modest sim disk** (8 MiB/s
    /// bandwidth on a shared byte timeline, no stall episodes so the
    /// foreground bound is crisp), and the seal pacer at 2 000 barriers/s
    /// per device. The oracles (`budget_oracles`) assert the accounting
    /// identity, the rate bound, engagement, progress, and the
    /// foreground bound; the m2 durability oracle runs unchanged.
    #[must_use]
    pub fn m2_device_budget(seed: u64) -> DurableScenario {
        let mut stall = m2_stall_config();
        stall.episode_gap_ns = u64::MAX;
        stall.episode_ms_min = 0;
        stall.episode_ms_max = 0;
        stall.write_bytes_per_s = 8 << 20;
        stall.read_bytes_per_s = 8 << 20;
        // 128 KiB/s per device: the checkpoint class's share (2/10 of the
        // per-cell 64 KiB/s) refills one ~140 KB checkpoint block every
        // ~11 s against a run of ~7 sim-seconds — the second checkpoint is
        // deferred by construction (the engagement oracle's regime), the
        // first is granted from the boot-full deficit (progress).
        let model = inf_runtime::DeviceModel {
            write_bytes_per_s: 128 << 10,
            write_ops_per_s: 4_000,
            read_bytes_per_s: 128 << 10,
            read_ops_per_s: 4_000,
        };
        let cells: u16 = 2;
        DurableScenario {
            seed,
            workload: DurableWorkload::KeyValue,
            cells,
            always_writers: 3,
            esec_think_ns_max: 0,
            always_think_ns_max: 0,
            esec_writers: 3,
            mem_writers: 1,
            ops_per_writer: 160,
            keys_per_writer: 40,
            value_max: 512,
            value_pad_max: 0,
            step_ns_max: 2_000_000,
            double_cut: seed % 8 == 3,
            plant: Plant::None,
            segment_bytes: 64 << 10,
            ckpt_interval_bytes: 24 << 10,
            ckpt_stream_bytes_per_sec: None,
            ckpt_section_bytes: Some(8 << 10),
            ckpt_section_bound: None,
            stall: Some(stall),
            replay_canary: false,
            clean_stop: false,
            io_mode: SegmentIoMode::Direct,
            frames_in_flight: 3,
            device: inf_server::DeviceConfig {
                model_share: model.share(cells),
                seal_barriers_per_s: 2_000 / u64::from(cells),
                provenance: Default::default(),
            },
            budget_oracle: true,
            reorder_oracle: false,
            ckpt_direct_refused_after: None,
            fill: Default::default(),
            group: Default::default(),
            prelude: None,
            // The budget scenario's zero-fill class must keep engaging
            // (its oracle counts deferrals): recycling off.
            recycle_slots: 0,
            prealloc: inf_server::PreallocPolicy::DEFAULT,
            recycle_oracle: false,
            recycle_open_fault: false,
            lift_regime: false,
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
            esec_think_ns_max: 0,
            always_think_ns_max: 0,
            esec_writers: 2,
            mem_writers: 0,
            ops_per_writer: 180,
            keys_per_writer: 1,
            value_max: 1,
            value_pad_max: 0,
            step_ns_max: 2_000_000,
            double_cut: seed % 8 == 3,
            plant: Plant::None,
            segment_bytes: 64 << 10,
            ckpt_interval_bytes: 24 << 10,
            ckpt_stream_bytes_per_sec: None,
            ckpt_section_bytes: None,
            ckpt_section_bound: None,
            stall: Some(m2_stall_config()),
            replay_canary: false,
            clean_stop: false,
            io_mode: SegmentIoMode::Buffered,
            frames_in_flight: 1,
            device: Default::default(),
            budget_oracle: false,
            reorder_oracle: false,
            ckpt_direct_refused_after: None,
            fill: Default::default(),
            group: Default::default(),
            prelude: None,
            recycle_slots: 0,
            prealloc: inf_server::PreallocPolicy::DEFAULT,
            recycle_oracle: false,
            recycle_open_fault: false,
            lift_regime: false,
        }
    }
}

/// The S39a fill policy at its accepted design point (ADR-0089).
pub(crate) fn m2_fill_config() -> inf_server::FillConfig {
    inf_server::FillConfig::DESIGN_POINT
}

/// What one seeded run produced. `trace` is the determinism artifact
/// (every apply event incl. the post-recovery audit reads).
#[derive(Debug)]
pub struct DurableReport {
    pub trace: Vec<u8>,
    pub trace_hash: u64,
    pub violations: Vec<String>,
    pub stalled: bool,
    /// ADR-0124: scheduler steps the clean stop took to drain every cell
    /// (0 without `clean_stop`), and the reboot's tail records applied
    /// (must be 0 after a clean stop — the stop checkpoint covers it).
    pub clean_stop_steps: u64,
    pub clean_stop_replay_records: u64,
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
    /// Frame-pipeline coverage (M4.5-S35, ADR-0087 D7), scraped from the
    /// cells at the cut: the deepest in-flight count any cell reached
    /// and the two bounded waits — a sweep that never filled the pipeline
    /// proves nothing about K > 1.
    pub frames_in_flight_max: u64,
    pub frame_waits_barrier: u64,
    pub frame_waits_rotation: u64,
    /// Reorder-window hold episodes (ADR-0087 D2 as amended), scraped at
    /// the cut — the `m2-reorder-window` oracle's observable.
    pub frame_waits_reorder: u64,
    /// Checkpoint `Direct` → `Buffered` in-band downgrades (ADR-0088 D3
    /// as amended), scraped at the cut — the `m2-ckpt-refused` oracle's
    /// coverage disclosure.
    pub ckpt_downgrades: u64,
    /// Sections sealed for the ADR-0117 section bound, scraped at the
    /// cut — the section-bound arm's engagement witness (batch 34).
    pub ckpt_bound_splits: u64,
    /// The requested plant fired (batch 34 disclosure for the positive
    /// controls — a plant that never fires is a vacuous run).
    pub plant_fired: bool,
    /// M4.5-S39a: fill-policy hold episodes, scraped at the cut — the
    /// manifest's coverage disclosure and the reorder scenario's oracle.
    pub frame_waits_fill: u64,
    /// M4.5-S43: group-hold episodes, scraped at the cut (coverage).
    pub frame_waits_group: u64,
    /// Batch 42 log oracles, sampled every traffic step. F-L01-01: ticks
    /// the ledger counted idle while records sat staged across the step
    /// (no seal in between) — the tick contract, zero required. F-L01-03:
    /// the ack map's deepest point and the ledger's deepest entry count;
    /// the map is bounded by the reorder window plus the entries plus one.
    pub idle_tick_violations: u64,
    pub frames_awaiting_max: u64,
    pub fsync_entries_max: u64,
    /// Batch 44: the ledger's deepest count of unfolded write-through
    /// tickets; bounded at `WRITE_THROUGH_WINDOW_ENTRIES`.
    pub write_through_entries_max: u64,
    /// Batch 43 (F-L01-04): samples with a fill / group-hold clock open
    /// and nothing held — a stale episode; zero required.
    pub hold_episode_violations: u64,
    /// Device-budget coverage (M4.5-S36, ADR-0088 D8), scraped at the
    /// cut: background bytes the budget granted, deferrals it issued
    /// (a sweep whose budget never deferred proves nothing), the seal
    /// pacer's wait episodes, and the worst frame-write latency.
    pub budget_background_bytes: u64,
    pub budget_deferrals: u64,
    pub frame_waits_pace: u64,
    pub write_stall_max_us: u64,
    /// Packed tails reopened `Buffered` under a `Direct` rotor at the
    /// transition boot (ADR-0086 D4 as amended) — the `m2-mode-transition`
    /// FLUSH → FUA seeds report ≥ 1 per cell that had a v2 tail.
    pub reopened_packed_tails: u64,
    /// M4.5-S39b (ADR-0090 D5): recycling coverage scraped at the cut —
    /// segments recycled, pool misses, fallbacks, rotations — and what
    /// the reboot proved about the residue (segment stops + slacks). A
    /// sweep whose cells never recycled proves nothing about the rule.
    pub segments_recycled: u64,
    pub recycle_misses: u64,
    pub recycle_fallbacks: u64,
    /// Recycle sentinels written (ADR-0090 A15): every recycled take
    /// leaves one, so `== segments_recycled` at the cut on the sync
    /// tier and `≤` it on the driver tier (a take whose slice has not
    /// issued yet).
    pub recycle_sentinels: u64,
    pub segment_rotations: u64,
    pub recycled_residue_slacks: u64,
    /// Boots after a **clean stop** (ADR-0124) that reported a torn tail
    /// — a drained log has none, so any report is a phantom (review
    /// 2026-08-30 F-L02-02, ADR-0090 A15). Always 0 on a passing run.
    pub clean_stop_torn_tails: u64,
    /// ADR-0090 D9 coverage: pool waits begun / fed / expired across the
    /// cells at the cut, and inline preallocs (a rotation that found no
    /// next segment — the wait must never cause one).
    pub recycle_waits_started: u64,
    pub recycle_waits_satisfied: u64,
    pub recycle_waits_expired: u64,
    pub segment_inline_preallocs: u64,
    /// The lift regime (F-L14-01): whether this seed ran it, the acked
    /// mutating ops the tiered and indexed writers landed in the main
    /// life (the records behind the lift), the sidecars the final boot
    /// loaded (the commit-ordering arm's coverage), and — every seed —
    /// the epoch-classified residue slacks the final boot lifted past.
    pub lift_regime: bool,
    pub lift_tiered_ops: u64,
    pub lift_indexed_ops: u64,
    pub lift_sidecars_loaded: u64,
    pub stale_residue_slacks: u64,
    /// The residue plant (batch 21): cells planted after the prelude
    /// cut, the residue slacks the transition boot lifted on them, and
    /// the sidecars it loaded — the plant's oracle requires one of each
    /// per planted cell, so a vacuous plant is red, never a pass.
    pub lift_plants: u64,
    pub lift_plant_lifts: u64,
    pub lift_plant_sidecars: u64,
    /// The torn-tail plant (batch 35, N17): cells whose FLUSH prelude
    /// tail was torn after the cut so the FUA transition boot reopens
    /// packed data on the half the lift plant does not own.
    pub torn_plants: u64,
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
        scope: ExecScope,
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
        crate::harness::trace_scope(&mut trace, scope);
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
    /// The lift regime's tiered `always` namespace (F-L14-01).
    Tiered,
    /// The lift regime's indexed document `always` namespace.
    Indexed,
}

impl NsClass {
    pub(crate) fn name(self) -> &'static [u8] {
        match self {
            NsClass::Always => b"alw",
            NsClass::Everysec => b"esec",
            NsClass::Tiered => b"tier",
            NsClass::Indexed => b"idx",
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
    /// Batch 42 think time: no send before this instant.
    pub(crate) idle_until: Nanos,
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
    // writer identity + stream seed + combined-scenario channels
    #[allow(clippy::too_many_arguments)]
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
            idle_until: Nanos(0),
            setup,
            ledger: Ledger::new(),
            tainted: std::collections::BTreeSet::new(),
            pub_seq: vec![0; channels],
            models: BTreeMap::new(),
            corpus_docs_used: 0,
        }
    }

    /// The lift regime's tiered traffic (the `m4-tiered` value classes):
    /// inline SETs that demote through the ring, blob SETs that ride
    /// extents, overwrites and deletes of cold keys that stage
    /// displacement pairs — every reply exact.
    fn next_tiered_command(&mut self, scenario: &DurableScenario) -> (Vec<u8>, Pending) {
        let key = self.key(scenario.keys_per_writer);
        let roll = self.rng.next_below(100);
        if roll < 67 {
            let len = if roll < 55 {
                1024 + self.rng.next_below(2048) as usize
            } else {
                (6 << 10) + self.rng.next_below(4096) as usize
            };
            let stamp = format!("t:{}:{}:", self.id, self.sent).into_bytes();
            let value: Vec<u8> = stamp.iter().copied().cycle().take(len).collect();
            let wire = encode(&[b"SET", &key, &value]);
            let pending = Pending {
                key,
                state_after: Some(value),
                expect: b"+OK\r\n".to_vec(),
                mutates: true,
                taints: false,
            };
            (wire, pending)
        } else if roll < 85 {
            let state_after = self.last_state(&key);
            let expect = state_after.as_ref().map_or(b"$-1\r\n".to_vec(), |v| bulk(v));
            let wire = encode(&[b"GET", &key]);
            (wire, Pending { key, state_after, expect, mutates: false, taints: false })
        } else {
            let existed = self.last_state(&key).is_some();
            let expect = if existed { b":1\r\n".to_vec() } else { b":0\r\n".to_vec() };
            let wire = encode(&[b"DEL", &key]);
            (wire, Pending { key, state_after: None, expect, mutates: true, taints: false })
        }
    }

    pub(crate) fn key(&mut self, keys_per_writer: u64) -> Vec<u8> {
        format!("k:{}:{}", self.id, self.rng.next_below(keys_per_writer)).into_bytes()
    }

    pub(crate) fn last_state(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.ledger.get(key).and_then(|ops| ops.last()).and_then(|op| op.state_after.clone())
    }

    /// Absorb one reply: the USE handshake, then the in-flight op's
    /// expectation, its ledger ack, and the class latency oracles (the
    /// everysec deferral bound, the always ack maximum).
    pub(crate) fn absorb_reply(
        &mut self,
        reply: Vec<u8>,
        now: Nanos,
        seed: u64,
        report: &mut DurableReport,
    ) {
        if self.setup {
            if reply != b"+OK\r\n" {
                report.violations.push(format!("writer {}: USE answered {reply:?}", self.id));
            }
            self.setup = false;
            return;
        }
        let Some(pending) = self.inflight.take() else {
            report.violations.push(format!("writer {}: unsolicited reply {reply:?}", self.id));
            return;
        };
        if reply != pending.expect {
            report.violations.push(format!(
                "writer {} key {:?}: expected {:?}, got {:?}",
                self.id,
                String::from_utf8_lossy(&pending.key),
                String::from_utf8_lossy(&pending.expect),
                String::from_utf8_lossy(&reply)
            ));
        }
        if pending.mutates {
            let ops = self.ledger.entry(pending.key.clone()).or_default();
            let rec = ops.last_mut().expect("sent op has a ledger entry");
            rec.acked_at = Some(now);
            // The group-commit-class oracle (M2.5-S14, §1.5): an everysec
            // ack is deferred, never device-gated — under a stall a gated
            // ack inherits the episode.
            let latency = now.saturating_sub(rec.sent_at);
            match self.class {
                NsClass::Everysec if latency > EVERYSEC_ACK_BOUND => {
                    report.violations.push(format!(
                        "EVERYSEC DEFERRAL VIOLATION seed {:#x} writer {} key {:?}: ack \
                         latency {} ms exceeds {} ms — everysec acked behind the device",
                        seed,
                        self.id,
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
        self.replied += 1;
        report.commands_done += 1;
    }

    /// Re-attach to a rebooted node (the prelude's clean restart): a fresh
    /// connection, the USE handshake again, the quota reset for the next
    /// life; the ledger carries forward.
    pub(crate) fn reconnect(&mut self, node: &mut Node, quota: u64) {
        debug_assert!(self.inflight.is_none(), "reconnect after quiescence only");
        self.fd = node.nets[self.cell].borrow_mut().connect();
        self.setup = self.class != NsClass::Memory;
        if self.setup {
            node.nets[self.cell]
                .borrow_mut()
                .client_send(self.fd, &encode(&[b"INF.NS", b"USE", self.class.name()]));
        }
        self.sent = 0;
        self.replied = 0;
        self.quota = quota;
        // A memory namespace holds nothing across a restart: the model
        // starts empty, as the keyspace does (the durable ledgers carry
        // forward — they are what the final audit binds).
        if self.class == NsClass::Memory {
            self.ledger.clear();
        }
    }

    /// Builds the next command + its exact expected reply.
    pub(crate) fn next_command(&mut self, scenario: &DurableScenario) -> (Vec<u8>, Pending) {
        if scenario.workload == DurableWorkload::Document || self.class == NsClass::Indexed {
            return crate::document::next_document_command(self, scenario);
        }
        if self.class == NsClass::Tiered {
            return self.next_tiered_command(scenario);
        }
        let key = self.key(scenario.keys_per_writer);
        let roll = self.rng.next_below(100);
        if roll < 70 {
            let mut value =
                format!("v:{}:{}:{}", self.id, self.sent, self.rng.next_below(scenario.value_max))
                    .into_bytes();
            // `always` writers only: their frames are what the residue is
            // made of, and an `everysec` ack must stay inside its 30 ms
            // deferral bound whatever the device is doing (the oracle
            // below) — padding those would measure the pad, not the ack.
            if scenario.value_pad_max > 0 && self.class == NsClass::Always {
                let pad =
                    usize::try_from(self.rng.next_below(scenario.value_pad_max)).expect("fits");
                value.extend(std::iter::repeat_n(b'p', pad));
            }
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
    /// ADR-0103 (the `m2-ns-create-window` scenario): while set, `step`
    /// skips the control-inbox drain — the catalog swap is "in flight"
    /// for as many steps as the scenario wants, the deterministic form
    /// of a slow `META` fdatasync.
    pub(crate) hold_inbox: bool,
    /// ADR-0108 (the `m2-ns-ddl-race` scenario): `(cell, steps)` — that
    /// cell's loop is not stepped for `steps` scheduler steps, the
    /// deterministic form of a stalled origin (a park, an I/O stall)
    /// whose DDL fan legs are then overtaken by another cell's.
    pub(crate) frozen: Option<(usize, u64)>,
}

pub(crate) fn boot(
    scenario: &DurableScenario,
    data_dir: PathBuf,
    disk: &SimDisk,
    clock: &Rc<VirtualClock>,
    observer: &TraceObserver,
) -> std::io::Result<Node> {
    // F-L04-06: a driver-tier durable boot runs on a device whose plain
    // writes can land after a later-issued fsync (ADR-0087 D7) — the
    // instant device orders everything by submission and proves nothing
    // about the drain rule. `StallConfig::write_reorder()` is the floor.
    assert!(
        disk.write_reorder_armed(),
        "harness: the write-vs-fsync reorder window is closed on this disk (F-L04-06) — \
         arm StallConfig::write_reorder() or the m2 stall device"
    );
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
        let mut ks =
            Keyspace::new(StoreConfig { hasher: node_hasher(scenario.seed), ..Default::default() });
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
            staging: inf_server::StagingConfig {
                frames_in_flight: scenario.frames_in_flight,
                ..Default::default()
            },
            segment: inf_server::SegmentConfig {
                segment_bytes: scenario.segment_bytes,
                io_mode: scenario.io_mode,
                recycle_slots: scenario.recycle_slots,
                prealloc: scenario.prealloc,
                ..Default::default()
            },
            ckpt: inf_server::CkptConfig {
                interval_bytes: scenario.ckpt_interval_bytes,
                stream_bytes_per_sec: scenario
                    .ckpt_stream_bytes_per_sec
                    .unwrap_or(inf_server::CkptConfig::default().stream_bytes_per_sec),
                section_bytes: scenario
                    .ckpt_section_bytes
                    .unwrap_or(inf_server::CkptConfig::default().section_bytes),
                section_bound: scenario
                    .ckpt_section_bound
                    .unwrap_or(inf_server::CkptConfig::default().section_bound),
                ..Default::default()
            },
            recover: Default::default(),
            flush_bound: 1,
            fua_p50_us_probed: 0,
            device: scenario.device,
            fill: scenario.fill,
            group: scenario.group,
        };
        plane.set_control(std::sync::Arc::clone(&control));
        plane.begin_recovery(disk.clone(), &cfg, i as u16, clock.now());
        let config = LoopConfig { spin_iters: 4, ..Default::default() };
        let cell_loop = CellLoop::new(driver, Rc::clone(clock), pool, config);
        nets.push(net);
        cells.push((cell_loop, plane));
    }
    Ok(Node { cells, nets, control, inbox, data_dir, hold_inbox: false, frozen: None })
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
        let skip = match self.frozen {
            Some((cell, steps)) if steps > 0 => {
                self.frozen = Some((cell, steps - 1));
                Some(cell)
            }
            _ => {
                self.frozen = None;
                None
            }
        };
        for i in 0..n {
            let idx = (i + rotate) % n;
            if Some(idx) == skip {
                continue;
            }
            let (cell_loop, plane) = &mut self.cells[idx];
            cell_loop.run_iteration(plane).expect("sim iteration");
            if let Some(err) = plane.take_boot_error() {
                return Err(err);
            }
        }
        if !self.hold_inbox {
            self.inbox.drain(disk, &self.data_dir)?;
        }
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

    pub(crate) fn plane_mut(&mut self, cell: usize) -> &mut SimPlane {
        &mut self.cells[cell].1
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

    /// Sends `argv` without awaiting its reply — for commands that must
    /// be in flight together with another connection's (the M4.5-S37
    /// overlapping-drain row); the caller steps the node and
    /// [`recv`](Self::recv)s.
    pub(crate) fn send(&mut self, node: &mut Node, argv: &[&[u8]]) {
        node.nets[self.cell].borrow_mut().client_send(self.fd, &encode(argv));
    }

    /// One framed reply if it has arrived (no stepping).
    pub(crate) fn recv(&mut self, node: &mut Node) -> Option<Vec<u8>> {
        let bytes = node.nets[self.cell].borrow_mut().client_recv(self.fd);
        self.rx.extend_from_slice(&bytes);
        let n = reply_len(&self.rx)?;
        Some(self.rx.drain(..n).collect())
    }
}

// ---- the §8.2 admissible-state rule (shared with the combined run) ---------

/// The §8.2 required index: the last op the promise binds for `class`
/// at the cut instant (`None` = nothing required).
pub(crate) fn required_index(
    class: NsClass,
    ops: &[OpRec],
    cut_time: Nanos,
    clean_stop: bool,
) -> Option<usize> {
    ops.iter().rposition(|op| match op.acked_at {
        None => false,
        Some(at) => match class {
            NsClass::Always | NsClass::Tiered | NsClass::Indexed => true,
            // A clean stop (ADR-0124 D4) keeps every acked everysec op;
            // the window is a crash property only.
            NsClass::Everysec => clean_stop || at + EVERYSEC_WINDOW <= cut_time,
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
