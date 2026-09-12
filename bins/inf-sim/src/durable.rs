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
use inf_log::{ReaderConfig, SegmentId, SegmentReader, read_manifest, scan_log_dir_from};
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
pub(crate) fn build_disk(seed: u64, stall: Option<&StallConfig>) -> SimDisk {
    match stall {
        Some(cfg) => SimDisk::with_stall(SimDiskConfig::default(), cfg.clone(), seed ^ 0x57A1_1ED0),
        None => SimDisk::new(),
    }
}

impl DurableScenario {
    #[must_use]
    /// ADR-0117 D2's generator widening: one seed in four walks every
    /// checkpoint under a 1 KiB section bound, so the in-chain resume
    /// runs at nearly every image under the scenario's churn and cuts.
    pub fn section_bound_for(seed: u64) -> Option<u32> {
        (seed % 4 == 1).then_some(1 << 10)
    }

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
            ckpt_section_bytes: None,
            ckpt_section_bound: Self::section_bound_for(seed),
            stall: Some(m2_stall_config()),
            replay_canary: false,
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
            esec_writers: 3,
            mem_writers: 1,
            ops_per_writer: 160,
            keys_per_writer: 40,
            value_max: 512,
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
            ckpt_section_bytes: None,
            ckpt_section_bound: None,
            stall: Some(m2_stall_config()),
            replay_canary: false,
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
    /// M4.5-S39a: fill-policy hold episodes, scraped at the cut — the
    /// manifest's coverage disclosure and the reorder scenario's oracle.
    pub frame_waits_fill: u64,
    /// M4.5-S43: group-hold episodes, scraped at the cut (coverage).
    pub frame_waits_group: u64,
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
    pub segment_rotations: u64,
    pub recycled_residue_slacks: u64,
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
pub(crate) fn required_index(class: NsClass, ops: &[OpRec], cut_time: Nanos) -> Option<usize> {
    ops.iter().rposition(|op| match op.acked_at {
        None => false,
        Some(at) => match class {
            NsClass::Always | NsClass::Tiered | NsClass::Indexed => true,
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
    if let Some(allowed) = scenario.ckpt_direct_refused_after {
        disk.refuse_direct_writes_after(allowed);
    }
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
        frames_in_flight_max: 0,
        frame_waits_barrier: 0,
        frame_waits_rotation: 0,
        frame_waits_reorder: 0,
        ckpt_downgrades: 0,
        frame_waits_fill: 0,
        frame_waits_group: 0,
        budget_background_bytes: 0,
        budget_deferrals: 0,
        frame_waits_pace: 0,
        write_stall_max_us: 0,
        reopened_packed_tails: 0,
        segments_recycled: 0,
        recycle_misses: 0,
        recycle_fallbacks: 0,
        segment_rotations: 0,
        recycled_residue_slacks: 0,
        recycle_waits_started: 0,
        recycle_waits_satisfied: 0,
        recycle_waits_expired: 0,
        segment_inline_preallocs: 0,
        lift_regime: scenario.lift_regime,
        lift_tiered_ops: 0,
        lift_indexed_ops: 0,
        lift_sidecars_loaded: 0,
        stale_residue_slacks: 0,
        lift_plants: 0,
        lift_plant_lifts: 0,
        lift_plant_sidecars: 0,
    };
    let fail = |report: &mut DurableReport, what: String| {
        report.violations.push(what);
    };

    // ---- boot 1 + DDL ------------------------------------------------
    // With a prelude the first life runs in the prelude's barrier class
    // (ADR-0086 D4 as amended); the scenario's own class boots at the
    // clean restart below.
    let first_life = match scenario.prelude {
        Some(prelude) => {
            let mut first = scenario.clone();
            first.io_mode = prelude.io_mode;
            first
        }
        None => scenario.clone(),
    };
    let mut node = match boot(&first_life, PathBuf::from("node"), &disk, &clock, &observer) {
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

    let mut lift_ns = None;
    if scenario.lift_regime {
        match lift_regime_ddl(&mut node, &mut setup, &mut rng, &clock, &disk, scenario, &mut report)
        {
            Ok(ns) => lift_ns = Some(ns),
            Err(what) => {
                fail(&mut report, what);
                return finish(report, &observer, &clock);
            }
        }
    }
    // The residue plant's precondition (batch 21): the index must be live
    // in the prelude life and a checkpoint must carry its sidecar before
    // the prelude cut — a clean restart seeds the declaration (the live
    // DDL fan is S10's), then seed documents and a forced checkpoint.
    if lift_ns.is_some() && scenario.prelude.is_some() {
        drop(setup);
        node = match lift_regime_seed_life(node, &first_life, &disk, &clock, &observer) {
            Ok(node) => node,
            Err(what) => {
                fail(&mut report, what);
                return finish(report, &observer, &clock);
            }
        };
        if let Err(what) =
            lift_regime_seed_checkpoint(&mut node, &mut rng, &clock, &disk, scenario, &mut report)
        {
            fail(&mut report, what);
            return finish(report, &observer, &clock);
        }
    }

    if scenario.recycle_open_fault {
        // The sim node runs every cell on this thread (the registry is
        // thread-local): the first pool reuse in any cell fires.
        inf_foundation::fault::arm(inf_log::fault::RECYCLE_OPEN_FAIL, FaultSpec::Nth(1));
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
                scenario.prelude.map_or(scenario.ops_per_writer, |p| p.ops_per_writer),
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

    // ---- the transition prelude (ADR-0086 D4 as amended) ---------------
    // One life in the other barrier class, cut **dirty** at a seeded step
    // inside its traffic window — the shape that reopens a data-bearing
    // tail: a torn tail truncates at the last valid frame's end (a v2
    // frame's unaligned end under FLUSH), MAINTAIN's empty next segment is
    // removed with the residue, and the next life's rotor resumes *there*.
    // (A clean quiesce leaves the empty next segment as the tail and the
    // transition never touches packed data.) The prelude's ledgers are
    // audited against this cut at the transition boot, then rebased to
    // the recovered state, so the final audit binds the main life's cut
    // with the recovered prefix required.
    if scenario.prelude.is_some() {
        assert_eq!(scenario.workload, DurableWorkload::KeyValue, "prelude is a KV-only shape");
        let prelude_ops: u64 = writers.iter().map(|w| w.quota).sum();
        let prelude_cut = 100 + rng.next_below(prelude_ops * 6);
        for _ in 0..prelude_cut {
            report.scheduler_steps += 1;
            if let Err(err) = node.step(&mut rng, &clock, &disk, scenario.step_ns_max) {
                fail(&mut report, format!("prelude phase: {err}"));
                return finish(report, &observer, &clock);
            }
            for writer in &mut writers {
                let mut net = node.nets[writer.cell].borrow_mut();
                let bytes = net.client_recv(writer.fd);
                writer.rx.extend_from_slice(&bytes);
                while let Some(n) = reply_len(&writer.rx) {
                    let reply: Vec<u8> = writer.rx.drain(..n).collect();
                    writer.absorb_reply(reply, clock.now(), scenario.seed, &mut report);
                }
                if writer.setup || writer.inflight.is_some() || writer.sent >= writer.quota {
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
            }
        }
        let prelude_cut_time = clock.now();
        drop(node);
        disk.power_cut(scenario.seed ^ 0x0FF5_EED2);
        // The residue plant (batch 21): the lift shape on every cell,
        // between the cut and the boot that must lift past it.
        let mut planted = Vec::new();
        if let Some(ns) = lift_ns {
            match crate::lift::plant_lifted_tail(
                &disk,
                Path::new("node"),
                scenario.cells,
                u64::from(scenario.segment_bytes),
                ns,
            ) {
                Ok((cells, skipped)) => {
                    for what in skipped {
                        eprintln!("lift plant skipped: {what}");
                    }
                    report.lift_plants += cells.len() as u64;
                    planted = cells;
                }
                Err(what) => {
                    fail(&mut report, format!("lift plant: {what}"));
                    return finish(report, &observer, &clock);
                }
            }
        }
        node = match boot(scenario, PathBuf::from("node"), &disk, &clock, &observer) {
            Ok(node) => node,
            Err(err) => {
                fail(&mut report, format!("transition boot refused: {err}"));
                return finish(report, &observer, &clock);
            }
        };
        let mut steps = 0u64;
        while !node.ready() {
            steps += 1;
            report.scheduler_steps += 1;
            if let Err(err) = node.step(&mut rng, &clock, &disk, scenario.step_ns_max) {
                // A taxonomy refusal at the transition boot is a finding
                // here, not a legal outcome: the transition must never
                // turn a recoverable image into a refused one.
                fail(&mut report, format!("transition boot failed: {err}"));
                return finish(report, &observer, &clock);
            }
            if steps > STALL_STEPS {
                report.stalled = true;
                fail(&mut report, "recovery stalled on the transition boot".to_owned());
                return finish(report, &observer, &clock);
            }
        }
        // The transition's observable: packed tails reopened packed
        // (FLUSH → FUA); FUA → FLUSH reopens at an aligned v3 end.
        for cell in 0..usize::from(scenario.cells) {
            if let Some(stats) = node.plane(cell).durable_stats() {
                report.reopened_packed_tails += stats.reopened_packed_tails;
            }
        }
        // The plant's oracle: every planted cell lifted, its sidecar
        // loaded, the tree equals the truth (the planted document
        // included), the lifted records serve, the residue never does.
        if !planted.is_empty() {
            lift_plant_oracle(&mut node, &planted, &mut rng, &clock, &disk, scenario, &mut report);
        }
        // Every writer's in-flight op is now unacked forever (the cut ate
        // the reply path): the ledger already holds it with `acked_at:
        // None`, which is exactly what the audit's admissible set expects.
        for writer in &mut writers {
            writer.inflight = None;
        }
        if audit_ledgers(
            &mut node,
            &mut writers,
            &mut rng,
            &clock,
            &disk,
            scenario,
            prelude_cut_time,
            Some(clock.now()),
            &mut report,
        )
        .is_err()
        {
            return finish(report, &observer, &clock);
        }
        for writer in &mut writers {
            writer.reconnect(&mut node, scenario.ops_per_writer);
        }
        // The lift regime's writers join the main life only: their records
        // are exactly what the final boot lifts past the prelude's residue.
        if scenario.lift_regime {
            for class in [NsClass::Tiered, NsClass::Tiered, NsClass::Indexed, NsClass::Indexed] {
                let cell = (rng.next_u64() % u64::from(scenario.cells)) as usize;
                let fd = node.nets[cell].borrow_mut().connect();
                let writer = Writer::new(
                    id,
                    cell,
                    fd,
                    class,
                    scenario.seed,
                    scenario.ops_per_writer,
                    true,
                    0,
                );
                node.nets[cell]
                    .borrow_mut()
                    .client_send(fd, &encode(&[b"INF.NS", b"USE", class.name()]));
                writers.push(writer);
                id += 1;
            }
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
                writer.absorb_reply(reply, clock.now(), scenario.seed, &mut report);
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
        // The log must be quiescent too (ADR-0087 D7): an `everysec` ack
        // precedes its frame landing, and under the stall model plain
        // writes land later — the shadow replay reads the file, so every
        // sealed frame must have its `LogWritten` and nothing may sit
        // staged behind a bounded wait.
        let log_quiet = (0..usize::from(scenario.cells)).all(|cell| {
            node.plane(cell)
                .durable_stats()
                .is_none_or(|s| s.frames_in_flight_now == 0 && s.records_staged == 0)
        });
        if quiesce && log_quiet && writers.iter().all(|w| !w.setup && w.inflight.is_none()) {
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
    for cell in 0..usize::from(scenario.cells) {
        if let Some(stats) = node.plane(cell).durable_stats() {
            report.frames_in_flight_max =
                report.frames_in_flight_max.max(stats.frames_in_flight_max);
            report.frame_waits_barrier += stats.frame_waits_barrier;
            report.frame_waits_rotation += stats.frame_waits_rotation;
            report.frame_waits_reorder += stats.frame_waits_reorder;
            report.frame_waits_fill += stats.frame_waits_fill;
            report.frame_waits_group += stats.frame_waits_group;
            report.frame_waits_pace += stats.frame_waits_pace;
            report.write_stall_max_us = report.write_stall_max_us.max(stats.write_stall_max_us);
            report.segments_recycled += stats.segments_recycled;
            report.recycle_misses += stats.recycle_misses;
            report.recycle_fallbacks += stats.recycle_fallbacks;
            report.segment_rotations += stats.segment_rotations;
            report.recycle_waits_started += stats.recycle_waits_started;
            report.recycle_waits_satisfied += stats.recycle_waits_satisfied;
            report.recycle_waits_expired += stats.recycle_waits_expired;
            report.segment_inline_preallocs += stats.segment_inline_preallocs;
            // ADR-0090 A8: every wait ends exactly once, and a wait never
            // strands a rotation without a next segment.
            if stats.recycle_waits_started
                < stats.recycle_waits_satisfied + stats.recycle_waits_expired
            {
                fail(
                    &mut report,
                    format!(
                        "POOL-WAIT ACCOUNTING VIOLATION seed {:#x} cell {cell}: started {} < \
                         satisfied {} + expired {}",
                        scenario.seed,
                        stats.recycle_waits_started,
                        stats.recycle_waits_satisfied,
                        stats.recycle_waits_expired
                    ),
                );
            }
            if scenario.recycle_oracle
                && scenario.prealloc != inf_server::PreallocPolicy::Immediate
                && stats.segment_inline_preallocs > 0
            {
                fail(
                    &mut report,
                    format!(
                        "POOL WAIT STRANDED A ROTATION seed {:#x} cell {cell}: {} inline \
                         preallocs under {:?}",
                        scenario.seed, stats.segment_inline_preallocs, scenario.prealloc
                    ),
                );
            }
            // ADR-0090 D5, the recycle oracle (per cell, at the cut).
            if scenario.recycle_oracle
                && scenario.recycle_slots > 0
                && scenario.io_mode == SegmentIoMode::Direct
            {
                let truncated = stats.segments_truncated;
                if stats.segment_rotations >= 3 && truncated >= 2 && stats.segments_recycled == 0 {
                    fail(
                        &mut report,
                        format!(
                            "RECYCLING NEVER ENGAGED seed {:#x} cell {cell}: {} rotations, {} \
                             truncations, 0 recycled ({} misses, {} fallbacks)",
                            scenario.seed,
                            stats.segment_rotations,
                            truncated,
                            stats.recycle_misses,
                            stats.recycle_fallbacks
                        ),
                    );
                }
                let unserved = stats.segment_preallocs.saturating_sub(stats.segments_recycled);
                let bound = unserved * u64::from(scenario.segment_bytes);
                if stats.zero_fill_bytes > bound {
                    fail(
                        &mut report,
                        format!(
                            "ZERO-FILL ACCOUNTING VIOLATION seed {:#x} cell {cell}: zero_fill_bytes \
                             {} > (preallocs {} − recycled {}) × segment_bytes {}",
                            scenario.seed,
                            stats.zero_fill_bytes,
                            stats.segment_preallocs,
                            stats.segments_recycled,
                            scenario.segment_bytes
                        ),
                    );
                }
            }
            for class in inf_runtime::IoClass::ALL {
                let c = stats.io_budget[class.index()];
                if !class.is_foreground() {
                    report.budget_background_bytes += c.spent_bytes;
                }
                report.budget_deferrals += c.deferrals;
            }
        }
    }
    if scenario.budget_oracle {
        budget_oracles(scenario, &node, clock.now(), &mut report);
    }
    if scenario.ckpt_direct_refused_after.is_some() {
        for cell in 0..usize::from(scenario.cells) {
            let (downgrades, frame_bytes) = node
                .plane(cell)
                .durable_stats()
                .map_or((0, 0), |s| (s.ckpt_io_mode_downgrades, s.log_frame_bytes));
            let (completed, aborted) = node.plane(cell).ckpt_stats_for_sim();
            report.ckpt_downgrades += downgrades;
            // A cell cut before four intervals of frames may not have
            // reached its retry's publish (one refused attempt, the
            // immediate retry, a few slices of walk): not a verdict.
            // The manifest discloses how many cells exercised the
            // downgrade (`ckpt_downgrades`), so a sweep that never did
            // is visible.
            if frame_bytes < 4 * scenario.ckpt_interval_bytes {
                continue;
            }
            if downgrades != 1 || completed == 0 {
                fail(
                    &mut report,
                    format!(
                        "CHECKPOINT LIVENESS VIOLATION seed {:#x} cell {cell}: direct writes \
                         refused after the probe — downgrades {downgrades} (want 1), \
                         checkpoints completed {completed} (want ≥ 1), aborted {aborted}, \
                         log frame bytes {frame_bytes}",
                        scenario.seed
                    ),
                );
            }
        }
    }
    // M4.5-S39a: on the aligned class every frame of the everysec-only
    // reorder shape is barrier-less and far below the target — the
    // policy must have held at least once, or the arm measured nothing.
    if scenario.reorder_oracle
        && scenario.fill.enabled()
        && scenario.io_mode == SegmentIoMode::Direct
        && report.frame_waits_fill == 0
    {
        fail(
            &mut report,
            format!(
                "FILL POLICY NOT ENGAGED seed {:#x}: the aligned everysec-only shape never held \
                 a frame",
                scenario.seed
            ),
        );
    }
    if scenario.reorder_oracle && report.frame_waits_reorder == 0 {
        let depth = report.frames_in_flight_max;
        fail(
            &mut report,
            format!(
                "REORDER WINDOW NOT ENGAGED seed {:#x}: the wedged device never filled the \
                 completion ledger's window (frames_in_flight_max {depth}) — the scenario \
                 proves nothing about the bound",
                scenario.seed
            ),
        );
    }
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
                // ADR-0090 D5: on the recycling scenario a refusal *is*
                // the refusal failure mode the residue rule exists to
                // close (residue taken for a seq gap or a hole) — a
                // finding, not a legal outcome. The planted-bug canary
                // (`--cfg inf_canary_foreign_segment`) must turn this red.
                if scenario.recycle_oracle {
                    fail(
                        &mut report,
                        format!(
                            "RECYCLED RESIDUE REFUSED seed {:#x}: honest power-cut image of a \
                             recycling log refused boot: {err}",
                            scenario.seed
                        ),
                    );
                    return finish(report, &observer, &clock);
                }
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
    // ADR-0090 D4: what the reboot proved about recycled residue — the
    // sweep's coverage disclosure (a sweep whose reboots never met
    // residue never exercised the rule).
    for cell in 0..usize::from(scenario.cells) {
        let residue = node.control.recovery_board().slot(cell as u16).residue();
        report.recycled_residue_slacks += residue.recycled_residue_slacks;
        report.stale_residue_slacks += residue.stale_residue_slacks;
    }
    if scenario.lift_regime {
        for writer in &writers {
            let acked =
                writer.ledger.values().flatten().filter(|op| op.acked_at.is_some()).count() as u64;
            match writer.class {
                NsClass::Tiered => report.lift_tiered_ops += acked,
                NsClass::Indexed => report.lift_indexed_ops += acked,
                _ => {}
            }
        }
        for cell in 0..usize::from(scenario.cells) {
            report.lift_sidecars_loaded +=
                u64::from(node.plane(cell).keyspace().idx_sidecar_info().loaded);
        }
        lift_regime_index_oracle(
            &mut node,
            &mut rng,
            &clock,
            &disk,
            scenario,
            "after the final boot",
            &mut report,
        );
    }

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
    if audit_ledgers(
        &mut node,
        &mut writers,
        &mut rng,
        &clock,
        &disk,
        scenario,
        cut_time,
        None,
        &mut report,
    )
    .is_err()
    {
        return finish(report, &observer, &clock);
    }
    if scenario.recycle_open_fault {
        let fired = inf_foundation::fault::fired(inf_log::fault::RECYCLE_OPEN_FAIL);
        inf_foundation::fault::disarm(inf_log::fault::RECYCLE_OPEN_FAIL);
        if fired == 0 {
            fail(
                &mut report,
                format!(
                    "RECYCLE-OPEN FAULT VACUOUS seed {:#x}: no pooled file was reused",
                    scenario.seed
                ),
            );
        } else if report.recycle_fallbacks == 0 {
            fail(
                &mut report,
                format!(
                    "RECYCLE-OPEN FALLBACK MISSING seed {:#x}: the point fired {fired}× and no \
                     generation fell back fresh",
                    scenario.seed
                ),
            );
        }
    }

    finish(report, &observer, &clock)
}

/// The §8.2 audit of every durable ledger against the recovered node:
/// per key, the required op (the last one acked inside the class's
/// promise before `cut_time`) and the admissible states. With
/// `rebase = Some(boot)` (the transition prelude, ADR-0086 D4 as
/// amended) each audited ledger is then replaced by one synthetic op
/// holding the **recovered** state, acked at `Nanos::ZERO` — recovered
/// state came off the device, so it is required at the next cut
/// regardless of the loss window; the main life's ops append behind it.
/// `Err` = a transport failure already recorded in the report.
#[allow(clippy::too_many_arguments)] // the scheduler tuple the MiniClient calls need
fn audit_ledgers(
    node: &mut Node,
    writers: &mut [Writer],
    rng: &mut SplitMix64,
    clock: &Rc<VirtualClock>,
    disk: &SimDisk,
    scenario: &DurableScenario,
    cut_time: Nanos,
    rebase: Option<Nanos>,
    report: &mut DurableReport,
) -> Result<(), ()> {
    let mut audit = MiniClient::connect(node, 0);
    for class in [NsClass::Always, NsClass::Everysec, NsClass::Tiered, NsClass::Indexed] {
        if !writers.iter().any(|w| w.class == class) {
            continue;
        }
        let reply = audit.call(
            node,
            rng,
            clock,
            disk,
            scenario.step_ns_max,
            &[b"INF.NS", b"USE", class.name()],
        );
        if !matches!(reply, Ok(Some(ref ok)) if ok == b"+OK\r\n") {
            report.violations.push(format!("audit USE {class:?} answered {reply:?}"));
            return Err(());
        }
        for writer in writers.iter_mut().filter(|w| w.class == class) {
            for (key, ops) in &mut writer.ledger {
                report.audited_keys += 1;
                let required = required_index(class, ops, cut_time);
                report.required_ops += required.map_or(0, |i| i as u64 + 1);
                report.allowed_lost_ops += ops.len() as u64 - required.map_or(0, |i| i as u64 + 1);
                let document =
                    scenario.workload == DurableWorkload::Document || class == NsClass::Indexed;
                let command: [&[u8]; 2] = if document { [b"JSON.GET", key] } else { [b"GET", key] };
                let reply = match audit.call(node, rng, clock, disk, scenario.step_ns_max, &command)
                {
                    Ok(Some(reply)) => reply,
                    other => {
                        report.violations.push(format!("audit GET {key:?} answered {other:?}"));
                        return Err(());
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
                if let Some(boot) = rebase {
                    debug_assert_eq!(scenario.workload, DurableWorkload::KeyValue);
                    let recovered = parse_bulk(&reply).unwrap_or_else(|| {
                        panic!("audit GET {key:?} answered a non-bulk reply {reply:?}")
                    });
                    *ops = vec![OpRec {
                        state_after: recovered,
                        sent_at: boot,
                        acked_at: Some(Nanos::ZERO),
                    }];
                }
            }
        }
    }
    Ok(())
}

/// The lift regime's index (F-L14-01): `$.meta.tag`, the document
/// model's always-present integer member.
const LIFT_INDEX: (u32, &str, IndexKeyType) = (1, "$.meta.tag", IndexKeyType::I64);

/// The lift regime's DDL: a tiered `always` namespace (the `m4-tiered`
/// budget shape — demotion, extents and displacement pairs inside a
/// short run) and an indexed `always` document namespace whose index is
/// declared through the production catalog swap and converged before
/// any traffic.
#[allow(clippy::too_many_arguments)] // the scheduler tuple the MiniClient calls need
fn lift_regime_ddl(
    node: &mut Node,
    setup: &mut MiniClient,
    rng: &mut SplitMix64,
    clock: &Rc<VirtualClock>,
    disk: &SimDisk,
    scenario: &DurableScenario,
    report: &mut DurableReport,
) -> Result<LiftNs, String> {
    let tier: &[&[u8]] = &[
        b"INF.NS",
        b"CREATE",
        NsClass::Tiered.name(),
        b"MODE",
        b"durable",
        b"FSYNC",
        b"always",
        b"MEM-BUDGET",
        b"3mb",
        b"MUTABLE-FRACTION",
        b"100",
        b"MAINTAIN-SLICE",
        b"1mb",
        b"BLOB-THRESHOLD",
        b"4kb",
        b"TIER-IO-MODE",
        b"buffered",
    ];
    let idx: &[&[u8]] =
        &[b"INF.NS", b"CREATE", NsClass::Indexed.name(), b"MODE", b"durable", b"FSYNC", b"always"];
    for create in [tier, idx] {
        match setup.call(node, rng, clock, disk, scenario.step_ns_max, create) {
            Ok(Some(ok)) if ok == b"+OK\r\n" => {}
            other => return Err(format!("lift-regime DDL {:?} answered {other:?}", create[2])),
        }
    }
    let ns_named =
        |name: &[u8]| node.plane(0).keyspace().ns_iter().find(|s| s.name == name).map(|s| s.id);
    let ns = ns_named(NsClass::Indexed.name())
        .ok_or_else(|| "lift-regime: idx namespace missing after DDL".to_owned())?;
    let tier_ns = ns_named(NsClass::Tiered.name())
        .ok_or_else(|| "lift-regime: tier namespace missing after DDL".to_owned())?;
    let (id, path, key_type) = LIFT_INDEX;
    let program = compile(path.as_bytes()).expect("valid index path").as_bytes().to_vec();
    let mut catalog = node.plane(0).keyspace().export_catalog(node.control.next_ns_id(), 2, 2);
    catalog.index.entries.push(IndexSpec {
        id: IndexId(id),
        generation: u64::from(id),
        ns,
        name: b"by-tag".to_vec(),
        program,
        key_type,
        state: IndexState::Declared,
    });
    node.control.request_persist(catalog);
    // The swap lands on disk here; the live registries see the
    // declaration at the transition boot (boot-seeding — the live DDL fan
    // is S10's), and the S05 machine converges during the main life.
    for _ in 0..64 {
        node.step(rng, clock, disk, scenario.step_ns_max)
            .map_err(|e| format!("lift-regime persist: {e}"))?;
        report.scheduler_steps += 1;
    }
    Ok(LiftNs { tier: tier_ns, idx: ns })
}

/// The plant's first precondition (batch 21): a clean restart in the
/// prelude's class so the persisted index declaration boot-seeds into
/// every cell's registry (no cut — the page cache is the OS's; a process
/// restart, not a crash). The log quiesces first so nothing staged rides
/// the drop.
fn lift_regime_seed_life(
    node: Node,
    first_life: &DurableScenario,
    disk: &SimDisk,
    clock: &Rc<VirtualClock>,
    observer: &TraceObserver,
) -> Result<Node, String> {
    let mut node = node;
    let mut rng = SplitMix64::new(first_life.seed ^ 0x5EED_11F7);
    let mut quiet_steps = 0u64;
    for _ in 0..STALL_STEPS {
        node.step(&mut rng, clock, disk, first_life.step_ns_max)
            .map_err(|e| format!("lift seed life: quiesce: {e}"))?;
        let quiet = (0..usize::from(first_life.cells)).all(|cell| {
            node.plane(cell)
                .durable_stats()
                .is_none_or(|s| s.frames_in_flight_now == 0 && s.records_staged == 0)
        });
        quiet_steps = if quiet { quiet_steps + 1 } else { 0 };
        if quiet_steps >= 64 {
            break;
        }
    }
    if quiet_steps < 64 {
        return Err("lift seed life: the log never quiesced before the restart".to_owned());
    }
    drop(node);
    let mut node = boot(first_life, PathBuf::from("node"), disk, clock, observer)
        .map_err(|e| format!("lift seed boot refused: {e}"))?;
    for _ in 0..STALL_STEPS {
        if node.ready() {
            return Ok(node);
        }
        node.step(&mut rng, clock, disk, first_life.step_ns_max)
            .map_err(|e| format!("lift seed boot failed: {e}"))?;
    }
    Err("lift seed boot: recovery stalled".to_owned())
}

/// The plant's second precondition: the boot-seeded index converges,
/// every cell owns a few indexed seed documents, and a checkpoint
/// requested on every cell publishes — so the transition boot loads a
/// sidecar with entries, the state the commit-ordering defect needs.
#[allow(clippy::too_many_arguments)] // the scheduler tuple the MiniClient calls need
fn lift_regime_seed_checkpoint(
    node: &mut Node,
    rng: &mut SplitMix64,
    clock: &Rc<VirtualClock>,
    disk: &SimDisk,
    scenario: &DurableScenario,
    report: &mut DurableReport,
) -> Result<(), String> {
    let (id, _, _) = LIFT_INDEX;
    let id = IndexId(id);
    let mut ready = false;
    for _ in 0..STALL_STEPS {
        ready = (0..usize::from(scenario.cells)).all(|cell| {
            node.plane(cell).keyspace().idx_registry().cell_state(id) == Some(IndexState::Ready)
        });
        if ready {
            break;
        }
        node.step(rng, clock, disk, scenario.step_ns_max)
            .map_err(|e| format!("lift seed: index convergence: {e}"))?;
        report.scheduler_steps += 1;
    }
    if !ready {
        return Err("lift seed: the boot-seeded index never converged".to_owned());
    }
    for cell in 0..usize::from(scenario.cells) {
        let mut client = MiniClient::connect(node, cell);
        let reply = client.call(
            node,
            rng,
            clock,
            disk,
            scenario.step_ns_max,
            &[b"INF.NS", b"USE", NsClass::Indexed.name()],
        );
        if !matches!(reply, Ok(Some(ref ok)) if ok == b"+OK\r\n") {
            return Err(format!("lift seed: USE idx on cell {cell} answered {reply:?}"));
        }
        for n in 0..3usize {
            let key = crate::lift::local_key(&format!("lift:seed{n}"), cell, scenario.cells);
            let text = crate::lift::planted_doc_text(i64::try_from(cell * 8 + n).expect("small"));
            let reply = client.call(
                node,
                rng,
                clock,
                disk,
                scenario.step_ns_max,
                &[b"JSON.SET", &key, b"$", &text],
            );
            if !matches!(reply, Ok(Some(ref ok)) if ok == b"+OK\r\n") {
                return Err(format!("lift seed: JSON.SET on cell {cell} answered {reply:?}"));
            }
        }
    }
    let epoch = node.control.request_ckpt_all();
    for _ in 0..STALL_STEPS {
        if node.control.ckpt_board().min_published() >= epoch {
            return Ok(());
        }
        node.step(rng, clock, disk, scenario.step_ns_max)
            .map_err(|e| format!("lift seed: checkpoint: {e}"))?;
        report.scheduler_steps += 1;
    }
    Err(format!("lift seed: checkpoint epoch {epoch} never published on every cell"))
}

/// The plant's oracle after the transition boot (batch 21): every
/// planted cell lifted exactly the planted residue (the board's
/// `stale_residue_slacks`), loaded its sidecar, its tree equals the
/// scan-derived truth with the planted document in it, the lifted tiered
/// record and document serve, and the discarded life's residue never
/// does. A cell that did not lift or load is a vacuous plant — red.
#[allow(clippy::too_many_arguments)] // the scheduler tuple the MiniClient calls need
fn lift_plant_oracle(
    node: &mut Node,
    planted: &[crate::lift::PlantedCell],
    rng: &mut SplitMix64,
    clock: &Rc<VirtualClock>,
    disk: &SimDisk,
    scenario: &DurableScenario,
    report: &mut DurableReport,
) {
    let seed = scenario.seed;
    for plant in planted {
        let cell = u16::try_from(plant.cell).expect("cell fits u16");
        let lifted = node.control.recovery_board().slot(cell).residue().stale_residue_slacks;
        let loaded = u64::from(node.plane(plant.cell).keyspace().idx_sidecar_info().loaded);
        report.lift_plant_lifts += lifted;
        report.lift_plant_sidecars += loaded;
        if lifted == 0 {
            report.violations.push(format!(
                "LIFT PLANT VACUOUS seed {seed:#x} cell {}: the transition boot lifted no residue \
                 (planted segment {} beyond segment {})",
                plant.cell, plant.lifted_segment.0, plant.residue_segment.0
            ));
        }
        if loaded == 0 {
            report.violations.push(format!(
                "LIFT PLANT VACUOUS seed {seed:#x} cell {}: no index sidecar loaded at the \
                 transition boot (the forced checkpoint carried none)",
                plant.cell
            ));
        }
    }
    lift_regime_index_oracle(node, rng, clock, disk, scenario, "after the transition boot", report);
    let (_, path, key_type) = LIFT_INDEX;
    let program = compile(path.as_bytes()).expect("valid index path");
    let now = clock.now();
    for plant in planted {
        let ks = node.plane(plant.cell).keyspace();
        let Some(ns) = ks.ns_iter().find(|s| s.name == NsClass::Indexed.name()).map(|s| s.id)
        else {
            continue;
        };
        let truth = crate::backfill::cell_truth(&ks, ns, &program, key_type, now);
        let hash = ks.hasher().hash(&plant.doc_key);
        if !truth.iter().any(|(_, h)| *h == hash) {
            report.violations.push(format!(
                "LIFT PLANT VIOLATION seed {seed:#x} cell {}: the lifted document {:?} is not in \
                 the recovered store",
                plant.cell,
                String::from_utf8_lossy(&plant.doc_key)
            ));
        }
        drop(ks);
        let mut client = MiniClient::connect(node, plant.cell);
        let expect: [(&[u8], &[u8], Vec<u8>); 3] = [
            (NsClass::Tiered.name(), &plant.tier_key, bulk(crate::lift::LIFTED_VALUE)),
            (NsClass::Tiered.name(), &plant.ghost_key, b"$-1\r\n".to_vec()),
            (
                NsClass::Indexed.name(),
                &plant.doc_key,
                bulk(&crate::lift::planted_doc_text(plant.tag)),
            ),
        ];
        for (ns_name, key, want) in expect {
            let reply = client.call(
                node,
                rng,
                clock,
                disk,
                scenario.step_ns_max,
                &[b"INF.NS", b"USE", ns_name],
            );
            if !matches!(reply, Ok(Some(ref ok)) if ok == b"+OK\r\n") {
                report.violations.push(format!(
                    "lift plant: USE {} answered {reply:?}",
                    String::from_utf8_lossy(ns_name)
                ));
                return;
            }
            let read: &[u8] = if ns_name == NsClass::Indexed.name() { b"JSON.GET" } else { b"GET" };
            let reply = client.call(node, rng, clock, disk, scenario.step_ns_max, &[read, key]);
            if !matches!(reply, Ok(Some(ref got)) if *got == want) {
                report.violations.push(format!(
                    "LIFT PLANT VIOLATION seed {seed:#x} cell {}: {} {:?} answered {:?}, want {:?}",
                    plant.cell,
                    String::from_utf8_lossy(read),
                    String::from_utf8_lossy(key),
                    reply
                        .as_ref()
                        .ok()
                        .and_then(|r| r.as_ref())
                        .map(|r| String::from_utf8_lossy(r).into_owned()),
                    String::from_utf8_lossy(&want)
                ));
            }
        }
    }
}

/// The lift regime's index oracle after a lifting boot: once every
/// cell's machine reads Ready again (a loaded sidecar caught up on the
/// tail, or the S05 rebuild ran), each cell's tree equals the
/// scan-derived truth over its recovered documents and no index is
/// degraded — the lifted documents' entries included. Pre-batch-19 the
/// sidecar committed before the lifted segments replayed, so a loaded
/// tree missed exactly those documents.
#[allow(clippy::too_many_arguments)] // the scheduler tuple the MiniClient calls need
fn lift_regime_index_oracle(
    node: &mut Node,
    rng: &mut SplitMix64,
    clock: &Rc<VirtualClock>,
    disk: &SimDisk,
    scenario: &DurableScenario,
    context: &str,
    report: &mut DurableReport,
) {
    let Some(ns) = node
        .plane(0)
        .keyspace()
        .ns_iter()
        .find(|s| s.name == NsClass::Indexed.name())
        .map(|s| s.id)
    else {
        report.violations.push(format!(
            "LIFT REGIME VIOLATION seed {:#x}: the idx namespace did not survive the cut",
            scenario.seed
        ));
        return;
    };
    let (id, path, key_type) = LIFT_INDEX;
    let id = IndexId(id);
    let mut ready = false;
    for _ in 0..STALL_STEPS {
        ready = (0..usize::from(scenario.cells)).all(|cell| {
            node.plane(cell).keyspace().idx_registry().cell_state(id) == Some(IndexState::Ready)
        });
        if ready {
            break;
        }
        if node.step(rng, clock, disk, scenario.step_ns_max).is_err() {
            break;
        }
        report.scheduler_steps += 1;
    }
    if !ready {
        report.violations.push(format!(
            "LIFT REGIME VIOLATION seed {:#x}: the index never re-converged {context}",
            scenario.seed
        ));
        return;
    }
    let program = compile(path.as_bytes()).expect("valid index path");
    let now = clock.now();
    for cell in 0..usize::from(scenario.cells) {
        let ks = node.plane(cell).keyspace();
        let truth = crate::backfill::cell_truth(&ks, ns, &program, key_type, now);
        let tree = crate::backfill::cell_tree(&ks, ns, id);
        if tree != truth {
            let cell16 = u16::try_from(cell).expect("cell fits u16");
            report.violations.push(format!(
                "LIFT REGIME INDEX VIOLATION seed {:#x} cell {cell}: index tree ≠ scan-derived \
                 truth {context} ({} tree entries vs {} derived; sidecars loaded {}, stale \
                 slacks lifted {})",
                scenario.seed,
                tree.len(),
                truth.len(),
                ks.idx_sidecar_info().loaded,
                node.control.recovery_board().slot(cell16).residue().stale_residue_slacks
            ));
        }
        if ks.idx_degraded(ns, id) == Some(true) {
            report.violations.push(format!(
                "LIFT REGIME INDEX VIOLATION seed {:#x} cell {cell}: index degraded {context}",
                scenario.seed
            ));
        }
    }
}

/// A RESP bulk reply → the value (`None` for the null bulk); `None` for
/// anything that is not a bulk string.
fn parse_bulk(reply: &[u8]) -> Option<Option<Vec<u8>>> {
    if reply == b"$-1\r\n" {
        return Some(None);
    }
    let rest = reply.strip_prefix(b"$")?;
    let nl = rest.iter().position(|&b| b == b'\r')?;
    let len: usize = std::str::from_utf8(&rest[..nl]).ok()?.parse().ok()?;
    let body = rest.get(nl + 2..nl + 2 + len)?;
    Some(Some(body.to_vec()))
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
    let mut ks =
        Keyspace::new(StoreConfig { hasher: node_hasher(scenario.seed), ..Default::default() });
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

/// The device-budget oracles (M4.5-S36, ADR-0088 D8), evaluated per cell
/// at the cut on the budget's own ledger and the sim driver's observed
/// bytes. Every failure is a named violation; every disclosure rides the
/// report.
fn budget_oracles(scenario: &DurableScenario, node: &Node, now: Nanos, report: &mut DurableReport) {
    use inf_runtime::{BURST_HORIZON_NS, IoClass};
    let stall = scenario.stall.as_ref().expect("the budget scenario arms a disk model");
    let share = scenario.device.model_share;
    let elapsed_s = now.0.saturating_sub(1) as f64 / 1e9;
    for cell in 0..usize::from(scenario.cells) {
        let Some(stats) = node.plane(cell).durable_stats() else {
            report.violations.push(format!("cell {cell}: no durable plane in the budget scenario"));
            continue;
        };
        let observed = node.cells[cell].0.driver().observed_io();
        let ckpt_slice = f64::from(scenario.ckpt_section_bytes.unwrap_or(256 << 10) + 4096);
        // (a) Accounting identity: what the budget counted is what the
        // driver saw, for every token-classed class — up to the ops the
        // cut caught between push and submit (`LoopCx::push` queues; the
        // driver drains at the *next* iteration's `submit_and_reap`, and
        // the cut eats that queue by design): at most one op per class
        // and one op's bytes (a segment for frames, a block for
        // checkpoint and zero-fill), never fewer than the driver saw.
        // The two cold-read classes together match the driver's reads.
        let one_op_bytes = |class: IoClass| -> u64 {
            match class {
                IoClass::LogFrame => u64::from(scenario.segment_bytes),
                IoClass::ZeroFill => 256 << 10,
                IoClass::Checkpoint => ckpt_slice as u64,
                _ => 0,
            }
        };
        for class in IoClass::ALL {
            let counted = stats.io_budget[class.index()];
            match class {
                IoClass::BlobWrite | IoClass::ColdReadForeground | IoClass::ColdReadMaintain => {}
                _ => {
                    let seen = observed.bytes[class.index()];
                    let seen_ops = observed.ops[class.index()];
                    let ops_slack = counted.spent_ops.wrapping_sub(seen_ops);
                    let bytes_slack = counted.spent_bytes.wrapping_sub(seen);
                    if counted.spent_bytes < seen || bytes_slack > one_op_bytes(class) {
                        report.violations.push(format!(
                            "cell {cell}: io_budget_bytes_{} = {} but the driver saw {seen} \
                             (ADR-0088 D8 accounting identity; slack ≤ one op's bytes)",
                            class.name(),
                            counted.spent_bytes
                        ));
                    }
                    if counted.spent_ops < seen_ops || ops_slack > 3 {
                        report.violations.push(format!(
                            "cell {cell}: io_budget_ops_{} = {} but the driver saw {seen_ops} \
                             (slack ≤ one LOG step's pushed-unsubmitted ops)",
                            class.name(),
                            counted.spent_ops
                        ));
                    }
                }
            }
        }
        let reads_counted = stats.io_budget[IoClass::ColdReadForeground.index()].spent_bytes
            + stats.io_budget[IoClass::ColdReadMaintain.index()].spent_bytes;
        if reads_counted < observed.read_bytes || reads_counted - observed.read_bytes > 16 << 10 {
            report.violations.push(format!(
                "cell {cell}: cold-read bytes counted {reads_counted} but the driver saw {}",
                observed.read_bytes
            ));
        }
        // (b) Rate bound — the budget's contract: background bytes over
        // the run never exceed the share's grant plus two burst horizons
        // (the class caps plus the pool) plus one slice per class (the
        // cap floor) plus the checkpoint keep-up floor (the log's bytes
        // over α — ADR-0088 D2 amended). Foreground subtraction only
        // lowers the weighted grant.
        let horizon_bytes = share.write_bytes_per_s as f64 * BURST_HORIZON_NS as f64 / 1e9;
        let slices = 256.0 * 1024.0 + ckpt_slice + 1024.0 * 1024.0 + 16.0 * 1024.0;
        let keepup = stats.log_frame_bytes as f64 / 2.0;
        let bound =
            share.write_bytes_per_s as f64 * elapsed_s + 2.0 * horizon_bytes + slices + keepup;
        let background: u64 = IoClass::ALL
            .iter()
            .filter(|c| !c.is_foreground() && !c.is_read())
            .map(|c| stats.io_budget[c.index()].spent_bytes)
            .sum();
        if background as f64 > bound {
            report.violations.push(format!(
                "cell {cell}: background wrote {background} bytes in {elapsed_s:.3} s against a \
                 share of {} B/s — bound {bound:.0} (ADR-0088 D2 rate bound)",
                share.write_bytes_per_s
            ));
        }
        // (c) Engagement: the regime is not vacuous — some background
        // class was deferred at least once (the S27 lesson: a pressure
        // row whose counter stayed at 0 measured nothing). The checkpoint
        // class itself is floored to keep up with the log (ADR-0088 D2
        // amended), so its deferrals are not the signal; zero-fill's are.
        let background_deferrals: u64 = IoClass::ALL
            .iter()
            .filter(|c| !c.is_foreground())
            .map(|c| stats.io_budget[c.index()].deferrals)
            .sum();
        if background_deferrals == 0 {
            let ckpt = node.plane(cell).ckpt_stats_for_sim();
            report.violations.push(format!(
                "cell {cell}: the budget never deferred a background block — the scenario's \
                 offered load did not exceed the share (vacuous regime); ckpts completed {} \
                 aborted {} in_progress {} interval {} records_since_begin {} log_frame_bytes {} \
                 ckpt_bytes {} zero_fill {} rotations_unzeroed {} waits_pace {} \
                 deferrals[zero_fill {} tier_flush {} ckpt {}] frames_in_flight_max {}",
                ckpt.0,
                ckpt.1,
                stats.ckpt_in_progress,
                stats.ckpt_interval_bytes,
                stats.ckpt_records_since_begin,
                stats.log_frame_bytes,
                stats.ckpt_bytes_total,
                stats.zero_fill_bytes,
                stats.rotations_unzeroed,
                stats.frame_waits_pace,
                stats.io_budget[IoClass::ZeroFill.index()].deferrals,
                stats.io_budget[IoClass::TierFlush.index()].deferrals,
                stats.io_budget[IoClass::Checkpoint.index()].deferrals,
                stats.frames_in_flight_max,
            ));
        }
        // (d) Progress: deferrals never starved the classes — a
        // checkpoint published and the zero-fill landed its bytes.
        if stats.io_budget[IoClass::Checkpoint.index()].spent_bytes == 0 {
            report.violations.push(format!("cell {cell}: no checkpoint bytes reached the device"));
        }
        if stats.zero_fill_bytes == 0 {
            report.violations.push(format!("cell {cell}: no zero-fill bytes reached the device"));
        }
        // (e) Foreground bound (physics, both modes): a frame waits for at
        // most its own transfer, one background block ahead on the byte
        // timeline, the budget's burst, the write-through base and tail,
        // and one scheduler step of completion quantization.
        let rate = stall.write_bytes_per_s as f64;
        let frame_max = f64::from(scenario.segment_bytes);
        let block_max = (256.0_f64 * 1024.0).max(ckpt_slice);
        let service_us = (stall.through_base_ns * (1 + stall.tail_mult)) as f64 / 1e3;
        let bound_us = (frame_max + block_max + horizon_bytes) / rate * 1e6
            + service_us
            + scenario.step_ns_max as f64 / 1e3
            + 1_000.0;
        if stats.write_stall_max_us as f64 > bound_us {
            report.violations.push(format!(
                "cell {cell}: worst frame write latency {} µs exceeds the foreground bound \
                 {bound_us:.0} µs (ADR-0088 D8)",
                stats.write_stall_max_us
            ));
        }
        // Disclosures: the model must be present and the pipeline must
        // have filled, or the scenario measured a different machine.
        if stats.io_budget_model_absent == 1 {
            report.violations.push(format!("cell {cell}: the budget model is absent"));
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
