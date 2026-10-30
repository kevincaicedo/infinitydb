//! `m2-ledger-wedge`: the fsync ledger's rotation class under a held seal.
//!
//! The shape is `m2-fua-pending`'s — one pure-`always` cell on `Direct`
//! 64 KiB segments, fine steps, no stall episode — with the main leg's 128
//! writers each thinking up to 50 ms after a send, a 1 ms flush base and no
//! automatic checkpoint, so every next segment is a fresh file. The sim
//! disk holds every fdatasync on the first pre-zeroed segment
//! (`seg-000001.ilog`): armed one step after the class-upgrade rotation is
//! read (`segment_rotations = 1`, `rotations_upgrade = 1`,
//! `rotations_unzeroed = 0`), it catches the segment's one fdatasync, its
//! seal, pushed at the second rotation. The driver's held list shows the
//! hold engaged (`t_seal`); the hold lifts at the first step past `t_seal`
//! plus 100 ms of injected time, and the leg cuts at its seeded step or the
//! step after the seal's fold, whichever is later.
//!
//! Inside the hold every ack sits behind the held seal, so each writer lands
//! at most one command and about every other step seals a one-block frame;
//! a fresh segment holds sixteen, so the third, fourth and fifth rotations
//! would fall due inside the hold. The rotation class registers one seal at
//! a time, so the frame that needs the third rotation holds until the held
//! seal folds, and only one next segment (the third) appears in the log
//! directory inside the hold. A ledger that registers seals unpaced creates
//! a segment per rotation: three or more.
//!
//! The clauses, in order, each by a token no other assertion prints:
//! - the arming reading came before the cut (`wedge-arming-unreached`);
//! - the driver held an fdatasync at some step while armed — the harness's
//!   own fact, never a product counter (`wedge-hold-unengaged`);
//! - on the main leg, the unfolded write-through tickets reached their
//!   window of sixteen, which also shows the frames past it fell due inside
//!   the hold (`wedge-window-unreached`);
//! - the disk clause: the segment ids that appear inside the hold, read from
//!   the sim disk's names once a step, number at most one on the main leg
//!   and none on the control (`wedge-ids-over`), whatever the product's
//!   counters read;
//! - the hold counter's wiring: the main leg counts a seal hold, the control
//!   none (`wedge-hold-uncounted`);
//! - the ledger's unfolded entries stay within its cap of 44 (a wiring read:
//!   no construction here passes it; its evidence is the ledger's own edge
//!   test, token `ledger-edge-short`), token `wedge-ledger-over`.
//!
//! The three vacuity clauses each have a plant: the log `Buffered` (no
//! class-upgrade rotation, so no arming), the hold on `seg-000000.ilog`
//! (which no fdatasync names after the arming), and a 1 ms hold (too short
//! for sixteen frames to land behind the seal).

use std::collections::BTreeSet;

use super::*;

/// The ledger-wedge shape's legs, chosen by name (`--wedge-leg`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LedgerWedgeLeg {
    /// 128 writers thinking up to 50 ms: rotations fall due inside the
    /// hold, at least three of them on a ledger that does not pace its
    /// seals.
    Main,
    /// One writer, no think time: its one command after the second
    /// rotation lands in one frame whose ack waits for the fold, so no
    /// further rotation and no new segment falls inside the hold.
    Control,
}

impl LedgerWedgeLeg {
    pub const ALL: [LedgerWedgeLeg; 2] = [LedgerWedgeLeg::Main, LedgerWedgeLeg::Control];

    /// The leg's name on the command line.
    pub fn name(self) -> &'static str {
        match self {
            LedgerWedgeLeg::Main => "main",
            LedgerWedgeLeg::Control => "control",
        }
    }

    /// The leg named `name`, if any.
    pub fn parse(name: &str) -> Option<LedgerWedgeLeg> {
        LedgerWedgeLeg::ALL.into_iter().find(|leg| leg.name() == name)
    }

    /// The leg's writers on the `always` namespace.
    fn writers(self) -> usize {
        match self {
            LedgerWedgeLeg::Main => 128,
            LedgerWedgeLeg::Control => 1,
        }
    }

    /// The longest think time after a writer's send.
    fn think_ns_max(self) -> u64 {
        match self {
            LedgerWedgeLeg::Main => 50_000_000,
            LedgerWedgeLeg::Control => 0,
        }
    }

    /// Segment ids the disk clause admits inside the hold: the one next
    /// segment MAINTAIN creates on the main leg, none on the control.
    fn ids_admitted(self) -> usize {
        match self {
            LedgerWedgeLeg::Main => 1,
            LedgerWedgeLeg::Control => 0,
        }
    }
}

impl DurableScenario {
    /// `m2-ledger-wedge` on `leg` (the module docs).
    #[must_use]
    pub fn m2_ledger_wedge(seed: u64, leg: LedgerWedgeLeg) -> DurableScenario {
        let mut scenario = DurableScenario::m2_fua_pending(seed);
        scenario.always_writers = leg.writers();
        scenario.always_think_ns_max = leg.think_ns_max();
        scenario.ckpt_interval_bytes = 0;
        // Plant: the log `Buffered`, so no class-upgrade rotation is ever
        // read and the hold is never armed.
        #[cfg(inf_canary_wedge_buffered)]
        {
            scenario.io_mode = SegmentIoMode::Buffered;
        }
        if let Some(stall) = scenario.stall.as_mut() {
            stall.base_ns = 1_000_000;
        }
        scenario.ledger_wedge = Some(leg);
        scenario
    }
}

/// How long the sim disk holds the seal, injected time: long enough for
/// three rotations' frames to land behind it (about 20 ms of arrivals).
#[cfg(not(inf_canary_wedge_short_hold))]
const HOLD: Nanos = Nanos::from_millis(100);
/// Plant: a hold too short for sixteen frames to land behind the seal.
#[cfg(inf_canary_wedge_short_hold)]
const HOLD: Nanos = Nanos::from_millis(1);

/// Scheduler steps an armed leg waits past its seeded cut for the hold to
/// engage and the seal to fold, before it cuts anyway: the hold engages
/// within sixteen frames of the arming and folds about 1 ms after its lift.
const ENGAGE_STEPS_MAX: u64 = 20_000;

/// The hold's predicate: an OS name of the first pre-zeroed segment (the
/// first segment a fresh cell creates after its active one).
#[cfg(not(inf_canary_wedge_wrong_name))]
fn held_segment(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == "seg-000001.ilog")
}

/// Plant: the hold names the first segment, whose frames rode linked syncs
/// and whose seal was submitted before the arming, so it holds nothing.
#[cfg(inf_canary_wedge_wrong_name)]
fn held_segment(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == "seg-000000.ilog")
}

/// A segment file in a cell's log directory.
fn log_segment(path: &Path) -> bool {
    path.parent().and_then(Path::file_name).is_some_and(|dir| dir == "log")
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(inf_log::parse_segment_file_name)
            .is_some()
}

/// Where the arming stands.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Arming {
    /// No rotation read yet.
    Waiting,
    /// The class-upgrade rotation was read at `read_at`; the hold arms the
    /// step after.
    Read { read_at: u64 },
    /// The first rotation read was not the class-upgrade rotation onto a
    /// pre-zeroed segment: the hold is never armed.
    Unreached(String),
}

/// The leg's state through one run, and what it read.
pub(crate) struct LedgerWedge {
    leg: LedgerWedgeLeg,
    arming: Arming,
    armed_at: Option<u64>,
    /// The first step the driver held an fdatasync while armed, and the
    /// injected time then: the hold's start.
    engaged_at: Option<(u64, Nanos)>,
    /// The segment names in the log directory at the hold's start.
    names_at_hold: BTreeSet<PathBuf>,
    /// Segment names that appeared inside the hold.
    appeared: BTreeSet<PathBuf>,
    held_max: usize,
    /// The step of the lift and the watermark then.
    lifted_at: Option<(u64, u64)>,
    folded_at: Option<u64>,
}

/// What the leg read, for the run's report line.
#[derive(Clone, Debug)]
pub struct LedgerWedgeReport {
    pub leg: LedgerWedgeLeg,
    pub armed_at: Option<u64>,
    pub engaged_at: Option<u64>,
    pub lifted_at: Option<u64>,
    pub folded_at: Option<u64>,
    pub held_max: usize,
    pub ids_inside_hold: Vec<String>,
    pub frame_waits_ledger: u64,
}

impl LedgerWedge {
    pub(crate) fn new(leg: LedgerWedgeLeg) -> LedgerWedge {
        LedgerWedge {
            leg,
            arming: Arming::Waiting,
            armed_at: None,
            engaged_at: None,
            names_at_hold: BTreeSet::new(),
            appeared: BTreeSet::new(),
            held_max: 0,
            lifted_at: None,
            folded_at: None,
        }
    }

    /// One scheduler step's reading, after the step: the arming, the
    /// hold's engagement, the names inside it, the lift and the fold.
    pub(crate) fn observe(&mut self, step: u64, node: &Node, disk: &SimDisk, now: Nanos) {
        let Some(stats) = node.plane(0).durable_stats() else { return };
        self.read_arming(step, &stats);
        if let Arming::Read { read_at } = self.arming
            && self.armed_at.is_none()
            && step > read_at
        {
            disk.hold_syncs(Some(held_segment));
            self.armed_at = Some(step);
        }
        if self.armed_at.is_none() {
            return;
        }
        let held = node.cells[0].0.driver().held_syncs();
        self.held_max = self.held_max.max(held);
        if self.engaged_at.is_none() && held > 0 {
            self.engaged_at = Some((step, now));
            self.names_at_hold = disk.names_where(log_segment).into_iter().collect();
        }
        let Some((_, t_seal)) = self.engaged_at else { return };
        if self.lifted_at.is_none() {
            for name in disk.names_where(log_segment) {
                if !self.names_at_hold.contains(&name) {
                    self.appeared.insert(name);
                }
            }
            if now > t_seal + HOLD {
                disk.hold_syncs(None);
                self.lifted_at = Some((step, stats.last_durable_lsn));
            }
            return;
        }
        if let Some((_, durable_at_lift)) = self.lifted_at
            && self.folded_at.is_none()
            && stats.last_durable_lsn > durable_at_lift
        {
            self.folded_at = Some(step);
        }
    }

    /// The first rotation read decides the arming once.
    fn read_arming(&mut self, step: u64, stats: &inf_server::DurableStats) {
        if self.arming != Arming::Waiting || stats.segment_rotations == 0 {
            return;
        }
        let upgrade = (stats.segment_rotations, stats.rotations_upgrade, stats.rotations_unzeroed);
        self.arming = if upgrade == (1, 1, 0) {
            Arming::Read { read_at: step }
        } else {
            Arming::Unreached(format!(
                "the first rotation read {} rotation(s), {} upgrade(s), {} unzeroed",
                upgrade.0, upgrade.1, upgrade.2
            ))
        };
    }

    /// Whether the leg cuts at `step`: at its seeded step, unless armed,
    /// then the step after the seal's fold, or past the engagement bound.
    pub(crate) fn cut_due(&self, step: u64, seeded: u64) -> bool {
        if step < seeded {
            return false;
        }
        let Some(armed_at) = self.armed_at else { return true };
        self.folded_at.is_some_and(|fold| step > fold) || step >= armed_at + ENGAGE_STEPS_MAX
    }

    /// The clauses, in order, at the cut (the module docs).
    pub(crate) fn judge(&self, seed: u64, node: &Node, report: &mut DurableReport) {
        let stats = node.plane(0).durable_stats().unwrap_or_default();
        report.ledger_wedge = Some(self.report(&stats));
        if let Some(vacuous) = self.vacuity(report) {
            report.violations.push(format!("VACUOUS seed {seed:#x}: {vacuous}"));
            return;
        }
        let admitted = self.leg.ids_admitted();
        if self.appeared.len() > admitted {
            report.violations.push(format!(
                "LEDGER WEDGE VIOLATION seed {seed:#x}: wedge-ids-over — {} segment id(s) \
                 appeared inside the hold, {admitted} admitted: {:?}",
                self.appeared.len(),
                self.appeared
            ));
        }
        let counted = stats.frame_waits_ledger;
        let uncounted = match self.leg {
            LedgerWedgeLeg::Main => counted == 0,
            LedgerWedgeLeg::Control => counted > 0,
        };
        if uncounted {
            report.violations.push(format!(
                "LEDGER WEDGE VIOLATION seed {seed:#x}: wedge-hold-uncounted — the {} leg read \
                 frame_waits_ledger {counted}",
                self.leg.name()
            ));
        }
        let cap = u64::from(inf_log::limits::FSYNC_LEDGER_ENTRIES);
        if report.fsync_entries_max > cap {
            report.violations.push(format!(
                "LEDGER WEDGE VIOLATION seed {seed:#x}: wedge-ledger-over — {} unfolded ledger \
                 entries against the cap of {cap}",
                report.fsync_entries_max
            ));
        }
    }

    /// The first vacuity clause that fails, by its token.
    fn vacuity(&self, report: &DurableReport) -> Option<String> {
        match (&self.arming, self.armed_at) {
            (Arming::Unreached(why), _) => {
                return Some(format!("wedge-arming-unreached — {why}"));
            }
            (Arming::Waiting | Arming::Read { .. }, None) => {
                return Some(
                    "wedge-arming-unreached — no class-upgrade rotation read before the cut"
                        .to_owned(),
                );
            }
            (Arming::Waiting | Arming::Read { .. }, Some(_)) => {}
        }
        if self.engaged_at.is_none() {
            return Some(
                "wedge-hold-unengaged — the driver held no fdatasync while armed".to_owned(),
            );
        }
        let window = u64::from(inf_log::limits::WRITE_THROUGH_WINDOW_ENTRIES);
        if self.leg == LedgerWedgeLeg::Main && report.write_through_entries_max < window {
            return Some(format!(
                "wedge-window-unreached — {} unfolded write-through tickets at most, under the \
                 window of {window}",
                report.write_through_entries_max
            ));
        }
        None
    }

    fn report(&self, stats: &inf_server::DurableStats) -> LedgerWedgeReport {
        LedgerWedgeReport {
            leg: self.leg,
            armed_at: self.armed_at,
            engaged_at: self.engaged_at.map(|(step, _)| step),
            lifted_at: self.lifted_at.map(|(step, _)| step),
            folded_at: self.folded_at,
            held_max: self.held_max,
            ids_inside_hold: self
                .appeared
                .iter()
                .filter_map(|name| name.file_name().map(|n| n.to_string_lossy().into_owned()))
                .collect(),
            frame_waits_ledger: stats.frame_waits_ledger,
        }
    }
}
