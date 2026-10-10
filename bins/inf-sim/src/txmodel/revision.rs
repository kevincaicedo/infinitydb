use super::Rng;
use std::collections::BTreeSet;

/// Field widths, shrunk so every wrap and the exhaustion are reachable
/// in a short history. The real widths are u24 and u32 (ADR-0114 D6);
/// the arithmetic in the ADR scales these.
pub const VERSION_BITS: u32 = 3;
pub const INCARNATION_BITS: u32 = 4;
/// Reservation chunk — small so the model crosses chunk boundaries.
pub const CHUNK: u64 = 4;

/// The D8 rule the 2026-09-10 review reopened (ADR-0116 A7).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Rules {
    /// The mutation that would wrap the version re-incarnates the
    /// record instead (A7). `false` is D8 as written: the version
    /// wraps and the incarnation stands.
    pub reincarnate_on_wrap: bool,
    /// The incarnation counter resumes from a durable, checkpoint-
    /// carried reservation (A7, A3's shape). `false`: from the highest
    /// incarnation the replayed records carry.
    pub resume_from_durable_reservation: bool,
    /// An exhausted counter refuses the create (`INF REVEXHAUSTED`).
    /// `false`: the counter wraps to zero.
    pub refuse_at_exhaustion: bool,
}

impl Rules {
    pub fn chosen() -> Rules {
        Rules {
            reincarnate_on_wrap: true,
            resume_from_durable_reservation: true,
            refuse_at_exhaustion: true,
        }
    }

    pub fn withdrawn() -> Rules {
        Rules {
            reincarnate_on_wrap: false,
            resume_from_durable_reservation: false,
            refuse_at_exhaustion: false,
        }
    }
}

/// `RecordRevision` — the `IF REV` token a client retains.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Token {
    pub incarnation: u64,
    pub version: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// `SET` of the key: a create when absent, an update when present.
    Create,
    /// A mutation of the present key (no-op when absent).
    Update,
    Delete,
    Fsync,
    /// The tail is truncated behind a checkpoint image of the key.
    Checkpoint,
    /// Non-durable records are lost; the cell reboots.
    Crash,
}

#[derive(Copy, Clone, Debug)]
enum Rec {
    Reserve { upto: u64 },
    Live(Token),
    Tombstone,
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub issued: u64,
    pub reincarnations: u64,
    pub refused: u64,
    pub boots: u64,
}

/// One key on one cell, one history. A token returned to a client is
/// a durable fact once its record is; the violation is a token issued
/// twice for distinct durable mutations. (A token acked under
/// `everysec` for a write the crash window lost may be reissued — the
/// same window as the write, disclosed in the ADR.)
pub fn run(rules: Rules, steps: &[Step]) -> Result<Stats, String> {
    let mut state = State { next_inc: 1, ..State::default() };
    for step in steps {
        match step {
            Step::Create | Step::Update => state.write(rules, *step)?,
            Step::Delete => {
                if state.live.take().is_some() {
                    state.log.push(Rec::Tombstone);
                }
            }
            Step::Fsync => state.fsync(),
            Step::Checkpoint => {
                state.fsync();
                state.ckpt = Some((state.live, state.reserved_upto));
                state.log.clear();
                state.durable = 0;
            }
            Step::Crash => state.crash(rules),
        }
    }
    Ok(state.stats)
}

#[derive(Default)]
struct State {
    log: Vec<Rec>,
    durable: usize,
    ckpt: Option<(Option<Token>, u64)>,
    live: Option<Token>,
    next_inc: u64,
    reserved_upto: u64,
    counter_wrapped: bool,
    issued_durable: BTreeSet<Token>,
    stats: Stats,
}

impl State {
    fn write(&mut self, rules: Rules, step: Step) -> Result<(), String> {
        let ver_max = 1u64 << VERSION_BITS;
        let next = match self.live {
            None if step == Step::Update => return Ok(()),
            Some(t) if t.version + 1 < ver_max => {
                Token { incarnation: t.incarnation, version: t.version + 1 }
            }
            Some(t) if !rules.reincarnate_on_wrap => {
                // The withdrawn rule wraps without changing incarnation.
                let wrapped = Token { incarnation: t.incarnation, version: 0 };
                if self.issued_durable.contains(&wrapped) {
                    return Err(format!(
                        "REVISION REPEAT: ({}, 0) issued again after {ver_max} mutations of a \
                             record that never left — the u{VERSION_BITS} version wrapped under \
                             incarnation {}",
                        t.incarnation, t.incarnation
                    ));
                }
                wrapped
            }
            _ => {
                let Some(fresh) = self.reincarnate(rules)? else { return Ok(()) };
                fresh
            }
        };
        self.stats.issued += 1;
        self.live = Some(next);
        self.log.push(Rec::Live(next));
        Ok(())
    }

    fn reincarnate(&mut self, rules: Rules) -> Result<Option<Token>, String> {
        let inc_max = 1u64 << INCARNATION_BITS;
        if rules.resume_from_durable_reservation && self.next_inc > self.reserved_upto {
            let upto = (self.reserved_upto + CHUNK).min(inc_max);
            self.log.push(Rec::Reserve { upto });
            self.fsync();
            self.reserved_upto = upto;
        }
        if self.next_inc >= inc_max {
            if rules.refuse_at_exhaustion {
                self.stats.refused += 1;
                return Ok(None);
            }
            self.next_inc = 0;
            self.counter_wrapped = true;
        }
        let incarnation = self.next_inc;
        self.next_inc += 1;
        if self.live.is_some() {
            self.stats.reincarnations += 1;
        }
        let fresh = Token { incarnation, version: 0 };
        if self.issued_durable.contains(&fresh) {
            let why = if self.counter_wrapped {
                format!("the u{INCARNATION_BITS} incarnation counter wrapped")
            } else {
                "the counter resumed below a dead incarnation after a restart".to_string()
            };
            return Err(format!(
                "REVISION REPEAT: ({incarnation}, 0) issued again for a new incarnation — {why}"
            ));
        }
        Ok(Some(fresh))
    }

    fn fsync(&mut self) {
        for rec in &self.log[self.durable..] {
            if let Rec::Live(t) = rec {
                self.issued_durable.insert(*t);
            }
        }
        self.durable = self.log.len();
    }

    fn crash(&mut self, rules: Rules) {
        self.stats.boots += 1;
        self.log.truncate(self.durable);
        let (image, ckpt_reserved) = self.ckpt.unwrap_or((None, 0));
        self.live = image;
        let mut replayed_max = image.map_or(0, |t| t.incarnation);
        let mut reservation = ckpt_reserved;
        for rec in &self.log {
            match rec {
                Rec::Reserve { upto } => reservation = reservation.max(*upto),
                Rec::Live(t) => {
                    self.live = Some(*t);
                    replayed_max = replayed_max.max(t.incarnation);
                }
                Rec::Tombstone => self.live = None,
            }
        }
        if rules.resume_from_durable_reservation {
            // Burn the unused part of the durable reservation on boot.
            self.next_inc = reservation + 1;
            self.reserved_upto = reservation;
        } else {
            self.next_inc = replayed_max + 1;
        }
    }
}

/// F05's history: one record, never deleted, updated past the version
/// width.
pub fn wrap_history() -> Vec<Step> {
    let mut steps = vec![Step::Create, Step::Fsync];
    steps.extend(std::iter::repeat_n([Step::Update, Step::Fsync], 1 << VERSION_BITS).flatten());
    steps
}

/// Two lives of the key, both durable, the second dead and its
/// tombstone folded into a checkpoint image: the replayed records
/// carry no incarnation at all.
pub fn dead_incarnation_history() -> Vec<Step> {
    vec![
        Step::Create,
        Step::Fsync,
        Step::Delete,
        Step::Fsync,
        Step::Create,
        Step::Fsync,
        Step::Delete,
        Step::Checkpoint,
        Step::Crash,
        Step::Create,
        Step::Fsync,
    ]
}

/// Creates and deletes past the incarnation width.
pub fn exhaustion_history() -> Vec<Step> {
    std::iter::repeat_n(
        [Step::Create, Step::Fsync, Step::Delete, Step::Fsync],
        1 << INCARNATION_BITS,
    )
    .flatten()
    .chain([Step::Create, Step::Fsync])
    .collect()
}

pub fn random_steps(seed: u64, len: usize) -> Vec<Step> {
    let mut rng = Rng::new(seed ^ 0x5EED_0007);
    (0..len)
        .map(|_| match rng.below(16) {
            0..=2 => Step::Create,
            3..=8 => Step::Update,
            9..=10 => Step::Delete,
            11..=12 => Step::Fsync,
            13 => Step::Checkpoint,
            _ => Step::Crash,
        })
        .collect()
}
