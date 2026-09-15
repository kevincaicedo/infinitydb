use super::Rng;
use std::collections::{BTreeMap, BTreeSet};

pub type Txid = u64;
pub type Key = usize;

/// D3's "pinned bytes are bounded by the retention cap × frame bound"
/// against a counted budget (ADR-0116 A10).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Rules {
    /// The qualified image's bytes — hot arena bytes, or a cold
    /// extent's bytes — are charged against the cell's pinned budgets
    /// at the grant for every written key not already pending, refused
    /// typed when over, and released when the key's pending set is
    /// durably decided (A10). `false` is D3 as written: nothing is
    /// charged; the frame bound is claimed to bound retention.
    pub charge_retained_images_at_grant: bool,
}

impl Rules {
    pub fn chosen() -> Rules {
        Rules { charge_retained_images_at_grant: true }
    }

    pub fn withdrawn() -> Rules {
        Rules { charge_retained_images_at_grant: false }
    }
}

/// The bounds in force on the cell.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Budget {
    /// D5: one publishable frame per transaction.
    pub frame_max: u64,
    /// S22: pending (published, not durably decided) decisions.
    pub retention_cap: usize,
    /// A10: `tx-pinned-max-bytes` (arena).
    pub pinned_max: u64,
    /// A10: `tx-pinned-max-extent-bytes` (cold storage).
    pub pinned_extent_max: u64,
}

/// A key's live image.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Image {
    Absent,
    Hot(u64),
    Cold(u64),
}

impl Image {
    fn hot(self) -> u64 {
        match self {
            Image::Hot(b) => b,
            _ => 0,
        }
    }

    fn extent(self) -> u64 {
        match self {
            Image::Cold(b) => b,
            _ => 0,
        }
    }
}

/// A staged effect and its serialized log bytes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// A tombstone: `TOMBSTONE_BYTES` in the frame, whatever it pins.
    Delete,
    Set(u64),
}

pub const TOMBSTONE_BYTES: u64 = 24;

impl Effect {
    fn staged(self) -> u64 {
        match self {
            Effect::Delete => TOMBSTONE_BYTES,
            Effect::Set(b) => b,
        }
    }

    fn apply(self) -> Image {
        match self {
            Effect::Delete => Image::Absent,
            Effect::Set(b) => Image::Hot(b),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Txn {
    pub id: Txid,
    pub writes: Vec<(Key, Effect)>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// The grant: admission against every bound; a refused transaction
    /// holds nothing.
    Admit(Txid),
    /// The in-memory decision publishes the staged effects.
    Publish(Txid),
    /// The decision record is durable on the coordinator.
    Decide(Txid),
    /// A plain write outside any transaction.
    Plain(Key, Effect),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Refusal {
    /// D5: the staged bytes exceed one frame.
    Frame,
    /// S22: the pending-decision cap.
    Retention,
    /// A10: the pinned budgets (`INF TXRETAIN`).
    Retain,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub committed: usize,
    pub refused: BTreeMap<Refusal, usize>,
    pub max_retained: u64,
    pub max_staged: u64,
}

/// One key's pending run: the qualified image and every txid it waits
/// for (A2).
#[derive(Clone, Debug)]
struct Run {
    base: Image,
    pending: BTreeSet<Txid>,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct Charge {
    hot: u64,
    extent: u64,
}

struct Model<'a> {
    rules: Rules,
    budget: Budget,
    txns: &'a [Txn],
    images: Vec<Image>,
    runs: BTreeMap<Key, Run>,
    /// Granted, not yet published: the intents held and, per written
    /// key, the live image's bytes reserved at the grant.
    admitted: BTreeMap<Txid, Vec<(Key, Charge)>>,
    published: BTreeSet<Txid>,
    decided: BTreeSet<Txid>,
    /// `tx_pinned_bytes` / `tx_pinned_extent_bytes` as accounted.
    accounted: Charge,
    stats: Stats,
}

/// Runs `steps` over `keys` with the given images; every published
/// transaction is decided at the end so the leak check is total.
/// Returns the stats, or the violation.
pub fn run(
    rules: Rules,
    budget: Budget,
    images: &[Image],
    txns: &[Txn],
    steps: &[Step],
) -> Result<Stats, String> {
    let mut m = Model::new(rules, budget, images, txns);
    for step in steps {
        m.apply(*step);
        m.check()?;
    }
    for t in m.admitted.keys().copied().collect::<Vec<_>>() {
        m.apply(Step::Publish(t));
        m.check()?;
    }
    for t in m.published.clone() {
        m.apply(Step::Decide(t));
        m.check()?;
    }
    m.finish()
}

impl<'a> Model<'a> {
    fn new(rules: Rules, budget: Budget, images: &[Image], txns: &'a [Txn]) -> Model<'a> {
        Model {
            rules,
            budget,
            txns,
            images: images.to_vec(),
            runs: BTreeMap::new(),
            admitted: BTreeMap::new(),
            published: BTreeSet::new(),
            decided: BTreeSet::new(),
            accounted: Charge::default(),
            stats: Stats::default(),
        }
    }

    fn txn(&self, t: Txid) -> &'a Txn {
        self.txns.iter().find(|x| x.id == t).expect("known txid")
    }

    /// A W intent on `k` is held by an admitted, unpublished transaction.
    fn held(&self, k: Key) -> bool {
        self.admitted.keys().any(|t| self.txn(*t).writes.iter().any(|(w, _)| *w == k))
    }

    fn pending_decisions(&self) -> usize {
        self.published.difference(&self.decided).count()
    }

    /// What the grant reserves: the live image of every written key,
    /// each key once. A key already pending returns it at publication
    /// (A2: its qualified image is already pinned) unless its run was
    /// released under the intent, in which case this is the new pin.
    fn charge_of(&self, txn: &Txn) -> Vec<(Key, Charge)> {
        let keys: BTreeSet<Key> = txn.writes.iter().map(|(k, _)| *k).collect();
        keys.iter()
            .map(|k| (*k, Charge { hot: self.images[*k].hot(), extent: self.images[*k].extent() }))
            .collect()
    }

    fn total(charges: &[(Key, Charge)]) -> Charge {
        charges.iter().fold(Charge::default(), |c, (_, x)| Charge {
            hot: c.hot + x.hot,
            extent: c.extent + x.extent,
        })
    }

    fn refusal(&self, txn: &Txn, charge: Charge) -> Option<Refusal> {
        let staged: u64 = txn.writes.iter().map(|(_, e)| e.staged()).sum();
        if staged > self.budget.frame_max {
            return Some(Refusal::Frame);
        }
        if self.pending_decisions() + self.admitted.len() >= self.budget.retention_cap {
            return Some(Refusal::Retention);
        }
        let over_hot = self.accounted.hot + charge.hot > self.budget.pinned_max;
        let over_extent = self.accounted.extent + charge.extent > self.budget.pinned_extent_max;
        if self.rules.charge_retained_images_at_grant && (over_hot || over_extent) {
            return Some(Refusal::Retain);
        }
        None
    }

    fn apply(&mut self, step: Step) {
        match step {
            Step::Admit(t) => self.admit(t),
            Step::Publish(t) => self.publish(t),
            Step::Decide(t) => self.decide(t),
            Step::Plain(k, effect) => {
                if !self.held(k) {
                    // A dependent write keeps the qualified image (A2);
                    // an independent one simply replaces its predecessor.
                    self.images[k] = effect.apply();
                }
            }
        }
    }

    fn admit(&mut self, t: Txid) {
        if self.admitted.contains_key(&t) || self.published.contains(&t) {
            return;
        }
        let txn = self.txn(t);
        if txn.writes.iter().any(|(k, _)| self.held(*k)) {
            return;
        }
        let charges = self.charge_of(txn);
        let charge = Self::total(&charges);
        if let Some(why) = self.refusal(txn, charge) {
            *self.stats.refused.entry(why).or_default() += 1;
            return;
        }
        let staged: u64 = txn.writes.iter().map(|(_, e)| e.staged()).sum();
        self.stats.max_staged = self.stats.max_staged.max(staged);
        if self.rules.charge_retained_images_at_grant {
            self.accounted.hot += charge.hot;
            self.accounted.extent += charge.extent;
        }
        self.admitted.insert(t, charges);
    }

    /// The reservation becomes the pin where the key was not pending
    /// (the image cannot have changed under the intent) and returns
    /// where it was.
    fn publish(&mut self, t: Txid) {
        let Some(charges) = self.admitted.remove(&t) else { return };
        let txn = self.txn(t);
        for (k, charge) in charges {
            if self.runs.contains_key(&k) && self.rules.charge_retained_images_at_grant {
                self.accounted.hot -= charge.hot;
                self.accounted.extent -= charge.extent;
            }
            let old = self.images[k];
            self.runs
                .entry(k)
                .or_insert(Run { base: old, pending: BTreeSet::new() })
                .pending
                .insert(t);
        }
        for (k, effect) in &txn.writes {
            self.images[*k] = effect.apply();
        }
        self.published.insert(t);
        self.stats.committed += 1;
    }

    /// A durable decision releases every pin whose pending set is
    /// decided, and its bytes.
    fn decide(&mut self, t: Txid) {
        if !self.published.contains(&t) {
            return;
        }
        self.decided.insert(t);
        let decided = self.decided.clone();
        let released: Vec<Key> = self
            .runs
            .iter()
            .filter(|(_, r)| r.pending.is_subset(&decided))
            .map(|(k, _)| *k)
            .collect();
        for k in released {
            let run = self.runs.remove(&k).expect("released run");
            if self.rules.charge_retained_images_at_grant {
                self.accounted.hot -= run.base.hot();
                self.accounted.extent -= run.base.extent();
            }
        }
    }

    /// What the cell really retains: the qualified image of every
    /// pending key.
    fn retained(&self) -> Charge {
        self.runs.values().fold(Charge::default(), |c, r| Charge {
            hot: c.hot + r.base.hot(),
            extent: c.extent + r.base.extent(),
        })
    }

    /// Charges reserved at the grant and not yet converted or returned.
    fn reserved(&self) -> Charge {
        self.admitted.values().fold(Charge::default(), |c, charges| {
            let x = Self::total(charges);
            Charge { hot: c.hot + x.hot, extent: c.extent + x.extent }
        })
    }

    fn check(&mut self) -> Result<(), String> {
        let retained = self.retained();
        self.stats.max_retained = self.stats.max_retained.max(retained.hot + retained.extent);
        if !self.rules.charge_retained_images_at_grant {
            let claimed = self.budget.retention_cap as u64 * self.budget.frame_max;
            if retained.hot + retained.extent > claimed {
                return Err(format!(
                    "RETAINED BYTES EXCEED THE CLAIMED BOUND: {} pending transaction(s) staged at \
                         most {} B each but retain {} B of images and {} B of extents — above the \
                         retention cap × frame bound of {claimed} B",
                    self.pending_decisions(),
                    self.stats.max_staged,
                    retained.hot,
                    retained.extent
                ));
            }
            return Ok(());
        }
        let reserved = self.reserved();
        let held =
            Charge { hot: retained.hot + reserved.hot, extent: retained.extent + reserved.extent };
        if self.accounted != held {
            return Err(format!(
                "PIN ACCOUNTING DRIFT: tx_pinned_bytes {} / tx_pinned_extent_bytes {} but the cell \
                     retains {} / {} and reserves {} / {}",
                self.accounted.hot,
                self.accounted.extent,
                retained.hot,
                retained.extent,
                reserved.hot,
                reserved.extent
            ));
        }
        if held.hot > self.budget.pinned_max || held.extent > self.budget.pinned_extent_max {
            return Err(format!(
                "PINNED BYTES OVER BUDGET: {} B of images (budget {}) and {} B of extents (budget \
                     {})",
                held.hot, self.budget.pinned_max, held.extent, self.budget.pinned_extent_max
            ));
        }
        Ok(())
    }

    fn finish(self) -> Result<Stats, String> {
        let retained = self.retained();
        if retained != Charge::default()
            || (self.rules.charge_retained_images_at_grant && self.accounted != Charge::default())
        {
            return Err(format!(
                "PIN LEAK: {} B of images and {} B of extents (accounted {} / {}) retained after \
                     every decision is durable",
                retained.hot, retained.extent, self.accounted.hot, self.accounted.extent
            ));
        }
        Ok(self.stats)
    }
}

pub const MIB: u64 = 1 << 20;

/// The review's budget: 64 B frames, four pending decisions, 2 MiB of
/// pinned images, 4 MiB of pinned extents.
pub fn budget() -> Budget {
    Budget { frame_max: 64, retention_cap: 4, pinned_max: 2 * MIB, pinned_extent_max: 4 * MIB }
}

/// F08: four 1 MiB records, four transactions each deleting one with a
/// 24 B tombstone, no decision durable yet.
pub fn tombstone_storm() -> (Vec<Image>, Vec<Txn>, Vec<Step>) {
    let images = vec![Image::Hot(MIB); 4];
    let txns: Vec<Txn> =
        (0..4).map(|k| Txn { id: k as Txid + 1, writes: vec![(k, Effect::Delete)] }).collect();
    let mut steps = Vec::new();
    for t in 1..=4 {
        steps.push(Step::Admit(t));
        steps.push(Step::Publish(t));
    }
    (images, txns, steps)
}

/// The same storm, then the decisions land and the refused
/// transactions are offered again.
pub fn tombstone_storm_then_decisions() -> (Vec<Image>, Vec<Txn>, Vec<Step>) {
    let (images, txns, mut steps) = tombstone_storm();
    steps.extend([Step::Decide(1), Step::Decide(2)]);
    for t in 3..=4 {
        steps.push(Step::Admit(t));
        steps.push(Step::Publish(t));
    }
    (images, txns, steps)
}

/// A cold document's extent pinned by a tombstone; a dependent plain
/// write and a second pending write over the same key pin nothing new.
pub fn cold_extent_and_dependents() -> (Vec<Image>, Vec<Txn>, Vec<Step>) {
    let images = vec![Image::Cold(3 * MIB), Image::Hot(MIB)];
    let txns = vec![
        Txn { id: 1, writes: vec![(0, Effect::Delete), (1, Effect::Set(8))] },
        Txn { id: 2, writes: vec![(1, Effect::Set(16))] },
    ];
    let steps = vec![
        Step::Admit(1),
        Step::Publish(1),
        Step::Plain(1, Effect::Set(32)),
        Step::Admit(2),
        Step::Publish(2),
        Step::Decide(1),
        Step::Decide(2),
    ];
    (images, txns, steps)
}

/// Seeded keys, images, transactions and interleavings.
pub fn random(seed: u64) -> (Vec<Image>, Vec<Txn>, Vec<Step>) {
    let mut rng = Rng::new(seed);
    let keys = 4;
    let images: Vec<Image> = (0..keys)
        .map(|_| match rng.below(3) {
            0 => Image::Absent,
            1 => Image::Hot(1 + rng.next_u64() % MIB),
            _ => Image::Cold(1 + rng.next_u64() % (2 * MIB)),
        })
        .collect();
    let effect = |rng: &mut Rng| {
        if rng.below(2) == 0 { Effect::Delete } else { Effect::Set(rng.below(40) as u64) }
    };
    let txns: Vec<Txn> = (1..=6)
        .map(|id| {
            let mut writes = vec![(rng.below(keys), effect(&mut rng))];
            let k = rng.below(keys);
            if writes[0].0 != k {
                writes.push((k, effect(&mut rng)));
            }
            Txn { id, writes }
        })
        .collect();
    let steps = (0..24)
        .map(|_| match rng.below(4) {
            0 => Step::Admit(1 + rng.below(6) as Txid),
            1 => Step::Publish(1 + rng.below(6) as Txid),
            2 => Step::Decide(1 + rng.below(6) as Txid),
            _ => Step::Plain(rng.below(keys), effect(&mut rng)),
        })
        .collect();
    (images, txns, steps)
}
