//! Mesh + credit flow control (M0-S09): exact producer-side bounds, credit
//! return via replies, doorbell signaling, reserved reply headroom, and a
//! threaded all-to-all smoke (the 10⁷-op DST deadlock battery proper lands
//! with the simulator, M0-S20 — noted in the milestone plan).

// The mesh is not loom-modeled (only the ring is); skip under `--cfg loom`.
#![cfg(not(loom))]

use std::collections::HashMap;

use inf_fabric::{CellFabric, FabricToken, Mesh, MeshConfig, Op, Outcome, SendError};
use inf_foundation::{CellId, KeySlot};

fn small_mesh(cells: u16, ring_capacity: usize, data_credits: u32) -> Vec<CellFabric> {
    Mesh::new(cells, MeshConfig { ring_capacity, data_credits })
}

fn read_op(token: FabricToken, key: &[u8]) -> Op<'_> {
    Op::Read { token, slot: KeySlot::of_key(key), key }
}

#[test]
fn send_flush_drain_reply_returns_credit() {
    let mut cells = small_mesh(2, 8, 4);
    let mut b = cells.pop().expect("cell 1");
    let mut a = cells.pop().expect("cell 0");
    let to_b = CellId(1);
    let to_a = CellId(0);

    let token = a.next_token();
    assert_eq!(token.origin_cell(), to_a);
    a.send(to_b, &read_op(token, b"user:1")).expect("credits available");
    assert_eq!(a.outstanding(to_b), 1);
    assert_eq!(a.staged_frames(), 1);

    assert_eq!(a.flush(), 1);
    assert_eq!(a.staged_frames(), 0);
    assert!(b.doorbell_pending(), "flush must ring the doorbell");

    // B drains the Read, replies.
    let mut seen = Vec::new();
    let drained = b.drain(64, |from, op| {
        if let Op::Read { token, key, .. } = op {
            seen.push((from, token, key.to_vec()));
        } else {
            panic!("unexpected op: {op:?}");
        }
    });
    assert_eq!(drained, 1);
    assert!(!b.doorbell_pending(), "drain consumes the doorbell");
    let (from, token, key) = seen.pop().expect("one read");
    assert_eq!((from, key.as_slice()), (to_a, b"user:1".as_slice()));

    b.reply(to_a, token, &Outcome::Bytes(b"value"));
    assert_eq!(b.flush(), 1);

    // A drains the reply: credit comes back BEFORE the callback runs.
    let mut replies = 0;
    a.drain(64, |from, op| {
        assert_eq!(from, to_b);
        assert!(matches!(op, Op::Reply { outcome: Outcome::Bytes(b"value"), .. }));
        replies += 1;
    });
    assert_eq!(replies, 1);
    assert_eq!(a.outstanding(to_b), 0, "reply returned the data credit");
}

#[test]
fn no_credit_is_exact_and_recovers_after_replies() {
    let credits = 3u32;
    let mut cells = small_mesh(2, 8, credits);
    let mut b = cells.pop().expect("cell 1");
    let mut a = cells.pop().expect("cell 0");
    let to_b = CellId(1);
    let to_a = CellId(0);

    let mut tokens = Vec::new();
    for _ in 0..credits {
        let token = a.next_token();
        tokens.push(token);
        a.send(to_b, &read_op(token, b"k")).expect("within budget");
    }
    // Budget exhausted: the 4th send is refused with exact accounting.
    let token = a.next_token();
    assert_eq!(
        a.send(to_b, &read_op(token, b"k")),
        Err(SendError::NoCredit { needed: 1, available: 0 })
    );
    assert_eq!(a.outstanding(to_b), credits);

    // Replies restore the budget one by one.
    a.flush();
    b.drain(64, |_, _| {});
    for token in tokens {
        b.reply(to_a, token, &Outcome::Nil);
    }
    b.flush();
    a.drain(64, |_, _| {});
    assert_eq!(a.outstanding(to_b), 0);
    a.send(to_b, &read_op(token, b"k")).expect("budget recovered");
}

#[test]
fn batch_costs_one_credit_per_nested_op() {
    let mut cells = small_mesh(2, 16, 4);
    let _b = cells.pop();
    let mut a = cells.pop().expect("cell 0");
    let to_b = CellId(1);

    let t1 = a.next_token();
    let t2 = a.next_token();
    let t3 = a.next_token();
    let batch = Op::Batch { ops: vec![read_op(t1, b"a"), read_op(t2, b"b"), read_op(t3, b"c")] };
    a.send(to_b, &batch).expect("3 of 4 credits");
    assert_eq!(a.outstanding(to_b), 3, "batch consumes per-op credits");

    let t4 = a.next_token();
    let t5 = a.next_token();
    let two = Op::Batch { ops: vec![read_op(t4, b"d"), read_op(t5, b"e")] };
    assert_eq!(
        a.send(to_b, &two),
        Err(SendError::NoCredit { needed: 2, available: 1 }),
        "partial budgets must not admit partial batches"
    );
}

#[test]
fn slow_consumer_growth_is_bounded_by_credits_exactly() {
    // Cell B never drains. A's producer-side footprint must cap at exactly
    // `data_credits` in-flight frames (ring + staged), then refuse — the
    // M0-S09 bounded-memory AC, asserted exactly.
    let credits = 16u32;
    let mut cells = small_mesh(2, 64, credits);
    let _b_stalled = cells.pop().expect("cell 1");
    let mut a = cells.pop().expect("cell 0");
    let to_b = CellId(1);

    let mut accepted = 0u32;
    for i in 0..credits * 10 {
        let token = a.next_token();
        let key = i.to_le_bytes();
        match a.send(to_b, &read_op(token, &key)) {
            Ok(()) => accepted += 1,
            Err(SendError::NoCredit { .. }) => break,
        }
        a.flush();
    }
    assert_eq!(accepted, credits, "exactly data_credits ops admitted, not one more");
    assert_eq!(a.outstanding(to_b), credits);
    // Everything in flight sits in the ring (or staged) — both bounded.
    assert!(a.staged_frames() <= credits as usize);
    // Continued pressure stays refused; no hidden queue grows.
    for _ in 0..1000 {
        let token = a.next_token();
        assert!(a.send(to_b, &read_op(token, b"x")).is_err());
    }
    assert_eq!(a.staged_frames(), 0, "refused sends must stage nothing");
}

#[test]
fn replies_always_fit_reserved_headroom_under_full_duplex_saturation() {
    // Both directions saturate their data credits, then both reply to
    // everything. With ring_capacity == 2 × data_credits the replies must
    // flush completely without a single retry stall — the deadlock-freedom
    // sizing invariant, exercised at the boundary.
    let credits = 8u32;
    let mut cells = small_mesh(2, 16, credits);
    let mut b = cells.pop().expect("cell 1");
    let mut a = cells.pop().expect("cell 0");
    let to_b = CellId(1);
    let to_a = CellId(0);

    for _ in 0..credits {
        let ta = a.next_token();
        a.send(to_b, &read_op(ta, b"from-a")).expect("a within budget");
        let tb = b.next_token();
        b.send(to_a, &read_op(tb, b"from-b")).expect("b within budget");
    }
    a.flush();
    b.flush();

    let mut a_owes = Vec::new();
    let mut b_owes = Vec::new();
    b.drain(64, |_, op| {
        if let Op::Read { token, .. } = op {
            b_owes.push(token);
        }
    });
    a.drain(64, |_, op| {
        if let Op::Read { token, .. } = op {
            a_owes.push(token);
        }
    });
    assert_eq!((a_owes.len(), b_owes.len()), (credits as usize, credits as usize));

    for token in b_owes {
        b.reply(to_a, token, &Outcome::Ok);
    }
    for token in a_owes {
        a.reply(to_b, token, &Outcome::Ok);
    }
    // Packed transport: the count of published SLOTS varies with packing;
    // the invariant is that every reply flushes completely (no retry stall),
    // proven by zero remaining staged transport units.
    assert!(b.flush() >= 1, "replies fit the reserved headroom");
    assert!(a.flush() >= 1);
    assert_eq!(b.staged_frames(), 0);
    assert_eq!(a.staged_frames(), 0);

    a.drain(64, |_, _| {});
    b.drain(64, |_, _| {});
    assert_eq!(a.outstanding(to_b), 0);
    assert_eq!(b.outstanding(to_a), 0);
}

#[test]
fn spill_tripwire_counts_oversize_frames() {
    // Packed transport (M0-R1): spills are counted per sealed SLOT — one
    // heap allocation amortized over the whole pack, surfaced at seal time
    // (flush or pack-cap), not per send.
    let mut cells = small_mesh(2, 8, 4);
    let _b = cells.pop();
    let mut a = cells.pop().expect("cell 0");
    let token = a.next_token();
    let big_key = vec![0xEEu8; 200]; // > INLINE_MSG_CAP once framed
    a.send(CellId(1), &read_op(token, &big_key)).expect("send");
    a.flush();
    assert_eq!(a.stats().spilled_frames, 1, "oversize pack spills at seal");
    let token = a.next_token();
    a.send(CellId(1), &read_op(token, b"small")).expect("send");
    a.flush();
    assert_eq!(a.stats().spilled_frames, 1, "inline-size packs never spill");
}

/// All-to-all threaded smoke: every cell sends `OPS` reads to every peer
/// under credit pressure, every op gets exactly one reply, nothing stalls,
/// and all credits are restored at quiescence. Dev-tier stand-in for the
/// DST deadlock battery (M0-S20).
///
/// Miri-excluded (measured: this one test cost ~20 min of a ~21 min Miri
/// run): it is an integration smoke, not an unsafe-code probe — the ring's
/// memory-model coverage lives in the smaller Miri tests + the Loom models,
/// and this scenario runs natively in every workspace test pass and at
/// 10⁷-op scale in the DST battery.
#[test]
#[cfg_attr(miri, ignore = "threaded integration smoke; Miri covers the unsafe leaves directly")]
fn threaded_all_to_all_saturation_quiesces() {
    const CELLS: u16 = 4;
    const OPS_PER_PEER: u64 = 5_000;

    let fabrics = small_mesh(CELLS, 64, 16);
    let handles: Vec<_> = fabrics
        .into_iter()
        .map(|mut fabric| {
            std::thread::spawn(move || {
                let me = fabric.cell();
                let peers: Vec<CellId> = (0..CELLS).map(CellId).filter(|c| *c != me).collect();
                let mut to_send: HashMap<CellId, u64> =
                    peers.iter().map(|p| (*p, OPS_PER_PEER)).collect();
                let mut replies_seen = 0u64;
                let want_replies = OPS_PER_PEER * peers.len() as u64;
                let mut replies_owed: Vec<(CellId, FabricToken)> = Vec::new();

                let mut spins = 0u64;
                while replies_seen < want_replies
                    || to_send.values().any(|&n| n > 0)
                    || !replies_owed.is_empty()
                {
                    // SEND under credit pressure.
                    for peer in &peers {
                        let left = to_send.get_mut(peer).expect("peer entry");
                        while *left > 0 {
                            let token = fabric.next_token();
                            let key = (*left).to_le_bytes();
                            match fabric.send(*peer, &read_op(token, &key)) {
                                Ok(()) => *left -= 1,
                                Err(SendError::NoCredit { .. }) => break,
                            }
                        }
                    }
                    // REPLY everything owed (always sendable).
                    for (peer, token) in replies_owed.drain(..) {
                        fabric.reply(peer, token, &Outcome::Int(1));
                    }
                    fabric.flush();
                    // DRAIN: collect new obligations + count our replies.
                    let drained = fabric.drain(256, |from, op| match op {
                        Op::Read { token, .. } => replies_owed.push((from, token)),
                        Op::Reply { outcome, .. } => {
                            assert_eq!(outcome, Outcome::Int(1));
                            replies_seen += 1;
                        }
                        other => panic!("unexpected fabric op: {other:?}"),
                    });
                    if drained == 0 {
                        spins += 1;
                        assert!(
                            spins < 50_000_000,
                            "cell {me}: no progress — possible deadlock \
                             (replies {replies_seen}/{want_replies})"
                        );
                        std::hint::spin_loop();
                    } else {
                        spins = 0;
                    }
                }
                // Quiescence: all credits restored toward every peer.
                let mut final_flush = fabric.flush();
                while final_flush > 0 {
                    final_flush = fabric.flush();
                }
                for peer in &peers {
                    assert_eq!(
                        fabric.outstanding(*peer),
                        0,
                        "cell {me}: outstanding ops toward {peer} after quiescence"
                    );
                }
                (replies_seen, fabric.stats())
            })
        })
        .collect();

    for handle in handles {
        let (replies, stats) = handle.join().expect("cell thread");
        assert_eq!(replies, OPS_PER_PEER * u64::from(CELLS - 1));
        assert_eq!(stats.decode_errors, 0);
    }
}

#[test]
#[should_panic(expected = "replies must always have headroom")]
fn mesh_rejects_undersized_rings() {
    let _ = Mesh::new(2, MeshConfig { ring_capacity: 16, data_credits: 9 });
}

#[test]
#[should_panic(expected = "Op::Reply goes through CellFabric::reply")]
fn send_rejects_reply_ops() {
    let mut cells = small_mesh(2, 8, 4);
    let _b = cells.pop();
    let mut a = cells.pop().expect("cell 0");
    let token = a.next_token();
    let _ = a.send(CellId(1), &Op::Reply { token, outcome: Outcome::Ok });
}
