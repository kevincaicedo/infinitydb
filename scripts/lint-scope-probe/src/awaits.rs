//! ADR-0144 A2: a reservation cannot cross an `await`. Each `// PLANT` line
//! holds a slot type of `inf_foundation::bounded`, listed in clippy.toml's
//! `await-holding-invalid-types`, across an `await`, and must draw
//! `await_holding_invalid_type` naming that path; the consumed-before-await
//! control draws nothing. Compile only; nothing here runs.
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
