use super::Rng;
use std::collections::BTreeMap;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Rules {
    /// The decision record is appended only after every participant's
    /// prepare is durable (ADR-0116 D3).
    pub decision_waits_for_durable_prepares: bool,
    /// A checkpoint streams the pinned predecessor of a published
    /// record whose decision is not yet durable (ADR-0116 D3).
    pub checkpoint_streams_predecessor: bool,
}

impl Rules {
    pub fn chosen() -> Rules {
        Rules { decision_waits_for_durable_prepares: true, checkpoint_streams_predecessor: true }
    }

    pub fn withdrawn() -> Rules {
        Rules { decision_waits_for_durable_prepares: false, checkpoint_streams_predecessor: false }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Rec {
    Prepare { txid: u64, key: u32, value: u32 },
    Decision { txid: u64 },
}

#[derive(Clone, Debug, Default)]
struct CellLog {
    records: Vec<Rec>,
    /// Records below this index survive the crash.
    durable: usize,
    ram: BTreeMap<u32, u32>,
    /// Predecessor images pinned for published-undecided records.
    pinned: BTreeMap<u32, Option<u32>>,
    ckpt_image: BTreeMap<u32, u32>,
    ckpt_begin: usize,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Participant `c` executes, logs its prepare and publishes.
    Prepare(usize),
    /// The coordinator (cell 0) decides once both prepared.
    Decide,
    /// An everysec timer on cell `c`.
    Fsync(usize),
    /// A checkpoint on cell `c`.
    Checkpoint(usize),
    Crash,
}

/// Two participants (cell 0 also coordinates) run one transaction
/// `T` writing key 0 on cell 0 and key 1 on cell 1 over pre-images
/// `10`/`11` (each cell booted from a checkpoint holding its
/// pre-image); `steps` interleave everysec-style fsyncs and
/// checkpoints with the protocol until the crash. Returns the
/// recovered values per key, or a description of the partial commit.
pub fn run(rules: Rules, steps: &[Step]) -> Result<[u32; 2], String> {
    let txid = 7;
    let mut cells = [CellLog::default(), CellLog::default()];
    for (c, cell) in cells.iter_mut().enumerate() {
        cell.ram.insert(c as u32, 10 + c as u32);
        cell.ckpt_image.insert(c as u32, 10 + c as u32);
    }
    let mut prepared = [false, false];
    let mut decided = false;
    let mut decision_pending = false;
    for step in steps {
        match *step {
            Step::Prepare(c) if !prepared[c] => {
                prepared[c] = true;
                let cell = &mut cells[c];
                cell.records.push(Rec::Prepare { txid, key: c as u32, value: 100 });
                // Publication (intents released) — the pre-image stays
                // pinned until the decision is durable.
                let old = cell.ram.insert(c as u32, 100);
                cell.pinned.insert(c as u32, old);
            }
            Step::Prepare(_) => {}
            Step::Decide if prepared.iter().all(|p| *p) && !decided => {
                decided = true;
                decision_pending = true;
            }
            Step::Decide => {}
            Step::Fsync(c) => cells[c].durable = cells[c].records.len(),
            Step::Checkpoint(c) => {
                let decision_durable = decision_durable(&cells, txid);
                let cell = &mut cells[c];
                let mut begin = cell.records.len();
                let mut image = BTreeMap::new();
                for (k, v) in &cell.ram {
                    let pending = cell.pinned.contains_key(k) && !decision_durable;
                    if pending && rules.checkpoint_streams_predecessor {
                        if let Some(Some(old)) = cell.pinned.get(k) {
                            image.insert(*k, *old);
                        }
                    } else {
                        image.insert(*k, *v);
                    }
                }
                cell.ckpt_image = image;
                // The tail must still carry an undecided prepare.
                if !decision_durable
                    && let Some(pos) =
                        cell.records.iter().position(|r| matches!(r, Rec::Prepare { .. }))
                {
                    begin = begin.min(pos);
                }
                cell.ckpt_begin = begin;
            }
            Step::Crash => break,
        }
        if decision_pending {
            let prepares_durable = cells.iter().all(|c| {
                c.records.iter().take(c.durable).any(|r| matches!(r, Rec::Prepare { .. }))
            });
            if prepares_durable || !rules.decision_waits_for_durable_prepares {
                cells[0].records.push(Rec::Decision { txid });
                decision_pending = false;
            }
        }
        if decision_durable(&cells, txid) {
            for cell in &mut cells {
                cell.pinned.clear();
            }
        }
    }
    // Recovery: every cell replays checkpoint + durable tail, parks
    // prepares, and resolves them against the coordinator's log.
    let decision = decision_durable(&cells, txid);
    let mut out = [0u32; 2];
    for (c, cell) in cells.iter().enumerate() {
        let mut state = cell.ckpt_image.clone();
        let tail = &cell.records[cell.ckpt_begin.min(cell.durable)..cell.durable];
        for rec in tail {
            if let Rec::Prepare { key, value, .. } = rec
                && decision
            {
                state.insert(*key, *value);
            }
        }
        out[c] = *state.get(&(c as u32)).unwrap_or(&0);
    }
    let all = out.iter().all(|v| *v == 100);
    let none = out.iter().all(|v| *v != 100);
    if all || none {
        Ok(out)
    } else {
        Err(format!(
            "PARTIAL COMMIT after crash: key0 = {}, key1 = {} (decision durable: {decision})",
            out[0], out[1]
        ))
    }
}

fn decision_durable(cells: &[CellLog; 2], txid: u64) -> bool {
    cells[0].records.iter().take(cells[0].durable).any(|r| *r == Rec::Decision { txid })
}

/// The everysec history the review named: both participants publish,
/// the coordinator decides, cell 1 checkpoints its published record,
/// the crash arrives before any fsync.
pub const EVERYSEC_HALF_CHECKPOINT: [Step; 5] =
    [Step::Prepare(0), Step::Prepare(1), Step::Decide, Step::Checkpoint(1), Step::Crash];

/// A seeded interleaving of protocol steps, fsyncs and checkpoints.
pub fn random_steps(seed: u64, len: usize) -> Vec<Step> {
    let mut rng = Rng::new(seed);
    let mut out = Vec::with_capacity(len + 1);
    for _ in 0..len {
        out.push(match rng.below(7) {
            0 => Step::Prepare(0),
            1 => Step::Prepare(1),
            2 => Step::Decide,
            3 => Step::Fsync(0),
            4 => Step::Fsync(1),
            5 => Step::Checkpoint(0),
            _ => Step::Checkpoint(1),
        });
    }
    out.push(Step::Crash);
    out
}
