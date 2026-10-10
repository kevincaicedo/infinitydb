//! ADR-0144 A2's discard spellings on a publish: a publish returns `()`, so
//! `let _ =`, `_ =` and `drop` each draw a lint and do not build under
//! `-D warnings` (`.ok()` has no receiver: a `compile_fail` doctest of
//! `CappedDeque` holds that). Applied to a reservation they discard only an
//! unused slot, which the deque's own test holds. Compile only.
use inf_foundation::bounded::{CappedDeque, Reserve, Reserved};

pub fn let_underscore(deque: &mut CappedDeque<u8, Reserve>) {
    if let Reserved::Slot(slot) = deque.reserve() {
        let _ = slot.publish(1); // PLANT clippy::let_unit_value
    }
}

pub fn assign_underscore(deque: &mut CappedDeque<u8, Reserve>) {
    if let Reserved::Slot(slot) = deque.reserve() {
        _ = slot.publish(2); // PLANT clippy::let_unit_value
    }
}

pub fn drop_call(deque: &mut CappedDeque<u8, Reserve>) {
    if let Reserved::Slot(slot) = deque.reserve() {
        drop(slot.publish(3)); // PLANT clippy::unit_arg,dropping_copy_types
    }
}

pub fn published(deque: &mut CappedDeque<u8, Reserve>) {
    if let Reserved::Slot(slot) = deque.reserve() {
        slot.publish(4); // CONTROL
    }
}
