//! The catalog writer's answer to a namespace DDL request (ADR-0189 D2):
//! the verdict, the slot it is answered through, the catalog word, and
//! the receipt an accepted answer carries.
//!
//! The catalog writer is `META`'s single writer and the catalog epoch's
//! single owner. It numbers each request as it dequeues it, so epochs
//! follow queue order and no sender takes one. A DDL request carries the
//! writer half of an [`AnswerSlot`]; its sender keeps the reader half in
//! a [`Requested`] token. Once an accepted request's `META` write has
//! returned, or at once for a refused one, the writer stores the slot and
//! then the word, each with Release ([`CatalogEpochs::answer`]). The word
//! is the highest answered epoch: every request numbered at or below it
//! is answered, each accepted one durable, and it never decreases. A token
//! reads its own slot and nothing else, so no other request's swap can
//! answer it.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::Arc;

use inf_log::NsId;

use crate::ns::NsSpec;

/// What the writer decided on one request (ADR-0189 D3).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DdlVerdict {
    /// The request's change is in a `META` the writer wrote and synced.
    Accepted,
    /// Refused before any effect: nothing was written and nothing moved
    /// but the epoch and the word.
    Refused(DdlRefusal),
}

/// Why the writer refused a request. The refusals are their own enum so
/// that a refused answer cannot carry an acceptance.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DdlRefusal {
    /// A `CREATE` whose name the export already holds.
    NameExists,
    /// Another DDL is pending: unreachable while the DDL ticket admits one
    /// program at a time, so the program answers it as an internal error.
    PendingConflict,
    /// A `CREATE` whose id is not the namespace-id counter's next, or a
    /// counter with no id left: unreachable while each `CREATE` reserves
    /// its id under the DDL ticket and nothing else moves the counter, so
    /// the program answers it as an internal error. Its id is never
    /// committed (ADR-0148 D4).
    IdNotNext,
}

/// A slot's verdict code: 0 is reserved for "unanswered".
const VERDICT_ACCEPTED: u8 = 1;
const VERDICT_NAME_EXISTS: u8 = 2;
const VERDICT_PENDING_CONFLICT: u8 = 3;
const VERDICT_ID_NOT_NEXT: u8 = 4;

impl DdlVerdict {
    fn code(self) -> u8 {
        match self {
            DdlVerdict::Accepted => VERDICT_ACCEPTED,
            DdlVerdict::Refused(DdlRefusal::NameExists) => VERDICT_NAME_EXISTS,
            DdlVerdict::Refused(DdlRefusal::PendingConflict) => VERDICT_PENDING_CONFLICT,
            DdlVerdict::Refused(DdlRefusal::IdNotNext) => VERDICT_ID_NOT_NEXT,
        }
    }

    /// The verdict a slot's code names; every code the writer stores.
    fn from_code(code: u8) -> DdlVerdict {
        match code {
            VERDICT_ACCEPTED => DdlVerdict::Accepted,
            VERDICT_NAME_EXISTS => DdlVerdict::Refused(DdlRefusal::NameExists),
            VERDICT_PENDING_CONFLICT => DdlVerdict::Refused(DdlRefusal::PendingConflict),
            VERDICT_ID_NOT_NEXT => DdlVerdict::Refused(DdlRefusal::IdNotNext),
            other => unreachable!("an answered slot holds verdict code {other}"),
        }
    }
}

/// One request's answer: the epoch the writer numbered it (0 until it is
/// answered) and the verdict, stored before the epoch.
#[derive(Debug, Default)]
struct Slot {
    epoch: AtomicU64,
    verdict: AtomicU8,
}

/// The writer half of one request's answer slot, consumed by its answer.
#[derive(Debug)]
pub struct AnswerSlot(Arc<Slot>);

/// The catalog word: the highest epoch the writer has answered.
#[derive(Debug, Default)]
pub struct CatalogWord(AtomicU64);

impl CatalogWord {
    /// Every request numbered at or below `epoch` is answered, and each
    /// accepted one is durable.
    #[must_use]
    pub fn covers(&self, epoch: u64) -> bool {
        self.0.load(Ordering::Acquire) >= epoch
    }

    /// The highest answered epoch (0 before the first answer).
    #[must_use]
    pub fn load(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

/// The epoch the writer took for the request it dequeued: answered once,
/// by [`CatalogEpochs::answer`].
#[derive(Debug)]
#[must_use = "every numbered request is answered"]
pub struct TakenEpoch(u64);

impl TakenEpoch {
    /// The epoch's value.
    #[must_use]
    pub fn get(&self) -> u64 {
        self.0
    }
}

/// The writer's half of the catalog epoch and the word: the one owner that
/// numbers requests and stores the word (one per boot, the writer's).
#[derive(Debug)]
pub struct CatalogEpochs {
    word: Arc<CatalogWord>,
    /// The last epoch taken (0 before the first request).
    last: u64,
}

impl CatalogEpochs {
    /// One boot's epochs, from 1, and the word their readers share.
    #[must_use]
    pub fn new() -> (CatalogEpochs, Arc<CatalogWord>) {
        let word = Arc::new(CatalogWord::default());
        (CatalogEpochs { word: Arc::clone(&word), last: 0 }, word)
    }

    /// Numbers the request just dequeued. 2⁶⁴ − 1 requests in one boot is
    /// ≈ 5.8 × 10⁵ years at 10⁶ a second: past it the writer stops the node.
    pub fn take(&mut self) -> TakenEpoch {
        let next = self.last.checked_add(1).expect("catalog epochs exhausted in one boot");
        self.last = next;
        TakenEpoch(next)
    }

    /// The planted canary: the epoch its sender took at send.
    #[cfg(inf_canary_ns_epoch_at_send)]
    pub fn take_numbered(&mut self, epoch: u64) -> TakenEpoch {
        self.last = epoch;
        TakenEpoch(epoch)
    }

    /// Answers the request `taken` numbered: its slot first, when it has
    /// one, then the word, each with Release, so a reader woken by the
    /// word's edge reads its slot. An `Accepted` answer is given only after
    /// the request's `META` write returned.
    pub fn answer(&mut self, taken: TakenEpoch, slot: Option<AnswerSlot>, verdict: DdlVerdict) {
        debug_assert_eq!(taken.0, self.last, "requests are answered in the order they are taken");
        if let Some(AnswerSlot(slot)) = slot {
            slot.verdict.store(verdict.code(), Ordering::Relaxed);
            slot.epoch.store(taken.0, Ordering::Release);
        }
        self.word.0.store(taken.0, Ordering::Release);
    }
}

mod sealed {
    use inf_log::NsId;

    /// Built only by an answer: no code outside this module mints one.
    pub trait Accepted {
        fn accepted(id: NsId, epoch: u64) -> Self;
    }
}

/// A receipt (ADR-0189 D4): the writer accepted the request after its
/// `META` write returned, and the word covered its epoch when the receipt
/// was read.
pub trait Receipt: sealed::Accepted {}

macro_rules! receipt {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug)]
        pub struct $name {
            id: NsId,
            epoch: u64,
        }

        impl $name {
            /// The namespace the request named.
            #[must_use]
            pub fn id(&self) -> NsId {
                self.id
            }

            /// The epoch the request was answered at: the one a leg of
            /// this DDL carries, and the one a teardown hold waits for.
            #[must_use]
            pub fn fan_epoch(&self) -> u64 {
                self.epoch
            }
        }

        impl sealed::Accepted for $name {
            fn accepted(id: NsId, epoch: u64) -> $name {
                $name { id, epoch }
            }
        }

        impl Receipt for $name {}
    };
}

receipt!(
    /// An accepted `CREATE`: `META` names the namespace.
    CreatePersisted
);
receipt!(
    /// An accepted `DROP`: `META` lacks the namespace, and a durable one's
    /// tombstone is in its place.
    DropPersisted
);
receipt!(
    /// An accepted `SET`: `META` holds the namespace's new configuration.
    SetPersisted
);

/// A leg whose epoch the catalog word does not cover: the writer has not
/// answered it, so no cell may apply it (ADR-0189 D4). Unreachable while
/// the origin fans only after its own receipt, read against the same word.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct NotDurable {
    /// The epoch the leg carried.
    pub epoch: u64,
    /// The word when the leg was read.
    pub word: u64,
}

impl DropPersisted {
    /// A peer's receipt for the `DROP` leg of namespace `id` the origin
    /// fanned at `epoch` (ADR-0189 D4): the word covers the epoch, so the
    /// writer accepted that drop after its `META` write returned. Epoch 0 is
    /// never answered (epochs count from 1).
    ///
    /// # Errors
    /// [`NotDurable`] when the word is below `epoch`, or `epoch` is 0.
    pub fn from_leg(id: NsId, epoch: u64, word: &CatalogWord) -> Result<DropPersisted, NotDurable> {
        if epoch == 0 || !word.covers(epoch) {
            return Err(NotDurable { epoch, word: word.load() });
        }
        Ok(DropPersisted { id, epoch })
    }
}

/// What a cell's keyspace held when it applied a `DROP` (ADR-0189 D4,
/// D5): the namespace it removed, or nothing, which is a peer that never
/// applied its `CREATE` leg or a rollback whose origin never applied its
/// own. Either is an applied drop.
#[derive(Debug, PartialEq, Eq)]
pub enum DropApplied {
    /// The namespace was here and is gone; its spec as it was.
    Removed(NsSpec),
    /// The namespace was not here.
    Absent,
}

/// One sent request's reader half: not `Clone`, so one requester holds
/// it, and [`Requested::answer`] consumes it, so it yields one receipt.
#[derive(Debug)]
pub struct Requested<R> {
    slot: Arc<Slot>,
    id: NsId,
    receipt: PhantomData<fn() -> R>,
}

/// What a token's slot answers.
#[derive(Debug)]
pub enum Answer<R> {
    /// Not answered yet: the token, to ask again on the word's next edge.
    Pending(Requested<R>),
    /// Refused before any effect.
    Refused(DdlRefusal),
    /// Accepted, and durable.
    Accepted(R),
}

/// A slot for one DDL request on `id`: the writer half travels with the
/// request, the token stays with its sender.
#[must_use]
pub fn answer_slot<R: Receipt>(id: NsId) -> (AnswerSlot, Requested<R>) {
    let slot = Arc::new(Slot::default());
    (AnswerSlot(Arc::clone(&slot)), Requested { slot, id, receipt: PhantomData })
}

impl<R: Receipt> Requested<R> {
    /// The answer once the writer stored it and `word` covers its epoch.
    #[must_use]
    pub fn answer(self, word: &CatalogWord) -> Answer<R> {
        let epoch = self.slot.epoch.load(Ordering::Acquire);
        if epoch == 0 || !word.covers(epoch) {
            return Answer::Pending(self);
        }
        match DdlVerdict::from_code(self.slot.verdict.load(Ordering::Relaxed)) {
            DdlVerdict::Accepted => Answer::Accepted(R::accepted(self.id, epoch)),
            DdlVerdict::Refused(refusal) => Answer::Refused(refusal),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A token reads its own slot and nothing else: a later request's
    /// answer moves the word past an unanswered earlier one without
    /// answering it, and each answer carries its own epoch and verdict.
    #[test]
    fn a_token_answers_only_from_its_own_slot() {
        let (mut epochs, word) = CatalogEpochs::new();
        let (first_slot, first) = answer_slot::<CreatePersisted>(NsId(16));
        let (second_slot, second) = answer_slot::<DropPersisted>(NsId(17));
        let first = match first.answer(&word) {
            Answer::Pending(token) => token,
            Answer::Refused(_) | Answer::Accepted(_) => panic!("answered before the writer ran"),
        };
        // The writer dequeues the second request first: queue order.
        let taken = epochs.take();
        assert_eq!(taken.get(), 1);
        epochs.answer(taken, Some(second_slot), DdlVerdict::Accepted);
        let receipt = match second.answer(&word) {
            Answer::Accepted(receipt) => receipt,
            Answer::Pending(_) | Answer::Refused(_) => panic!("the answered token is accepted"),
        };
        assert_eq!((receipt.id(), receipt.fan_epoch()), (NsId(17), 1));
        let first = match first.answer(&word) {
            Answer::Pending(token) => token,
            Answer::Refused(_) | Answer::Accepted(_) => panic!("another request's answer"),
        };
        // A request with no slot (the readiness persist) moves the word.
        let taken = epochs.take();
        epochs.answer(taken, None, DdlVerdict::Accepted);
        assert!(word.covers(2));
        let first = match first.answer(&word) {
            Answer::Pending(token) => token,
            Answer::Refused(_) | Answer::Accepted(_) => panic!("still not its answer"),
        };
        let taken = epochs.take();
        epochs.answer(taken, Some(first_slot), DdlVerdict::Refused(DdlRefusal::NameExists));
        assert!(matches!(first.answer(&word), Answer::Refused(DdlRefusal::NameExists)));
        assert_eq!(word.load(), 3, "the word is the highest answered epoch");
    }

    /// A peer's `DROP` receipt needs the word to cover its leg's epoch.
    #[test]
    fn a_leg_receipt_needs_the_word_to_cover_its_epoch() {
        let (mut epochs, word) = CatalogEpochs::new();
        let ns = NsId(16);
        let early = DropPersisted::from_leg(ns, 1, &word).err();
        assert_eq!(early, Some(NotDurable { epoch: 1, word: 0 }), "the word is below the epoch");
        let taken = epochs.take();
        epochs.answer(taken, None, DdlVerdict::Accepted);
        let receipt = DropPersisted::from_leg(ns, 1, &word).expect("the word covers epoch 1");
        assert_eq!((receipt.id(), receipt.fan_epoch()), (ns, 1));
        assert!(DropPersisted::from_leg(ns, 2, &word).is_err(), "an epoch past the word");
        assert!(DropPersisted::from_leg(ns, 0, &word).is_err(), "epoch 0 is never answered");
    }

    /// Every verdict survives its slot's code.
    #[test]
    fn every_verdict_round_trips_its_code() {
        for verdict in [
            DdlVerdict::Accepted,
            DdlVerdict::Refused(DdlRefusal::NameExists),
            DdlVerdict::Refused(DdlRefusal::PendingConflict),
            DdlVerdict::Refused(DdlRefusal::IdNotNext),
        ] {
            assert_eq!(DdlVerdict::from_code(verdict.code()), verdict);
        }
    }
}
