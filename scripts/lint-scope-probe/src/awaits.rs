//! ADR-0144 A2: a reservation cannot cross an `await`. Each `// PLANT` line
//! holds a slot type of `inf_foundation::bounded`, listed in clippy.toml's
//! `await-holding-invalid-types`, across an `await`, and must draw
//! `await_holding_invalid_type` naming that path; the consumed-before-await
//! control draws nothing. The lint matches the outermost type of each local
//! live across the await, so the slot and the answer that carries it are two
//! entries with a plant each; a wrapper of the caller's own (an `Option`, a
//! tuple or a struct holding a slot) is not matched and stays review's.
//! Compile only; nothing here runs.
use inf_foundation::bounded::{CappedDeque, Reserve, Reserved};

async fn yield_once() {}

pub async fn deque_slot_across_await(deque: &mut CappedDeque<u8, Reserve>) {
    match deque.reserve() {
        Reserved::Slot(slot) => { // PLANT clippy::await_holding_invalid_type inf_foundation::bounded::DequeSlot
            yield_once().await;
            slot.publish(1);
        }
        Reserved::Full => {}
    }
}

pub async fn deque_slot_consumed_before_await(deque: &mut CappedDeque<u8, Reserve>) {
    match deque.reserve() {
        Reserved::Slot(slot) => slot.publish(1), // CONTROL
        Reserved::Full => {}
    }
    yield_once().await; // CONTROL
}

// The answer itself, not yet matched, carries the slot across the await: the
// natural spelling of a step that reserves, yields and then looks.
pub async fn reserved_across_await(deque: &mut CappedDeque<u8, Reserve>) {
    let reserved = deque.reserve(); // PLANT clippy::await_holding_invalid_type inf_foundation::bounded::Reserved
    yield_once().await;
    publish_one(reserved);
}

pub async fn reserved_matched_before_await(deque: &mut CappedDeque<u8, Reserve>) {
    let reserved = deque.reserve(); // CONTROL
    publish_one(reserved); // CONTROL
    yield_once().await; // CONTROL
}

fn publish_one(reserved: Reserved<'_, u8>) {
    match reserved {
        Reserved::Slot(slot) => slot.publish(1),
        Reserved::Full => {}
    }
}
