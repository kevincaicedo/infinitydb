//! The fabric key past its generation field (ADR-0194 A2, ADR-0177 D4): a fabric gate whose
//! holder slots start at 2^23 − 2, the test as the peer. Each holder serves 32 requests, so
//! every slot's generation crosses the key's 23-bit field, and each request is completed with
//! its own key's word, out of order. Every value reaches its own wait, and no completion is
//! stale. Built only with `fault-points`, the slab's first-generation hook (`just check` runs
//! it apart).

use std::cell::RefCell;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use inf_foundation::CellId;
use inf_foundation::bounded::{Cap, CapCensus, CapFill};
use inf_foundation::limits::GATE_KEY_GENERATION_BITS;
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
            ops.push((key, (round << 32) | leg, permit.register(&gate)));
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
    assert!(
        gate.wrapped_takes() >= u64::from(HOLDERS),
        "VACUOUS: {} takes past the field, fewer than one a holder",
        gate.wrapped_takes()
    );
}
