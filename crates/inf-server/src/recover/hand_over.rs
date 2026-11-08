//! The hand-over (ADR-0174 R10's last half, A1.3): once replay has ended
//! and every machine's end settle reached its tail, each recovered tiered
//! namespace in turn, in namespace order, seeds the extent orphan sweep from
//! the boot listing (ADR-0061 D6: liveness is the post-replay refcount
//! truth), renames back the quarantined extents the replayed map references
//! (ADR-0096 D3), rebuilds ADR-0093's ticket set from the finished index, and
//! hands the plane its pipeline and handles.
//!
//! Each runs under the step budget, in units at fixed prices: one parked
//! replay death stamped durable or one listed or quarantined extent id
//! seeded at the RAM unit price, one rename at the boot file-operation
//! price, one probe group the rebuild scans at its bytes, one slot it
//! returns at its settle read's price, and the machine's hand-over — its
//! drain's seal and barrier — at its boot I/O. A step yields once its
//! charge reaches the budget, at the next unit, and the next step resumes
//! at the stage's cursor: an index into a list nothing appends to once its
//! stage began (the boot listings and the revive list), the table's own
//! stamp cursor over its parked deaths, or the rebuild's own cursor. The
//! units run in the order a whole hand-over runs them, so the reclaim queue
//! takes its candidates in one order at every budget.

use std::io;

use inf_log::fs::SegmentFs;
use inf_store::limits::{REBUILD_GROUP_CHARGE_BYTES, RECOVER_PROBE_CHARGE_BYTES};
use inf_store::{Keyspace, RebuildStep, ShadowRebuild, TieredTable};

use super::{
    BootFile, Phase, RecoverStats, RecoveredTierNs, RecoveringTierNs, Recovery, RecoveryProgress,
    StepBudget, boot_io_charge, io_invalid, io_msg, replay_refused,
};
use crate::limits::BOOT_FILE_OP_CHARGE_BYTES;

/// Where the hand-over of the next machine stands.
pub(super) enum HandOverStage {
    /// The table's parked replay deaths stamped durable, from the table's
    /// own stamp cursor.
    Stamp,
    /// The listed extent ids seeded, from `at` in the machine's listing.
    SeedListed { at: usize },
    /// The quarantined ids seeded or kept for revival, from `at`.
    SeedQuarantined { at: usize },
    /// The kept ids renamed back, from `at` in the machine's revive list.
    Revive { at: usize },
    /// ADR-0093's ticket rebuild, at its own cursor.
    Rebuild(Box<ShadowRebuild>),
    /// The machine's hand-over: its drain's seal, then the plane's pipeline
    /// and handles.
    Seal,
}

/// What follows the hand-over's last machine.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum ResumeFrom {
    /// A replayed log: the sidecar commit and the log's resume point.
    Log,
    /// A fresh cell, nothing replayed: its first segment.
    FreshCell,
}

/// How many units of `price` what the budget has left buys, at least one: a
/// step with work to do completes a unit at any budget (ADR-0174 A1).
fn units_left(budget: &mut StepBudget, price: u64) -> usize {
    let buys = (budget.left() / price.max(1)).max(1);
    usize::try_from(buys).unwrap_or(usize::MAX)
}

/// Charges `count` units of `price` to the step's budget. The planted canary
/// charges the sweep seed's stamps and seeds, the revive renames and the
/// rebuild's probe groups nothing.
fn charge_units(budget: &mut StepBudget, count: usize, price: u64) {
    if cfg!(inf_canary_recover_probe_uncharged) {
        return;
    }
    let count = u64::try_from(count).unwrap_or(u64::MAX);
    budget.charge(count.saturating_mul(price));
}

/// Adds `count` to a unit counter.
fn count_units(counter: &mut u64, count: usize) {
    *counter = counter.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
}

impl<F: SegmentFs + Clone> Recovery<F> {
    /// Replay has ended: the machines leave the replay seam for the
    /// hand-over, the next one last. Answers the phase that follows: the
    /// hand-over, or with no machine the resume `then` names.
    pub(super) fn begin_hand_over(&mut self, then: ResumeFrom) -> Phase<BootFile<F>> {
        debug_assert!(self.recovering_tiers.lent.is_empty(), "every lent machine drained");
        let mut machines = std::mem::take(&mut self.recovering_tiers.tiers);
        machines.reverse();
        self.handing_over = machines;
        if self.handing_over.is_empty() {
            Phase::Resume { from: then }
        } else {
            Phase::HandOver { stage: HandOverStage::Stamp, then }
        }
    }

    /// One hand-over step: units of the next machine's stage under
    /// `budget`, a batch at a time, each stage in turn and each machine in
    /// turn, until the budget is spent or every machine is handed over —
    /// then the resume follows.
    pub(super) fn step_hand_over(
        &mut self,
        ks: &mut Keyspace,
        mut stage: HandOverStage,
        then: ResumeFrom,
        budget: &mut StepBudget,
    ) -> io::Result<RecoveryProgress> {
        // Bound: each turn moves a stage on, or runs a batch the budget left
        // buys (at least one unit), or hands one machine over.
        loop {
            if self.handing_over.is_empty() {
                self.phase = Phase::Resume { from: then };
                return Ok(RecoveryProgress::Working);
            }
            if budget.spent() {
                self.phase = Phase::HandOver { stage, then };
                return Ok(RecoveryProgress::Working);
            }
            stage = match stage {
                HandOverStage::Seal => self.seal_next(ks, budget)?,
                stage @ (HandOverStage::Stamp
                | HandOverStage::SeedListed { .. }
                | HandOverStage::SeedQuarantined { .. }
                | HandOverStage::Revive { .. }
                | HandOverStage::Rebuild(_)) => self.hand_over_batch(ks, stage, budget)?,
            };
        }
    }

    /// One batch of the next machine's stage, or the move to its next stage.
    fn hand_over_batch(
        &mut self,
        ks: &mut Keyspace,
        stage: HandOverStage,
        budget: &mut StepBudget,
    ) -> io::Result<HandOverStage> {
        let Recovery { handing_over, stats, fs, shard_dir, step_charge, .. } = self;
        let Some(tier) = handing_over.last_mut() else { return Ok(stage) };
        let ns = tier.ns;
        let table = ks
            .tiered_store_mut(ns)
            .ok_or_else(|| io_msg(format!("ns {}: a boot machine with no table", ns.0)))?;
        Ok(match stage {
            HandOverStage::Stamp => {
                let stamped =
                    table.extent_stamp_parked(units_left(budget, RECOVER_PROBE_CHARGE_BYTES));
                count_units(&mut stats.units.sweep_stamped, stamped);
                charge_units(budget, stamped, RECOVER_PROBE_CHARGE_BYTES);
                if table.extent_parked_unstamped() == 0 {
                    HandOverStage::SeedListed { at: 0 }
                } else {
                    HandOverStage::Stamp
                }
            }
            HandOverStage::SeedListed { at } => {
                let listed = tier.extents_listed.run_from(at);
                if listed.is_empty() {
                    return Ok(HandOverStage::SeedQuarantined { at: 0 });
                }
                let batch = units_left(budget, RECOVER_PROBE_CHARGE_BYTES).min(listed.len());
                table.extent_seed_listed(&listed[..batch]);
                count_units(&mut stats.units.sweep_seeded, batch);
                charge_units(budget, batch, RECOVER_PROBE_CHARGE_BYTES);
                HandOverStage::SeedListed { at: at + batch }
            }
            HandOverStage::SeedQuarantined { at } => {
                let quarantined = tier.extents_quarantined.run_from(at);
                if quarantined.is_empty() {
                    return Ok(HandOverStage::Revive { at: 0 });
                }
                let batch = units_left(budget, RECOVER_PROBE_CHARGE_BYTES).min(quarantined.len());
                table.extent_seed_quarantined(&quarantined[..batch], &mut tier.revive);
                count_units(&mut stats.units.sweep_seeded, batch);
                charge_units(budget, batch, RECOVER_PROBE_CHARGE_BYTES);
                HandOverStage::SeedQuarantined { at: at + batch }
            }
            HandOverStage::Revive { at } => {
                let revive = tier.revive.run_from(at);
                if revive.is_empty() {
                    return Ok(HandOverStage::Rebuild(Box::new(table.begin_shadow_rebuild())));
                }
                let fs =
                    fs.as_ref().ok_or_else(|| io_msg("a hand-over with no filesystem".into()))?;
                let batch = units_left(budget, BOOT_FILE_OP_CHARGE_BYTES).min(revive.len());
                let shard = shard_dir.join(format!("ns-{}", ns.0));
                // A rename I/O failure is a recovery fail-stop; "nothing to
                // revive" is fine (already revived or gone — reads answer the
                // latter typed). Reads never see the twin once the node serves.
                for &extent_id in &revive[..batch] {
                    inf_log::blob::revive_extent_file(fs, &shard, inf_log::ExtentId(extent_id))?;
                    stats.blob_revived += 1;
                }
                charge_units(budget, batch, BOOT_FILE_OP_CHARGE_BYTES);
                HandOverStage::Revive { at: at + batch }
            }
            HandOverStage::Rebuild(mut rebuild) => {
                let groups = units_left(budget, REBUILD_GROUP_CHARGE_BYTES);
                let step = table.shadow_rebuild_step(&mut rebuild, groups);
                let turn = RebuildTurn { tier, table, stats, step_charge, budget };
                turn.settle(step, rebuild)?
            }
            HandOverStage::Seal => HandOverStage::Seal,
        })
    }

    /// The next machine's hand-over (ADR-0174 R10): the pipeline under the
    /// live claim rule, with a handle for every sealed file; its counters
    /// fold into the cell's D6 stats and its drain's seal and barrier into
    /// this step's charge.
    fn seal_next(
        &mut self,
        ks: &mut Keyspace,
        budget: &mut StepBudget,
    ) -> io::Result<HandOverStage> {
        let Some(tier) = self.handing_over.pop() else { return Ok(HandOverStage::Stamp) };
        let ns = tier.ns;
        let table = ks
            .tiered_store_mut(ns)
            .ok_or_else(|| io_msg(format!("ns {}: a boot machine with no table", ns.0)))?;
        let done = tier.replay.hand_over(table).map_err(|refusal| replay_refused(ns, &refusal))?;
        self.stats.tier_replay.counters.absorb(done.counters);
        let charge = boot_io_charge(done.work);
        self.step_charge = self.step_charge.saturating_add(charge);
        budget.charge(charge);
        self.recovered_tiers.push(RecoveredTierNs {
            ns,
            flush: done.handed.flush,
            files: done.handed.handles,
        });
        Ok(HandOverStage::Stamp)
    }
}

/// One turn of the rebuild: what its step did, charged — its groups at their
/// bytes, and a returned slot read through the machine's settle read and
/// settled by its full key.
struct RebuildTurn<'t, F: SegmentFs> {
    tier: &'t mut RecoveringTierNs<F>,
    table: &'t mut TieredTable,
    stats: &'t mut RecoverStats,
    step_charge: &'t mut u64,
    budget: &'t mut StepBudget,
}

impl<F: SegmentFs> RebuildTurn<'_, F> {
    /// Charges the groups the step scanned and settles the slot it
    /// returned. A slot is a cold slot the rebuild cannot pair by
    /// construction (two RAM keys with one hash beside a cold twin) or one
    /// beyond the ticket cap: read through the held creation-mode handle —
    /// the slot's key window, with the bytes left to its file's end — and
    /// settled by the full key the table verifies against the slot's hash
    /// (ADR-0174 D3, ADR-0093 A4′). An unreadable or unsettleable slot is a
    /// recovery fail-stop (corrupt input, ADR-0057's posture).
    fn settle(self, step: RebuildStep, rebuild: Box<ShadowRebuild>) -> io::Result<HandOverStage> {
        let (RebuildStep::Settle { groups, .. }
        | RebuildStep::More { groups }
        | RebuildStep::Done { groups }) = step;
        count_units(&mut self.stats.units.rebuild_groups, groups);
        charge_units(self.budget, groups, REBUILD_GROUP_CHARGE_BYTES);
        let slot = match step {
            RebuildStep::Settle { slot, .. } => slot,
            RebuildStep::More { .. } => return Ok(HandOverStage::Rebuild(rebuild)),
            RebuildStep::Done { .. } => return Ok(HandOverStage::Seal),
        };
        let window = self.tier.replay.read_key_window(slot.cold).map_err(io_invalid);
        let window = window.map(|window| {
            self.stats.shadow_settle_reads += 1;
            inf_store::KeyWindow { bytes: window.bytes.to_vec(), left: window.left }
        });
        let ns = self.tier.ns;
        self.table
            .settle_rebuilt_window(slot, window)
            .map_err(|err| io_msg(format!("ns {}: {err} (ADR-0093 A4′)", ns.0)))?;
        let charge = boot_io_charge(self.tier.replay.take_work());
        *self.step_charge = self.step_charge.saturating_add(charge);
        self.budget.charge(charge);
        Ok(HandOverStage::Rebuild(rebuild))
    }
}
