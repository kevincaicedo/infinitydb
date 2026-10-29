//! ADR-0144 A2: a reservation cannot cross an `await`. Each `// PLANT` line
//! holds a slot type of `inf_foundation::bounded`, listed in clippy.toml's
//! `await-holding-invalid-types`, across an `await`, and must draw
//! `await_holding_invalid_type` naming that path; the consumed-before-await
//! control draws nothing. The lint matches the outermost type of each local
//! live across the await, so the slot and the answer that carries it are two
//! entries with a plant each; a wrapper of the caller's own (an `Option`, a
//! tuple or a struct holding a slot) is not matched and stays review's. A
//! slab's lookups borrow its occupant for one step, so they are listed too.
//! Compile only; nothing here runs.
use inf_foundation::bounded::{
    CappedDeque, CappedSlab, Lookup, LookupMut, Reserve, Reserved, Retire, SlabReserved, SlotId,
};

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

// A slab's reservation, the answer that carries it, and its two lookups,
// each a borrow of the slab for one step. Their awaits are `settle`, so the
// self-test's text edits of the deque's plants above reach none of them.
async fn settle() {}

pub async fn slab_slot_across_await(slab: &mut CappedSlab<u8, u32, Retire>) {
    match slab.reserve() {
        SlabReserved::Slot(slot) => { // PLANT clippy::await_holding_invalid_type inf_foundation::bounded::SlotReservation
            settle().await;
            let _id = slot.publish(2);
        }
        SlabReserved::Full | SlabReserved::Exhausted => {}
    }
}

pub async fn slab_slot_consumed_before_await(slab: &mut CappedSlab<u8, u32, Retire>) {
    match slab.reserve() {
        SlabReserved::Slot(slot) => {
            let _id = slot.publish(1); // CONTROL
        }
        SlabReserved::Full | SlabReserved::Exhausted => {}
    }
    settle().await; // CONTROL
}

pub async fn slab_reserved_across_await(slab: &mut CappedSlab<u8, u32, Retire>) {
    let reserved = slab.reserve(); // PLANT clippy::await_holding_invalid_type inf_foundation::bounded::SlabReserved
    settle().await;
    publish_slab(reserved);
}

fn publish_slab(reserved: SlabReserved<'_, u8, u32, Retire>) {
    match reserved {
        SlabReserved::Slot(slot) => {
            let _id = slot.publish(1);
        }
        SlabReserved::Full | SlabReserved::Exhausted => {}
    }
}

pub async fn lookup_across_await(slab: &CappedSlab<u8, u32, Retire>, id: SlotId<u32>) {
    let found = slab.get(id); // PLANT clippy::await_holding_invalid_type inf_foundation::bounded::Lookup
    settle().await;
    read(found);
}

pub async fn lookup_read_before_await(slab: &CappedSlab<u8, u32, Retire>, id: SlotId<u32>) {
    let found = slab.get(id); // CONTROL
    read(found); // CONTROL
    settle().await; // CONTROL
}

fn read(found: Lookup<'_, u8>) -> u8 {
    match found {
        Lookup::Live(value) => *value,
        Lookup::Stale | Lookup::Vacant => 0,
    }
}

pub async fn lookup_mut_across_await(slab: &mut CappedSlab<u8, u32, Retire>, id: SlotId<u32>) {
    let found = slab.get_mut(id); // PLANT clippy::await_holding_invalid_type inf_foundation::bounded::LookupMut
    settle().await;
    write(found);
}

fn write(found: LookupMut<'_, u8>) {
    match found {
        LookupMut::Live(value) => *value = 1,
        LookupMut::Stale | LookupMut::Vacant => {}
    }
}
