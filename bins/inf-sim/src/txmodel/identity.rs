use super::Rng;
use std::collections::BTreeSet;

/// The D9 rule the 2026-09-10 review reopened (ADR-0116 A3).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Rules {
    /// A boot resumes above the last durable `txid-reserve` high-water,
    /// which is fsynced before any txid in its range leaves the cell
    /// (A3). `false` is D9 as written: above the highest txid the
    /// cell's own log replayed.
    pub resume_from_durable_reservation: bool,
    /// The checkpoint carries the reservation high-water, so truncating
    /// the tail never loses it (A3). `false`: the reservation records
    /// are truncated with the tail.
    pub checkpoint_carries_reservation: bool,
}

impl Rules {
    pub fn chosen() -> Rules {
        Rules { resume_from_durable_reservation: true, checkpoint_carries_reservation: true }
    }

    pub fn withdrawn() -> Rules {
        Rules { resume_from_durable_reservation: false, checkpoint_carries_reservation: false }
    }
}

/// Reservation chunk — small so the model crosses chunk boundaries.
pub const CHUNK: u64 = 16;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Rec {
    Reserve { upto: u64 },
    Tag { seq: u64 },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// The coordinator issues the next txid to a transaction.
    Issue,
    /// The coordinator logs its own leg's prepare for the last txid.
    LocalPrepare,
    /// A remote participant's durable prepare carries the last txid.
    RemoteSurvives,
    Fsync,
    /// The tail is truncated behind a checkpoint.
    Checkpoint,
    /// Non-durable local records are lost; the cell reboots.
    Crash,
}

/// One coordinator cell (cell 0) issuing txids against remote
/// participants whose durable prepares outlive the coordinator's own
/// records. Returns the number of txids issued, or the reissue.
pub fn run(rules: Rules, steps: &[Step]) -> Result<u64, String> {
    let mut log: Vec<Rec> = Vec::new();
    let mut durable = 0usize;
    let mut next = 1u64;
    let mut reserved_upto = 0u64;
    let mut ckpt_reserved = 0u64;
    let mut last_issued = None;
    let mut remote = BTreeSet::new();
    let mut issued = 0u64;
    for step in steps {
        match *step {
            Step::Issue => {
                if rules.resume_from_durable_reservation && next > reserved_upto {
                    reserved_upto = next + CHUNK - 1;
                    log.push(Rec::Reserve { upto: reserved_upto });
                    // Durable before any txid of the range leaves the cell.
                    durable = log.len();
                }
                let seq = next;
                next += 1;
                issued += 1;
                if remote.contains(&seq) {
                    return Err(format!(
                        "TXID REISSUE: (0, {seq}) issued again while a remote participant's \
                             durable prepare still carries it"
                    ));
                }
                last_issued = Some(seq);
            }
            Step::LocalPrepare => {
                if let Some(seq) = last_issued {
                    log.push(Rec::Tag { seq });
                }
            }
            Step::RemoteSurvives => {
                if let Some(seq) = last_issued {
                    remote.insert(seq);
                }
            }
            Step::Fsync => durable = log.len(),
            Step::Checkpoint => {
                if rules.checkpoint_carries_reservation {
                    ckpt_reserved = ckpt_reserved.max(reserved_upto);
                }
                log.clear();
                durable = 0;
            }
            Step::Crash => {
                log.truncate(durable);
                let floor = replay_floor(rules, &log, ckpt_reserved);
                next = floor + 1;
                reserved_upto = floor;
                last_issued = None;
            }
        }
    }
    Ok(issued)
}

fn replay_floor(rules: Rules, log: &[Rec], ckpt_reserved: u64) -> u64 {
    let mut replayed_max = 0;
    let mut reserved_max = ckpt_reserved;
    for rec in log {
        match rec {
            Rec::Tag { seq } => replayed_max = replayed_max.max(*seq),
            Rec::Reserve { upto } => reserved_max = reserved_max.max(*upto),
        }
    }
    if rules.resume_from_durable_reservation {
        replayed_max.max(reserved_max)
    } else {
        replayed_max
    }
}

/// F03: forty transactions persisted locally; txid 41 issued, a remote
/// prepare survives, the coordinator crashes before logging anything
/// for 41, reboots, and issues again.
pub fn remote_prepare_history() -> Vec<Step> {
    let mut steps = Vec::new();
    for _ in 0..40 {
        steps.push(Step::Issue);
        steps.push(Step::LocalPrepare);
    }
    steps.extend([Step::Fsync, Step::Issue, Step::RemoteSurvives, Step::Crash, Step::Issue]);
    steps
}

/// A seeded history of issues, local/remote prepares, fsyncs,
/// checkpoints and crashes.
pub fn random_steps(seed: u64, len: usize) -> Vec<Step> {
    let mut rng = Rng::new(seed);
    (0..len)
        .map(|_| match rng.below(9) {
            0..=2 => Step::Issue,
            3 => Step::LocalPrepare,
            4 => Step::RemoteSurvives,
            5 => Step::Fsync,
            6 => Step::Checkpoint,
            _ => Step::Crash,
        })
        .collect()
}
