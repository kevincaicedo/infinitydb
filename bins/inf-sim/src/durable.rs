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
use inf_foundation::bounded::CapRow;
use inf_foundation::fault::FaultSpec;
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::{Clock, Nanos, VirtualClock};
use inf_foundation::{CellId, hash64};
use inf_log::ckpt::{IckReaderConfig, ick_file_name, read_ick};
use inf_log::{
    REORDER_WINDOW_FRAMES, ReaderConfig, SegmentId, SegmentReader, WRITE_THROUGH_WINDOW_ENTRIES,
    read_manifest, scan_log_dir_from,
};
use inf_runtime::{CellLoop, IterStats, LoopConfig};
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
#[cfg(test)]
mod lastsave;
#[cfg(test)]
mod park;
mod run;
pub use run::run_durable_scenario;

pub(crate) use audit::survival_audit;
use audit::{
    audit_ledgers, budget_oracles, engagement_checks, finish, lift_plant_oracle, lift_regime_ddl,
    lift_regime_index_oracle, lift_regime_seed_checkpoint, lift_regime_seed_life, overrun_oracles,
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
    /// ADR-0178's arm of the budget scenario (`m2-device-budget`, seeds ≡
    /// 1 mod 4): `Some` pads one in 16 of an `always` writer's `SET`s, in
    /// the run's first quarter, to a value whose checkpoint section block
    /// is above the class cap plus the pool — issued only by an overrun.
    /// `None` everywhere else: every other trace stays byte-identical.
    pub ckpt_overrun: Option<CkptOverrunArm>,
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
    /// The reactor's busy iterations before it parks (`LoopConfig`):
    /// [`Self::SPIN_ITERS`] on every scenario. The loop-tier park tests set
    /// 0, the value at which an idle iteration parks at once, and read the
    /// decision back from [`Node::iter_stats`].
    pub spin_iters: u32,
    /// The tier-budget shape (`m4-tier-budget`): `Some` runs one or more
    /// tiered `always` namespaces on every cell, each with its own
    /// closed-loop writers, under the device model (or with none), so the
    /// tier-flush class is offered beside the foreground. The regime
    /// reached is the leg's ([`TierBudgetLeg`]): a round is issued in
    /// groups of at most one group's bytes and ops, each offered at its
    /// exact size, so an `8mb` round waits for refills between its
    /// groups; at the hostile share the first namespace's load outruns the
    /// class and the second waits its turn behind it; with no model a
    /// `64mb` round issues across turns. The lift regime's one tiered namespace
    /// is a different arm. `None` everywhere else: every other trace
    /// stays byte-identical.
    pub tier_budget: Option<TierBudgetShape>,
    /// The epoch-marks shape (`m4-epoch-marks`): `Some` runs one tiered
    /// namespace, `em`, whose writers drive the extent-reclaim marks in the
    /// leg's regime ([`EpochMarksLeg`]), and reads the marks' census row at
    /// the cut. `None` everywhere else: every other trace stays
    /// byte-identical.
    pub epoch_marks: Option<EpochMarksLeg>,
}

/// The epoch-marks shape's legs, chosen by name (`--marks-leg`), never by
/// the seed: every seed of a run drives the one leg named, and the smoke
/// lane runs each leg on its own row.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EpochMarksLeg {
    /// Two cells, a tiered `everysec` namespace whose closed-loop writers
    /// set inline and blob values, overwrite and delete them (extent
    /// deaths), many MAINTAIN ticks inside each everysec period, on a device
    /// with no stall episode: no wedge holds a barrier, so every merge of
    /// the marks is exact and the marks row reads no crossing.
    Everysec,
    /// One cell, a tiered `always` namespace on `Direct` segments of
    /// 256 KiB over a device whose FLUSH-class barrier is slow while
    /// write-through writes are fast: each rotation's seal fdatasync holds
    /// the front of the fsync ledger while write-through frames carrying
    /// the namespace's inline writes complete behind it, so the marks see
    /// more ack classes than their cap. Each writer's first write of its
    /// first key is a blob, which a later write kills: the deaths a merged
    /// mark gates. The row must read its cap and at least one crossing at
    /// the cut, else the run is vacuous; the cut is the first extent
    /// reclaim after the first crossing (`MarksCut`).
    Wedge,
}

impl EpochMarksLeg {
    pub const ALL: [EpochMarksLeg; 2] = [EpochMarksLeg::Everysec, EpochMarksLeg::Wedge];

    /// The leg's name on the command line.
    pub fn name(self) -> &'static str {
        match self {
            EpochMarksLeg::Everysec => "everysec",
            EpochMarksLeg::Wedge => "wedge",
        }
    }

    /// The leg named `name`, if any.
    pub fn parse(name: &str) -> Option<EpochMarksLeg> {
        EpochMarksLeg::ALL.into_iter().find(|leg| leg.name() == name)
    }

    /// The scheduler step of the leg's cut, given the seeded one: the
    /// everysec leg cuts at the seeded step; the wedge leg runs until its
    /// own cut (`MarksCut`: the first extent reclaim after the marks crossed
    /// their cap) or, past every writer's quota, this ceiling.
    pub(crate) fn cut_step(self, seeded: u64) -> u64 {
        match self {
            EpochMarksLeg::Everysec => seeded,
            EpochMarksLeg::Wedge => 40_000,
        }
    }

    /// The namespace's `FSYNC` class.
    fn fsync(self) -> &'static [u8] {
        match self {
            EpochMarksLeg::Everysec => b"everysec",
            EpochMarksLeg::Wedge => b"always",
        }
    }

    /// The longest think time after a writer's send: none on the everysec
    /// leg; on the wedge leg up to one FLUSH-class barrier (10 ms), so the
    /// writers acked together at a barrier send again spread over the next
    /// one, a write-through frame each, instead of in one frame.
    pub(crate) fn think_ns_max(self) -> u64 {
        match self {
            EpochMarksLeg::Everysec => 0,
            EpochMarksLeg::Wedge => 10_000_000,
        }
    }

    /// The namespace's `BLOB-THRESHOLD`: at its 4 KiB floor, so the
    /// generators' larger values ride extents and their overwrites and
    /// deletes are extent deaths: the deaths the marks gate.
    fn blob_threshold(self) -> &'static [u8] {
        match self {
            EpochMarksLeg::Everysec | EpochMarksLeg::Wedge => b"4kb",
        }
    }
}

/// How many tiered namespaces the tier-budget shape runs:
/// `1..=TIER_BUDGET_NS_MAX`, the indices the fixed name table covers. The
/// one constructor checks the range, so every index below a count names
/// a namespace.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TierBudgetCount(u8);

impl TierBudgetCount {
    /// `count` in `1..=TIER_BUDGET_NS_MAX`, else `None`.
    pub const fn new(count: u8) -> Option<TierBudgetCount> {
        if count >= 1 && count <= TIER_BUDGET_NS_MAX { Some(TierBudgetCount(count)) } else { None }
    }

    pub const fn get(self) -> u8 {
        self.0
    }

    /// The namespaces' indices, each a valid [`NsClass::TierBudget`] index.
    pub fn indices(self) -> std::ops::Range<u8> {
        0..self.0
    }
}

/// The tier-budget shape's legs, chosen by name (`--tier-leg`), never by
/// the seed: every seed of a run drives the one leg named, so a sweep is
/// one shape and the smoke lane runs each leg on its own row. The list
/// grows by a variant here and a row there.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TierBudgetLeg {
    /// Two namespaces at `MAINTAIN-SLICE 8mb` on the reference share: a
    /// round of eight full groups waits for refills between them, so the
    /// turn is contended and refuses.
    TwoAt8mb,
    /// Eight namespaces at `1mb`, the most a cell carries here: the
    /// one-plan-per-turn oracle's leg.
    EightAt1mb,
    /// One namespace at `1mb` on the reference share: the control leg,
    /// which reads no contention and no plan passed over.
    OneAt1mb,
    /// Two namespaces at the hostile share (the reference box's rate at
    /// the share a cell holds among 256): the first at `1mb` under a
    /// burst its flush cannot drain, the second at `64kb` under a light
    /// steady load, so the second's small groups take their turn between
    /// the first's — the fair turn's wait-bound oracle's leg, and the
    /// leg on which a whole round offered at once is above the class cap.
    TwoAt1mbHostile,
    /// One namespace at `64mb` with no device model (the regime of
    /// `--device-probe off`: every offer is `Now`, nothing waits for a
    /// refill): a 64 MiB round — 67 writes and 3 barriers — is more ops
    /// than one turn's 64 descriptor visits, so it issues across turns,
    /// and with the write-fault plant its retry group is built behind a
    /// run of done writes: the leg of the group builder's cursor rule.
    /// The window is 72 MiB with nine tenths mutable, so while the fill
    /// lasts each seal takes the excess above 64.8 MiB, a page or so;
    /// once the writers are done the harness drops `MUTABLE-FRACTION` to
    /// 10 permille, the next seal takes the whole 64 MiB slice and the
    /// next plan stages the round. The write fault is armed once the
    /// driver has seen a pass of the turn's 64 ops, so the retry is that
    /// round's.
    OneAt64mbNoModel,
    /// The op bound's wire leg (campaign tier: one seed, run alone, never a
    /// smoke row; about 9 × 10⁶ scheduler steps, 13.5 GiB resident through
    /// the drain, 18 GiB at the boot that replays the whole log): one cell,
    /// one namespace at `1mb` in a 4,609 MiB window with one permille
    /// mutable, filled by 64 writers with 4,800 `SET`s each of 15,360 B
    /// values on unique keys (4,505 MiB, under the window by 104 MiB), on a
    /// `Buffered` log of 256 MiB segments with no automatic checkpoint, the
    /// model at a thousandth of the disk. The fill ends inside the second
    /// round's 42.9 s wait for credit, while one seal step a scheduler step
    /// records a flush cut per 1 MiB: 4,096 cuts are retained and every
    /// later one is dropped, so the run from the last retained cut to the
    /// ro-boundary has no cut and is far above 256 MiB. A stage that ends
    /// its span at a cut takes that run whole, past the round's op bound;
    /// a span selected by the records' own lengths takes 1 MiB. The run
    /// cuts once `flushed` passes the last retained cut
    /// ([`TierBudgetLeg::cut_step`] is the ceiling), with the TAIL-STALL
    /// timeout at its 60 s maximum so no writer is answered while it waits.
    CoarseCuts,
}

impl TierBudgetLeg {
    pub const ALL: [TierBudgetLeg; 6] = [
        TierBudgetLeg::TwoAt8mb,
        TierBudgetLeg::EightAt1mb,
        TierBudgetLeg::OneAt1mb,
        TierBudgetLeg::TwoAt1mbHostile,
        TierBudgetLeg::OneAt64mbNoModel,
        TierBudgetLeg::CoarseCuts,
    ];

    /// The coarse-cuts leg's step ceiling: its fill is about 1.2 × 10⁴
    /// steps and its drain of 4,096 one-cut rounds at a thousandth of the
    /// disk about 8.8 × 10⁶, so a run that has not cut by here never will.
    const COARSE_CUTS_STEP_CEILING: u64 = 30_000_000;

    /// The leg's name on the command line.
    pub fn name(self) -> &'static str {
        match self {
            TierBudgetLeg::TwoAt8mb => "two-8mb",
            TierBudgetLeg::EightAt1mb => "eight-1mb",
            TierBudgetLeg::OneAt1mb => "one-1mb",
            TierBudgetLeg::TwoAt1mbHostile => "two-1mb-hostile",
            TierBudgetLeg::OneAt64mbNoModel => "one-64mb-no-model",
            TierBudgetLeg::CoarseCuts => "coarse-cuts",
        }
    }

    /// The scheduler step of the leg's cut, given the seeded one: every
    /// leg cuts at the seeded step but the coarse-cuts leg, which runs
    /// until its own cut (`flushed` past the last retained flush cut) or,
    /// failing that, its ceiling.
    pub(crate) fn cut_step(self, seeded: u64) -> u64 {
        match self {
            TierBudgetLeg::CoarseCuts => TierBudgetLeg::COARSE_CUTS_STEP_CEILING,
            TierBudgetLeg::TwoAt8mb
            | TierBudgetLeg::EightAt1mb
            | TierBudgetLeg::OneAt1mb
            | TierBudgetLeg::TwoAt1mbHostile
            | TierBudgetLeg::OneAt64mbNoModel => seeded,
        }
    }

    /// The coarse-cuts leg.
    pub fn is_coarse_cuts(self) -> bool {
        matches!(self, TierBudgetLeg::CoarseCuts)
    }

    /// The leg named `name`, if any.
    pub fn parse(name: &str) -> Option<TierBudgetLeg> {
        TierBudgetLeg::ALL.into_iter().find(|leg| leg.name() == name)
    }

    /// The leg's device model: the reference box's (489 MB/s, 40,000
    /// ops/s), shared by the node's cells, or that box's rate at the
    /// share a cell holds among 256, carried by the two cells here; the
    /// coarse-cuts leg's at a thousandth of it, so its second round waits
    /// 20 MiB of share at 489 kB/s, 42.9 s, longer than its fill; the
    /// no-model leg has none (`DeviceModel::ABSENT`).
    pub fn model(self) -> inf_runtime::DeviceModel {
        let divisor = match self {
            TierBudgetLeg::TwoAt8mb | TierBudgetLeg::EightAt1mb | TierBudgetLeg::OneAt1mb => 1,
            TierBudgetLeg::TwoAt1mbHostile => 128,
            TierBudgetLeg::CoarseCuts => 1_000,
            TierBudgetLeg::OneAt64mbNoModel => return inf_runtime::DeviceModel::ABSENT,
        };
        inf_runtime::DeviceModel {
            write_bytes_per_s: 489_000_000 / divisor,
            write_ops_per_s: 40_000 / divisor,
            read_bytes_per_s: 489_000_000 / divisor,
            read_ops_per_s: 40_000 / divisor,
        }
    }

    /// Whether the leg's oracle requires the turn to have been contended
    /// and to have refused (the wait-bound oracle's engagement); the
    /// other legs read the bound without requiring the regime (on the
    /// reference share a round's groups fit the class cap, so nothing
    /// waits for a refill).
    pub fn expects_contention(self) -> bool {
        matches!(self, TierBudgetLeg::TwoAt1mbHostile)
    }

    /// Whether the leg's oracle requires a namespace with work to have
    /// been passed over by the cell's one plan (the one-plan oracle's
    /// engagement).
    pub fn expects_plan_waits(self) -> bool {
        matches!(self, TierBudgetLeg::EightAt1mb)
    }

    /// The control leg: one namespace, so the turn never contends and
    /// the plan never passes one over.
    pub fn is_control(self) -> bool {
        matches!(self, TierBudgetLeg::OneAt1mb)
    }

    /// The no-model leg: the budget reads its model absent, every offer
    /// is `Now`, and the cap and wait-bound oracles have no reading; its
    /// regime is a round of more ops than one turn's descriptor visits.
    pub fn model_absent(self) -> bool {
        matches!(self, TierBudgetLeg::OneAt64mbNoModel)
    }

    /// The leg's shape. On the reference-share legs, 3 writers per
    /// namespace with no think time and a 3 MiB window with a tenth of
    /// it mutable: seal marks land per 1 MiB commit page and a cell seals
    /// once a namespace's tail is a page past the mutable target. On the
    /// hostile leg the first namespace's twelve writers burst with no
    /// think time into a 256 MiB window with one percent of it mutable:
    /// their frames hold the foreground at the share, so the tier class
    /// refills at its floor while the sealed backlog outruns it for the
    /// whole burst, and no writer parks on its tail. The second
    /// namespace's one writer, also without think time, writes into an
    /// 8 MiB window at a 64 KiB slice: its small rounds are ready from
    /// early on, each takes its turn between the first's groups, and the
    /// window holds what the floor cannot drain during the burst. On the
    /// no-model leg six writers without think time fill a 72 MiB window
    /// whose first seal comes at 64.8 MiB: 84,000 commands of about 2 KiB
    /// each on the tail (two thirds are `SET`s of 1–10 KiB) are 84 MiB a
    /// cell by key hash, above the seal with a margin.
    fn shape(self) -> TierBudgetShape {
        const ONE: TierBudgetCount = match TierBudgetCount::new(1) {
            Some(count) => count,
            None => panic!("one tier-budget namespace is within the name table"),
        };
        const TWO: TierBudgetCount = match TierBudgetCount::new(2) {
            Some(count) => count,
            None => panic!("two tier-budget namespaces are within the name table"),
        };
        const EIGHT: TierBudgetCount = match TierBudgetCount::new(TIER_BUDGET_NS_MAX) {
            Some(count) => count,
            None => panic!("the name table's size is within itself"),
        };
        let reference = |namespaces: TierBudgetCount, maintain_slice: &'static [u8], ops, keys| {
            let load = TierBudgetLoad {
                writers: 3,
                ops_per_writer: ops,
                keys_per_writer: keys,
                think_ns_max: 0,
                mem_budget: b"3mb",
                mutable_permille: b"100",
                maintain_slice,
                tail_stall_timeout_ms: None,
                values: TierBudgetValues::Mixed,
            };
            TierBudgetShape { leg: self, namespaces, first: load, rest: load }
        };
        match self {
            TierBudgetLeg::TwoAt8mb => reference(TWO, b"8mb", 1_000, 40),
            // 2,000 keys a writer: a live set of about 9 MB a cell, above
            // the 3 MiB window, so the recovered life seals every
            // namespace at its first MAINTAIN.
            TierBudgetLeg::EightAt1mb => reference(EIGHT, b"1mb", 1_000, 2_000),
            TierBudgetLeg::OneAt1mb => reference(ONE, b"1mb", 3_000, 40),
            TierBudgetLeg::TwoAt1mbHostile => TierBudgetShape {
                leg: self,
                namespaces: TWO,
                first: TierBudgetLoad::HOSTILE_FIRST,
                rest: TierBudgetLoad::HOSTILE_REST,
            },
            TierBudgetLeg::OneAt64mbNoModel => {
                let load = TierBudgetLoad {
                    writers: 6,
                    ops_per_writer: 14_000,
                    keys_per_writer: 40,
                    think_ns_max: 0,
                    mem_budget: b"72mb",
                    mutable_permille: b"900",
                    maintain_slice: b"64mb",
                    tail_stall_timeout_ms: None,
                    values: TierBudgetValues::Mixed,
                };
                TierBudgetShape { leg: self, namespaces: ONE, first: load, rest: load }
            }
            TierBudgetLeg::CoarseCuts => {
                let load = TierBudgetLoad::COARSE_CUTS;
                TierBudgetShape { leg: self, namespaces: ONE, first: load, rest: load }
            }
        }
    }
}

/// One tier-budget namespace's load and window: its closed-loop writers,
/// each on a seeded cell, their ops and think time, and its `INF.NS
/// CREATE` arguments.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TierBudgetLoad {
    pub writers: usize,
    /// Ops per writer (the other classes keep the scenario's
    /// `ops_per_writer`).
    pub ops_per_writer: u64,
    /// Keys per writer: the namespace's live set is the writers' keys,
    /// so a count whose live set is above the window is what a recovered
    /// life seals at its first MAINTAIN (the other classes keep the
    /// scenario's `keys_per_writer`).
    pub keys_per_writer: u64,
    /// The longest think time between a writer's commands, drawn per
    /// command; 0 = none.
    pub think_ns_max: u64,
    pub mem_budget: &'static [u8],
    pub mutable_permille: &'static [u8],
    /// The `MAINTAIN-SLICE`: the most record bytes one round stages; a
    /// round issues in groups of at most one group's bytes and ops
    /// whatever the slice.
    pub maintain_slice: &'static [u8],
    /// The `TAIL-STALL-TIMEOUT` the `CREATE` names, in milliseconds;
    /// `None` keeps the namespace default (1,000 ms).
    pub tail_stall_timeout_ms: Option<&'static [u8]>,
    /// What the writers send.
    pub values: TierBudgetValues,
}

impl TierBudgetLoad {
    /// The hostile leg's first namespace: twelve writers bursting into a
    /// 256 MiB window with one percent of it mutable, at `1mb`.
    const HOSTILE_FIRST: TierBudgetLoad = TierBudgetLoad {
        writers: 12,
        ops_per_writer: 4_000,
        keys_per_writer: 40,
        think_ns_max: 0,
        mem_budget: b"256mb",
        mutable_permille: b"10",
        maintain_slice: b"1mb",
        tail_stall_timeout_ms: None,
        values: TierBudgetValues::Mixed,
    };

    /// The hostile leg's second namespace: one writer into an 8 MiB window
    /// at a 64 KiB slice.
    const HOSTILE_REST: TierBudgetLoad = TierBudgetLoad {
        writers: 1,
        ops_per_writer: 6_000,
        keys_per_writer: 40,
        think_ns_max: 0,
        mem_budget: b"8mb",
        mutable_permille: b"100",
        maintain_slice: b"64kb",
        tail_stall_timeout_ms: None,
        values: TierBudgetValues::Mixed,
    };

    /// The coarse-cuts leg's load: 4,800 `SET`s a writer, each a new key,
    /// so the fill is 64 × 4,800 records of at most 8 + 9 + 15,360 B,
    /// 4,505 MiB, under the 4,609 MiB window (`MEM-BUDGET` plus the slice)
    /// with the head at 0: no writer parks on its tail.
    const COARSE_CUTS: TierBudgetLoad = TierBudgetLoad {
        writers: 64,
        ops_per_writer: 4_800,
        keys_per_writer: 4_800,
        think_ns_max: 0,
        mem_budget: b"4608mb",
        mutable_permille: b"1",
        maintain_slice: b"1mb",
        tail_stall_timeout_ms: Some(b"60000"),
        values: TierBudgetValues::SetOnlyUnique { value_bytes: 15_360 },
    };
}

/// The value class of a tier-budget namespace's writers.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TierBudgetValues {
    /// The lift regime's mix: `SET`s of 1–10 KiB, `GET`s and `DEL`s over
    /// the writer's keys.
    Mixed,
    /// `SET`s alone, each of a new key (`k:<writer>:<sent>`), each value
    /// `value_bytes` long: every record lands once and stays live.
    SetOnlyUnique { value_bytes: usize },
}

/// How many tiered namespaces the tier-budget shape runs and how each
/// is loaded (see [`DurableScenario::tier_budget`]); built only by
/// [`TierBudgetLeg::shape`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TierBudgetShape {
    /// The leg this shape is, for the oracles' per-leg reads.
    pub leg: TierBudgetLeg,
    /// Tiered namespaces created on the node; every cell materializes
    /// each one (the DDL fans out).
    pub namespaces: TierBudgetCount,
    /// The first namespace's load, and every other's.
    pub first: TierBudgetLoad,
    pub rest: TierBudgetLoad,
}

impl TierBudgetShape {
    /// The load of the namespace at `index`.
    pub fn load(&self, index: u8) -> TierBudgetLoad {
        if index == 0 { self.first } else { self.rest }
    }
}

/// One cell's tier-flush class at the cut of a tier-budget run: what the
/// budget counted against what the sim disk saw, and the flush each
/// namespace reached. Read from the budget's own ledger and the driver's
/// observed bytes; `flushed` is each namespace's watermark advance.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TierBudgetCell {
    pub cell: u16,
    pub unattainable: u64,
    pub deferrals: u64,
    pub spent_bytes: u64,
    pub spent_ops: u64,
    pub overrun_bytes: u64,
    pub driver_bytes: u64,
    pub driver_ops: u64,
    /// Tier-class bytes and ops pushed and not yet submitted at the cut.
    pub queued_bytes: u64,
    pub queued_ops: u64,
    /// The most tier-class bytes and ops one driver entry carried.
    pub pass_bytes_max: u64,
    pub pass_ops_max: u64,
    /// The turn's counters as the last MAINTAIN flushed them.
    pub groups_deferred: u64,
    pub group_wait_ns_max: u64,
    pub turn_waits: u64,
    pub turn_contended: u64,
    pub plan_waits: u64,
    pub rounds_planned: u64,
    pub write_retries: u64,
    /// `(namespace index, flushed, ro-boundary, tail)` bytes past the
    /// life origin, per tier-budget namespace materialized on the cell.
    pub flushed: Vec<(u8, u64, u64, u64)>,
}

/// The oversized-value pad of ADR-0178's sim arm (see
/// [`DurableScenario::ckpt_overrun`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CkptOverrunArm {
    /// Padded value lengths, inclusive: `bytes_min` is one above the
    /// checkpoint class's cap plus the pool at the scenario's share.
    pub bytes_min: u64,
    pub bytes_max: u64,
    /// A writer pads only while it has sent fewer commands than this: the
    /// run's first quarter, so every padded block's checkpoint lands in it.
    pub before_command: u64,
}

impl CkptOverrunArm {
    /// One pad in this many of an `always` writer's `SET`s.
    pub const ONE_IN_SETS: u64 = 16;

    /// The arm's largest checkpoint block (ADR-0178 D3's table: one record
    /// plus less than one section target, aligned): the padded value plus
    /// one target, and one 4 KiB block each for the record's framing and
    /// the section's header and alignment.
    #[must_use]
    pub const fn block_max(&self, section_target: u64) -> u64 {
        self.bytes_max + section_target + 2 * 4096
    }
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

/// ADR-0178's arm of `m2-device-budget` (seeds ≡ 1 mod 4) and the device
/// model each seed runs. Off the arm: 128 KiB/s per device. On it the
/// device is raised to 768 KiB/s (384 KiB/s per cell at 2 cells), where
/// the checkpoint class's cap is its 12 288 B section slice and the pool
/// holds 19 660 B; values are padded from one above those two together
/// to 40 KiB, and the segment is raised to 256 KiB so a frame holding the
/// three `always` writers' pads fits it. The largest block
/// is then 57 344 B, and ADR-0178 D4 with α = 2 (the class's credit gains
/// at least share / 7 = 56 173 B/s), one 4 KiB header and five barrier
/// ops charged per checkpoint and a 2 ms step gives `T_ckpt` ≈ 1.17 s on
/// the byte axis and ≈ 0.66 s on the ops axis — under a quarter of the
/// ~7 s run, so the pads' checkpoints complete inside it.
fn device_budget_arm(
    seed: u64,
    cells: u16,
    section_target: u32,
    ops_per_writer: u64,
) -> (inf_runtime::DeviceModel, Option<CkptOverrunArm>) {
    let armed = seed % 4 == 1;
    let per_device: u64 = if armed { 768 << 10 } else { 128 << 10 };
    let model = inf_runtime::DeviceModel {
        write_bytes_per_s: per_device,
        write_ops_per_s: 4_000,
        read_bytes_per_s: per_device,
        read_ops_per_s: 4_000,
    };
    if !armed {
        return (model, None);
    }
    // ADR-0178 D2's cap and pool at the cell's share, from the table.
    let share = model.share(cells).write_bytes_per_s;
    let horizon = share * inf_runtime::BURST_HORIZON_NS / 1_000_000_000;
    let slice = inf_log::ckpt::ick_align_up(section_target as usize + 16) as u64;
    let cap = slice.max(horizon * 2 / 10);
    let arm = CkptOverrunArm {
        bytes_min: cap + horizon + 1,
        bytes_max: 40 << 10,
        before_command: ops_per_writer / 4,
    };
    (model, Some(arm))
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

    /// Every scenario's `spin_iters`: a few polls after work goes quiet,
    /// so a stepped cell reaps what its last iteration submitted.
    pub const SPIN_ITERS: u32 = 4;

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
            ckpt_overrun: None,
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
            spin_iters: Self::SPIN_ITERS,
            tier_budget: None,
            epoch_marks: None,
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
        // The device budget (ADR-0178 D2) at the `m2-device-budget`
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
    /// foreground bound; the m2 durability oracle runs unchanged. Seeds ≡
    /// 1 mod 4 run ADR-0178's arm ([`device_budget_arm`]): oversized
    /// checkpoint blocks, issued only by the budget's overrun, under the
    /// arm's engagement and block-wait oracles; the other seeds are its
    /// control leg.
    #[must_use]
    pub fn m2_device_budget(seed: u64) -> DurableScenario {
        let mut stall = m2_stall_config();
        stall.episode_gap_ns = u64::MAX;
        stall.episode_ms_min = 0;
        stall.episode_ms_max = 0;
        stall.write_bytes_per_s = 8 << 20;
        stall.read_bytes_per_s = 8 << 20;
        // 128 KiB/s per device off the arm: the checkpoint class's share
        // (2/10 of the per-cell 64 KiB/s) refills one ~140 KB checkpoint
        // every ~11 s against a run of ~7 sim-seconds — the second
        // checkpoint is deferred by construction (the engagement oracle's
        // regime), the first is granted from the boot-full credit.
        let cells: u16 = 2;
        let (section_target, ops_per_writer) = (8 << 10, 160);
        let (model, ckpt_overrun) = device_budget_arm(seed, cells, section_target, ops_per_writer);
        DurableScenario {
            seed,
            workload: DurableWorkload::KeyValue,
            cells,
            always_writers: 3,
            esec_think_ns_max: 0,
            always_think_ns_max: 0,
            esec_writers: 3,
            mem_writers: 1,
            ops_per_writer,
            keys_per_writer: 40,
            value_max: 512,
            value_pad_max: 0,
            step_ns_max: 2_000_000,
            double_cut: seed % 8 == 3,
            plant: Plant::None,
            // ADR-0178's arm: a frame of three maximal pads fits a segment.
            segment_bytes: if ckpt_overrun.is_some() { 256 << 10 } else { 64 << 10 },
            ckpt_interval_bytes: 24 << 10,
            ckpt_stream_bytes_per_sec: None,
            ckpt_section_bytes: Some(section_target),
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
            ckpt_overrun,
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
            spin_iters: Self::SPIN_ITERS,
            tier_budget: None,
            epoch_marks: None,
        }
    }

    /// `m4-tier-budget`: the budget scenario's durable shape with one or
    /// more tiered `always` namespaces on both cells, flushing under the
    /// leg's device model, or with none (the sim disk carries the
    /// reference bandwidth on its byte timeline either way, no stall
    /// episodes). The leg names the namespaces,
    /// their slice, their load and the model ([`TierBudgetLeg`]). Every
    /// value is inline (seal marks land per 1 MiB commit page, so a cell
    /// seals once its namespace's tail is a page past the mutable
    /// target). The m2 durability oracle and the reply oracle run
    /// unchanged; the tier class's own oracles — no offer above the cap,
    /// one pass's tier bytes within the cap and the pool, every ready
    /// group issued within the fair turn's bound, the spend equal to the
    /// disk's tier bytes and ops, one plan per cell turn — run on the
    /// per-cell scrape at the cut, and the run reports the tier class per
    /// cell. The budget scenario's generic oracles stay off: their
    /// engagement clause (some background class deferred) is vacuous on
    /// the legs whose class never waits for a refill.
    #[must_use]
    pub fn m4_tier_budget(seed: u64, leg: TierBudgetLeg) -> DurableScenario {
        let cells: u16 = 2;
        // The sim disk is the reference box (489 MB/s): the leg's model is
        // the budget's share of it, which on the hostile leg is a cell's
        // among 256 while the device itself stays the box's — the
        // binary's situation, where the budget's model is the share and the
        // device is the whole drive.
        let disk = inf_runtime::DeviceModel {
            write_bytes_per_s: 489_000_000,
            write_ops_per_s: 40_000,
            read_bytes_per_s: 489_000_000,
            read_ops_per_s: 40_000,
        };
        let model = leg.model();
        let mut stall = m2_stall_config();
        stall.episode_gap_ns = u64::MAX;
        stall.episode_ms_min = 0;
        stall.episode_ms_max = 0;
        stall.write_bytes_per_s = disk.write_bytes_per_s;
        stall.read_bytes_per_s = disk.read_bytes_per_s;
        if leg.is_coarse_cuts() {
            return DurableScenario::coarse_cuts(seed, leg, stall, model);
        }
        DurableScenario {
            cells,
            // 64 KiB segments hold a frame of several inline tiered
            // records: the generator's values are 1 KiB to 10 KiB, every
            // one inline under the 16 KiB threshold (no blob rides an
            // extent on this shape).
            segment_bytes: 64 << 10,
            stall: Some(stall),
            device: inf_server::DeviceConfig {
                model_share: model.share(cells),
                seal_barriers_per_s: 2_000 / u64::from(cells),
                provenance: Default::default(),
            },
            budget_oracle: false,
            ckpt_overrun: None,
            tier_budget: Some(leg.shape()),
            ..DurableScenario::m2_device_budget(seed)
        }
    }

    /// The coarse-cuts leg of `m4-tier-budget` (see
    /// [`TierBudgetLeg::CoarseCuts`]): one cell, so the model's share is
    /// the whole thousandth; a `Buffered` log of 256 MiB segments (a frame
    /// of at most 64 records, 0.94 MiB, fits one, so 18 rotations carry the
    /// fill, none zero-filled; the inherited `Direct` 64 KiB segments carry
    /// at most four records a frame and zero-fill each next segment at the
    /// class floor); a 5 ms step bound (2.5 ms mean, so the fill's
    /// ≈ 1.2 × 10⁴ steps take 24–36 sim-seconds, inside the second round's
    /// wait); no automatic checkpoint, so the log keeps the fill and no walk
    /// pins the table.
    fn coarse_cuts(
        seed: u64,
        leg: TierBudgetLeg,
        stall: StallConfig,
        model: inf_runtime::DeviceModel,
    ) -> DurableScenario {
        let cells: u16 = 1;
        DurableScenario {
            cells,
            segment_bytes: 256 << 20,
            io_mode: SegmentIoMode::Buffered,
            step_ns_max: 5_000_000,
            ckpt_interval_bytes: 0,
            stall: Some(stall),
            device: inf_server::DeviceConfig {
                model_share: model.share(cells),
                seal_barriers_per_s: 2_000,
                provenance: Default::default(),
            },
            budget_oracle: false,
            ckpt_overrun: None,
            tier_budget: Some(leg.shape()),
            ..DurableScenario::m2_device_budget(seed)
        }
    }

    /// `m4-epoch-marks`: the extent-reclaim marks' steel-thread row, the
    /// leg's regime ([`EpochMarksLeg`]). The everysec leg is the m2 shape
    /// on 1 MiB segments with the stall device's episodes off, six writers
    /// of 400 ops on six keys each; the wedge leg is `m2-fua-pending`'s
    /// device and pipeline (one cell, `Direct` segments, a 10 ms FLUSH-class
    /// barrier beside 40 µs write-through writes, fine steps) with its 48
    /// writers on the tiered namespace, on 256 KiB segments: on 64 KiB ones
    /// a rotation's wedge fills its next segment before 16 write-through
    /// frames complete behind it, so the write-through window and the marks
    /// never fill.
    #[must_use]
    pub fn m4_epoch_marks(seed: u64, leg: EpochMarksLeg) -> DurableScenario {
        match leg {
            EpochMarksLeg::Everysec => {
                let mut stall = m2_stall_config();
                stall.episode_gap_ns = u64::MAX;
                stall.episode_ms_min = 0;
                stall.episode_ms_max = 0;
                DurableScenario {
                    always_writers: 0,
                    esec_writers: 0,
                    mem_writers: 0,
                    ops_per_writer: 400,
                    keys_per_writer: 6,
                    segment_bytes: 1 << 20,
                    stall: Some(stall),
                    epoch_marks: Some(leg),
                    ..DurableScenario::m2_durable(seed)
                }
            }
            EpochMarksLeg::Wedge => {
                let base = DurableScenario::m2_fua_pending(seed);
                DurableScenario {
                    always_writers: 0,
                    segment_bytes: 256 << 10,
                    epoch_marks: Some(leg),
                    ..base
                }
            }
        }
    }

    /// The epoch-marks shape's writers on its namespace: six on the
    /// everysec leg, `m2-fua-pending`'s 48 on the wedge leg.
    pub(crate) fn epoch_marks_writers(&self) -> usize {
        match self.epoch_marks {
            Some(EpochMarksLeg::Everysec) => 6,
            Some(EpochMarksLeg::Wedge) => 48,
            None => 0,
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
            ckpt_overrun: None,
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
            spin_iters: Self::SPIN_ITERS,
            tier_budget: None,
            epoch_marks: None,
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
    pub state_hash: u64,
    pub violations: Vec<String>,
    /// Cap census rows read at the end of every life, summed over the
    /// cells and lives (node sum): the census oracle's liveness.
    pub cap_rows_checked: u64,
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
    /// ADR-0178's arm coverage: offers above a class cap (every class,
    /// every cell) and the worst checkpoint block wait in injected ns.
    pub budget_unattainable: u64,
    pub ckpt_block_wait_ns_max: u64,
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
    /// Preallocs that found the pool non-empty (neither a miss nor a
    /// space failure), both lives (ADR-0090 A16/A17): the precondition
    /// of the engagement rule and of the `recycle_open_fail` arm.
    pub recycle_served: u64,
    /// The `recycle_open_fail` arm was planted and no prealloc ever
    /// reached the pool (F-L19-20): disclosed, not a violation — the seed
    /// proves nothing about the fallback rule.
    pub recycle_open_unreached: bool,
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
    /// The tier-budget shape's per-cell tier-flush class at the cut
    /// (empty off the shape): the budget's own counters beside the sim
    /// disk's tier-file bytes and ops, and each namespace's flush.
    pub tier_budget_cells: Vec<TierBudgetCell>,
    /// The epoch-marks shape's reclaim-marks row per cell at the cut (empty
    /// off the shape).
    pub epoch_marks_cells: Vec<EpochMarksCell>,
    /// The coarse-cuts leg's cut sampler at the end of its traffic (`None`
    /// off the leg).
    pub coarse_cut: Option<CoarseCutScrape>,
}

/// The coarse-cuts leg's cut sampler as it stood when its traffic ended:
/// the most flush cuts the namespace's address space retained, the step
/// they first reached their cap and the highest of them then (the dense
/// run's end), the longest cut-less run read from `flushed`, the last
/// watermarks read, and whether `flushed` passed the dense run's end.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct CoarseCutScrape {
    pub retained_max: usize,
    pub capped_at_step: Option<u64>,
    pub dense_end: Option<u64>,
    pub cut_gap_bytes_max: u64,
    pub flushed: u64,
    pub ro_boundary: u64,
    pub reached: bool,
}

/// One cell's reclaim-marks census row at the cut of an epoch-marks run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EpochMarksCell {
    /// The cell the row was read on; a cell with no row has no entry.
    pub cell: u16,
    /// The `epoch-marks` row as the cell's census reads it at the cut.
    pub row: CapRow,
}

impl DurableReport {
    #[must_use]
    pub fn ok(&self) -> bool {
        !self.stalled && self.violations.is_empty()
    }
}

// ---- trace observer (no model replay — the ledger is the oracle) -------

#[derive(Clone, Default)]
pub(crate) struct TraceObserver(
    Rc<RefCell<Vec<u8>>>,
    crate::state::Recorder,
    Rc<RefCell<crate::census::CensusVerdict>>,
);

/// A run's observer once every node has been dropped: the state hash (the
/// determinism artifact) and the cap census verdict of every cell of every
/// life, which each node recorded as it was dropped.
pub(crate) struct Sealed {
    pub(crate) state_hash: u64,
    pub(crate) census: crate::census::CensusVerdict,
}

impl TraceObserver {
    /// The accumulated apply-event trace (the determinism artifact). Used
    /// by every scenario's `finish` (durable, combined).
    pub(crate) fn trace_bytes(&self) -> Vec<u8> {
        self.0.borrow().clone()
    }

    /// The state hash alone, for the observer's own tests; a run seals.
    #[cfg(test)]
    pub(crate) fn state_hash(&self) -> u64 {
        self.1.value()
    }

    /// Seals the run after every node has been dropped: its state hash and
    /// the census verdict of every life, which the run's report carries.
    pub(crate) fn seal(&self) -> Sealed {
        Sealed { state_hash: self.1.value(), census: self.2.take() }
    }

    pub(crate) fn power_cut(&self, disk: &SimDisk, now: Nanos, seed: u64) {
        self.1.update(|state| {
            state.number(b"cut-time", now.0);
            state.number(b"cut-seed", seed);
            state.disk(disk);
        });
        disk.power_cut(seed);
        self.1.update(|state| state.disk(disk));
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
        now: Nanos,
    ) {
        self.1.update(|state| state.number(b"apply-time", now.0));
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
    /// One of the tier-budget shape's tiered `always` namespaces, by its
    /// index below [`TIER_BUDGET_NS_MAX`]: several tiered namespaces on
    /// the same cells, flushing under the device model.
    TierBudget(u8),
    /// The epoch-marks shape's tiered namespace, in its leg's `FSYNC` class.
    EpochMarks(EpochMarksLeg),
}

/// The most tiered namespaces the tier-budget shape runs at once: the
/// eight-namespace leg of the tier-flush fairness oracle, whose names
/// are the fixed table below.
pub const TIER_BUDGET_NS_MAX: u8 = 8;

/// `INF.NS` names of the tier-budget namespaces, by index.
const TIER_BUDGET_NS: [&[u8]; TIER_BUDGET_NS_MAX as usize] =
    [b"tb0", b"tb1", b"tb2", b"tb3", b"tb4", b"tb5", b"tb6", b"tb7"];

impl NsClass {
    pub(crate) fn name(self) -> &'static [u8] {
        match self {
            NsClass::Always => b"alw",
            NsClass::Everysec => b"esec",
            NsClass::Tiered => b"tier",
            NsClass::Indexed => b"idx",
            NsClass::TierBudget(index) => TIER_BUDGET_NS[usize::from(index)],
            NsClass::EpochMarks(_) => b"em",
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
    /// `SET`s this writer built: the ADR-0178 arm pads ordinals ≡ 0 mod
    /// 16, so every armed `always` writer pads its first `SET`.
    pub(crate) sets: u64,
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
            sets: 0,
        }
    }

    /// The lift regime's tiered traffic (the `m4-tiered` value classes):
    /// inline SETs that demote through the ring, blob SETs that ride
    /// extents, overwrites and deletes of cold keys that stage
    /// displacement pairs — every reply exact.
    fn next_tiered_command(&mut self, scenario: &DurableScenario) -> (Vec<u8>, Pending) {
        if let (NsClass::TierBudget(index), Some(shape)) = (self.class, scenario.tier_budget)
            && let TierBudgetValues::SetOnlyUnique { value_bytes } = shape.load(index).values
        {
            return self.next_unique_set(value_bytes);
        }
        let keys = match (self.class, scenario.tier_budget) {
            (NsClass::TierBudget(index), Some(shape)) => shape.load(index).keys_per_writer,
            _ => scenario.keys_per_writer,
        };
        let key = self.key(keys);
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

    /// A `SET` of a key this writer never wrote (`k:<writer>:<sent>`) to a
    /// `value_bytes` value stamped with the writer and its count: the
    /// coarse-cuts leg's fill, every record live until the cut.
    fn next_unique_set(&mut self, value_bytes: usize) -> (Vec<u8>, Pending) {
        let key = format!("k:{}:{}", self.id, self.sent).into_bytes();
        let stamp = format!("u:{}:{}:", self.id, self.sent).into_bytes();
        let value: Vec<u8> = stamp.iter().copied().cycle().take(value_bytes).collect();
        let wire = encode(&[b"SET", &key, &value]);
        let pending = Pending {
            key,
            state_after: Some(value),
            expect: b"+OK\r\n".to_vec(),
            mutates: true,
            taints: false,
        };
        (wire, pending)
    }

    /// The epoch-marks wedge leg's traffic: the first write of a writer's
    /// first key is a blob of 4 KiB to 6 KiB (an extent), every other `SET`
    /// inline at 64 B to 1 KiB, and one later command in eight a `DEL`,
    /// every reply exact. One blob a writer, early: its next write kills its
    /// extent, and a blob's extent barrier, which holds every frame, holds
    /// few of the wedges; every frame stays small.
    fn next_inline_command(&mut self, scenario: &DurableScenario) -> (Vec<u8>, Pending) {
        let key = self.key(scenario.keys_per_writer);
        let first_write = !self.ledger.contains_key(&key) && key.ends_with(b":0");
        if !first_write && self.rng.next_below(8) == 0 {
            let existed = self.last_state(&key).is_some();
            let expect = if existed { b":1\r\n".to_vec() } else { b":0\r\n".to_vec() };
            let wire = encode(&[b"DEL", &key]);
            return (
                wire,
                Pending { key, state_after: None, expect, mutates: true, taints: false },
            );
        }
        let len = if first_write {
            (4 << 10) + self.rng.next_below(2049) as usize
        } else {
            64 + self.rng.next_below(961) as usize
        };
        let stamp = format!("w:{}:{}:", self.id, self.sent).into_bytes();
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
                "REPLY VIOLATION writer {} key {:?}: expected {:?}, got {:?}",
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
                NsClass::Everysec
                | NsClass::Memory
                | NsClass::Tiered
                | NsClass::Indexed
                | NsClass::TierBudget(_)
                | NsClass::EpochMarks(_) => {}
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
        if self.class == NsClass::EpochMarks(EpochMarksLeg::Wedge) {
            return self.next_inline_command(scenario);
        }
        if matches!(
            self.class,
            NsClass::Tiered | NsClass::TierBudget(_) | NsClass::EpochMarks(EpochMarksLeg::Everysec)
        ) {
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
            // ADR-0178's arm: a value whose checkpoint block only an
            // overrun can issue (`always` writers, the run's first quarter,
            // every 16th `SET` from the first — a fixed schedule, so no
            // armed seed goes without one).
            if let Some(arm) = scenario.ckpt_overrun
                && self.class == NsClass::Always
                && self.sent < arm.before_command
                && self.sets.is_multiple_of(CkptOverrunArm::ONE_IN_SETS)
            {
                let len = arm.bytes_min + self.rng.next_below(arm.bytes_max - arm.bytes_min + 1);
                value.resize(usize::try_from(len).expect("fits"), b'o');
            }
            self.sets += 1;
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
    observer: TraceObserver,
    clock: Rc<VirtualClock>,
    disk: SimDisk,
    observed_ready: bool,
    cells: Vec<(SimLoop, SimPlane)>,
    /// What each cell's reactor iteration reported in the last scheduler
    /// step, as `run_iteration` returned it: `None` for a cell that step
    /// did not run (frozen), and before the first step.
    #[allow(dead_code, reason = "read by the loop-tier tests, through `Node::iter_stats`")]
    iter_stats: Vec<Option<IterStats>>,
    pub(crate) nets: Vec<Rc<RefCell<CellNet>>>,
    pub(crate) control: std::sync::Arc<inf_server::ControlHandle>,
    inbox: ControlInbox,
    /// The control writer's checkpoint quota, held by this host after
    /// construction (ADR-0159 A1.2): the harness's direct requests.
    pub(crate) ckpt_host: inf_server::CkptQuota,
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

/// One cell's durable configuration for `scenario` (every cell of a node
/// gets the same one).
fn durable_config(
    scenario: &DurableScenario,
    data_dir: &std::path::Path,
) -> inf_server::DurableConfig {
    inf_server::DurableConfig {
        data_dir: data_dir.to_path_buf(),
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
    }
}

pub(crate) fn boot(
    scenario: &DurableScenario,
    data_dir: PathBuf,
    disk: &SimDisk,
    clock: &Rc<VirtualClock>,
    observer: &TraceObserver,
) -> std::io::Result<Node> {
    observer.1.update(|state| {
        state.number(b"boot", clock.now().0);
        state.disk(disk);
    });
    // F-L04-06: a driver-tier durable boot runs on a device whose plain
    // writes can land after a later-issued fsync (ADR-0087 D7) — the
    // instant device orders everything by submission and proves nothing
    // about the drain rule. `StallConfig::write_reorder()` is the floor.
    assert!(
        disk.write_reorder_armed(),
        "harness: the write-vs-fsync reorder window is closed on this disk (F-L04-06) — \
         arm StallConfig::write_reorder() or the m2 stall device"
    );
    // ADR-0159 A1.1: the DST host takes the checked cell count too.
    let cell_count = inf_foundation::CellCount::new(scenario.cells).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("--cells {e}"))
    })?;
    let catalog = load_catalog_from(disk, &data_dir)?;
    let (control, inbox, issuers) = inf_server::ControlHandle::detached_with_catalog(
        catalog.as_ref(),
        cell_count,
        // Virtual boot instant (ms): control-plane display only.
        clock.now().as_millis(),
    );
    let inf_server::CkptIssuers { host: ckpt_host, cells: cell_issuers } = issuers;
    let mut cell_issuers = cell_issuers.into_iter();
    let fabrics = Mesh::new(scenario.cells, MeshConfig { ring_capacity: 1024, data_credits: 256 });
    let mut nets = Vec::new();
    let mut cells = Vec::new();
    for (i, fabric) in fabrics.into_iter().enumerate() {
        let net = CellNet::new(i as u16, scenario.seed, scenario.plant);
        // The clock rides into the driver for the stall device (M2.5-S14);
        // with no stall model armed this is exactly `with_disk`.
        let driver = SimDriver::with_disk_stall(Rc::clone(&net), disk.clone(), Rc::clone(clock));
        let pool = BufferPool::new(128, 1024);
        let node_info = Rc::new(NodeInfo::try_default().expect("fixture cache allocation"));
        node_info.rng_state.set(scenario.seed ^ (0xA11D_0000 + i as u64));
        let mut ks =
            Keyspace::new(StoreConfig { hasher: node_hasher(scenario.seed), ..Default::default() });
        if let Some(catalog) = &catalog {
            ks.seed_catalog(catalog).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e:?}"))
            })?;
        }
        let (mut plane, serve_mark) = SimPlane::new(
            CellId(i as u16),
            scenario.cells,
            listener_fd(i as u16),
            ks,
            fabric,
            node_info,
            observer.clone(),
            false,
        );
        let cfg = durable_config(scenario, &data_dir);
        let issuer = cell_issuers.next().ok_or_else(|| {
            std::io::Error::other("the partition minted one checkpoint issuer per cell")
        })?;
        plane
            .set_control(std::sync::Arc::clone(&control), issuer)
            .map_err(|refused| std::io::Error::other(refused.to_string()))?;
        plane.begin_recovery(disk.clone(), &cfg, i as u16, clock.now());
        let config = LoopConfig { spin_iters: scenario.spin_iters, ..Default::default() };
        let cell_loop = CellLoop::new(driver, Rc::clone(clock), pool, config, serve_mark);
        nets.push(net);
        cells.push((cell_loop, plane));
    }
    Ok(Node {
        iter_stats: vec![None; cells.len()],
        cells,
        nets,
        control,
        inbox,
        ckpt_host,
        data_dir,
        hold_inbox: false,
        frozen: None,
        observer: observer.clone(),
        clock: Rc::clone(clock),
        disk: disk.clone(),
        observed_ready: false,
    })
}

impl Drop for Node {
    /// The end of a life: its state observed, and every cell's cap census
    /// judged after the cell's last iteration (the cap bound, the assembly
    /// rule and their liveness).
    fn drop(&mut self) {
        self.observe_state(b"node-end");
        let mut verdict = crate::census::CensusVerdict::default();
        for (index, (_, plane)) in self.cells.iter().enumerate() {
            // A cell cut before its recovery completed has replayed its
            // tiered namespaces into the keyspace but not yet built their
            // plane half, which registers the marks' row, nor its durable
            // plane, which registers the window's row: neither is owed.
            let durable = plane.durable_stats();
            let tiered_tables =
                if durable.is_some() { plane.keyspace().tiered_tables() } else { 0 };
            let turn_refused = durable.map(|stats| stats.turn_refused);
            let facts = crate::census::CellFacts { tiered_tables, turn_refused };
            verdict.judge(index, plane.cap_census(), facts);
        }
        self.observer.2.borrow_mut().absorb(verdict);
    }
}

impl Node {
    fn observe_state(&self, tag: &[u8]) {
        self.observer.1.update(|state| {
            state.number(tag, self.clock.now().0);
            state.disk(&self.disk);
            for (index, (_, plane)) in self.cells.iter().enumerate() {
                state.number(b"cell", index as u64);
                state.keyspace(&plane.keyspace(), self.clock.now());
            }
        });
    }

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
        self.observer.1.update(|state| {
            state.number(b"step", clock.now().0);
            state.number(b"rotate", rotate as u64);
        });
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
                self.iter_stats[idx] = None;
                continue;
            }
            let (cell_loop, plane) = &mut self.cells[idx];
            self.iter_stats[idx] = Some(cell_loop.run_iteration(plane).expect("sim iteration"));
            if let Some(err) = plane.take_boot_error() {
                return Err(err);
            }
        }
        if !self.hold_inbox {
            self.inbox.drain(disk, &self.data_dir)?;
        }
        clock.advance(Nanos(1_000 + rng.next_u64() % step_ns_max));
        if !self.observed_ready && self.ready() {
            self.observe_state(b"recovered");
            self.observed_ready = true;
        }
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

    /// What `cell`'s reactor iteration reported in the last scheduler step,
    /// `None` when that step did not run the cell (frozen): a skipped cell
    /// has no iteration to read. `parked` is the reactor's own decision,
    /// made before the driver call: the sim driver ignores the wait, so a
    /// park shows as this flag and never as elapsed virtual time. Read
    /// between scheduler steps.
    #[cfg(test)]
    pub(crate) fn iter_stats(&self, cell: usize) -> Option<IterStats> {
        self.iter_stats[cell]
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
            NsClass::Always
            | NsClass::Tiered
            | NsClass::Indexed
            | NsClass::TierBudget(_)
            | NsClass::EpochMarks(EpochMarksLeg::Wedge) => true,
            // A clean stop (ADR-0124 D4) keeps every acked everysec op;
            // the window is a crash property only.
            NsClass::Everysec | NsClass::EpochMarks(EpochMarksLeg::Everysec) => {
                clean_stop || at + EVERYSEC_WINDOW <= cut_time
            }
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
