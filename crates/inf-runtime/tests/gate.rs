//! The fabric key past its generation field (ADR-0194 A2, ADR-0177 D4): a fabric gate whose
//! holder slots start at 2^23 − 2, the test as the peer. Each holder serves 32 requests, so
//! every slot's generation crosses the key's 23-bit field, and each request is completed with
//! its own key's word, out of order. Every value reaches its own wait, and no completion is
//! stale. A second gate's slots start at the generation's last value: a permit dropped there
//! leaves its slot free at the first generation. Built only with `fault-points`, the slab's
//! first-generation hook (`just check` runs it apart).

use std::cell::RefCell;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use inf_foundation::CellId;
use inf_foundation::bounded::{Cap, CapCensus, CapFill};
use inf_foundation::limits::{GATE_KEY_GENERATION_BITS, GATE_KEY_INDEX_BITS};
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::Nanos;
use inf_runtime::{Delivery, FabricAdmission, FabricGate, GateWait};

/// Holders of the test's gate, and the requests each serves.
const HOLDERS: u32 = 32;
const ROUNDS: u64 = 32;
/// The first generation past the key's field.
const FIELD: u32 = 1 << GATE_KEY_GENERATION_BITS;

#[test]
fn every_value_reaches_its_own_wait_past_the_generation_field() {
    let (census, _mark) = CapCensus::new(CellId(2));
    let cap = Cap::entries::<{ HOLDERS }>("test-gate-holders", CapFill::Serving);
    let gate = RefCell::new(
        FabricGate::<u64>::build_at_generation(2, HOLDERS, Some(cap), &census, FIELD - 2)
            .expect("the test gate"),
    );
    let units = gate.borrow().units(HOLDERS).expect("every unit");
    let mut rng = SplitMix64::new(0x6E65_6172_5772_6170);
    let mut cx = Context::from_waker(Waker::noop());
    for round in 0..ROUNDS {
        let mut ops: Vec<(u64, u64, GateWait<'_, u64>)> = Vec::new();
        for leg in 0..u64::from(HOLDERS) {
            let mut borrowed = gate.borrow_mut();
            let FabricAdmission::Granted(permit) = borrowed.reserve(&units) else {
                panic!("round {round}: a unit and a holder are free for leg {leg}");
            };
            let key = permit.key();
            ops.push((key, (round << 32) | leg, permit.register(&gate, None)));
        }
        // The peer answers in an order of its own, each op with its own key.
        let mut order: Vec<usize> = (0..ops.len()).collect();
        for at in (1..order.len()).rev() {
            let other = usize::try_from(rng.next_u64() % (at as u64 + 1)).expect("small");
            order.swap(at, other);
        }
        for &at in &order {
            let (key, payload, _) = &ops[at];
            let delivery = FabricGate::complete(&gate, *key, *payload, Nanos(0));
            assert_eq!(delivery, Delivery::Delivered, "round {round}: key {key:#x}");
        }
        for (key, payload, mut wait) in ops {
            let polled = Pin::new(&mut wait).poll(&mut cx);
            assert_eq!(polled, Poll::Ready(payload), "round {round}: key {key:#x}'s own value");
        }
    }
    let gate = gate.borrow();
    assert_eq!(gate.stale(), 0, "gate_stale: no completion missed its holder");
    // Each slot starts two generations below the field, so its first two takes are below it and
    // every later one past it: the tap counts exactly those, no more.
    assert_eq!(gate.wrapped_takes(), (ROUNDS - 2) * u64::from(HOLDERS), "the takes past the field");
}

/// A permit dropped unregistered, as a refused stage drops it, on a slot at the generation's
/// last value: the named reservation's drop advances by the slab's policy, which wraps there
/// (ADR-0194 A2), so the slot is free at the first generation and the dropped key names no
/// later occupant. Every holder starts at `u32::MAX`; the next permit reads the same slot, its
/// key's generation field at 0, and the occupant's take is not one past the field.
#[test]
fn a_dropped_permit_at_the_last_generation_leaves_its_slot_at_the_first() {
    const INDEX: u64 = (1 << GATE_KEY_INDEX_BITS) - 1;
    let generation = |key: u64| (key >> GATE_KEY_INDEX_BITS) & u64::from(FIELD - 1);
    let (census, _mark) = CapCensus::new(CellId(2));
    let cap = Cap::entries::<{ HOLDERS }>("test-gate-holders", CapFill::Serving);
    let gate = RefCell::new(
        FabricGate::<u64>::build_at_generation(2, HOLDERS, Some(cap), &census, u32::MAX)
            .expect("the test gate"),
    );
    let units = gate.borrow().units(1).expect("one unit");
    let dropped = {
        let mut borrowed = gate.borrow_mut();
        let FabricAdmission::Granted(permit) = borrowed.reserve(&units) else {
            panic!("a unit and a holder are free");
        };
        permit.key()
    };
    assert_eq!(generation(dropped), u64::from(FIELD - 1), "engagement: the field's last value");
    assert_eq!(units.free(), 1, "the dropped permit's unit returns");
    assert_eq!(gate.borrow().holders(), 0, "the dropped permit's holder returns");
    let (key, mut wait) = {
        let mut borrowed = gate.borrow_mut();
        let FabricAdmission::Granted(permit) = borrowed.reserve(&units) else {
            panic!("the returned unit and holder are free");
        };
        (permit.key(), permit.register(&gate, None))
    };
    assert_eq!(key & INDEX, dropped & INDEX, "engagement: the dropped permit's slot");
    assert_eq!(generation(key), 0, "the dropped permit's slot wrapped to the first generation");
    assert_eq!(FabricGate::complete(&gate, dropped, 1, Nanos(0)), Delivery::Orphan);
    assert_eq!(gate.borrow().stale(), 1, "the dropped key names no later occupant");
    assert_eq!(FabricGate::complete(&gate, key, 2, Nanos(0)), Delivery::Delivered);
    let mut cx = Context::from_waker(Waker::noop());
    assert_eq!(Pin::new(&mut wait).poll(&mut cx), Poll::Ready(2));
    assert_eq!(gate.borrow().wrapped_takes(), 0, "the occupant's generation is below the field");
}
