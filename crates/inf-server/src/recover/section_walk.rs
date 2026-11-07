//! The section walk of the checkpoint load (ADR-0174 A1): a checkpoint step
//! that has charged its budget stops at the next record or entry, inside a
//! checkpoint section, and the next step resumes there through the reader's
//! own cursor over the section it still holds.
//!
//! A section's entry — its read and its class's audit — is one unit and
//! stays whole. A **walk** is the items from the cursor to the section's
//! end, priced at the as-built byte price and, for an entry section, at
//! [`RECOVER_PROBE_CHARGE_BYTES`] an entry beside its bytes. It is *whole*
//! when what the section has not charged yet is below what the budget has
//! left (one ask at its start, as one compare per section did), and
//! *metered* otherwise: a metered records walk asks before every record but
//! its first — the ask after the record before it — and an entry section's
//! handler applies the entries the budget left buys, at least one, so a
//! step makes progress at any budget. A record that answers `NeedsRoom` has
//! changed nothing: the walk runs the namespace's demote step under the
//! budget left and presents the record again once the step made the room,
//! or parks at it; a `DEL` that read its cold twins (a priced answer) is
//! charged, and the rest of the walk is metered. An index-sidecar section is
//! applied whole and asked after. A section's items are charged once, as
//! the walk passes them, and its header, meta, trailer and padding when it
//! ends.
//!
//! The reader moves the cursor on these answers alone. It lies below the
//! store, so no type proves that a move followed a consumed answer: budget
//! independence — every budget recovers the state of the walk that never
//! parks — holds it.
//!
//! The end of the checkpoint's releases (ADR-0174 R9) run here too, one
//! listed address a unit at the RAM price, in the settle order the lists
//! hold.

use std::io;

use inf_log::NsId;
use inf_log::ckpt::{
    IckReader, IckSectionEntry, IckSectionKind, IckStep, IckWalk, SectionFlow, ick_file_name,
};
use inf_log::fs::SegmentFs;
use inf_log::{IckBlobRefSection, IckIdxSidecarStep, IckLiveSetSection, IckRefSection, RecordView};
use inf_store::limits::RECOVER_PROBE_CHARGE_BYTES;
use inf_store::{DemoteProgress, Keyspace, ReplaySpill, Replayed};

use super::{
    BootFile, Phase, Recovery, RecoveryProgress, ReplayReader, StepBudget, io_invalid, io_msg,
};

/// One walk of the section the reader announced: its kind, metered or
/// whole, an item's RAM price and an entry's bytes, the section's price in
/// budget bytes, the price of the items the walk passed and of what the
/// section has charged (across the steps that walked it), whether a record
/// was presented in this step, and whether an earlier step parked in it.
#[derive(Copy, Clone, Debug)]
struct Walk {
    kind: IckSectionKind,
    metered: bool,
    item_price: u64,
    entry_bytes: u64,
    price: u64,
    passed: u64,
    charged: u64,
    presented: bool,
    resumed: bool,
}

impl Walk {
    /// No section announced yet (the step reads the footer).
    const NONE: Walk = Walk {
        kind: IckSectionKind::Records,
        metered: false,
        item_price: 0,
        entry_bytes: 0,
        price: 0,
        passed: 0,
        charged: 0,
        presented: false,
        resumed: false,
    };

    /// What the section has not charged yet.
    fn unpaid(&self) -> u64 {
        self.price.saturating_sub(self.charged)
    }
}

/// Where a walk parked (ADR-0174 A1.2's liveness counters).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Stop {
    /// Inside the section, at its next item.
    Section,
    /// Inside a demote step's seal walk, its record waiting for room.
    DemoteStep,
}

/// The handlers of one checkpoint step's walk over the reader's next (or
/// parked) section: the recovery machine, the keyspace and the step's
/// budget, lent for one call of the reader.
struct IckWalker<'w, F: SegmentFs + Clone> {
    rec: &'w mut Recovery<F>,
    ks: &'w mut Keyspace,
    budget: &'w mut StepBudget,
    /// Each tiered namespace's manifested flushed watermark.
    tier_flushed: &'w [(u32, u64)],
    walk: Walk,
    /// Why the walk parked, when it did.
    stop: Option<Stop>,
    /// Sections of a tombstoned namespace skipped (ADR-0100 D6).
    dropped_sections: u64,
}

impl<F: SegmentFs + Clone> IckWalker<'_, F> {
    /// Charges what the walk passed and has not charged yet.
    fn charge_passed(&mut self) {
        let unpaid = self.walk.passed.saturating_sub(self.walk.charged);
        self.budget.charge(unpaid);
        self.walk.charged = self.walk.passed;
    }

    /// A consumed record's or applied entries' price: passed, and charged at
    /// once by a metered walk (a whole walk's are charged at the section's
    /// end).
    fn pass(&mut self, price: u64) {
        self.walk.passed = self.walk.passed.saturating_add(price);
        if self.walk.metered {
            self.charge_passed();
        }
    }

    /// The walk turns metered: what it passed is charged now, and from here
    /// every item as it is passed.
    fn meter(&mut self) {
        self.walk.metered = true;
        self.charge_passed();
    }

    /// How many of an entry section's `len` entries this call applies: all
    /// of them on a whole walk, else the number the budget left buys at an
    /// entry's price, at least one.
    fn entry_batch(&mut self, len: usize) -> usize {
        if !self.walk.metered {
            return len;
        }
        let each = self.walk.entry_bytes.saturating_add(self.walk.item_price).max(1);
        let buys = (self.budget.left() / each).max(1);
        usize::try_from(buys).unwrap_or(usize::MAX).min(len)
    }

    /// `applied` of an entry section's `len` entries were applied: counted
    /// as units and passed; fewer than given parks the walk after them.
    fn applied(&mut self, applied: usize, len: usize) {
        let count = u64::try_from(applied).unwrap_or(u64::MAX);
        let units = &mut self.rec.stats.units.ckpt_entries;
        *units = units.saturating_add(count);
        let each = self.walk.entry_bytes.saturating_add(self.walk.item_price);
        self.pass(count.saturating_mul(each));
        if applied < len {
            self.stop = Some(Stop::Section);
        }
    }

    /// A namespace whose drop tombstone explains its section: its entries
    /// are skipped (ADR-0100 D6), the section counted once, at its entry.
    fn tombstoned(&mut self, ns: u32, first_index: u32) -> bool {
        let skip = self.ks.ns_tombstoned(NsId(ns));
        if skip && first_index == 0 {
            self.dropped_sections += 1;
        }
        skip
    }

    /// The tiered table of an entry section's namespace.
    fn table(&mut self, ns: u32, what: &str) -> io::Result<&mut inf_store::TieredTable> {
        self.ks
            .tiered_store_mut(NsId(ns))
            .ok_or_else(|| io_msg(format!("{what} section for unknown tier ns {ns}")))
    }

    /// A record that answered `NeedsRoom` in `ns`: the walk turns metered,
    /// the demote step runs under what the budget has left and its boot I/O
    /// is charged.
    fn make_room(&mut self, ns: NsId) -> io::Result<DemoteProgress> {
        self.meter();
        let left = self.budget.left();
        let (Some(machine), Some(table)) =
            (self.rec.recovering_tiers.replay_mut(ns), self.ks.tiered_store_mut(ns))
        else {
            return Err(io_msg(format!(
                "ns {}: a checkpoint record needs room with no machine",
                ns.0
            )));
        };
        let progress = machine.demote_step(table, left).map_err(|refusal| refused(ns, &refusal))?;
        self.rec.drain_into(self.budget);
        Ok(progress)
    }
}

impl<F: SegmentFs + Clone> IckWalk<io::Error> for IckWalker<'_, F> {
    fn hybrid(&self) -> bool {
        true
    }

    fn enter(&mut self, entry: IckSectionEntry) {
        let entry_bytes = entry.kind.entry_width().map_or(0, |width| width as u64);
        let item_price = if entry_bytes > 0 { RECOVER_PROBE_CHARGE_BYTES } else { 0 };
        let price = entry.block_bytes.saturating_add(u64::from(entry.items) * item_price);
        let done = entry.item_bytes_done.saturating_add(u64::from(entry.items_done) * item_price);
        let mut walk = Walk {
            kind: entry.kind,
            metered: false,
            item_price,
            entry_bytes,
            price,
            passed: done,
            charged: done,
            presented: false,
            resumed: entry.resumed,
        };
        // An index-sidecar section is applied whole and asked after; every
        // other walk asks once, at its start.
        if entry.kind != IckSectionKind::IdxSidecar {
            walk.metered = walk.unpaid() >= self.budget.left();
        }
        self.walk = walk;
    }

    fn record(&mut self, view: RecordView<'_>, bytes: u32) -> io::Result<SectionFlow> {
        // The ask after the record before this one (a metered walk's).
        if self.walk.metered && self.walk.presented && self.budget.spent() {
            self.stop = Some(Stop::Section);
            return Ok(SectionFlow::Park);
        }
        self.walk.presented = true;
        let (now, anchor) = (self.rec.now, self.rec.anchor);
        // Bound: one presentation, plus one more per demote step that made
        // room (a record asks for room at most four times).
        loop {
            let answer = self
                .ks
                .apply_record(&view, now, anchor, &mut self.rec.recovering_tiers)
                .map_err(io_invalid)?;
            match answer {
                Replayed::Consumed(token) => drop(token),
                Replayed::ConsumedPriced(token) => {
                    drop(token);
                    // Its reads are charged, and the rest of the walk metered.
                    self.rec.drain_into(self.budget);
                    self.meter();
                }
                Replayed::NeedsRoom(ns) => {
                    match self.make_room(ns)? {
                        DemoteProgress::More => {
                            self.stop = Some(Stop::DemoteStep);
                            return Ok(SectionFlow::Park);
                        }
                        DemoteProgress::Room if self.budget.spent() => {
                            self.stop = Some(Stop::Section);
                            return Ok(SectionFlow::Park);
                        }
                        // The room is made: the record again.
                        DemoteProgress::Room => continue,
                    }
                }
            }
            self.pass(u64::from(bytes));
            return Ok(SectionFlow::Next);
        }
    }

    fn refs(&mut self, section: IckRefSection<'_>) -> io::Result<usize> {
        let len = section.len();
        if self.tombstoned(section.ns, section.first_index) {
            return Ok(len);
        }
        let flushed = self
            .tier_flushed
            .iter()
            .find(|(id, _)| *id == section.ns)
            .map(|&(_, flushed)| flushed)
            .ok_or_else(|| {
                io_msg(format!("ref section for unmanifested tier ns {}", section.ns))
            })?;
        let max = self.entry_batch(len);
        let table = self.table(section.ns, "ref")?;
        let applied = inf_store::apply_ref_entries(table, &section, flushed, max)?;
        self.applied(applied, len);
        Ok(applied)
    }

    fn live_set(&mut self, section: IckLiveSetSection<'_>) -> io::Result<usize> {
        let len = section.len();
        if self.tombstoned(section.ns, section.first_index) {
            return Ok(len);
        }
        let max = self.entry_batch(len);
        let table = self.table(section.ns, "live-set")?;
        let applied = inf_store::apply_live_set_entries(table, &section, max);
        self.applied(applied, len);
        Ok(applied)
    }

    fn blob_refs(&mut self, section: IckBlobRefSection<'_>) -> io::Result<usize> {
        let len = section.len();
        if self.tombstoned(section.ns, section.first_index) {
            return Ok(len);
        }
        let max = self.entry_batch(len);
        let table = self.table(section.ns, "blob-ref")?;
        let applied = inf_store::apply_blob_ref_entries(table, &section, max);
        self.applied(applied, len);
        Ok(applied)
    }

    /// Index sidecars (M4.5-S06, ADR-0078 D4/D6): every verdict is a
    /// loader-state transition — body damage is counted and the read
    /// continues; a boot is never refused past the file's framing audit.
    fn idx_sidecar(&mut self, step: IckIdxSidecarStep<'_>) -> io::Result<()> {
        #[cfg(feature = "doc")]
        {
            let sidecar = self.rec.sidecar.as_mut().expect("loader present until the audit");
            match step {
                IckIdxSidecarStep::Section(section) => sidecar.apply_section(self.ks, &section),
                IckIdxSidecarStep::Damaged { at } => {
                    eprintln!(
                        "cell {}: damaged index-sidecar section at offset {at} — unattributed; \
                         affected streams resolve as incomplete (ADR-0078 D4)",
                        self.rec.cell
                    );
                    sidecar.note_damaged();
                }
            }
            Ok(())
        }
        // A slim build refuses index-bearing catalogs at seed (ADR-0075
        // D2.5), so this arm is typed, never reached on a healthy node.
        #[cfg(not(feature = "doc"))]
        {
            let at = match step {
                IckIdxSidecarStep::Section(ref s) => u64::from(s.ns),
                IckIdxSidecarStep::Damaged { at } => at,
            };
            Err(io_msg(format!(
                "index-sidecar section (near {at}) but this build carries no document support"
            )))
        }
    }
}

impl<F: SegmentFs + Clone> Recovery<F> {
    /// One checkpoint step: sections walked under `budget` — the one it
    /// entered or resumes, then each next one while the budget lasts — until
    /// a walk parks, a metered walk or an index-sidecar section ends with
    /// the budget spent, or the footer validates.
    pub(super) fn step_ick(
        &mut self,
        ks: &mut Keyspace,
        mut reader: Box<IckReader<BootFile<F>>>,
        budget: &mut StepBudget,
    ) -> io::Result<RecoveryProgress> {
        let manifest = self.manifest.as_ref().expect("ick phase");
        let ick_path = self.ckpt_dir.join(ick_file_name(manifest.ckpt_id));
        // Tiered section routing (M4-S26; ADR-0057 D3/D6): the per-ns
        // manifested flushed watermark cross-checks every ref section.
        let tier_flushed: Vec<(u32, u64)> =
            manifest.tiers.iter().map(|t| (t.ns, t.flushed)).collect();
        // Bound: the checkpoint's sections; a step leaves at its budget.
        loop {
            let mut walker = IckWalker {
                rec: self,
                ks: &mut *ks,
                budget: &mut *budget,
                tier_flushed: &tier_flushed,
                walk: Walk::NONE,
                stop: None,
                dropped_sections: 0,
            };
            let step = reader.next_step_walk(&mut walker);
            let (walk, stop, dropped) = (walker.walk, walker.stop, walker.dropped_sections);
            self.stats.dropped_sections_skipped += dropped;
            let step =
                step.map_err(|err| io_msg(format!("checkpoint {}: {err:?}", ick_path.display())))?;
            match step {
                IckStep::Section { bytes } => {
                    self.ick_credited += bytes;
                    self.bytes_done += bytes;
                    self.bytes_consumed += bytes;
                    // The section's price not charged yet — all of it for a
                    // whole walk, its header, meta, trailer and padding for a
                    // metered one — and its boot I/O.
                    budget.charge(walk.unpaid());
                    self.drain_into(budget);
                    let sidecar = walk.kind == IckSectionKind::IdxSidecar;
                    if (walk.metered || sidecar) && budget.spent() {
                        self.phase = Phase::Ick { reader };
                        return Ok(RecoveryProgress::Working);
                    }
                }
                IckStep::Parked => {
                    self.drain_into(budget);
                    if !walk.resumed {
                        self.step_parked_bytes = walk.unpaid();
                    }
                    let facts = &mut self.stats.step;
                    match stop {
                        Some(Stop::DemoteStep) => facts.yields_in_demote_step += 1,
                        Some(Stop::Section) | None => facts.yields_in_section += 1,
                    }
                    self.phase = Phase::Ick { reader };
                    return Ok(RecoveryProgress::Working);
                }
                IckStep::Done(summary) => {
                    self.stats.ckpt_records = summary.records;
                    self.end_ick(ks, &reader);
                    return Ok(RecoveryProgress::Working);
                }
            }
        }
    }

    /// The checkpoint's footer validated: the sidecar streams close, the
    /// file's header and footer are credited, and every tiered machine's
    /// release list closes — the next phase releases what they list, else
    /// the tail replays.
    fn end_ick(&mut self, ks: &mut Keyspace, reader: &IckReader<BootFile<F>>) {
        // Sidecar streams close with the checkpoint: open ones discard as
        // incomplete, and `CatchUp` arms on every loaded namespace *before*
        // the first tail record applies (ADR-0078 D6).
        #[cfg(feature = "doc")]
        if let Some(sidecar) = self.sidecar.as_mut() {
            sidecar.finish_load(ks);
        }
        #[cfg(not(feature = "doc"))]
        {
            let _ = ks;
        }
        // Header + footer bytes complete the file's credit.
        let rest = reader.file_size().saturating_sub(self.ick_credited);
        self.bytes_done += rest;
        self.bytes_consumed += rest;
        self.stats.phases.ckpt_bytes = self.bytes_consumed;
        // ADR-0174 R9: the end of the checkpoint, before the tail.
        let mut listed = 0usize;
        for tier in &mut self.recovering_tiers.tiers {
            listed = listed.saturating_add(tier.replay.close_checkpoint());
        }
        self.phase = if listed > 0 {
            Phase::CkptEnd { tier: 0, at: 0 }
        } else {
            Phase::Replay { idx: 0, reader: ReplayReader::Unopened }
        };
    }

    /// One step of the end of the checkpoint's releases (ADR-0174 R9): from
    /// `(tier, at)`, each machine's listed addresses in their settle order,
    /// as many a call as the budget left buys at the RAM price (at least
    /// one), until the budget is spent; a machine's list is freed once
    /// walked. Then the tail replays.
    pub(super) fn step_ckpt_end(
        &mut self,
        ks: &mut Keyspace,
        mut tier: usize,
        mut at: usize,
        budget: &mut StepBudget,
    ) -> io::Result<RecoveryProgress> {
        // Bound: one call a batch of listed addresses, at most the budget
        // over the price plus one a step.
        while let Some(machine) = self.recovering_tiers.tiers.get_mut(tier) {
            if at >= machine.replay.releases_listed() {
                machine.replay.end_releases();
                (tier, at) = (tier + 1, 0);
                continue;
            }
            if budget.spent() {
                self.phase = Phase::CkptEnd { tier, at };
                return Ok(RecoveryProgress::Working);
            }
            let buys = (budget.left() / RECOVER_PROBE_CHARGE_BYTES).max(1);
            let max = usize::try_from(buys).unwrap_or(usize::MAX);
            let ns = machine.ns;
            let table = ks
                .tiered_store_mut(ns)
                .ok_or_else(|| io_msg(format!("ns {}: a boot machine with no table", ns.0)))?;
            let walked = machine.replay.release_from(table, at, max);
            at = at.saturating_add(walked);
            let count = u64::try_from(walked).unwrap_or(u64::MAX);
            let units = &mut self.stats.units.ckpt_end_releases;
            *units = units.saturating_add(count);
            budget.charge(count.saturating_mul(RECOVER_PROBE_CHARGE_BYTES));
        }
        self.phase = Phase::Replay { idx: 0, reader: ReplayReader::Unopened };
        Ok(RecoveryProgress::Working)
    }
}

fn refused(ns: NsId, refusal: &inf_store::ReplayRefusal) -> io::Error {
    super::replay_refused(ns, refusal)
}
