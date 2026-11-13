//! The tier-budget shape (`m4-tier-budget`): its legs, each leg's
//! namespaces, their loads and windows and its device model, the reads the
//! oracles take per leg, and the per-cell record the scrape takes at the
//! cut.

use super::*;

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
    /// Two namespaces at the hostile share, the first a thief of the tier
    /// class's credit: twelve writers of 4 KiB `SET`s on new keys into a
    /// 128 MiB window at `64kb` with one percent of it mutable, so it seals
    /// from its first seconds and faster than the class flushes while it
    /// sends; past its first, each round is one write group of at most two
    /// writes and 69,632 B, then a sync, ready again within a few steps of
    /// its grant. The second is the hostile leg's 1 MiB backlog, whose
    /// write group is larger on bytes and not smaller on ops. The fair
    /// turn's refusal ends the pass, so the credit builds to the second's
    /// group within `T_c`; a visit that starts at index 0 and continues
    /// past a refusal grants the thief at every opening while it holds a
    /// sealed backlog and holds the second's group until that backlog is
    /// flushed, more than three bounds: the fair turn's wait-bound
    /// oracle's planted leg.
    TwoAt1mbThief,
}

impl TierBudgetLeg {
    pub const ALL: [TierBudgetLeg; 6] = [
        TierBudgetLeg::TwoAt8mb,
        TierBudgetLeg::EightAt1mb,
        TierBudgetLeg::OneAt1mb,
        TierBudgetLeg::TwoAt1mbHostile,
        TierBudgetLeg::OneAt64mbNoModel,
        TierBudgetLeg::TwoAt1mbThief,
    ];

    /// The thief leg's cut, in fair-turn bounds: a group the planted turn
    /// holds until the thief's backlog is flushed has waited past one bound
    /// by then, and the cut reads its wait.
    const THIEF_CUT_BOUNDS: u64 = 4;

    /// A scheduler step's mean advance on the tier-budget shape, about
    /// 1 ms (1 µs plus a draw below its 2 ms bound): the unit the thief
    /// leg's cut counts its bounds in.
    const STEP_NS_MEAN: u64 = 1_000_000;

    /// The leg's name on the command line.
    pub fn name(self) -> &'static str {
        match self {
            TierBudgetLeg::TwoAt8mb => "two-8mb",
            TierBudgetLeg::EightAt1mb => "eight-1mb",
            TierBudgetLeg::OneAt1mb => "one-1mb",
            TierBudgetLeg::TwoAt1mbHostile => "two-1mb-hostile",
            TierBudgetLeg::OneAt64mbNoModel => "one-64mb-no-model",
            TierBudgetLeg::TwoAt1mbThief => "two-1mb-thief",
        }
    }

    /// The scheduler step of the leg's cut, given the seeded one and the
    /// fair turn's bound `n × T_c` in nanoseconds: every leg cuts at the
    /// seeded step but the thief leg, which cuts no earlier than
    /// [`Self::THIEF_CUT_BOUNDS`] bounds of mean steps, so a group its
    /// planted turn holds waits past the bound before the cut reads it.
    pub(crate) fn cut_step(self, seeded: u64, turn_bound_ns: u64) -> u64 {
        match self {
            TierBudgetLeg::TwoAt1mbThief => {
                let bound_steps = turn_bound_ns.div_ceil(Self::STEP_NS_MEAN);
                seeded.max(Self::THIEF_CUT_BOUNDS.saturating_mul(bound_steps))
            }
            TierBudgetLeg::TwoAt8mb
            | TierBudgetLeg::EightAt1mb
            | TierBudgetLeg::OneAt1mb
            | TierBudgetLeg::TwoAt1mbHostile
            | TierBudgetLeg::OneAt64mbNoModel => seeded,
        }
    }

    /// The leg named `name`, if any.
    pub fn parse(name: &str) -> Option<TierBudgetLeg> {
        TierBudgetLeg::ALL.into_iter().find(|leg| leg.name() == name)
    }

    /// The leg's device model: the reference box's (489 MB/s, 40,000
    /// ops/s), shared by the node's cells, or that box's rate at the
    /// share a cell holds among 256, carried by the two cells here; the
    /// no-model leg has none (`DeviceModel::ABSENT`).
    pub fn model(self) -> inf_runtime::DeviceModel {
        let divisor = match self {
            TierBudgetLeg::TwoAt8mb | TierBudgetLeg::EightAt1mb | TierBudgetLeg::OneAt1mb => 1,
            TierBudgetLeg::TwoAt1mbHostile | TierBudgetLeg::TwoAt1mbThief => 128,
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
        matches!(self, TierBudgetLeg::TwoAt1mbHostile | TierBudgetLeg::TwoAt1mbThief)
    }

    /// Whether the leg's oracle requires a namespace with work to have
    /// been passed over by the cell's one plan (the one-plan oracle's
    /// engagement).
    pub fn expects_plan_waits(self) -> bool {
        matches!(self, TierBudgetLeg::EightAt1mb)
    }

    /// The thief leg: its oracles read the planted turn's regime, the
    /// turn's shape from the driver (its drivers alone tally each entry's
    /// tier writes by namespace directory) and the first namespace's
    /// backlog.
    pub fn is_thief(self) -> bool {
        matches!(self, TierBudgetLeg::TwoAt1mbThief)
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
    /// cell by key hash, above the seal with a margin. On the thief leg the
    /// first namespace is [`TierBudgetLoad::THIEF`] and the second the
    /// hostile leg's first.
    pub(super) fn shape(self) -> TierBudgetShape {
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
            TierBudgetLeg::TwoAt1mbThief => TierBudgetShape {
                leg: self,
                namespaces: TWO,
                first: TierBudgetLoad::THIEF,
                rest: TierBudgetLoad::HOSTILE_FIRST,
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
                    values: TierBudgetValues::Mixed,
                };
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
        values: TierBudgetValues::Mixed,
    };

    /// The thief leg's first namespace: twelve writers of 3,000 `SET`s,
    /// each a 4,096 B value on a new key (a record of at most 4,114 B),
    /// into a 128 MiB window at `64kb` with one percent of it mutable. A
    /// cell's half of the writes, about 74 MB, is below the window and its
    /// 256 MiB ring: no writer parks, and no round meets the ring top or a
    /// rotation, so past its first a round of at most 15 records is one
    /// batch and its tail, two writes of at most 17 frames, then its sync.
    #[cfg(not(inf_canary_thief_premise_short))]
    const THIEF: TierBudgetLoad = TierBudgetLoad {
        writers: 12,
        ops_per_writer: 3_000,
        keys_per_writer: 3_000,
        think_ns_max: 0,
        mem_budget: b"128mb",
        mutable_permille: b"10",
        maintain_slice: b"64kb",
        values: TierBudgetValues::SetOnlyUnique { value_bytes: 4_096 },
    };

    /// A planted canary: the thief of two writers with a tenth of its
    /// window mutable, which seals once its tail passes 12.8 MiB and sends
    /// at a sixth of the twelve writers' rate, so its backlog runs below
    /// its slice inside the bound's window and the leg's premise clause
    /// reads it vacuous.
    #[cfg(inf_canary_thief_premise_short)]
    const THIEF: TierBudgetLoad = TierBudgetLoad {
        writers: 2,
        ops_per_writer: 20_000,
        keys_per_writer: 20_000,
        think_ns_max: 0,
        mem_budget: b"128mb",
        mutable_permille: b"100",
        maintain_slice: b"64kb",
        values: TierBudgetValues::SetOnlyUnique { value_bytes: 4_096 },
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
    /// Tier-class bytes and ops granted and waiting in the cell loop's
    /// queue for the next submit, at the cut.
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
