//! Lifetime identity issuance (ADR-0159 D1, A1.2–A1.6).
//!
//! A boot-scoped [`IssueClock`] hands out identities that must never wrap
//! or repeat. Every issue is paid for by a credit that [`partition`] minted
//! before anything could issue: one quota per participant, one more for the
//! owner, and one final credit per participant. The partition never mints
//! more credits than the space above the clock's start holds, so the clock
//! cannot pass `u64::MAX`, and every issued value is nonzero and above every
//! earlier issue of its boot. Running out is a quota's typed refusal
//! ([`IssueExhausted`]) before any effect, never the counter's width.
//!
//! The orderings of late issuance (A1.6) live here and nowhere else: the
//! issue is an `AcqRel` `fetch_add`; a [`RequestWord`] is written only by a
//! `Release` `fetch_max` and read with `Acquire`. An effect that happens
//! before the issue of `e` then happens before whatever a reader does after
//! it loads a word value `>= e`. The Loom models at the end of this file
//! check that rule against these same types (`RUSTFLAGS="--cfg loom"`).

use core::num::NonZeroU64;

/// Atomics and `Arc`, swapped for Loom's model-checked versions under
/// `--cfg loom` (the pattern `inf-fabric`'s ring uses; ADR-0159 A1.7).
#[cfg(loom)]
mod sync {
    pub(super) use loom::sync::Arc;
    pub(super) use loom::sync::atomic::{AtomicU64, Ordering};
}

#[cfg(not(loom))]
mod sync {
    pub(super) use core::sync::atomic::{AtomicU64, Ordering};
    pub(super) use std::sync::Arc;
}

use sync::{Arc, AtomicU64, Ordering};

/// A1.6: the issue is `AcqRel`, so the issues of one clock form a
/// happens-before chain (each `fetch_add` reads the previous one's write).
#[cfg(not(inf_canary_issue_clock_relaxed))]
const ISSUE_ORDERING: Ordering = Ordering::AcqRel;
/// Canary: the issue without its ordering. The Loom effect witness must
/// fail on this build.
#[cfg(inf_canary_issue_clock_relaxed)]
const ISSUE_ORDERING: Ordering = Ordering::Relaxed;

/// A1.6: a request word is raised by a `Release` RMW, so a reader that
/// loads the value synchronizes with the issuer through its release
/// sequence.
#[cfg(not(inf_canary_request_raise_relaxed))]
const RAISE_ORDERING: Ordering = Ordering::Release;
/// Canary: the raise without its ordering.
#[cfg(inf_canary_request_raise_relaxed)]
const RAISE_ORDERING: Ordering = Ordering::Relaxed;

/// A1.6: a request word is read with `Acquire`, the other half of the
/// raise's synchronization.
#[cfg(not(inf_canary_request_read_relaxed))]
const READ_ORDERING: Ordering = Ordering::Acquire;
/// Canary: the read without its ordering.
#[cfg(inf_canary_request_read_relaxed)]
const READ_ORDERING: Ordering = Ordering::Relaxed;

/// One issued identity: nonzero, and above every earlier issue of its
/// clock (ADR-0159 D1, A1.2). Built only by [`IssueCredit::issue`].
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Issued(NonZeroU64);

impl Issued {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// A quota holds fewer units than a reservation asks for (ADR-0159 D1).
/// Nothing changed. The refusal is permanent for the boot: a quota only
/// shrinks.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct IssueExhausted;

/// A headroom space whose `quotas × headroom + finals` does not fit `u64`
/// (A1.5): a construction error, before any credit exists.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct PartitionRefused;

/// The arithmetic of one boot's issuance space, checked once (A1.2, A1.5).
///
/// `participants` each get a quota and a final credit, and the owner gets
/// one more quota: `q = participants + 1` quotas of `units_per_quota` units
/// and `f = participants` final credits. The clock starts at `start`, and
/// `start + q × units_per_quota + f <= u64::MAX` is the proof that no issue
/// wraps (ADR-0159 D1's proof, as A1.2 and A1.5 partition it).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct IssueSpace {
    participants: u32,
    start: u64,
    units_per_quota: u64,
}

impl IssueSpace {
    /// The production split (A1.2): the clock starts at 0, and every quota
    /// holds `floor((u64::MAX − f) / q)` units.
    #[must_use]
    pub fn full(participants: u32) -> IssueSpace {
        let quotas = u64::from(participants) + 1;
        let units_per_quota = (u64::MAX - u64::from(participants)) / quotas;
        IssueSpace { participants, start: 0, units_per_quota }
    }

    /// The test seam (A1.5): every quota holds `headroom` units, and the
    /// clock starts `q × headroom + f` below `u64::MAX`, so real issues
    /// reach the top of the range. The proof is unchanged: the space still
    /// ends at `u64::MAX`.
    ///
    /// # Errors
    /// [`PartitionRefused`] when `q × headroom + f` does not fit `u64`.
    pub fn with_headroom(
        participants: u32,
        headroom: NonZeroU64,
    ) -> Result<IssueSpace, PartitionRefused> {
        let quotas = u64::from(participants) + 1;
        let slack = quotas
            .checked_mul(headroom.get())
            .and_then(|units| units.checked_add(u64::from(participants)))
            .ok_or(PartitionRefused)?;
        Ok(IssueSpace { participants, start: u64::MAX - slack, units_per_quota: headroom.get() })
    }

    /// The clock's value before the first issue.
    #[must_use]
    pub const fn start(self) -> u64 {
        self.start
    }

    /// Units in each quota.
    #[must_use]
    pub const fn units_per_quota(self) -> u64 {
        self.units_per_quota
    }

    /// Participants (each with a quota and a final credit).
    #[must_use]
    pub const fn participants(self) -> u32 {
        self.participants
    }

    /// Epochs above the last one the partition's credits can issue:
    /// `(u64::MAX − f) mod q` for the full split, 0 for a headroom space.
    #[must_use]
    pub fn unallocated(self) -> u64 {
        let quotas = u64::from(self.participants) + 1;
        // bound: `start + q × units + f <= u64::MAX` by both constructors.
        u64::MAX - self.start - quotas * self.units_per_quota - u64::from(self.participants)
    }
}

/// A request word (A1.3): written only by a `Release` `fetch_max` (an RMW,
/// so every later write stays in the first one's release sequence) and
/// read with `Acquire`. Monotone.
#[derive(Debug)]
pub struct RequestWord(AtomicU64);

impl RequestWord {
    #[must_use]
    pub fn new() -> RequestWord {
        RequestWord(AtomicU64::new(0))
    }

    /// Raises the word to `issued` (never lowers it).
    pub fn raise(&self, issued: Issued) {
        self.0.fetch_max(issued.get(), RAISE_ORDERING);
    }

    /// The highest identity raised so far (0 before the first).
    #[must_use]
    pub fn read(&self) -> u64 {
        self.0.load(READ_ORDERING)
    }
}

impl Default for RequestWord {
    fn default() -> RequestWord {
        RequestWord::new()
    }
}

/// One boot's issuance clock and the board its issues publish into. Only a
/// credit advances it, so a credit can issue only into its own boot's board
/// (ADR-0159 D1, A1.2).
#[derive(Debug)]
pub struct IssueClock<B> {
    last: AtomicU64,
    board: B,
}

impl<B> IssueClock<B> {
    /// The board issues publish into.
    #[must_use]
    pub fn board(&self) -> &B {
        &self.board
    }

    /// The last identity issued (the space's start before the first).
    #[must_use]
    pub fn last(&self) -> u64 {
        self.last.load(Ordering::Acquire)
    }
}

/// A participant's allowance: its quota and its final credit (A1.2, D3).
#[derive(Debug)]
pub struct Participant<B> {
    pub quota: IssueQuota<B>,
    pub final_credit: FinalCredit<B>,
}

/// Every credit of one boot, minted at once (A1.2).
#[derive(Debug)]
pub struct Partition<B> {
    pub clock: Arc<IssueClock<B>>,
    /// The owner's quota (for the checkpoint clock: the control writer's).
    pub owner: IssueQuota<B>,
    pub participants: Vec<Participant<B>>,
}

/// Mints every credit `space` allows over a new clock that owns `board`.
/// O(participants): one quota and one final credit each, at boot.
pub fn partition<B>(board: B, space: IssueSpace) -> Partition<B> {
    let clock = Arc::new(IssueClock { last: AtomicU64::new(space.start), board });
    let quota = |clock: &Arc<IssueClock<B>>| IssueQuota {
        clock: Arc::clone(clock),
        remaining: space.units_per_quota,
    };
    let participants = (0..space.participants)
        .map(|_| Participant {
            quota: quota(&clock),
            final_credit: FinalCredit(IssueCredit { clock: Arc::clone(&clock) }),
        })
        .collect();
    Partition { owner: quota(&clock), participants, clock }
}

/// One owner's allowance of ordinary issues. Cell-local: a plain counter,
/// no shared state (L1). Reserving is all-or-nothing.
#[derive(Debug)]
pub struct IssueQuota<B> {
    clock: Arc<IssueClock<B>>,
    remaining: u64,
}

impl<B> IssueQuota<B> {
    /// Takes `K` credits, or none (ADR-0159 D1): a two-unit refusal leaves
    /// the one remaining unit for a one-unit reservation.
    ///
    /// # Errors
    /// [`IssueExhausted`] when fewer than `K` units remain; nothing changed.
    pub fn reserve<const K: usize>(&mut self) -> Result<[IssueCredit<B>; K], IssueExhausted> {
        let Ok(units) = u64::try_from(K) else { return Err(IssueExhausted) };
        let Some(remaining) = self.remaining.checked_sub(units) else {
            return Err(IssueExhausted);
        };
        self.remaining = remaining;
        Ok(core::array::from_fn(|_| IssueCredit { clock: Arc::clone(&self.clock) }))
    }

    /// Units left.
    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.remaining
    }
}

/// The right to one issue. Not `Clone`; consumed by value. A credit dropped
/// unused is spent: units never return to a quota (ADR-0159 D1).
#[derive(Debug)]
pub struct IssueCredit<B> {
    clock: Arc<IssueClock<B>>,
}

impl<B> IssueCredit<B> {
    /// Issues the next identity and hands it to `publish` with the clock's
    /// board; returns it once published.
    pub fn issue(self, publish: impl FnOnce(&B, Issued)) -> Issued {
        let previous = self.clock.last.fetch_add(1, ISSUE_ORDERING);
        // ADR-0159 D1, A1.2: the partition minted at most
        // `u64::MAX − start` credits and each issues once, so
        // `previous < u64::MAX` and its successor is nonzero.
        let issued = previous
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .map(Issued)
            .expect("the partition mints no credit past u64::MAX");
        publish(&self.clock.board, issued);
        issued
    }
}

/// A participant's protected last issue (ADR-0159 D3): available after its
/// quota is exhausted, issued once.
#[derive(Debug)]
pub struct FinalCredit<B>(IssueCredit<B>);

impl<B> FinalCredit<B> {
    /// Issues the final identity; see [`IssueCredit::issue`].
    pub fn issue(self, publish: impl FnOnce(&B, Issued)) -> Issued {
        self.0.issue(publish)
    }
}

#[cfg(test)]
impl<B> Partition<B> {
    /// Canary: one credit past the partition. Its issue must be caught.
    fn over_mint(&self) -> IssueCredit<B> {
        IssueCredit { clock: Arc::clone(&self.clock) }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    const MAX: u128 = u64::MAX as u128;

    /// The ADR formula, recomputed in `u128`: `(q, Q, slack)`.
    fn full_split_u128(participants: u32) -> (u128, u128, u128) {
        let quotas = u128::from(participants) + 1;
        let finals = u128::from(participants);
        let units = (MAX - finals) / quotas;
        (quotas, units, MAX - quotas * units - finals)
    }

    #[test]
    fn the_full_split_matches_the_formula_at_every_topology_edge() {
        for participants in [1u32, 2, 4, 64, 16_384] {
            let space = IssueSpace::full(participants);
            let (quotas, units, slack) = full_split_u128(participants);
            assert_eq!(u128::from(space.units_per_quota()), units, "Q at N = {participants}");
            assert_eq!(u128::from(space.unallocated()), slack, "slack at N = {participants}");
            assert_eq!(space.start(), 0);
            let issues = quotas * units + u128::from(participants);
            assert!(issues <= MAX, "(N + 1)·Q + N fits u64 at N = {participants}");
            assert!(units >= 1 << 49, "Q ≥ 2^49 at N = {participants}");
            let partition = partition((), space);
            assert_eq!(partition.participants.len(), participants as usize);
            assert_eq!(partition.owner.remaining(), space.units_per_quota());
        }
    }

    #[test]
    fn a_headroom_space_that_overflows_is_refused() {
        let huge = NonZeroU64::new(u64::MAX / 2).expect("nonzero");
        assert_eq!(IssueSpace::with_headroom(2, huge), Err(PartitionRefused));
        let space = IssueSpace::with_headroom(1, NonZeroU64::MIN).expect("fits");
        assert_eq!(space.start(), u64::MAX - 3, "(N + 1)·h + N = 3 below the top");
        assert_eq!(space.unallocated(), 0);
    }

    /// Drives every credit of a headroom partition: each issue is nonzero,
    /// distinct and increasing, and the last reaches the top of the space
    /// (engagement: a drive that stayed low is vacuous).
    #[test]
    fn every_credit_of_a_headroom_partition_issues_up_to_the_top() {
        let headroom = NonZeroU64::new(3).expect("nonzero");
        let space = IssueSpace::with_headroom(2, headroom).expect("fits");
        let Partition { clock, mut owner, participants } = partition((), space);
        let mut ledger = BTreeSet::new();
        let mut previous = clock.last();
        let mut record = |issued: Issued| {
            assert!(issued.get() > previous, "{} is not above {previous}", issued.get());
            previous = issued.get();
            assert!(ledger.insert(issued.get()), "{} repeats", issued.get());
        };
        let mut quotas = vec![&mut owner];
        let mut finals = Vec::new();
        let mut rest = Vec::new();
        for participant in participants {
            rest.push(participant.quota);
            finals.push(participant.final_credit);
        }
        quotas.extend(rest.iter_mut());
        for quota in quotas {
            while let Ok([credit]) = quota.reserve::<1>() {
                record(credit.issue(|(), _| {}));
            }
            assert_eq!(quota.remaining(), 0);
        }
        for final_credit in finals {
            record(final_credit.issue(|(), _| {}));
        }
        assert_eq!(clock.last(), u64::MAX - space.unallocated(), "the drive reached the top");
        assert_eq!(ledger.len(), 3 * 3 + 2);
    }

    #[test]
    fn a_two_unit_reservation_with_one_unit_left_takes_nothing() {
        let space = IssueSpace::with_headroom(1, NonZeroU64::MIN).expect("fits");
        let Partition { clock, mut owner, .. } = partition((), space);
        assert_eq!(owner.reserve::<2>().map(|_| ()), Err(IssueExhausted));
        assert_eq!(owner.remaining(), 1, "a refused reservation changes nothing");
        let [credit] = owner.reserve::<1>().expect("the unit a two-unit refusal left");
        assert_eq!(credit.issue(|(), _| {}).get(), clock.last());
        assert_eq!(owner.reserve::<1>().map(|_| ()), Err(IssueExhausted));
    }

    #[test]
    fn a_final_credit_issues_after_its_quota_is_exhausted() {
        let space = IssueSpace::with_headroom(1, NonZeroU64::MIN).expect("fits");
        let Partition { mut participants, .. } = partition((), space);
        let Participant { mut quota, final_credit } = participants.remove(0);
        let [credit] = quota.reserve::<1>().expect("one unit");
        let ordinary = credit.issue(|(), _| {});
        assert_eq!(quota.reserve::<1>().map(|_| ()), Err(IssueExhausted));
        assert!(final_credit.issue(|(), _| {}) > ordinary);
    }

    /// Canary: one credit past the partition. The sequence oracle must
    /// see a panic (the counter wraps to 0, whose successor is refused).
    #[test]
    fn canary_an_over_minted_credit_is_caught() {
        let space = IssueSpace::with_headroom(1, NonZeroU64::MIN).expect("fits");
        let partition = partition((), space);
        let extra = partition.over_mint();
        let Partition { clock, mut owner, participants } = partition;
        let [credit] = owner.reserve::<1>().expect("unit");
        let _ = credit.issue(|(), _| {});
        for Participant { mut quota, final_credit } in participants {
            let [credit] = quota.reserve::<1>().expect("unit");
            let _ = credit.issue(|(), _| {});
            let _ = final_credit.issue(|(), _| {});
        }
        assert_eq!(clock.last(), u64::MAX);
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            extra.issue(|(), _| {});
        }));
        assert!(caught.is_err(), "an over-minted issue past u64::MAX went unnoticed");
    }

    #[test]
    fn a_request_word_only_rises() {
        let Partition { clock, mut owner, .. } = partition(RequestWord::new(), IssueSpace::full(1));
        let [low, high] = owner.reserve::<2>().expect("units");
        let low = low.issue(|_, _| {});
        let high = high.issue(|word, issued| word.raise(issued));
        clock.board().raise(low);
        assert_eq!(clock.board().read(), high.get(), "a lower raise never lowers the word");
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use loom::sync::atomic::AtomicU64;
    use loom::thread;

    use super::*;

    /// The model's board, built from the product's [`RequestWord`]: a word
    /// for all-participant requests and the observer's own slot.
    struct ModelBoard {
        word: RequestWord,
        slot: RequestWord,
    }

    impl ModelBoard {
        fn new() -> ModelBoard {
            ModelBoard { word: RequestWord::new(), slot: RequestWord::new() }
        }

        fn requested(&self) -> u64 {
            self.word.read().max(self.slot.read())
        }
    }

    fn one_credit(quota: &mut IssueQuota<ModelBoard>) -> IssueCredit<ModelBoard> {
        let Ok([credit]) = quota.reserve::<1>() else { panic!("a model quota holds a unit") };
        credit
    }

    /// Model 1: two quotas and a final credit issue concurrently while an
    /// observer reads. Epochs are distinct and nonzero, the word holds the
    /// largest all-participant epoch, and no read is above `last`.
    #[test]
    fn loom_issues_are_distinct_and_never_read_above_last() {
        loom::model(|| {
            let space = IssueSpace::with_headroom(1, NonZeroU64::MIN).expect("fits");
            let Partition { clock, mut owner, mut participants } =
                partition(ModelBoard::new(), space);
            let Participant { mut quota, final_credit } = participants.remove(0);
            let first = one_credit(&mut owner);
            let second = one_credit(&mut quota);
            let all_a = thread::spawn(move || first.issue(|b, e| b.word.raise(e)));
            let all_b = thread::spawn(move || second.issue(|b, e| b.word.raise(e)));
            let own = thread::spawn(move || final_credit.issue(|b, e| b.slot.raise(e)));
            let seen = clock.board().requested();
            assert!(seen <= clock.last(), "a request {seen} read above the clock");
            let a = all_a.join().expect("issuer");
            let b = all_b.join().expect("issuer");
            let c = own.join().expect("issuer");
            let mut epochs = [a.get(), b.get(), c.get()];
            epochs.sort_unstable();
            assert!(epochs[0] > space.start(), "an epoch at or below the start");
            assert!(epochs[0] < epochs[1], "repeated epochs");
            assert!(epochs[1] < epochs[2], "repeated epochs");
            assert_eq!(clock.board().word.read(), a.max(b).get());
            assert_eq!(clock.board().slot.read(), c.get());
        });
    }

    /// The over-mint canary inside the model: an over-minted credit's issue
    /// panics.
    #[test]
    #[should_panic(expected = "the partition mints no credit past u64::MAX")]
    fn loom_over_mint_is_caught() {
        loom::model(|| {
            let space = IssueSpace::with_headroom(1, NonZeroU64::MIN).expect("fits");
            let partition = partition(ModelBoard::new(), space);
            let extra = partition.over_mint();
            let Partition { mut owner, mut participants, .. } = partition;
            let Participant { mut quota, final_credit } = participants.remove(0);
            let issuer = thread::spawn(move || {
                one_credit(&mut owner).issue(|b, e| b.word.raise(e));
                one_credit(&mut quota).issue(|b, e| b.word.raise(e));
            });
            final_credit.issue(|b, e| b.slot.raise(e));
            issuer.join().expect("issuer");
            extra.issue(|b, e| b.word.raise(e));
        });
    }

    /// Executions in which the witness read at least one effect.
    static WITNESS_READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// Model 2, the effect witness (ADR-0159 A1.6). One issuer requests
    /// All, the other the observer's own slot; each writes its effect before
    /// it issues and records its epoch in a `Relaxed` ledger that carries no
    /// happens-before. After loading `requested = v`, the observer reads the
    /// effect of every ledger epoch `<= v`. The effect is a `Relaxed` atomic:
    /// Loom lets a load return any store not ordered before it, so a missing
    /// edge shows as a stale 0. (A `loom::cell::UnsafeCell` needs `unsafe`,
    /// which this crate forbids.)
    #[test]
    fn loom_an_effect_before_the_issue_is_visible_after_the_request() {
        loom::model(|| {
            let space = IssueSpace::with_headroom(1, NonZeroU64::MIN).expect("fits");
            let Partition { clock, mut owner, mut participants } =
                partition(ModelBoard::new(), space);
            let all = one_credit(&mut owner);
            let own = one_credit(&mut participants[0].quota);
            let effects = Arc::new([AtomicU64::new(0), AtomicU64::new(0)]);
            let ledger = Arc::new([AtomicU64::new(0), AtomicU64::new(0)]);
            let spawn_issuer = |index: usize, credit: IssueCredit<ModelBoard>, all: bool| {
                let (effects, ledger) = (Arc::clone(&effects), Arc::clone(&ledger));
                thread::spawn(move || {
                    effects[index].store(1, Ordering::Relaxed);
                    let issued = credit.issue(|board, issued| {
                        if all { board.word.raise(issued) } else { board.slot.raise(issued) }
                    });
                    ledger[index].store(issued.get(), Ordering::Relaxed);
                })
            };
            let issuer_all = spawn_issuer(0, all, true);
            let issuer_own = spawn_issuer(1, own, false);
            let requested = clock.board().requested();
            let mut read = false;
            for index in 0..2 {
                let epoch = ledger[index].load(Ordering::Relaxed);
                if epoch != 0 && epoch <= requested {
                    read = true;
                    let effect = effects[index].load(Ordering::Relaxed);
                    assert_eq!(effect, 1, "A1.6: an effect before epoch {epoch} is unseen");
                }
            }
            if read {
                WITNESS_READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            issuer_all.join().expect("issuer");
            issuer_own.join().expect("issuer");
        });
        let reads = WITNESS_READS.load(std::sync::atomic::Ordering::Relaxed);
        assert!(reads > 0, "VACUOUS: the witness never read an effect");
    }
}
